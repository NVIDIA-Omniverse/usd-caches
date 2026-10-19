// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fmt::{self, Debug, Display, Formatter};
use std::ops::Deref;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use hub_cache::block::ConnectRequest;
use hub_cache::block::PutRequest;
use tracing::{debug, error, instrument, trace};
use uuid::Uuid;

use hub_rpc::block_cache as bc;
use hub_rpc::cache_aside as ca;
use hub_rpc::{ConnectParams, ResultCode};

use crate::context::Context;
use crate::Result;

type FnvHashMap<K, V> = HashMap<K, V, fnv::FnvBuildHasher>;

macro_rules! check_uuid {
    ( $obj:ident ) => {{
        if $obj.client_uuid.is_nil() {
            return Err(anyhow::anyhow!(
                "Unknown client uuid. Client must call connect() first!"
            ));
        }
    }};
}

#[derive(Debug)]
enum OpenHandle {
    BlockGet(Arc<[u8]>),
    BlockPut(Arc<[u8]>),
    FileGet(Arc<[u8]>),
    FilePut(Arc<[u8]>),
}

impl Deref for OpenHandle {
    type Target = Arc<[u8]>;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::BlockGet(key) => key,
            Self::BlockPut(key) => key,
            Self::FileGet(key) => key,
            Self::FilePut(key) => key,
        }
    }
}

#[derive(Debug)]
pub struct Dispatcher {
    context: Arc<Context>,
    exiting: Arc<AtomicBool>,
    client_name: String,
    client_uuid: Uuid,
    open_handles: FnvHashMap<u64, OpenHandle>,
    next_handle: AtomicU64,
}

impl Display for Dispatcher {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "Dispatcher {{ exiting: {}, client_name: {}, client_uuid: {}, handles: {} }}",
            self.exiting.load(Ordering::Relaxed),
            self.client_name,
            self.client_uuid,
            self.open_handles.len(),
        )
    }
}

impl Drop for Dispatcher {
    #[instrument(level = "debug", skip(self), fields(%self))]
    fn drop(&mut self) {
        // if this object is dropped and we are not exiting, then client disconnected.
        // we need to clean up any open handles. get/read handles will be closed.
        // any put/write handles will be abandoned. abandoned handles will allow the client
        // to resume on reconenct or allow another client to take over, but eventually will be
        // garbage collected after some grace period

        if !self.exiting.load(Ordering::SeqCst) {
            debug!("dropping dispatcher while running");

            let mut block_close_keys = Vec::with_capacity(self.open_handles.len());
            let mut block_abandon_keys = Vec::with_capacity(self.open_handles.len());
            let mut file_close_keys = Vec::with_capacity(self.open_handles.len());
            let mut file_abandon_keys = Vec::with_capacity(self.open_handles.len());

            for (_, open_handle) in self.open_handles.iter() {
                match open_handle {
                    OpenHandle::BlockGet(key) => {
                        block_close_keys.push(key.clone());
                    },
                    OpenHandle::BlockPut(key) => {
                        block_abandon_keys.push(key.clone());
                    },
                    OpenHandle::FileGet(key) => {
                        file_close_keys.push(key.clone());
                    },
                    OpenHandle::FilePut(key) => {
                        file_abandon_keys.push(key.clone());
                    },
                }
            }

            // close open block gets
            self.context
                .cache
                .block_close_deprecated(&block_close_keys)
                .into_iter()
                .filter(Result::is_err)
                .for_each(|result| {
                    error!(?result, "error closing block");
                });

            // abandon open block puts
            match self.context.cache.block_abandon(&block_abandon_keys) {
                Ok(results) => {
                    results.into_iter().filter(Result::is_err).for_each(|result| {
                        error!(?result, "error abandoning block");
                    });
                },
                Err(e) => {
                    error!(?e, "error abandoning blocks")
                },
            }

            // close open file gets
            for key in &file_close_keys {
                if let Err(e) = self.context.cache.file_close_deprecated(key) {
                    error!(?e, "error closing file");
                }
            }

            // abandon open file puts
            for key in &file_abandon_keys {
                if let Err(e) = self.context.cache.file_abandon(key) {
                    error!(?e, "error abandoning file");
                }
            }
        } else {
            trace!("dropping dispatcher without clean up");
        }
    }
}

impl Dispatcher {
    pub fn new(context: Arc<Context>, exiting: Arc<AtomicBool>) -> Self {
        Self {
            context,
            exiting,
            client_name: String::new(),
            client_uuid: Uuid::nil(),
            open_handles: FnvHashMap::default(),
            next_handle: AtomicU64::new(1),
        }
    }

    #[instrument(level = "debug", skip_all, fields(%self))]
    pub fn connect(&mut self, params: ConnectParams) -> Result<()> {
        self.client_name = params.client_name;
        self.client_uuid = params.client_uuid;
        Ok(())
    }

    #[instrument(level = "debug", skip_all)]
    pub fn cache_connect(&mut self, params: ca::ConnectParams) -> Result<ca::ConnectResponse> {
        check_uuid!(self);

        let mut handles = Vec::with_capacity(params.open_files.len());
        let mut requests = Vec::with_capacity(params.open_files.len());

        for open_file in params.open_files.into_iter() {
            debug!(?open_file, "file connect");
            handles.push(open_file.handle);
            requests.push(ConnectRequest {
                key: to_file_key(&open_file.key, &open_file.extension),
                extension: Some(open_file.extension),
                open_for_put: open_file.open_for_write,
            });
        }

        Ok(ca::ConnectResponse {
            result: ResultCode::Success,
            file_results: self
                .context
                .cache
                .connect(&requests, &self.client_uuid)
                .into_iter()
                .enumerate()
                .map(|(index, r)| match r {
                    Ok(_) => {
                        let open_handle = match requests[index].open_for_put {
                            true => OpenHandle::FilePut(requests[index].key.clone()),
                            false => OpenHandle::FileGet(requests[index].key.clone()),
                        };
                        let res = self.open_handles.insert(handles[index], open_handle);
                        assert!(res.is_none());

                        // update next handle id
                        self.update_handle_id(handles[index]);

                        ResultCode::Success
                    },
                    Err(e) => convert_block_cache_error(e),
                })
                .collect(),
        })
    }

    #[instrument(level = "debug", skip(self), fields(%self))]
    pub fn cache_open_for_read(&mut self, params: ca::OpenForReadParams) -> Result<ca::OpenResponse> {
        check_uuid!(self);

        let key = to_file_key(&params.key, &params.extension);

        // get key from cache
        Ok(match self.context.cache.file_get(&key, &params.extension) {
            Ok(Some(local_file)) => {
                let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
                self.open_handles.insert(handle, OpenHandle::FileGet(key));
                ca::OpenResponse {
                    result: ResultCode::Success,
                    local_file: Some(local_file),
                    handle: Some(handle),
                }
            },
            Ok(None) => ca::OpenResponse {
                result: ResultCode::ErrorNotFound,
                local_file: None,
                handle: None,
            },
            Err(e) => ca::OpenResponse {
                result: convert_block_cache_error(e),
                local_file: None,
                handle: None,
            },
        })
    }

    #[instrument(level = "debug", skip(self), fields(%self))]
    pub fn cache_open_for_write(&mut self, params: ca::OpenForWriteParams) -> Result<ca::OpenResponse> {
        check_uuid!(self);

        let request = PutRequest {
            key: to_file_key(&params.key, &params.extension),
            size: params.size,
            rank: 0,
            extension: Some(params.extension),
        };

        Ok(match self.context.cache.file_put(&request, &self.client_uuid) {
            Ok(local_file) => {
                let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
                self.open_handles.insert(handle, OpenHandle::FilePut(request.key));
                ca::OpenResponse {
                    result: ResultCode::Success,
                    local_file: Some(local_file),
                    handle: Some(handle),
                }
            },
            Err(e) => ca::OpenResponse {
                result: convert_block_cache_error(e),
                local_file: None,
                handle: None,
            },
        })
    }

    #[instrument(level = "debug", skip(self), fields(%self))]
    pub fn cache_close(&mut self, params: ca::CloseParams) -> Result<ca::CloseResponse> {
        check_uuid!(self);

        Ok(match self.open_handles.remove(&params.handle) {
            Some(key) => match self.context.cache.file_close_deprecated(&key) {
                Ok(_) => ca::CloseResponse {
                    result: ResultCode::Success,
                },
                Err(e) => ca::CloseResponse {
                    result: convert_block_cache_error(e),
                },
            },
            None => ca::CloseResponse {
                result: ResultCode::ErrorHandleNotFound,
            },
        })
    }

    #[instrument(level = "debug", skip(self), fields(%self))]
    pub fn cache_remove(&mut self, params: ca::RemoveParams) -> Result<ca::RemoveResponse> {
        check_uuid!(self);

        Ok(match self.open_handles.remove(&params.handle) {
            Some(key) => match self.context.cache.file_remove_deprecated(&key) {
                Ok(_) => ca::RemoveResponse {
                    result: ResultCode::Success,
                },
                Err(e) => ca::RemoveResponse {
                    result: convert_block_cache_error(e),
                },
            },
            None => ca::RemoveResponse {
                result: ResultCode::ErrorHandleNotFound,
            },
        })
    }

    #[instrument(level = "debug", skip_all)]
    pub fn block_connect(&mut self, params: bc::ConnectParams) -> Result<bc::ConnectResponse> {
        check_uuid!(self);

        let mut handles = Vec::with_capacity(params.requests.len());
        let mut requests = Vec::with_capacity(params.requests.len());

        for open_block in params.requests.into_iter() {
            debug!(?open_block, "block connect");
            handles.push(open_block.handle);
            requests.push(ConnectRequest {
                key: to_block_key(&open_block.key),
                extension: open_block.extension,
                open_for_put: open_block.open_for_put,
            });
        }

        Ok(bc::ConnectResponse {
            results: self
                .context
                .cache
                .connect(&requests, &self.client_uuid)
                .into_iter()
                .enumerate()
                .map(|(index, r)| match r {
                    Ok(_) => {
                        let open_handle = match requests[index].open_for_put {
                            true => OpenHandle::BlockPut(requests[index].key.clone()),
                            false => OpenHandle::BlockGet(requests[index].key.clone()),
                        };
                        let res = self.open_handles.insert(handles[index], open_handle);
                        assert!(res.is_none());

                        // update next handle id
                        self.update_handle_id(handles[index]);

                        ResultCode::Success
                    },
                    Err(e) => convert_block_cache_error(e),
                })
                .collect(),
        })
    }

    #[instrument(level = "debug", skip_all, fields(%self, key_count = params.keys.len(), ?first_key=params.keys[0]))]
    pub fn block_get(&mut self, params: bc::GetParams) -> Result<bc::GetResponse> {
        check_uuid!(self);

        let keys = to_block_keylist(&params.keys);
        let results = self.context.cache.block_get(keys.as_slice());

        Ok(bc::GetResponse {
            results: results
                .locations
                .into_iter()
                .enumerate()
                .map(|(index, result)| match result {
                    Ok(l) => match l {
                        Some(location) => {
                            let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
                            self.open_handles
                                .insert(handle, OpenHandle::BlockGet(keys[index].clone()));
                            bc::OpenResult {
                                result: ResultCode::Success,
                                block: Some(bc::Block {
                                    handle,
                                    location: bc::Location {
                                        path: location.path,
                                        id: location.id,
                                        offset: location.offset,
                                        size: location.size,
                                    },
                                }),
                            }
                        },
                        None => bc::OpenResult {
                            result: ResultCode::ErrorNotFound,
                            block: None,
                        },
                    },
                    Err(e) => bc::OpenResult {
                        result: convert_block_cache_error(e),
                        block: None,
                    },
                })
                .collect(),
            base_path: results.base,
        })
    }

    #[instrument(level = "debug", skip_all, fields(%self, handle_count = params.handles.len(), first_handle=params.handles[0]))]
    pub fn block_close(&mut self, params: bc::CloseParams) -> Result<bc::CloseResponse> {
        check_uuid!(self);

        // Gather keys
        let mut keys = Vec::with_capacity(params.handles.len());
        let mut req_index = Vec::with_capacity(params.handles.len());

        // Prepoluate results so we can set individual values later
        let mut results = vec![ResultCode::Success; params.handles.len()];

        for (index, handle) in params.handles.iter().enumerate() {
            match self.open_handles.remove(handle) {
                Some(open_handle) => {
                    trace!(?open_handle, "block close");
                    keys.push(open_handle.clone());
                    req_index.push(index);
                },
                None => {
                    debug!(handle, "block close handle not found");
                    results[index] = ResultCode::ErrorHandleNotFound;
                },
            }
        }

        let result = self.context.cache.block_close_deprecated(&keys);

        // iterate the results, updating any that had error
        for (index, r) in result.into_iter().enumerate() {
            if let Err(e) = r {
                results[req_index[index]] = convert_block_cache_error(e);
            }
        }

        Ok(bc::CloseResponse { results })
    }

    #[instrument(level = "debug", skip_all, fields(%self, key_count = params.requests.len(), ?first_key=params.requests[0].key))]
    pub fn block_put(&mut self, params: bc::PutParams) -> Result<bc::PutResponse> {
        check_uuid!(self);

        let requests: Vec<PutRequest> = params
            .requests
            .into_iter()
            .map(|r| PutRequest {
                key: to_block_key(&r.key),
                size: r.size,
                rank: r.rank,
                extension: r.extension,
            })
            .collect();

        let result = self.context.cache.block_put(&requests, &self.client_uuid)?;

        Ok(bc::PutResponse {
            results: result
                .locations
                .into_iter()
                .enumerate()
                .map(|(index, result)| match result {
                    Ok(location) => {
                        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
                        self.open_handles
                            .insert(handle, OpenHandle::BlockPut(requests[index].key.clone()));
                        bc::OpenResult {
                            result: ResultCode::Success,
                            block: Some(bc::Block {
                                handle,
                                location: bc::Location {
                                    path: location.path,
                                    id: location.id,
                                    offset: location.offset,
                                    size: location.size,
                                },
                            }),
                        }
                    },
                    Err(e) => bc::OpenResult {
                        result: convert_block_cache_error(e),
                        block: None,
                    },
                })
                .collect(),
            base_path: result.base,
        })
    }

    #[instrument(level = "debug", skip_all, fields(%self, handle_count = params.handles.len(), first_handle=params.handles[0]))]
    pub fn block_commit(&mut self, params: bc::CommitParams) -> Result<bc::CommitResponse> {
        check_uuid!(self);

        // Gather keys
        let mut keys = Vec::with_capacity(params.handles.len());
        let mut req_index = Vec::with_capacity(params.handles.len());

        // Prepoluate results so we can set individual values later
        let mut results = vec![ResultCode::Success; params.handles.len()];

        for (index, handle) in params.handles.iter().enumerate() {
            match self.open_handles.remove(handle) {
                Some(open_handle) => {
                    trace!(?open_handle, "block commit");
                    keys.push(open_handle.clone());
                    req_index.push(index);
                },
                None => {
                    debug!(handle, "block commit handle not found");
                    results[index] = ResultCode::ErrorHandleNotFound;
                },
            }
        }

        let result = self.context.cache.block_commit_deprecated(&keys)?;

        for (index, r) in result.into_iter().enumerate() {
            if let Err(e) = r {
                results[req_index[index]] = convert_block_cache_error(e);
            }
        }

        Ok(bc::CommitResponse { results })
    }

    #[instrument(level = "debug", skip_all, fields(%self, handle_count = params.handles.len(), first_handle=params.handles[0]))]
    pub fn block_abort(&mut self, params: bc::AbortParams) -> Result<bc::AbortResponse> {
        check_uuid!(self);

        let mut keys = Vec::with_capacity(params.handles.len());
        let mut req_index = Vec::with_capacity(params.handles.len());

        // Prepoluate results so we can set individual values later
        let mut results = vec![ResultCode::Success; params.handles.len()];

        for (index, handle) in params.handles.iter().enumerate() {
            match self.open_handles.remove(handle) {
                Some(open_handle) => {
                    trace!(?open_handle, "block abort");
                    keys.push(open_handle.clone());
                    req_index.push(index);
                },
                None => {
                    debug!(handle, "block abort handle not found");
                    results[index] = ResultCode::ErrorHandleNotFound;
                },
            }
        }

        let result = self.context.cache.block_abort_deprecated(&keys)?;

        for (index, r) in result.into_iter().enumerate() {
            if let Err(e) = r {
                results[req_index[index]] = convert_block_cache_error(e);
            }
        }

        Ok(bc::AbortResponse { results })
    }

    #[instrument(level = "debug", skip(self, params), fields(%self, key_count = params.keys.len(), ?first_key=params.keys[0]))]
    pub fn block_stat(&mut self, params: bc::StatParams) -> Result<bc::StatResponse> {
        check_uuid!(self);

        let keys = to_block_keylist(&params.keys);
        let result = self.context.cache.block_stat(&keys);

        Ok(bc::StatResponse {
            results: result
                .into_iter()
                .map(|result| match result {
                    Ok(Some(metadata)) => bc::StatResult {
                        result: ResultCode::Success,
                        metadata: Some(bc::Metadata {
                            last_accessed: metadata.last_accessed,
                            accesses: metadata.accesses,
                            size: metadata.size,
                        }),
                    },
                    Ok(None) => bc::StatResult {
                        result: ResultCode::ErrorNotFound,
                        metadata: None,
                    },
                    Err(e) => bc::StatResult {
                        result: convert_block_cache_error(e),
                        metadata: None,
                    },
                })
                .collect(),
        })
    }

    // update next handle id, CAS, we always have the highest number
    // on connect(), multiple clients can be connecting simulttaneously
    fn update_handle_id(&self, id: u64) {
        loop {
            let current = self.next_handle.load(Ordering::Acquire);

            if id >= current
                && self
                    .next_handle
                    .compare_exchange(current, id + 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_err()
            {
                // exchange failed, try again
                continue;
            }

            // current is already > id or exchange was successful
            break;
        }
    }
}

// NOTE: The cache back-end is implemented as a block cache, where a block may be cached as a
//       single file in cases where the block is very large. Explicit file caching is supported as
//       a specialization, so blocks will _also_ be cached as a file if an extension is supplied,
//       in which case the cached file will have the supplied extension as a part of its path
//       name. This is important because it means that the extension is effectively a part of
//       the cache key--it should be possible to cache two ditinct blocks if their extensions
//       differ, even if the keys match.
//
//       Unfortunately, the back-end cache is inconsistent in where it accepts the extension
//       for cache operations. File-specific operations accept an extension and block-specific
//       operations do not, but there are a few operations that work for both files and blocks,
//       like the connect() call, that accept an Option<extension> parameter.
//
//       To make matters even more confusing, a few objects in the block_cache portion of the
//       RPC protocol accept an optional extension, though the caching layer will kinda break if
//       you actually populate it. So what I'm doing here is just fully ignoring the extension
//       for block operations, and if at some point the caching logic is updated to fully merge
//       the file_cache and block_cache logic, the code here can be updated then as well.
//
//       You may ask why the extension is being combined with the key here instead of in the
//       cache itself, and the reason is that the cache code was not written in a way that easily
//       supports this--there is too much risk of a key being encoded multiple times across nested
//       internal calls. Ultimately however, I think it's reasonable to assert that the cache is
//       behaving correctly by using the keys we supply, and if we, the client, actually want files
//       with different extensions but otherwise identical keys to exist together within the cache,
//       then it's up to us to compose the keys accordingly.
//
//       As far as the key format is concerned, it is: <key>0<extension>. The null separator is
//       inserted to ensure that a key+extension combination doesn't collide with a key that has
//       no extension (ie. a block key). A null isn't allowed in filenames, so <key>0 will never
//       match <key>0<extension> for non-empty extensions. This does mean that a file with an
//       empty extension may collide with a block key in the index. The old approach had the same
//       limitation however, and the chance of this seems low. If it becomes an issue, the key
//       format can be further changed to prevent this.

fn to_block_key(key: &Arc<[u8]>) -> Arc<[u8]> {
    let mut val = Vec::with_capacity(key.as_ref().len() + 1);
    val.extend_from_slice(key.as_ref());
    val.push(0);
    val.into()
}

fn to_block_keylist(from: &[Arc<[u8]>]) -> Vec<Arc<[u8]>> {
    from.iter().map(to_block_key).collect()
}

fn to_file_key(key: &Arc<str>, extension: &Arc<str>) -> Arc<[u8]> {
    let mut val = Vec::with_capacity(key.as_ref().len() + 1 + extension.as_ref().len());
    val.extend_from_slice(key.as_bytes());
    val.push(0);
    val.extend_from_slice(extension.as_bytes());
    val.into()
}

// TODO: better error handling
fn convert_block_cache_error(err: hub_cache::Error) -> ResultCode {
    match err {
        hub_cache::Error::BlockAbandoned(_) => ResultCode::ErrorAbandoned,
        hub_cache::Error::KeyAlreadyExists(_) => ResultCode::ErrorAlreadyExists,
        hub_cache::Error::KeyNotFound(_) => ResultCode::ErrorNotFound,
        hub_cache::Error::KeyNotOpen(_) => ResultCode::ErrorHandleNotFound,
        hub_cache::Error::KeyOpenForPut(_) => ResultCode::ErrorPendingWrite,
        hub_cache::Error::KeyTooLarge(_, _, _) => ResultCode::ErrorTooLarge,
        _ => ResultCode::ErrorInternal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use assert_matches::assert_matches;
    use bytesize::ByteSize;
    use rstest::{fixture, rstest};
    use tracing::trace;
    use uuid::Uuid;

    use hub_cache::block::Cache;

    use crate::context::TestContext;

    const WRITE_SIZE: u64 = 10;

    #[fixture]
    fn test_cache() -> TestCache {
        TestCache::new()
    }

    struct TestCache {
        test_context: Arc<TestContext>,
        client_name: String,
        client_uuid: Uuid,
        exiting: Arc<AtomicBool>,
        dispatch: Option<Dispatcher>,
    }

    impl TestCache {
        fn new() -> Self {
            let tmp_dir = tempfile::tempdir().unwrap();
            let cache = Arc::new(
                Cache::builder(tmp_dir.path())
                    .max_size(ByteSize::b(WRITE_SIZE + 1))
                    .unwrap()
                    .open()
                    .expect("Failed to initialize cache"),
            );

            let client_name = String::from("dispatcher.test_client");
            let client_uuid = Uuid::new_v4();
            trace!(client_name, ?client_uuid);

            let test_context = Arc::new(TestContext::new_with_cache(tmp_dir, cache));

            let exiting = Arc::new(AtomicBool::new(false));
            let mut dispatch = Dispatcher::new(test_context.context.clone(), exiting.clone());
            dispatch
                .connect(ConnectParams {
                    client_name: client_name.clone(),
                    client_uuid,
                })
                .unwrap();

            Self {
                test_context,
                client_name,
                client_uuid,
                exiting,
                dispatch: Some(dispatch),
            }
        }

        fn dispatch(&mut self) -> &mut Dispatcher {
            self.dispatch.as_mut().unwrap()
        }

        fn drop_dispatch(&mut self) {
            {
                let _ = self.dispatch.take();
            }

            let mut dispatch = Dispatcher::new(self.test_context.context.clone(), self.exiting.clone());
            dispatch
                .connect(ConnectParams {
                    client_name: self.client_name.clone(),
                    client_uuid: self.client_uuid,
                })
                .unwrap();

            self.dispatch = Some(dispatch);
        }

        fn exit_dispatcher(&self) {
            self.exiting.store(true, Ordering::SeqCst)
        }
    }

    #[fixture]
    fn test_cases() -> TestCases {
        TestCases::new()
    }

    struct TestCases {
        write_size: u64,
        _key: Arc<str>,
        _provider: Arc<str>,
        extension: Arc<str>,
        write_params: ca::OpenForWriteParams,
        read_params: ca::OpenForReadParams,
        put_params: bc::PutParams,
        get_params: bc::GetParams,
    }

    impl TestCases {
        fn new() -> Self {
            let write_size = WRITE_SIZE;
            let key = Arc::from("key");
            let provider: Arc<str> = Arc::from("provider");
            let extension: Arc<str> = Arc::from("txt");
            let write_params = ca::OpenForWriteParams::new(&key, &provider, &extension, write_size);
            let read_params = ca::OpenForReadParams::new(&key, &provider, &extension);
            let put_params = bc::PutParams::with_requests(&[bc::PutRequest::new(key.as_bytes(), write_size, 0, None)]);
            let get_params = bc::GetParams::with_keys(&[key.as_bytes()]);

            Self {
                write_size,
                _key: key,
                _provider: provider,
                extension,
                write_params,
                read_params,
                put_params,
                get_params,
            }
        }
    }

    #[rstest]
    #[test_log::test]
    fn file_read_empty(mut test_cache: TestCache, test_cases: TestCases) {
        let result = test_cache
            .dispatch()
            .cache_open_for_read(test_cases.read_params)
            .unwrap();
        assert_eq!(result.result, ResultCode::ErrorNotFound);
        assert_matches!(result.local_file, None);
        assert_matches!(result.handle, None);
    }

    #[rstest]
    #[test_log::test]
    fn file_read_write(mut test_cache: TestCache, test_cases: TestCases) {
        // Open the file for write
        let write_result = test_cache
            .dispatch()
            .cache_open_for_write(test_cases.write_params.clone())
            .unwrap();
        assert_eq!(write_result.result, ResultCode::Success);
        assert_matches!(write_result.local_file, Some(_));
        let write_handle = write_result.handle.unwrap();
        let file_path = write_result.local_file.unwrap();
        assert_eq!(
            file_path.extension().unwrap().to_str().unwrap(),
            test_cases.extension.as_ref()
        );

        // Open the file for write again, which should fail because it's already open
        let write_result = test_cache
            .dispatch()
            .cache_open_for_write(test_cases.write_params.clone())
            .unwrap();
        assert_eq!(write_result.result, ResultCode::ErrorPendingWrite);

        // Try to read the file, which should fail
        let read_result = test_cache
            .dispatch()
            .cache_open_for_read(test_cases.read_params.clone())
            .unwrap();
        assert_eq!(read_result.result, ResultCode::ErrorPendingWrite);

        // Close the file
        let close_result = test_cache.dispatch().cache_close(write_handle.into()).unwrap();
        assert_eq!(close_result.result, ResultCode::Success);

        // Now read the file, which should succeed
        let read_result = test_cache
            .dispatch()
            .cache_open_for_read(test_cases.read_params.clone())
            .unwrap();
        assert_eq!(read_result.result, ResultCode::Success);
        let read_handle = read_result.handle.unwrap();

        // Try to write the file again, which should fail because it's open for read
        let write_result = test_cache
            .dispatch()
            .cache_open_for_write(test_cases.write_params.clone())
            .unwrap();
        assert_eq!(write_result.result, ResultCode::ErrorAlreadyExists);

        // Close the file for the reader
        let close_result = test_cache.dispatch().cache_close(read_handle.into()).unwrap();
        assert_eq!(close_result.result, ResultCode::Success);

        // Open for write, which should still fail because the file still exists
        let write_result = test_cache
            .dispatch()
            .cache_open_for_write(test_cases.write_params.clone())
            .unwrap();
        assert_eq!(write_result.result, ResultCode::ErrorAlreadyExists);
    }

    #[rstest]
    #[test_log::test]
    fn file_key_extension_collision(mut test_cache: TestCache, _test_cases: TestCases) {
        let key = Arc::from("key+ext");
        let provider: Arc<str> = Arc::from("provider");
        let extension1: Arc<str> = Arc::from("one");
        let extension2: Arc<str> = Arc::from("two");

        // It should be valid to cache two files with identical keys but different extensions
        let write1_params = ca::OpenForWriteParams::new(&key, &provider, &extension1, WRITE_SIZE);
        let write2_params = ca::OpenForWriteParams::new(&key, &provider, &extension2, WRITE_SIZE);

        // Open the first file for write
        let write1_result = test_cache
            .dispatch()
            .cache_open_for_write(write1_params.clone())
            .unwrap();
        assert_eq!(write1_result.result, ResultCode::Success);
        assert_matches!(write1_result.local_file, Some(_));
        let file1_path = write1_result.local_file.unwrap();
        assert!(file1_path.exists());

        // Open the second file for write
        let write2_result = test_cache
            .dispatch()
            .cache_open_for_write(write2_params.clone())
            .unwrap();
        assert_eq!(write2_result.result, ResultCode::Success);
        assert_matches!(write2_result.local_file, Some(_));
        let file2_path = write2_result.local_file.unwrap();
        assert!(file2_path.exists());
    }

    #[rstest]
    #[test_log::test]
    fn file_close_multiple(mut test_cache: TestCache, test_cases: TestCases) {
        // Open the file for write
        let write_result = test_cache
            .dispatch()
            .cache_open_for_write(test_cases.write_params.clone())
            .unwrap();
        assert_eq!(write_result.result, ResultCode::Success);
        assert_matches!(write_result.local_file, Some(_));
        let write_handle = write_result.handle.unwrap();
        let file_path = write_result.local_file.unwrap();
        assert!(file_path.exists());

        // Close the file
        let close_result = test_cache.dispatch().cache_close(write_handle.into()).unwrap();
        assert_eq!(close_result.result, ResultCode::Success);

        let close_result = test_cache.dispatch().cache_close(write_handle.into()).unwrap();
        assert_eq!(close_result.result, ResultCode::ErrorHandleNotFound);
    }

    #[rstest]
    #[test_log::test]
    fn file_remove_multiple(mut test_cache: TestCache, test_cases: TestCases) {
        // Open the file for write
        let write_result = test_cache
            .dispatch()
            .cache_open_for_write(test_cases.write_params.clone())
            .unwrap();
        assert_eq!(write_result.result, ResultCode::Success);
        assert_matches!(write_result.local_file, Some(_));
        let write_handle = write_result.handle.unwrap();
        let file_path = write_result.local_file.unwrap();
        assert!(file_path.exists());

        // Remove the file
        let close_result = test_cache.dispatch().cache_remove(write_handle.into()).unwrap();
        assert_eq!(close_result.result, ResultCode::Success);

        let close_result = test_cache.dispatch().cache_remove(write_handle.into()).unwrap();
        assert_eq!(close_result.result, ResultCode::ErrorHandleNotFound);
    }

    #[rstest]
    #[test_log::test]
    fn drop_during_running(mut test_cache: TestCache, test_cases: TestCases) {
        // Open the file for write
        let write_result = test_cache
            .dispatch()
            .cache_open_for_write(test_cases.write_params.clone())
            .unwrap();
        assert_eq!(write_result.result, ResultCode::Success);
        assert_matches!(write_result.local_file, Some(_));
        let file_path = write_result.local_file.unwrap();
        trace!(?file_path, "FOO");
        assert!(file_path.exists());

        // Drop the dispatch while running, simulating a client exiting. This should remove the file
        test_cache.drop_dispatch();

        // we do not check for file existence, but check with the cache itself, since the underlying file
        // would only get removed after GC runs some arbitrary time later
        let read_result = test_cache
            .dispatch()
            .cache_open_for_read(test_cases.read_params.clone())
            .unwrap();
        assert_eq!(read_result.result, ResultCode::ErrorNotFound);
    }

    #[rstest]
    #[test_log::test]
    fn drop_during_shutdown(mut test_cache: TestCache, test_cases: TestCases) {
        // Open the file for write
        let write_result = test_cache
            .dispatch()
            .cache_open_for_write(test_cases.write_params)
            .unwrap();
        assert_eq!(write_result.result, ResultCode::Success);
        assert_matches!(write_result.local_file, Some(_));
        let file_path = write_result.local_file.unwrap();
        trace!(?file_path, "BAR");
        assert!(file_path.exists());

        // Move to shutdown and drop the test_cache.dispatch(). This should not remove the file
        test_cache.exit_dispatcher();
        test_cache.drop_dispatch();

        let read_result = test_cache
            .dispatch()
            .cache_open_for_read(test_cases.read_params.clone())
            .unwrap();
        assert_eq!(read_result.result, ResultCode::ErrorPendingWrite);
    }

    #[rstest]
    #[test_log::test]
    fn file_too_large(mut test_cache: TestCache, mut test_cases: TestCases) {
        test_cases.write_params.size = u64::MAX;
        let write_result = test_cache
            .dispatch()
            .cache_open_for_write(test_cases.write_params)
            .unwrap();
        assert_eq!(write_result.result, ResultCode::ErrorTooLarge);
    }

    #[rstest]
    #[test_log::test]
    fn block_too_large(mut test_cache: TestCache, mut test_cases: TestCases) {
        test_cases.put_params.requests[0].size = u64::MAX;
        let put_result = test_cache.dispatch().block_put(test_cases.put_params).unwrap();
        assert_eq!(put_result.results.len(), 1);
        assert_eq!(put_result.results[0].result, ResultCode::ErrorTooLarge);
    }

    #[rstest]
    #[test_log::test]
    fn block_read_write(mut test_cache: TestCache, test_cases: TestCases) {
        // Open the block for put
        let put_result = test_cache.dispatch().block_put(test_cases.put_params.clone()).unwrap();
        assert_eq!(put_result.results.len(), 1);
        assert_eq!(put_result.results[0].result, ResultCode::Success);
        assert_matches!(put_result.results[0].block, Some(_));

        let put_handle = put_result.results[0].block.as_ref().unwrap().handle;

        // Put the block again, which should fail because it's already open
        let put_result = test_cache.dispatch().block_put(test_cases.put_params.clone()).unwrap();
        assert_eq!(put_result.results.len(), 1);
        assert_eq!(put_result.results[0].result, ResultCode::ErrorPendingWrite);
        assert_matches!(put_result.results[0].block, None);

        // Try to read the block, which should fail
        let get_result = test_cache.dispatch().block_get(test_cases.get_params.clone()).unwrap();
        assert_eq!(get_result.results.len(), 1);
        assert_eq!(get_result.results[0].result, ResultCode::ErrorPendingWrite);
        assert_matches!(get_result.results[0].block, None);

        // Commit the block
        let commit_result = test_cache
            .dispatch()
            .block_commit(bc::CommitParams::with_handles(&[put_handle]))
            .unwrap();
        assert_eq!(commit_result.results.len(), 1);
        assert_eq!(commit_result.results[0], ResultCode::Success);

        // Now read the block, which should succeed
        let get_result = test_cache.dispatch().block_get(test_cases.get_params.clone()).unwrap();
        assert_eq!(get_result.results.len(), 1);
        assert_eq!(get_result.results[0].result, ResultCode::Success);
        assert_matches!(get_result.results[0].block, Some(_));
        assert_eq!(
            get_result.results[0].block.as_ref().unwrap().location.size,
            test_cases.write_size
        );
        let get_handle = get_result.results[0].block.as_ref().unwrap().handle;

        // Try to write the block again, which should fail because it's open for read
        let put_result = test_cache.dispatch().block_put(test_cases.put_params.clone()).unwrap();
        assert_eq!(put_result.results.len(), 1);
        assert_eq!(put_result.results[0].result, ResultCode::ErrorAlreadyExists);
        assert_matches!(put_result.results[0].block, None);

        // Close the block for the reader
        let commit_result = test_cache
            .dispatch()
            .block_close(bc::CloseParams::with_handles(&[get_handle]))
            .unwrap();
        assert_eq!(commit_result.results.len(), 1);
        assert_eq!(commit_result.results[0], ResultCode::Success);

        // Open for write, which should still fail because the block still exists
        let put_result = test_cache.dispatch().block_put(test_cases.put_params.clone()).unwrap();
        assert_eq!(put_result.results.len(), 1);
        assert_eq!(put_result.results[0].result, ResultCode::ErrorAlreadyExists);
        assert_matches!(put_result.results[0].block, None);
    }

    #[rstest]
    #[test_log::test]
    fn block_close_multiple(mut test_cache: TestCache, test_cases: TestCases) {
        // Open the block for put
        let put_result = test_cache.dispatch().block_put(test_cases.put_params.clone()).unwrap();
        let put_handle = put_result.results[0].block.as_ref().unwrap().handle;

        test_cache
            .dispatch()
            .block_commit(bc::CommitParams {
                handles: vec![put_handle],
            })
            .unwrap();

        let get_result = test_cache.dispatch().block_get(test_cases.get_params.clone()).unwrap();
        let get_handle = get_result.results[0].block.as_ref().unwrap().handle;

        // Close the block the first time
        let close_result = test_cache
            .dispatch()
            .block_close(bc::CloseParams::with_handles(&[get_handle]))
            .unwrap();
        assert_eq!(close_result.results.len(), 1);
        assert_eq!(close_result.results[0], ResultCode::Success);

        // Close the block again
        let close_result = test_cache
            .dispatch()
            .block_close(bc::CloseParams {
                handles: vec![get_handle],
            })
            .unwrap();
        assert_eq!(close_result.results.len(), 1);
        assert_eq!(close_result.results[0], ResultCode::ErrorHandleNotFound);
    }

    #[rstest]
    #[test_log::test]
    fn block_commit_multiple(mut test_cache: TestCache, test_cases: TestCases) {
        // Open the block for put
        let put_result = test_cache.dispatch().block_put(test_cases.put_params.clone()).unwrap();
        let put_handle = put_result.results[0].block.as_ref().unwrap().handle;

        // Commit the block
        let commit_result = test_cache
            .dispatch()
            .block_commit(bc::CommitParams {
                handles: vec![put_handle],
            })
            .unwrap();
        assert_eq!(commit_result.results.len(), 1);
        assert_eq!(commit_result.results[0], ResultCode::Success);

        // Commit the block again
        let commit_result = test_cache
            .dispatch()
            .block_commit(bc::CommitParams::with_handles(&[put_handle]))
            .unwrap();
        assert_eq!(commit_result.results.len(), 1);
        assert_eq!(commit_result.results[0], ResultCode::ErrorHandleNotFound);
    }

    #[rstest]
    #[test_log::test]
    fn block_abort_multiple(mut test_cache: TestCache, test_cases: TestCases) {
        // Open the block for put
        let put_result = test_cache.dispatch().block_put(test_cases.put_params.clone()).unwrap();
        let put_handle = put_result.results[0].block.as_ref().unwrap().handle;

        // Abort the block
        let abort_result = test_cache
            .dispatch()
            .block_abort(bc::AbortParams::with_handles(&[put_handle]))
            .unwrap();
        assert_eq!(abort_result.results.len(), 1);
        assert_eq!(abort_result.results[0], ResultCode::Success);

        // Abort the block again
        let abort_result = test_cache
            .dispatch()
            .block_abort(bc::AbortParams::with_handles(&[put_handle]))
            .unwrap();
        assert_eq!(abort_result.results.len(), 1);
        assert_eq!(abort_result.results[0], ResultCode::ErrorHandleNotFound);
    }
}
