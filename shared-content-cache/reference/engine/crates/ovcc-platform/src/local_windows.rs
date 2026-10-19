// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::ptr::{null, null_mut};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{ERROR_PIPE_BUSY, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READONLY,
    UnmapViewOfFile,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

pub(super) fn default_endpoint() -> io::Result<PathBuf> {
    let sid = user_sid(unsafe { GetCurrentProcess() })?;
    Ok(PathBuf::from(format!(r"\\.\pipe\ovcc-local-{sid}")))
}

fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn user_sid(process: HANDLE) -> io::Result<String> {
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = owned(token)?;
        let mut size = 0;
        GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut size);
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text = null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let allocation = LocalAllocation(text.cast());
        let mut length = 0;
        while *text.add(length) != 0 {
            length += 1;
        }
        let sid = String::from_utf16(std::slice::from_raw_parts(text, length))
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        drop(allocation);
        Ok(sid)
    }
}

struct UserSecurity(LocalAllocation);
impl UserSecurity {
    fn new() -> io::Result<Self> {
        let sid = user_sid(unsafe { GetCurrentProcess() })?;
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})\0").encode_utf16().collect();
        let mut descriptor = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(LocalAllocation(descriptor)))
    }
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: 0,
        }
    }
}

fn create_pipe(path: &Path, first: bool) -> io::Result<NamedPipeServer> {
    let security = UserSecurity::new()?;
    let mut attributes = security.attributes();
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                path,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    }
}

pub struct Listener {
    pending: NamedPipeServer,
    path: PathBuf,
}
impl Listener {
    /// Binds the current user's default local server with the same-user ACL.
    pub fn bind_default() -> io::Result<Self> {
        Self::bind(&default_endpoint()?)
    }

    pub fn bind(path: &Path) -> io::Result<Self> {
        if !path.as_os_str().to_string_lossy().starts_with(r"\\.\pipe\") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a local Windows pipe name",
            ));
        }
        Ok(Self {
            pending: create_pipe(path, true)?,
            path: path.to_owned(),
        })
    }
    pub async fn accept(&mut self) -> io::Result<Connection> {
        self.pending.connect().await?;
        let next = create_pipe(&self.path, false)?;
        let stream = std::mem::replace(&mut self.pending, next);
        Ok(Connection {
            stream: Pipe::Server(stream),
        })
    }
}

pub(super) enum Pipe {
    Server(NamedPipeServer),
    Client(NamedPipeClient),
}
impl AsyncRead for Pipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Server(s) => Pin::new(s).poll_read(cx, buf),
            Self::Client(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}
impl AsyncWrite for Pipe {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Server(s) => Pin::new(s).poll_write(cx, buf),
            Self::Client(s) => Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Server(s) => Pin::new(s).poll_flush(cx),
            Self::Client(s) => Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Server(s) => Pin::new(s).poll_shutdown(cx),
            Self::Client(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

pub struct Connection {
    pub(super) stream: Pipe,
}
impl Connection {
    pub async fn connect(path: &Path) -> io::Result<Self> {
        let stream = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match ClientOptions::new().open(path) {
                    Ok(stream) => return Ok::<_, io::Error>(stream),
                    Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    Err(error) => return Err(error),
                }
            }
        })
        .await??;
        let mut pid = 0;
        if unsafe { GetNamedPipeServerProcessId(stream.as_raw_handle(), &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let process = owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })?;
        if user_sid(process.as_raw_handle())? != user_sid(unsafe { GetCurrentProcess() })? {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(Self {
            stream: Pipe::Client(stream),
        })
    }
}

pub struct ReadOnlyMapping {
    address: MEMORY_MAPPED_VIEW_ADDRESS,
    length: usize,
}

// SAFETY: the unsafe constructor requires immutable backing bytes; a mapped
// view is process-wide and its unique owner unmaps only after all borrows end.
unsafe impl Send for ReadOnlyMapping {}
unsafe impl Sync for ReadOnlyMapping {}
impl ReadOnlyMapping {
    unsafe fn from_section(section: HANDLE, length: usize) -> io::Result<Self> {
        if length == 0 || length > isize::MAX as usize {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let address = unsafe { MapViewOfFile(section, FILE_MAP_READ, 0, 0, length) };
        if address.Value.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { address, length })
    }

    /// # Safety
    /// The file must remain immutable for the lifetime of the returned mapping.
    pub unsafe fn new(file: &File, length: usize) -> io::Result<Self> {
        if length as u64 > file.metadata()?.len() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let section = owned(unsafe {
            CreateFileMappingW(file.as_raw_handle(), null(), PAGE_READONLY, 0, 0, null())
        })?;
        unsafe { Self::from_section(section.as_raw_handle(), length) }
    }

    pub fn as_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.address.Value.cast::<u8>(), self.length) }
    }
}
impl Drop for ReadOnlyMapping {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(self.address);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_get_busy_pipe_waits_for_an_available_instance() -> io::Result<()> {
        // Arrange.
        let endpoint = PathBuf::from(format!(
            r"\\.\pipe\ovcc-busy-{:032x}",
            rand::random::<u128>()
        ));
        let mut listener = Listener::bind(&endpoint)?;
        let _first = Connection::connect(&endpoint).await?;
        let second = Connection::connect(&endpoint);
        tokio::pin!(second);
        // Act.
        let early = tokio::time::timeout(std::time::Duration::from_millis(50), &mut second).await;
        // Assert.
        assert!(early.is_err(), "busy pipe must wait for the accepting task");
        let _accepted_first = listener.accept().await?;
        let _second = tokio::time::timeout(std::time::Duration::from_secs(2), second).await??;
        let _accepted_second = listener.accept().await?;
        Ok(())
    }
}
