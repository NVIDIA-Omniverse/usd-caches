// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use core::slice;
use std::{
    ffi::{c_char, c_void, CStr, CString},
    sync::Arc,
};

use crate::{
    error::Result,
    interfaces,
    types::{
        ClientPutRequest, CloseResult, GetCloseCallback, GetOpenCallback, LogCallback, LogLevel, OpenResult,
        PutAbortCallback, PutCommitCallback, PutOpenCallback, StatCallback, StatResult, FAILED_TO_ALLOCATE_STRING,
    },
    CacheOpenForReadCallback, CacheOpenForWriteCallback, Client, ClientResultCode, CloseCallback, Error, ErrorCallback,
    GetServerApiVersionCallback, RemoveCallback, CLIENT_NOT_INITIALIZED_STRING,
};
use hub_rpc::SemVer;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use stdext::function_name;
use tracing::error;

static CLIENT: OnceCell<Client> = OnceCell::new();
static INIT_ERROR: OnceCell<Error> = OnceCell::new();
static LOG_CALLBACK: Mutex<Option<LogCallback>> = Mutex::new(None);
static LOG_LEVEL: Mutex<Option<LogLevel>> = Mutex::new(None);

/// Initializes the Hub Client. This will launch the Hub executable if it is not already running.
/// This function should be called before calling any other functions.
///
/// client_id: String to identify this client in logs/traces on the Hub server
///
/// Returns HubClientResultCode::Success upon success, or a corresponding error code if unsuccessful.
/// If an unsuccessful result code is returned, no other calls will succeed.
///
/// # Safety
///
/// It is the caller's guarantee to ensure `client_id`:
///
/// - is not a null pointer
/// - points to valid, initialized data
/// - points to memory ending in a null byte
/// - won't be mutated for the duration of this function call
#[no_mangle]
pub unsafe extern "C" fn hub_initialize(client_id: *const c_char) -> ClientResultCode {
    unsafe {
        if client_id.is_null() {
            return ClientResultCode::InvalidArgument;
        }

        let Ok(client_str) = CStr::from_ptr(client_id).to_str() else {
            return ClientResultCode::Utf8Error;
        };

        // Lock the logging info so we don't miss an update while constructing the client
        let locked_cb = LOG_CALLBACK.lock();
        let locked_level = LOG_LEVEL.lock();

        let mut builder = Client::builder(client_str);
        if let Some(cb) = *locked_cb {
            builder = builder.log_callback(cb);
        }

        if let Some(level) = *locked_level {
            builder = builder.log_level(level);
        }

        match builder.build() {
            Ok(client) => match CLIENT.set(client) {
                Ok(_) => ClientResultCode::Success,
                Err(_) => ClientResultCode::AlreadyInitialized,
            },
            Err(e) => {
                let _ = INIT_ERROR.set(e.clone());
                ClientResultCode::from(&e)
            },
        }
    }
}

/// Shuts down the hub client.
///
/// If there are any outstanding operations when this function is called, they will have their error callback
/// called with HubClientResultCode::Shutdown. Any interface calls made after this function will also have their
/// error callback called immediately with HubClientResultCode::Shutdown.
#[no_mangle]
pub unsafe extern "C" fn hub_shutdown() -> ClientResultCode {
    if let Some(client) = CLIENT.get() {
        client.shutdown();
        ClientResultCode::Success
    } else {
        ClientResultCode::ClientNotInitialized
    }
}

/// Sets a log callback that the Hub Client will call with log messages.
///
/// If this function is called before hub_initialize, the callback will be stored until the client is initialized
///
/// # Safety
///
/// A valid function pointer must be provided to this function. It is undefinied behavior to pass a nullptr to this function.
///
/// To disable the log callback, use `hub_disable_log_callback`
#[no_mangle]
pub unsafe extern "C" fn hub_set_log_callback(callback: LogCallback) -> ClientResultCode {
    // Lock the callback so we know no one else is modifying it and the client is not being initialized
    let mut locked_cb = LOG_CALLBACK.lock();

    // If the client is already initialized then update the client
    if let Some(client) = CLIENT.get() {
        match client.set_log_callback(callback) {
            Ok(_) => ClientResultCode::Success,
            Err(e) => ClientResultCode::from(&e),
        }
    } else {
        *locked_cb = Some(callback);
        ClientResultCode::Success
    }
}

/// Disables the log callback in the Hub Client
///
/// If this function is called before hub_initialize, the disabled state will be applied when the client is initialized
#[no_mangle]
pub unsafe extern "C" fn hub_disable_log_callback() -> ClientResultCode {
    // Lock the callback so we know no one else is modifying it and the client is not being initialized
    let mut locked_cb = LOG_CALLBACK.lock();

    // If the client is already initialized then update the client
    if let Some(client) = CLIENT.get() {
        match client.disable_log_callback() {
            Ok(_) => ClientResultCode::Success,
            Err(e) => ClientResultCode::from(&e),
        }
    } else {
        *locked_cb = None;
        ClientResultCode::Success
    }
}

/// Sets the log level for log messages output to the log callback (if provided).
///
/// If this function is not called, the default log level is HubLogLevel::Info
///
/// If this function is called before hub_initialize, the level will be stored until the client is initialized
#[no_mangle]
pub unsafe extern "C" fn hub_set_log_level(level: LogLevel) -> ClientResultCode {
    // Lock the level so we know no one else is modifying it and the client is not being initialized
    let mut locked_level = LOG_LEVEL.lock();

    // If the client is already initialized then update the client
    if let Some(client) = CLIENT.get() {
        match client.set_log_level(level) {
            Ok(_) => ClientResultCode::Success,
            Err(e) => ClientResultCode::from(&e),
        }
    } else {
        *locked_level = Some(level);
        ClientResultCode::Success
    }
}

/// Retrieves information about the last error that occured in the hub client. A string buffer is provided to this function
/// that will be populated with the error message. The string placed in the buffer will always be null terminated.
///
/// buffer: Buffer to populate with the error message. If this is null, `ClientResultCode::InvalidArgument` is returned.
///
/// buffer_size: Size of the buffer to populate. If the error message is longer than the buffer size, (buffer_size - 1)
///              characters will be placed in the buffer.
///
/// Returns the ClientResultCode associated with the last error.
///     If there was no previous error, `ClientResultCode::Success` is returned and the buffer is not modified.
///     If the client is not initialized, `ClientResultCode::ClientNotInitialized` is returned.
#[no_mangle]
pub unsafe extern "C" fn hub_get_last_error(buffer: *mut c_char, buffer_size: usize) -> ClientResultCode {
    if buffer.is_null() {
        return ClientResultCode::InvalidArgument;
    }

    if let Some(client) = CLIENT.get() {
        match client.get_last_error() {
            Some(e) => populate_buffer(&e, buffer, buffer_size),
            None => ClientResultCode::Success,
        }
    } else {
        // If there was an error during initialization, return that error
        if let Some(init_error) = INIT_ERROR.get() {
            populate_buffer(init_error, buffer, buffer_size)
        } else {
            // Client was never initialized
            ClientResultCode::ClientNotInitialized
        }
    }
}

#[no_mangle]
extern "C" fn hub_get_software_version(out_version: *mut SemVer) -> ClientResultCode {
    if let Some(client) = CLIENT.get() {
        if !out_version.is_null() {
            unsafe {
                *out_version = client.get_software_version();
            }
        }
        ClientResultCode::Success
    } else {
        ClientResultCode::ClientNotInitialized
    }
}

#[no_mangle]
extern "C" fn hub_get_hub_http_port(out_port: *mut u16) -> ClientResultCode {
    if let Some(client) = CLIENT.get() {
        if !out_port.is_null() {
            unsafe {
                *out_port = client.get_hub_http_port();
            }
        }
        ClientResultCode::Success
    } else {
        ClientResultCode::ClientNotInitialized
    }
}

// Populates a c-string buffer with the contents of the error
unsafe fn populate_buffer(error: &Error, buffer: *mut c_char, buffer_size: usize) -> ClientResultCode {
    match CString::new(error.to_string()) {
        Ok(msg) => {
            // error message fits in buffer
            if msg.to_bytes_with_nul().len() <= buffer_size {
                buffer.copy_from(msg.as_ptr(), msg.to_bytes_with_nul().len());
            } else {
                // error message won't fit, fill what we can
                buffer.copy_from(msg.as_ptr(), buffer_size - 1);
                *buffer.add(buffer_size - 1) = 0;
            }
            ClientResultCode::from(error)
        },
        Err(_) => {
            buffer.copy_from(
                FAILED_TO_ALLOCATE_STRING,
                CStr::from_ptr(FAILED_TO_ALLOCATE_STRING).to_bytes_with_nul().len(),
            );
            ClientResultCode::InternalError
        },
    }
}

#[repr(C)]
/// The CacheAside interface of the Hub Client.
///
/// If the server does not support this interface, any calls to these functions will immediately have their error
/// callback called with HubClientResultCode::Version.
pub struct CacheAside {
    /// Get the API version the server is provider.
    ///
    /// context: Opaque pointer provided to callbacks
    ///
    /// callback: Will be called with the provided `context` if the RPC request is successful.
    ///
    /// error_callback: Will be called with the provided `context` if the RPC request is unsuccessful.
    get_server_api_version:
        extern "C" fn(context: *mut c_void, callback: GetServerApiVersionCallback, error_callback: ErrorCallback),

    /// Open a file for reading.
    ///
    /// key: The key that identifies the resource to access in the cache. It is just a string so that each provider can
    ///      choose how it wants to identify resources. For Nucleus this could be the hash, or S3 the e-tag.
    ///
    /// provider: The provider of the file.
    ///
    /// context: Opaque pointer provided to callbacks
    ///
    /// callback: Will be called with the provided `context` if the RPC request is successful.
    ///
    /// error_callback: Will be called with the provided `context` if the RPC request is unsuccessful.
    ///
    /// Will return ResultCode::Success and a CachedFile if the file exists in the cache and can be read.
    /// Will return ResultCode::ErrorNotFound if the file does not exist in the cache.
    /// Will return ResultCode::ErrorOpenForWrite if the file exists in the cache but is currently being written.
    ///
    /// # Safety
    ///
    /// It is the caller's guarantee to ensure `key`, `provider`:
    ///
    /// - are not null pointers
    /// - point to valid, initialized data
    /// - point to memory ending in a null byte
    /// - won't be mutated for the duration of this function call
    cache_open_for_read: extern "C" fn(
        key: *const c_char,
        provider: *const c_char,
        extension: *const c_char,
        context: *mut c_void,
        callback: CacheOpenForReadCallback,
        error_callback: ErrorCallback,
    ),

    /// Open a file for writing.
    ///
    /// key: The key that identifies the resource to access in the cache. It is just a string so that each provider can
    ///      choose how it wants to identify resources. For Nucleus this could be the hash, or S3 the e-tag.
    ///
    /// provider: The provider of the file.
    ///
    /// extension: The extension of the source file. This is used so the cache can create the local file with the same
    ///            extension.
    ///
    /// size: The size of the file being written.
    ///
    /// context: Opaque pointer provided to callbacks
    ///
    /// callback: Will be called with the provided `context` if the RPC request is successful.
    ///
    /// error_callback: Will be called with the provided `context` if the RPC request is unsuccessful.
    ///
    /// Will return ResultCode::Success and a CachedFile if the file is not in the cache.
    /// Will return ResultCode::ErrorOpenForWrite if the file exists in the cache and is currently being written.
    /// Will return ResultCode::ErrorAlreadyexists if the file exists in the cache and is read-only.
    /// Will return ResultCode::ErrorTooLarge if the file is too large to fit in the cache.
    ///
    /// # Safety
    ///
    /// It is the caller's guarantee to ensure `key`, `provider`, `extension`:
    ///
    /// - are not null pointers
    /// - point to valid, initialized data
    /// - point to memory ending in a null byte
    /// - won't be mutated for the duration of this function call
    cache_open_for_write: extern "C" fn(
        key: *const c_char,
        provider: *const c_char,
        extension: *const c_char,
        size: u64,
        context: *mut c_void,
        callback: CacheOpenForWriteCallback,
        error_callback: ErrorCallback,
    ),

    /// Close a file previously opened with either open function.
    ///
    /// For files open for write, this function will move the file to a readable state decrement the reference count by one.
    /// For files open for read, this function will decrement the reference count for the file by one.
    ///
    /// handle: Handle to close
    ///
    /// context: Opaque pointer provided to callbacks
    ///
    /// callback: Will be called with the provided `context` if the RPC request is successful.
    ///
    /// error_callback: Will be called with the provided `context` if the RPC request is unsuccessful.
    ///
    /// Will return ResultCode::Success on success.
    /// Will return ResultCode::ErrorNotFound if they key does not exist, or if the local cached file does not exist.
    /// Will return ResultCode::HandleNotFound if the provided handle was not returned by an open call, or has already been
    ///    closed or removed.
    close: extern "C" fn(handle: u64, context: *mut c_void, callback: CloseCallback, error_callback: ErrorCallback),

    /// Remove a file from cache
    ///
    /// If a file that was opened for write fails to write, this will allow the
    /// user to remove that file from cache.
    ///
    /// handle: Handle to remove
    ///
    /// context: Opaque pointer provided to callbacks
    ///
    /// callback: Will be called with the provided `context` if the RPC request is successful.
    ///
    /// error_callback: Will be called with the provided `context` if the RPC request is unsuccessful.
    ///
    /// Will return ResultCode::Success on success.
    /// Will return ResultCode::ErrorNotFound if they key does not exist.
    /// Will return ResultCode::HandleNotFound if the provided handle was not returned by an open call, or has already been
    ///    closed or removed.
    remove: extern "C" fn(handle: u64, context: *mut c_void, callback: RemoveCallback, error_callback: ErrorCallback),
}

/// Returns the interface struct to use for CacheAside operations
#[no_mangle]
pub extern "C" fn hub_get_cache_aside() -> CacheAside {
    CacheAside {
        get_server_api_version: cache_aside_get_server_api_version,
        cache_open_for_read: cache_aside_cache_open_for_read,
        cache_open_for_write: cache_aside_cache_open_for_write,
        close: cache_aside_close,
        remove: cache_aside_remove,
    }
}

#[no_mangle]
extern "C" fn cache_aside_get_server_api_version(
    context: *mut c_void,
    callback: GetServerApiVersionCallback,
    error_callback: ErrorCallback,
) {
    if let Some(client) = CLIENT.get() {
        if let Err(e) = client
            .cache_aside
            .get_server_api_version_c(context, callback, error_callback)
        {
            error!("{} error: {:?}", function_name!(), e);
            interfaces::CacheAside::handle_c_callback_error(error_callback, context, e);
        }
    } else {
        error!("{} called before client initialized", function_name!());
        error_callback(
            ClientResultCode::ClientNotInitialized,
            CLIENT_NOT_INITIALIZED_STRING,
            context,
        );
    }
}

#[no_mangle]
extern "C" fn cache_aside_cache_open_for_read(
    key: *const c_char,
    provider: *const c_char,
    extension: *const c_char,
    context: *mut c_void,
    callback: CacheOpenForReadCallback,
    error_callback: ErrorCallback,
) {
    if let Some(client) = CLIENT.get() {
        if let (Some(k), Some(p), Some(e)) = (
            validate_c_str_param(key, "key", error_callback, context),
            validate_c_str_param(provider, "provider", error_callback, context),
            validate_c_str_param(extension, "extension", error_callback, context),
        ) {
            if let Err(err) = client
                .cache_aside
                .cache_open_for_read_c(k, p, e, context, callback, error_callback)
            {
                error!("{} error: {:?}", function_name!(), err);
                interfaces::CacheAside::handle_c_callback_error(error_callback, context, err);
            }
        }
    } else {
        error!("{} called before client initialized", function_name!());
        error_callback(
            ClientResultCode::ClientNotInitialized,
            CLIENT_NOT_INITIALIZED_STRING,
            context,
        );
    }
}

#[no_mangle]
extern "C" fn cache_aside_cache_open_for_write(
    key: *const c_char,
    provider: *const c_char,
    extension: *const c_char,
    size: u64,
    context: *mut c_void,
    callback: CacheOpenForWriteCallback,
    error_callback: ErrorCallback,
) {
    if let Some(client) = CLIENT.get() {
        if let (Some(k), Some(p), Some(e)) = (
            validate_c_str_param(key, "key", error_callback, context),
            validate_c_str_param(provider, "provider", error_callback, context),
            validate_c_str_param(extension, "extension", error_callback, context),
        ) {
            if let Err(e) = client
                .cache_aside
                .cache_open_for_write_c(k, p, e, size, context, callback, error_callback)
            {
                error!("{} error: {:?}", function_name!(), e);
                interfaces::CacheAside::handle_c_callback_error(error_callback, context, e);
            }
        }
    } else {
        error!("{} called before client initialized", function_name!());
        error_callback(
            ClientResultCode::ClientNotInitialized,
            CLIENT_NOT_INITIALIZED_STRING,
            context,
        );
    }
}

#[no_mangle]
extern "C" fn cache_aside_close(
    handle: u64,
    context: *mut c_void,
    callback: CloseCallback,
    error_callback: ErrorCallback,
) {
    if let Some(client) = CLIENT.get() {
        if let Err(e) = client.cache_aside.close_c(handle, context, callback, error_callback) {
            error!("{} error: {:?}", function_name!(), e);
            interfaces::CacheAside::handle_c_callback_error(error_callback, context, e);
        }
    } else {
        error!("{} called before client initialized", function_name!());
        error_callback(
            ClientResultCode::ClientNotInitialized,
            CLIENT_NOT_INITIALIZED_STRING,
            context,
        );
    }
}

#[no_mangle]
extern "C" fn cache_aside_remove(
    handle: u64,
    context: *mut c_void,
    callback: RemoveCallback,
    error_callback: ErrorCallback,
) {
    if let Some(client) = CLIENT.get() {
        if let Err(e) = client.cache_aside.remove_c(handle, context, callback, error_callback) {
            error!("{} error: {:?}", function_name!(), e);
            interfaces::CacheAside::handle_c_callback_error(error_callback, context, e);
        }
    } else {
        error!("{} called before client initialized", function_name!());
        error_callback(
            ClientResultCode::ClientNotInitialized,
            CLIENT_NOT_INITIALIZED_STRING,
            context,
        );
    }
}

fn validate_c_str_param(
    param: *const c_char,
    param_name: &str,
    error_callback: ErrorCallback,
    context: *mut c_void,
) -> Option<String> {
    if !param.is_null() {
        unsafe {
            let c_str = CStr::from_ptr(param);
            match c_str.to_str() {
                Ok(str) => Some(str.to_owned()),
                Err(e) => {
                    interfaces::CacheAside::handle_c_callback_error(
                        error_callback,
                        context,
                        Error::InvalidArgument(format!("Provided parameter {param_name} contains UTF-8 error: {e}")),
                    );
                    None
                },
            }
        }
    } else {
        interfaces::CacheAside::handle_c_callback_error(
            error_callback,
            context,
            Error::InvalidArgument(format!("Provided parameter {param_name} is null")),
        );
        None
    }
}

#[repr(C)]
/// The BlockCache interface of the Hub Client.
///
/// If the server does not support this interface, any calls to these functions will immediately fail and have their
/// callback called with HubClientResultCode::Version.
pub struct BlockCache {
    /// Opens a block to be read. Clients should first call this function to get the location of the block,
    /// then read the data from the returned block location, then call get_close when done reading.
    ///
    /// keys: The keys to get data for. Each key is byte array that can be up to MAX_KEY_SIZE in length.
    ///
    /// key_lengths: Array of lengths for each keys
    ///
    /// key_count: Number of keys provided
    ///
    /// timeout_us: If this value is non-zero, the get operation will check if the block is currently being written, and if so, it will block
    ///             for up to the provided number of microsocends waiting for the write to finish. If the write finishes in time, the block
    ///             will be opened for reading. If the write does not finish, the get operation will return ErrorPendingWrite for the block.
    ///             If the provided timeout is 0, the call will not block, and will immediately return ErrorPendingWrite if the requested block
    ///             is being written.
    ///             The timeout value applies to all keys in the batch request.
    ///
    /// context: Opaque pointer passed back to the callback
    ///
    /// callback: A callback to be called with the result of the operation.
    ///
    /// It is the caller's guarantee to ensure `keys` and `key_lengths`:
    ///
    /// - are not null pointers
    /// - point to valid, initialized data
    /// - contain `key_count` elements
    /// - won't be mutated for the duration of this function call
    get_open: extern "C" fn(
        keys: *const *const u8,
        key_lengths: *const u16,
        key_count: u64,
        timeout_us: u64,
        context: *mut c_void,
        callback: GetOpenCallback,
    ),

    /// Closes blocks that were open for read. Clients should call this function with the handles returned from get_open.
    /// Failing to call this function could leave a block marked as "in-use", prevent the block from being garbage collected.
    ///
    /// handles: The handles to close. These handles were returned by get_open
    ///
    /// handles_count: Number of handles provided
    ///
    /// context: Opaque pointer passed back to the callback
    ///
    /// callback: A callback to be called with the result of the operation.
    ///
    /// It is the caller's guarantee to ensure `handles`:
    ///
    /// - is not a null pointer
    /// - point to valid, initialized data
    /// - contains `handle_count` elements
    /// - won't be mutated for the duration of this function call
    get_close: extern "C" fn(handles: *const u64, handle_count: u64, context: *mut c_void, callback: GetCloseCallback),

    /// Opens a block to be stored. Clients should first call this function to get the location to store the block,
    /// then write the data to the returned block location, then call put_commit if the write was successful,
    /// or put_abort to indicate the write failed.
    ///
    /// blocks: The blocks to store
    ///
    /// block_count: The number of blocks in the blocks array
    ///
    /// context: Opaque pointer passed back to the callback
    ///
    /// callback: A callback to be called with the result of the operation.
    ///
    /// It is the caller's guarantee to ensure `blocks`
    ///
    /// - are not null pointers
    /// - point to valid, initialized data
    /// - contain `key_count` elements
    /// - won't be mutated for the duration of this function call
    put_open: extern "C" fn(
        blocks: *const ClientPutRequest,
        block_count: u64,
        context: *mut c_void,
        callback: PutOpenCallback,
    ),

    /// Closes blocks that were written successfully. Clients should call this function with the handles returned from put_open
    /// for all blocks that were written successfully. Blocks will not be available for read until this function returns.
    ///
    /// handles: The handles to close. These handles were returned by get_open
    ///
    /// handles_count: Number of handles provided
    ///
    /// context: Opaque pointer passed back to the callback
    ///
    /// callback: A callback to be called with the result of the operation.
    ///
    /// It is the caller's guarantee to ensure `handles`:
    ///
    /// - is not a null pointer
    /// - point to valid, initialized data
    /// - contains `handle_count` elements
    /// - won't be mutated for the duration of this function call
    put_commit:
        extern "C" fn(handles: *const u64, handle_count: u64, context: *mut c_void, callback: PutCommitCallback),

    /// Closes blocks that failed to be written. Clients should call this function with the handles returned from put_open
    /// for all blocks that were opened but not written. This function will remove the block entry so the block can be rewritten.
    ///
    /// handles: The handles to close. These handles were returned by get_open
    ///
    /// handles_count: Number of handles provided
    ///
    /// context: Opaque pointer passed back to the callback
    ///
    /// callback: A callback to be called with the result of the operation.
    ///
    /// It is the caller's guarantee to ensure `handles`:
    ///
    /// - is not a null pointer
    /// - point to valid, initialized data
    /// - contains `handle_count` elements
    /// - won't be mutated for the duration of this function call
    put_abort: extern "C" fn(handles: *const u64, handle_count: u64, context: *mut c_void, callback: PutAbortCallback),

    /// Checks for a keys existence and returns information about it
    ///
    /// keys: The keys to get data for. Each key is byte array that can be up to MAX_KEY_SIZE in length.
    ///
    /// key_lengths: Array of lengths for each keys
    ///
    /// key_count: Number of keys provided
    ///
    /// timeout_us: If this value is non-zero, the stat operation will check if the block is currently being written, and if so, it will block
    ///             for up to the provided number of microsocends waiting for the write to finish. If the write finishes in time, the stat operation
    ///             will continue as normal. If the write does not finish, the stat operation will return ErrorPendingWrite for the block.
    ///             If the provided timeout is 0, the call will not block, and will immediately return ErrorPendingWrite if the requested block
    ///             is being written.
    ///             The timeout value applies to all keys in the batch request.
    ///
    /// context: Opaque pointer passed back to the callback
    ///
    /// callback: A callback to be called with the result of the operation.
    ///
    /// It is the caller's guarantee to ensure `keys` and `key_lengths`:
    ///
    /// - are not null pointers
    /// - point to valid, initialized data
    /// - contain `key_count` elements
    /// - won't be mutated for the duration of this function call
    stat: extern "C" fn(
        keys: *const *const u8,
        key_sizes: *const u16,
        key_count: u64,
        timeout_us: u64,
        context: *mut c_void,
        callback: StatCallback,
    ),
}

/// Returns the interface struct to use for CacheAside operations
#[no_mangle]
pub extern "C" fn hub_get_block_cache() -> BlockCache {
    BlockCache {
        get_open: block_cache_get_open,
        get_close: block_cache_get_close,
        put_open: block_cache_put_open,
        put_commit: block_cache_put_commit,
        put_abort: block_cache_put_abort,
        stat: block_cache_stat,
    }
}
#[no_mangle]
extern "C" fn block_cache_get_open(
    keys: *const *const u8,
    key_sizes: *const u16,
    key_count: u64,
    timeout_us: u64,
    context: *mut c_void,
    callback: GetOpenCallback,
) {
    if keys.is_null() || key_sizes.is_null() {
        callback(OpenResult::from(ClientResultCode::InvalidArgument), context);
        return;
    }

    if let Some(client) = CLIENT.get() {
        let key_slice = match create_input_slice(keys, key_sizes, key_count as usize, "keys") {
            Ok(s) => s,
            Err(e) => {
                error!("{} error: {:?}", function_name!(), e);
                callback(OpenResult::from(&e), context);
                return;
            },
        };

        if let Err(e) = client.block_cache.get_open_c(&key_slice, timeout_us, context, callback) {
            callback(OpenResult::from(&e), context);
        }
    } else {
        error!("{} called before client initialized", function_name!());
        callback(OpenResult::from(ClientResultCode::ClientNotInitialized), context);
    }
}

#[no_mangle]
extern "C" fn block_cache_get_close(
    handles: *const u64,
    handle_count: u64,
    context: *mut c_void,
    callback: GetCloseCallback,
) {
    if handles.is_null() {
        callback(CloseResult::from(ClientResultCode::InvalidArgument), context);
        return;
    }

    if let Some(client) = CLIENT.get() {
        let handle_slice = unsafe { slice::from_raw_parts(handles, handle_count as usize) };

        if let Err(e) = client.block_cache.get_close_c(handle_slice, context, callback) {
            error!("{} error: {:?}", function_name!(), e);
            callback(CloseResult::from(&e), context);
        }
    } else {
        error!("{} called before client initialized", function_name!());
        callback(CloseResult::from(ClientResultCode::ClientNotInitialized), context);
    }
}

#[no_mangle]
extern "C" fn block_cache_put_open(
    blocks: *const ClientPutRequest,
    block_count: u64,
    context: *mut c_void,
    callback: PutOpenCallback,
) {
    if blocks.is_null() {
        callback(OpenResult::from(ClientResultCode::InvalidArgument), context);
        return;
    }

    if let Some(client) = CLIENT.get() {
        let block_slice = unsafe { slice::from_raw_parts(blocks, block_count as usize) };
        let block_requests: Result<Vec<hub_rpc::block_cache::PutRequest>> = block_slice
            .iter()
            .map(|b| {
                if b.key.is_null() {
                    Err(Error::InvalidArgument("Null pointer provided for key".to_owned()))
                } else {
                    Ok(hub_rpc::block_cache::PutRequest {
                        key: Arc::from(unsafe { slice::from_raw_parts(b.key, b.key_length as usize) }),
                        size: b.block_size,
                        rank: b.rank,
                        extension: None,
                    })
                }
            })
            .collect();

        match block_requests {
            Ok(r) => {
                if let Err(e) = client.block_cache.put_open_c(r, context, callback) {
                    error!("{} error: {:?}", function_name!(), e);
                    callback(OpenResult::from(&e), context);
                }
            },
            Err(e) => {
                callback(OpenResult::from(&e), context);
            },
        }
    } else {
        error!("{} called before client initialized", function_name!());
        callback(OpenResult::from(ClientResultCode::ClientNotInitialized), context);
    }
}

#[no_mangle]
extern "C" fn block_cache_put_commit(
    handles: *const u64,
    handle_count: u64,
    context: *mut c_void,
    callback: PutCommitCallback,
) {
    if handles.is_null() {
        callback(CloseResult::from(ClientResultCode::InvalidArgument), context);
        return;
    }

    if let Some(client) = CLIENT.get() {
        let handle_slice = unsafe { slice::from_raw_parts(handles, handle_count as usize) };

        if let Err(e) = client.block_cache.put_commit_c(handle_slice, context, callback) {
            error!("{} error: {:?}", function_name!(), e);
            callback(CloseResult::from(&e), context);
        }
    } else {
        error!("{} called before client initialized", function_name!());
        callback(CloseResult::from(ClientResultCode::ClientNotInitialized), context);
    }
}

#[no_mangle]
extern "C" fn block_cache_put_abort(
    handles: *const u64,
    handle_count: u64,
    context: *mut c_void,
    callback: PutAbortCallback,
) {
    if handles.is_null() {
        callback(CloseResult::from(ClientResultCode::InvalidArgument), context);
        return;
    }

    if let Some(client) = CLIENT.get() {
        let handle_slice = unsafe { slice::from_raw_parts(handles, handle_count as usize) };

        if let Err(e) = client.block_cache.put_abort_c(handle_slice, context, callback) {
            error!("{} error: {:?}", function_name!(), e);
            callback(CloseResult::from(&e), context);
        }
    } else {
        error!("{} called before client initialized", function_name!());
        callback(CloseResult::from(ClientResultCode::ClientNotInitialized), context);
    }
}

#[no_mangle]
extern "C" fn block_cache_stat(
    keys: *const *const u8,
    key_sizes: *const u16,
    key_count: u64,
    timeout_us: u64,
    context: *mut c_void,
    callback: StatCallback,
) {
    if keys.is_null() || key_sizes.is_null() {
        callback(StatResult::from(ClientResultCode::InvalidArgument), context);
        return;
    }

    if let Some(client) = CLIENT.get() {
        let key_slice = match create_input_slice(keys, key_sizes, key_count as usize, "keys") {
            Ok(s) => s,
            Err(e) => {
                callback(StatResult::from(&e), context);
                return;
            },
        };
        if let Err(e) = client.block_cache.stat_c(&key_slice, timeout_us, context, callback) {
            error!("{} error: {:?}", function_name!(), e);
            callback(StatResult::from(&e), context);
        }
    } else {
        error!("{} called before client initialized", function_name!());
        callback(StatResult::from(ClientResultCode::ClientNotInitialized), context);
    }
}

fn create_input_slice<T>(
    values: *const *const u8,
    value_sizes: *const T,
    value_count: usize,
    value_name: impl AsRef<str>,
) -> Result<Vec<Arc<[u8]>>>
where
    T: Into<u64> + Copy,
{
    if values.is_null() {
        return Err(Error::InvalidArgument(format!("{} is null", value_name.as_ref())));
    }
    if value_sizes.is_null() {
        return Err(Error::InvalidArgument(format!("{}_sizes is null", value_name.as_ref())));
    }

    let mut input_slice = Vec::new();
    for i in 0..(value_count as isize) {
        if unsafe { values.offset(i).is_null() } {
            return Err(Error::InvalidArgument(format!("{}[{i}] is null", value_name.as_ref())));
        }
        input_slice.push(Arc::from(unsafe {
            slice::from_raw_parts(*values.offset(i), (*value_sizes.offset(i)).into() as usize)
        }));
    }

    Ok(input_slice)
}
