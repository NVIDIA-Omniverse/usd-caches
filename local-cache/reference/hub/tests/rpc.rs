// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#[macro_use]
mod common;

use crate::common::*;

use std::fs;

use hub_client::utils::PutRequest;
use hub_rpc::ResultCode;
use rstest::rstest;

/// Tests getting the Hub server version
/// Paramatized on:
///     - Client Type
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn get_version(#[case] test_service: impl TestService) {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let mut client = test_service.connect_client().await;

            // Check Hub interface server version
            let current_version = client.get_server_api_version().await;

            assert_eq!(current_version.major, 1);
            assert_eq!(current_version.minor, 0);
            assert_eq!(current_version.patch, 0);
        })
        .await;
}

/// Tests basic reading and writing of files
/// Paramatized on:
///     - Client Type
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn read_write(#[case] test_service: impl TestService) {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let mut test_client = test_service.connect_client().await;

            // Try to read the open file, which fail because it doesn't exist
            test_client.read_file("key", ResultCode::ErrorNotFound).await;

            // Open the file for writing
            let (path, _) = test_client.write_file("key", ResultCode::Success).await.unwrap();

            // create file (otherwise close() will fail)
            fs::write(path, "data").unwrap();

            // Try to read the file, which will fail because it's open for writing
            test_client.read_file("key", ResultCode::ErrorPendingWrite).await;

            // Try writing the file again
            test_client.write_file("key", ResultCode::ErrorPendingWrite).await;

            // Close the file for write
            test_client.close("key", ResultCode::Success).await;

            // Try to read the file, which should now succeed
            test_client.read_file("key", ResultCode::Success).await;

            // Close the file for read
            test_client.close("key", ResultCode::Success).await;

            // Try to write the file again, which will fail
            test_client.write_file("key", ResultCode::ErrorAlreadyExists).await;
        })
        .await;
}

/// Tests that files are automically closed by the server on client disconnect
/// Paramatized on:
///     - Client Type
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn close_on_disconnect(#[case] test_service: impl TestService) {
    tokio::task::LocalSet::new()
        .run_until(async {
            {
                let mut test_client = test_service.connect_client().await;
                let key = "key";

                // Open some files
                for i in 0..5 {
                    test_client
                        .write_file(format!("{key}_{i}").as_str(), ResultCode::Success)
                        .await;
                }
            }
        })
        .await;

    tokio::task::LocalSet::new()
        .run_until(async move {
            {
                let mut test_client = test_service.connect_client().await;
                let key = "key";

                test_client.wait_for_close_on_disconnect(key).await;

                // Now try to reopen those files for read. If they were not closed on client disconnect,
                // these reads would fail because the files would still be open for write.
                for i in 0..5 {
                    test_client
                        .read_file(format!("{key}_{i}").as_str(), ResultCode::ErrorNotFound)
                        .await;
                }
            }
        })
        .await;
}

/// Tests calling the close RPC with the same handle multiple times
/// Paramatized on:
///     - Client Type
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn close_multiple(#[case] test_service: impl TestService) {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let mut test_client = test_service.connect_client().await;
            let (path, handle) = test_client.write_file("key", ResultCode::Success).await.unwrap();

            // create file (otherwise close() will fail)
            fs::write(path, "data").unwrap();

            test_client.close_handle(handle, ResultCode::Success).await;
            test_client.close_handle(handle, ResultCode::ErrorHandleNotFound).await;
        })
        .await;
}

/// Tests calling the remove RPC with the same handle multiple times
/// Paramatized on:
///     - Client Type
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn remove_multiple(#[case] test_service: impl TestService) {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let mut test_client = test_service.connect_client().await;
            let (path, handle) = test_client.write_file("key", ResultCode::Success).await.unwrap();

            // create file (otherwise close() will fail)
            fs::write(path, "data").unwrap();

            test_client.remove_handle(handle, ResultCode::Success).await;
            test_client.remove_handle(handle, ResultCode::ErrorHandleNotFound).await;
        })
        .await;
}

/// Tests reading files while the server restarts
/// Paramatized on:
///     - Number of files
///     - Server Restart method
///     - Client Type
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn restart_open_read(
    #[case] test_service: impl TestService,
    #[values(1, 2, 10, 100)] number_of_files: u64,
    #[values(RestartMethod::Api, RestartMethod::Kill)] restart_method: RestartMethod,
) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut test_client = test_service.connect_client().await;

            for i in 0..number_of_files {
                let key = format!("key_{i}");
                test_client.create_file(&key).await;
                test_client.read_file(&key, ResultCode::Success).await;
            }

            test_client.restart(restart_method).await;

            test_client.connect().await;

            for i in 0..number_of_files {
                let key = format!("key_{i}");
                test_client.write_file(&key, ResultCode::ErrorAlreadyExists).await;
                test_client.close(&key, ResultCode::Success).await;
            }
        })
        .await;
}

/// Tests writing files while the server restarts
/// Paramatized on:
///     - Number of files
///     - Server Restart method
///     - Client Type
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn restart_open_write(
    #[case] test_service: impl TestService,
    #[values(1, 2, 10, 100)] number_of_files: u64,
    #[values(RestartMethod::Api, RestartMethod::Kill)] restart_method: RestartMethod,
) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut test_client = test_service.connect_client().await;

            for i in 0..number_of_files {
                let key = format!("key_{i}");
                let (path, _) = test_client.write_file(&key, ResultCode::Success).await.unwrap();
                let _ = fs::File::create(path).unwrap();
            }

            test_client.restart(restart_method).await;

            test_client.connect().await;

            for i in 0..number_of_files {
                let key = format!("key_{i}");
                test_client.read_file(&key, ResultCode::ErrorPendingWrite).await;
                test_client.close(&key, ResultCode::Success).await;
                test_client.read_file(&key, ResultCode::Success).await;
            }
        })
        .await;
}

/// Tests reading and writing files while the server restarts
/// Paramatized on:
///     - Number of files
///     - Server Restart method
///     - Client Type
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn restart_read_write(
    #[case] test_service: impl TestService,
    #[values(1, 2, 10, 100)] number_of_files: u64,
    #[values(RestartMethod::Api, RestartMethod::Kill)] restart_method: RestartMethod,
) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut test_client = test_service.connect_client().await;

            for i in 0..number_of_files {
                let key = format!("key_{i}");

                if i % 2 == 0 {
                    let (path, _) = test_client.write_file(&key, ResultCode::Success).await.unwrap();
                    let _ = fs::File::create(path).unwrap();
                } else {
                    test_client.create_file(&key).await;
                    test_client.read_file(&key, ResultCode::Success).await;
                }
            }

            test_client.restart(restart_method).await;

            test_client.connect().await;

            for i in 0..number_of_files {
                let key = format!("key_{i}");
                if i % 2 == 0 {
                    test_client.read_file(&key, ResultCode::ErrorPendingWrite).await;
                    test_client.close(&key, ResultCode::Success).await;
                    test_client.read_file(&key, ResultCode::Success).await;
                } else {
                    test_client.write_file(&key, ResultCode::ErrorAlreadyExists).await;
                    test_client.close(&key, ResultCode::Success).await;
                }
            }
        })
        .await;
}

/// Tests reading and writing files while both the server and client restart
/// Paramatized on:
///     - Number of files
///     - Server Restart method
///     - Client Type
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn restart_client_also_crashes(
    #[case] test_service: impl TestService,
    #[values(1, 2, 10, 100)] number_of_files: u64,
    #[values(RestartMethod::Api, RestartMethod::Kill)] restart_method: RestartMethod,
) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut test_client = test_service.connect_client().await;

            for i in 0..number_of_files {
                let key = format!("key_{i}");
                test_client.create_file(&key).await;
                test_client.read_file(&key, ResultCode::Success).await;
            }

            test_client.clear_open_entries();
            test_client.restart(restart_method).await;

            test_client.connect().await;

            for i in 0..number_of_files {
                let key = format!("key_{i}");
                test_client.write_file(&key, ResultCode::ErrorAlreadyExists).await;
                test_client.close(&key, ResultCode::ErrorHandleNotFound).await;
            }
        })
        .await;
}

/// tests launching hub with a bogus/non-existent hub path
#[ignore]
#[rstest]
#[test_log::test(tokio::test)]
async fn launch_exclusive_with_nonexistent_exe_path() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    std::env::set_var("OMNICLIENT_HUB_EXE", "/totally/bogus/path");
    assert!(hub_client::Client::builder("test_msgpack_client")
        .mode(hub_client::Mode::Exclusive)
        .cache_path(tmp_dir.path())
        .build()
        .is_err());
}

/// tests launching hub with a non-existent cache dir (typical of launching hub for the first time)
#[rstest]
#[test_log::test(tokio::test)]
async fn launch_exclusive_with_nonexistent_cache_path() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let sub_dir = tmp_dir.path().join("does_not_exist");
    std::env::set_var("OMNICLIENT_HUB_EXE", env!("CARGO_BIN_EXE_hub"));
    assert!(hub_client::Client::builder("test_msgpack_client")
        .mode(hub_client::Mode::Exclusive)
        .cache_path(sub_dir)
        .build()
        .is_ok());
}

/// Tests basic reading and writing of blocks
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn read_write_block(#[case] test_service: impl TestService) {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let test_client = test_service.connect_client().await;

            let key = "key";
            let size = 100;

            let handles = test_client
                .put_open(vec![PutRequest::new(key, size, 0, None)], &[ResultCode::Success])
                .await;
            assert_eq!(handles.len(), 1);

            test_client
                .put_open(
                    vec![PutRequest::new(key, size, 0, None)],
                    &[ResultCode::ErrorPendingWrite],
                )
                .await;

            test_client
                .get_open(&[key.as_bytes()], &[ResultCode::ErrorPendingWrite], 0)
                .await;

            test_client.put_commit(&handles, &[ResultCode::Success]).await;

            let handles = test_client.get_open(&[key.as_bytes()], &[ResultCode::Success], 0).await;
            assert_eq!(handles.len(), 1);

            test_client
                .put_open(
                    vec![PutRequest::new(key, size, 0, None)],
                    &[ResultCode::ErrorAlreadyExists],
                )
                .await;

            test_client.get_close(&handles, &[ResultCode::Success]).await;

            test_client
                .put_open(
                    vec![PutRequest::new(key, size, 0, None)],
                    &[ResultCode::ErrorAlreadyExists],
                )
                .await;
        })
        .await;
}

/// Tests aborting a block
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn abort_block(#[case] test_service: impl TestService) {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let test_client = test_service.connect_client().await;

            let key = "key";
            let size = 100;

            let handles = test_client
                .put_open(vec![PutRequest::new(key, size, 0, None)], &[ResultCode::Success])
                .await;
            assert_eq!(handles.len(), 1);

            test_client.put_abort(&handles, &[ResultCode::Success]).await;

            test_client
                .get_open(&[key.as_bytes()], &[ResultCode::ErrorNotFound], 0)
                .await;

            // Should be able to rewrite the block
            let handles = test_client
                .put_open(vec![PutRequest::new(key, size, 0, None)], &[ResultCode::Success])
                .await;
            assert_eq!(handles.len(), 1);
            test_client.put_commit(&handles, &[ResultCode::Success]).await;

            let handles = test_client.get_open(&[key.as_bytes()], &[ResultCode::Success], 0).await;
            assert_eq!(handles.len(), 1);

            test_client.get_close(&handles, &[ResultCode::Success]).await;
        })
        .await;
}

#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn close_multiple_block(#[case] test_service: impl TestService) {
    let test_client = test_service.connect_client().await;

    let key = "key";
    let size = 100;
    let handles = test_client
        .put_open(vec![PutRequest::new(key, size, 0, None)], &[ResultCode::Success])
        .await;
    test_client.put_commit(&handles, &[ResultCode::Success]).await;

    let handles = test_client.get_open(&[key.as_bytes()], &[ResultCode::Success], 0).await;
    test_client.get_close(&handles, &[ResultCode::Success]).await;
    test_client
        .get_close(&handles, &[ResultCode::ErrorHandleNotFound])
        .await;
}

#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn commit_multiple_block(#[case] test_service: impl TestService) {
    let test_client = test_service.connect_client().await;

    let key = "key";
    let size = 100;
    let handles = test_client
        .put_open(vec![PutRequest::new(key, size, 0, None)], &[ResultCode::Success])
        .await;
    test_client.put_commit(&handles, &[ResultCode::Success]).await;
    test_client
        .put_commit(&handles, &[ResultCode::ErrorHandleNotFound])
        .await;
}

#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn abort_multiple_block(#[case] test_service: impl TestService) {
    let test_client = test_service.connect_client().await;

    let key = "key";
    let size = 100;
    let handles = test_client
        .put_open(vec![PutRequest::new(key, size, 0, None)], &[ResultCode::Success])
        .await;
    test_client.put_abort(&handles, &[ResultCode::Success]).await;
    test_client
        .put_abort(&handles, &[ResultCode::ErrorHandleNotFound])
        .await;
}

#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn commit_and_abort_block(#[case] test_service: impl TestService) {
    let test_client = test_service.connect_client().await;

    let key = "key";
    let size = 100;
    let handles = test_client
        .put_open(vec![PutRequest::new(key, size, 0, None)], &[ResultCode::Success])
        .await;
    test_client.put_commit(&handles, &[ResultCode::Success]).await;
    test_client
        .put_abort(&handles, &[ResultCode::ErrorHandleNotFound])
        .await;
}

/// Tests that when a client disconnects, the server aborts the open put
#[ignore]
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn abort_put_on_disconnect(#[case] test_service: impl TestService) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let tmp_dir = tempfile::TempDir::new().unwrap();
            let key = "key";
            let size = 100;
            {
                let test_client = test_service.connect_client_to_existing_hub(tmp_dir.path()).await;

                // Start a put on this client
                test_client
                    .put_open(vec![PutRequest::new(key, size, 0, None)], &[ResultCode::Success])
                    .await;
            }

            // Client is dropped, which should abort the put, allowing a new client to do the put
            let test_client = test_service.connect_client_to_existing_hub(tmp_dir.path()).await;
            let handles = test_client
                .put_open(vec![PutRequest::new(key, size, 0, None)], &[ResultCode::Success])
                .await;
            test_client.put_commit(&handles, &[ResultCode::Success]).await;

            let handles = test_client.get_open(&[key.as_bytes()], &[ResultCode::Success], 0).await;

            test_client.get_close(&handles, &[ResultCode::Success]).await;
        })
        .await;
}

#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn restart_read_write_blocks(
    #[case] test_service: impl TestService,
    #[values(RestartMethod::Api, RestartMethod::Kill)] restart_method: RestartMethod,
) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut test_client = test_service.connect_client().await;

            let key1 = "key1";
            let key2 = "key2";
            let size = 100;
            let put_handles = test_client
                .put_open(
                    vec![
                        PutRequest::new(key1, size, 0, None),
                        PutRequest::new(key2, size, 0, None),
                    ],
                    &[ResultCode::Success, ResultCode::Success],
                )
                .await;

            // Only commit the first key
            test_client.put_commit(&[put_handles[0]], &[ResultCode::Success]).await;

            let get_handles = test_client
                .get_open(&[key1.as_bytes()], &[ResultCode::Success], 0)
                .await;

            // Restart with one key open for get, and one open_for put
            test_client.restart(restart_method).await;

            test_client.connect().await;

            // Key 1 is already commited
            test_client
                .put_open(
                    vec![PutRequest::new(key1, size, 0, None)],
                    &[ResultCode::ErrorAlreadyExists],
                )
                .await;

            // Should be able to close the read
            test_client.get_close(&get_handles, &[ResultCode::Success]).await;

            // Key2 is still open for put
            test_client
                .get_open(&[key2.as_bytes()], &[ResultCode::ErrorPendingWrite], 0)
                .await;

            // Commit the key now
            test_client.put_commit(&[put_handles[1]], &[ResultCode::Success]).await;

            let get_handles = test_client
                .get_open(&[key2.as_bytes()], &[ResultCode::Success], 0)
                .await;
            test_client.get_close(&get_handles, &[ResultCode::Success]).await;
        })
        .await;
}

#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn restart_abandon_block_put_finished(
    #[case] test_service: impl TestService,
    #[values(RestartMethod::Api, RestartMethod::Kill)] restart_method: RestartMethod,
) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut test_client = test_service.connect_client().await;
            test_client.hub_client.set_wait_on_reconnect();

            let key1 = "key1";
            let size = 100;
            let put_handles = test_client
                .put_open(vec![PutRequest::new(key1, size, 0, None)], &[ResultCode::Success])
                .await;

            // Restart with the open put
            test_client.restart(restart_method).await;

            let test_client2 = test_service
                .connect_client_to_existing_hub(test_client.get_cache_path())
                .await;

            // Client two assumes the write
            let put_handles2 = test_client2
                .put_open(vec![PutRequest::new(key1, size, 0, None)], &[ResultCode::Success])
                .await;

            test_client2.put_commit(&put_handles2, &[ResultCode::Success]).await;

            // Let Client 1 reconnect now that client 2 has finished
            test_client.hub_client.notify_on_reconnect();

            // Commit the key now, which should be abandoned
            test_client
                .put_commit(&put_handles, &[ResultCode::ErrorHandleNotFound])
                .await;
        })
        .await;
}

#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn restart_abandon_block_put_pending(
    #[case] test_service: impl TestService,
    #[values(RestartMethod::Api, RestartMethod::Kill)] restart_method: RestartMethod,
) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut test_client = test_service.connect_client().await;
            test_client.hub_client.set_wait_on_reconnect();

            let key1 = "key1";
            let size = 100;
            let put_handles = test_client
                .put_open(vec![PutRequest::new(key1, size, 0, None)], &[ResultCode::Success])
                .await;

            // Restart with the open put
            test_client.restart(restart_method).await;

            let test_client2 = test_service
                .connect_client_to_existing_hub(test_client.get_cache_path())
                .await;

            // Client two assumes the write
            let put_handles2 = test_client2
                .put_open(vec![PutRequest::new(key1, size, 0, None)], &[ResultCode::Success])
                .await;

            // Let Client 1 reconnect now that client 2 has finished
            test_client.hub_client.notify_on_reconnect();

            // Finish put
            test_client2.put_commit(&put_handles2, &[ResultCode::Success]).await;

            // Commit the key now, which should be not found
            test_client
                .put_commit(&put_handles, &[ResultCode::ErrorHandleNotFound])
                .await;
        })
        .await;
}

/// Tests reading and writing files while both the server and client restart
/// Paramatized on:
///     - Server Restart method
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[case::protobuf(protobuf_test_service())]
#[test_log::test(tokio::test)]
async fn restart_block_client_also_crashes(
    #[case] test_service: impl TestService,
    #[values(RestartMethod::Api, RestartMethod::Kill)] restart_method: RestartMethod,
) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut test_client = test_service.connect_client().await;

            let key = "key";
            let size = 100;
            let handles = test_client
                .put_open(vec![PutRequest::new(key, size, 0, None)], &[ResultCode::Success])
                .await;
            test_client.put_commit(&handles, &[ResultCode::Success]).await;

            let handles = test_client.get_open(&[key.as_bytes()], &[ResultCode::Success], 0).await;

            test_client.clear_open_entries();
            test_client.restart(restart_method).await;

            test_client
                .put_open(
                    vec![PutRequest::new(key, size, 0, None)],
                    &[ResultCode::ErrorAlreadyExists],
                )
                .await;

            test_client
                .get_close(&handles, &[ResultCode::ErrorHandleNotFound])
                .await;
        })
        .await;
}
