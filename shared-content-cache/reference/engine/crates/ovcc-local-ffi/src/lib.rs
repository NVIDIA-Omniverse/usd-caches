// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Experimental, lockstep C bridge for the Python and C++ path clients.
//! No file is opened here. Returned values describe the server's deadline promise.

use std::ffi::{CStr, CString, c_char};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::time::Duration;

use ovcc_local::{LeaseRequest, LeaseResponse, LeaseResult, Outcome, SharedClient};
use tokio::runtime::{Builder, Runtime};

#[repr(C)]
pub struct Header {
    pub name: *const c_char,
    pub value: *const c_char,
}

#[repr(C)]
pub struct Request {
    pub authority: *const c_char,
    pub path: *const c_char,
    pub query: *const c_char,
    pub headers: *const Header,
    pub header_count: usize,
    pub nucleus: u8,
    pub lease_duration_millis: u64,
}

#[repr(C)]
pub struct Error {
    pub code: i32,
    pub message: [c_char; 256],
}

/// All strings are UTF-8; pointers borrow a Response until response_free.
#[repr(C)]
pub struct ResponseView {
    pub outcome: u32,
    pub status: u16,
    pub generation: u64,
    pub offset: u64,
    pub length: u64,
    pub deadline_unix_millis: u64,
    pub path: *const c_char,
    pub cache_status: *const c_char,
    pub headers: *const Header,
    pub header_count: usize,
}

pub struct LocalClient {
    runtime: Runtime,
    client: SharedClient,
    timeout: Duration,
}

pub struct Response {
    response: LeaseResponse,
    path: Option<CString>,
    cache_status: Option<CString>,
    _strings: Vec<(CString, CString)>,
    headers: Vec<Header>,
}

impl Response {
    fn new(response: LeaseResponse) -> Result<Self, Failure> {
        let path = response
            .path
            .as_ref()
            .map(|p| {
                p.to_str()
                    .ok_or(Failure(3, "path is not UTF-8"))
                    .and_then(cstring)
            })
            .transpose()?;
        let cache_status = response.cache_status.as_deref().map(cstring).transpose()?;
        let strings: Vec<_> = response
            .headers
            .iter()
            .map(|(name, value)| Ok((cstring(name)?, cstring(value)?)))
            .collect::<Result<_, Failure>>()?;
        let headers = strings
            .iter()
            .map(|(name, value)| Header {
                name: name.as_ptr(),
                value: value.as_ptr(),
            })
            .collect();
        Ok(Self {
            response,
            path,
            cache_status,
            _strings: strings,
            headers,
        })
    }

    fn view(&self) -> ResponseView {
        ResponseView {
            outcome: match self.response.outcome {
                Outcome::Ready => 0,
                Outcome::NotLeasable => 1,
                Outcome::UnsupportedLayout => 2,
                Outcome::Rejected => 3,
                Outcome::Expired => 4,
            },
            status: self.response.status,
            generation: self.response.generation,
            offset: self.response.offset,
            length: self.response.length,
            deadline_unix_millis: self.response.deadline_unix_millis,
            path: self.path.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
            cache_status: self
                .cache_status
                .as_ref()
                .map_or(ptr::null(), |s| s.as_ptr()),
            headers: self.headers.as_ptr(),
            header_count: self.headers.len(),
        }
    }
}

#[derive(Debug)]
struct Failure(i32, &'static str);

fn cstring(value: &str) -> Result<CString, Failure> {
    CString::new(value).map_err(|_| Failure(3, "response contains a NUL byte"))
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::TimedOut => Self(2, "local access timed out"),
            io::ErrorKind::InvalidData => Self(3, "invalid local response"),
            io::ErrorKind::NotFound => Self(4, "local server unavailable"),
            io::ErrorKind::PermissionDenied => Self(5, "local access denied"),
            _ => Self(1, "local access I/O failure"),
        }
    }
}

unsafe fn boundary(error: *mut Error, action: impl FnOnce() -> Result<(), Failure>) -> i32 {
    let result =
        catch_unwind(AssertUnwindSafe(action)).unwrap_or(Err(Failure(7, "native client failure")));
    let failure = result.err().unwrap_or(Failure(0, ""));
    if !error.is_null() {
        let mut value = Error {
            code: failure.0,
            message: [0; 256],
        };
        for (to, from) in value.message.iter_mut().zip(failure.1.bytes()) {
            *to = from as c_char;
        }
        unsafe {
            error.write(value);
        }
    }
    failure.0
}

unsafe fn text(value: *const c_char) -> Result<String, Failure> {
    if value.is_null() {
        return Ok(String::new());
    }
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| Failure(8, "request strings must be UTF-8"))
}

unsafe fn request(value: *const Request) -> Result<LeaseRequest, Failure> {
    let value = unsafe { value.as_ref() }.ok_or(Failure(8, "missing request"))?;
    if value.header_count > 1024
        || (value.header_count > 0 && value.headers.is_null())
        || value.nucleus > 1
        || !(1..=300_000).contains(&value.lease_duration_millis)
    {
        return Err(Failure(8, "invalid request fields"));
    }
    let mut request = LeaseRequest::new(unsafe { text(value.authority)? }, unsafe {
        text(value.path)?
    });
    request.query = unsafe { text(value.query)? };
    request.nucleus = value.nucleus != 0;
    request.lease_duration_millis = value.lease_duration_millis;
    if value.header_count != 0 {
        for header in unsafe { std::slice::from_raw_parts(value.headers, value.header_count) } {
            request
                .headers
                .push((unsafe { text(header.name)? }, unsafe {
                    text(header.value)?
                }));
        }
    }
    Ok(request)
}

/// Identifies this bridge layout, not a public ABI stability promise.
#[unsafe(no_mangle)]
pub extern "C" fn ovcc_local_abi_version() -> u32 {
    1
}

/// Creates a lazy same-user client; timeout includes queueing and the exchange.
/// # Safety
/// Output and optional error pointers must be writable and correctly aligned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ovcc_local_client_new_v1(
    timeout_ms: u64,
    output: *mut *mut LocalClient,
    error: *mut Error,
) -> i32 {
    unsafe {
        ovcc_local_client_new_with_pool_v1(
            timeout_ms,
            ovcc_local::DEFAULT_MAX_CONNECTIONS,
            output,
            error,
        )
    }
}

/// Creates a lazy same-user client with a configurable shared connection cap.
/// # Safety
/// Output and optional error pointers must be writable and correctly aligned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ovcc_local_client_new_with_pool_v1(
    timeout_ms: u64,
    max_connections: usize,
    output: *mut *mut LocalClient,
    error: *mut Error,
) -> i32 {
    unsafe {
        boundary(error, || {
            if output.is_null() {
                return Err(Failure(8, "missing output"));
            }
            output.write(ptr::null_mut());
            if timeout_ms == 0 || timeout_ms > 86_400_000 {
                return Err(Failure(
                    8,
                    "timeout must be between 1 and 86400000 milliseconds",
                ));
            }
            let client = SharedClient::new(max_connections)
                .map_err(|_| Failure(8, "invalid connection limit"))?;
            let runtime = Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()?;
            output.write(Box::into_raw(Box::new(LocalClient {
                runtime,
                client,
                timeout: Duration::from_millis(timeout_ms),
            })));
            Ok(())
        })
    }
}

/// Returns copied metadata without opening the file or retrying the request.
/// Ordinary unavailable replies are successful exchanges with a non-ready outcome.
/// # Safety
/// Input pointers must stay valid through the call; output/error must be writable.
/// Concurrent calls may share a client, but destruction must not race a call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ovcc_local_get_v1(
    client: *const LocalClient,
    input: *const Request,
    output: *mut *mut Response,
    error: *mut Error,
) -> i32 {
    unsafe {
        boundary(error, || {
            if output.is_null() {
                return Err(Failure(8, "missing output"));
            }
            output.write(ptr::null_mut());
            let client = client.as_ref().ok_or(Failure(8, "missing client"))?;
            let request = request(input)?;
            let result = client.runtime.block_on(async {
                tokio::time::timeout(client.timeout, client.client.lease(&request))
                    .await
                    .map_err(|_| Failure(2, "local access timed out"))?
                    .map_err(Failure::from)
            })?;
            let response = match result {
                LeaseResult::Ready(lease) => lease.into_response(),
                LeaseResult::Unavailable(response) => response,
            };
            output.write(Box::into_raw(Box::new(Response::new(response)?)));
            Ok(())
        })
    }
}

/// Frees local client resources; returned metadata is independent.
/// # Safety
/// Client must be null or an owned pointer from new_v1, with no active calls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ovcc_local_client_free_v1(client: *mut LocalClient) {
    if !client.is_null() {
        drop(unsafe { Box::from_raw(client) });
    }
}

/// Borrows a response's metadata.
/// # Safety
/// Response must be live and output must point to writable ResponseView storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ovcc_local_response_view_v1(
    response: *const Response,
    output: *mut ResponseView,
) {
    unsafe {
        output.write((&*response).view());
    }
}

/// Frees metadata only; never releases a file deadline on the server.
/// # Safety
/// Response must be null or an owned response with no remaining view borrows.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ovcc_local_response_free_v1(response: *mut Response) {
    if !response.is_null() {
        drop(unsafe { Box::from_raw(response) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_get_bridge_parallel_calls_do_not_serialize() -> io::Result<()> {
        // Arrange.
        let mut pointer = ptr::null_mut();
        let code = unsafe { ovcc_local_client_new_v1(3000, &mut pointer, ptr::null_mut()) };
        assert_eq!(code, 0);
        let client = std::sync::Arc::from(unsafe { Box::from_raw(pointer) });
        let server = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        server.block_on(async {
            let mut listener = ovcc_platform::local::Listener::bind_default()?;
            let mut calls = Vec::new();
            // Act.
            for _ in 0..2 {
                let client = std::sync::Arc::clone(&client);
                calls.push(tokio::task::spawn_blocking(move || {
                    let input = Request {
                        authority: c"pool.test".as_ptr(),
                        path: c"/foo.usd".as_ptr(),
                        query: ptr::null(),
                        headers: ptr::null(),
                        header_count: 0,
                        nucleus: 0,
                        lease_duration_millis: 1000,
                    };
                    let mut output = ptr::null_mut();
                    let code = unsafe {
                        ovcc_local_get_v1(&*client, &input, &mut output, ptr::null_mut())
                    };
                    unsafe { ovcc_local_response_free_v1(output) };
                    code
                }));
            }
            let result = tokio::time::timeout(Duration::from_secs(2), async {
                let mut first = listener.accept().await?;
                ovcc_local::read_request(&mut first).await?;
                // Withholds replies until both calls have their own connection.
                let mut second = listener.accept().await?;
                ovcc_local::read_request(&mut second).await?;
                let reply = LeaseResponse {
                    version: 1,
                    outcome: Outcome::Rejected,
                    status: 403,
                    headers: vec![],
                    cache_status: None,
                    generation: 0,
                    offset: 0,
                    length: 0,
                    path: None,
                    deadline_unix_millis: 0,
                };
                ovcc_local::write_response(&mut first, &reply).await?;
                ovcc_local::write_response(&mut second, &reply).await
            })
            .await;
            let mut codes = Vec::new();
            for call in calls {
                codes.push(call.await.map_err(io::Error::other)?);
            }
            // Assert.
            result.map_err(|_| {
                io::Error::other("parallel calls serialized behind the first reply")
            })??;
            assert_eq!(codes, [0, 0]);
            Ok(())
        })
    }

    #[test]
    fn local_get_bridge_preserves_request_fields() -> io::Result<()> {
        // Arrange.
        let authority = c"objects.test";
        let path = c"/foo.usd";
        let query = c"version=example";
        let headers = [
            Header {
                name: c"x-test".as_ptr(),
                value: c"one".as_ptr(),
            },
            Header {
                name: c"x-test".as_ptr(),
                value: c"two".as_ptr(),
            },
        ];
        let input = Request {
            authority: authority.as_ptr(),
            path: path.as_ptr(),
            query: query.as_ptr(),
            headers: headers.as_ptr(),
            header_count: headers.len(),
            nucleus: 1,
            lease_duration_millis: 1234,
        };
        // Act.
        let result = unsafe { request(&input) }.map_err(|e| io::Error::other(e.1))?;
        // Assert.
        assert_eq!(result.authority, "objects.test");
        assert_eq!(result.path, "/foo.usd");
        assert_eq!(result.query, "version=example");
        assert_eq!(
            result.headers,
            [
                ("x-test".into(), "one".into()),
                ("x-test".into(), "two".into())
            ]
        );
        assert!(result.nucleus);
        assert_eq!(result.lease_duration_millis, 1234);
        Ok(())
    }

    #[test]
    fn local_get_bridge_rejects_invalid_arguments() {
        // Arrange.
        let mut output = ptr::null_mut();
        let mut failure = Error {
            code: 0,
            message: [0; 256],
        };
        // Act.
        let code = unsafe { ovcc_local_client_new_v1(0, &mut output, &mut failure) };
        // Assert.
        assert_eq!(code, 8);
        assert_eq!(failure.code, 8);
        assert!(output.is_null());

        // Arrange.
        let mut response = ptr::null_mut();
        // Act.
        let code =
            unsafe { ovcc_local_get_v1(ptr::null(), ptr::null(), &mut response, &mut failure) };
        // Assert.
        assert_eq!(code, 8);
        assert!(response.is_null());
    }

    #[test]
    fn local_get_bridge_preserves_metadata_and_rejects_nul() -> io::Result<()> {
        // Arrange.
        let reply = LeaseResponse {
            version: 1,
            outcome: Outcome::Rejected,
            status: 403,
            headers: vec![
                ("x-test".into(), "first".into()),
                ("x-test".into(), "second".into()),
            ],
            cache_status: Some("BYPASS".into()),
            generation: u64::MAX,
            offset: 9,
            length: 4,
            path: None,
            deadline_unix_millis: 123,
        };
        // Act.
        let response = Response::new(reply).map_err(|e| io::Error::other(e.1))?;
        let view = response.view();
        // Assert.
        assert_eq!(view.outcome, 3);
        assert_eq!(view.status, 403);
        assert_eq!(view.generation, u64::MAX);
        assert_eq!(
            (view.offset, view.length, view.deadline_unix_millis),
            (9, 4, 123)
        );
        assert_eq!(view.header_count, 2);
        assert!(view.path.is_null());
        assert_eq!(
            unsafe { CStr::from_ptr((*view.headers.add(1)).value) }.to_bytes(),
            b"second"
        );
        assert!(cstring("invalid\0text").is_err());
        Ok(())
    }
}
