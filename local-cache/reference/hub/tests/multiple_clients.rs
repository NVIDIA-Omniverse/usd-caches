// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#[macro_use]
mod common;

use std::collections::HashMap;
use std::fs;
use std::rc::Rc;
use std::sync::Arc;

use hub_client::utils::PutRequest;
use hub_rpc::ResultCode;

use rstest::{fixture, rstest};
use tokio::sync::{Mutex, RwLock};

use crate::common::*;

#[fixture]
pub fn shared_test_service() -> Arc<impl TestService> {
    Arc::new(MsgPackTestService::new())
}

#[fixture]
pub fn shared_restart_test_service() -> Rc<RwLock<Option<impl TestService>>> {
    Rc::new(RwLock::new(Some(MsgPackTestService::new())))
}

/// The tests in this file test connecting multiple client to a single Hub server instance, both in sunny day and rainy day
/// scenarios.
/// For CapnProto clients, this is done by starting one client that starts the Hub process and then starting multiple other
/// client that all connect to that Hub process.
/// For the MessagePack clients, this is done by creating a single client first so we know that client is currently tied to the
/// Hub process, and then launching other clients using the same cache directory so they find the existing server.
//
/// Sunny day multiple client test. Tests multiple clients reading and writing to the same server.
/// Paramaterized on:
///     - Number of clients
///     - Number of Files
///     - Client Type
#[rstest]
#[case::two_clients(2)]
#[ignore]
#[case::ten_clients(10)]
#[ignore]
#[case::one_hundred_clients(100)]
#[ignore]
#[case::one_thousand_clients(1000)]
#[test_log::test(tokio::test)]
async fn multiple_clients_create_file(
    shared_test_service: std::sync::Arc<impl TestService + 'static>,
    #[case] number_of_clients: u64,
    #[values(1, 10)] number_of_files: u64,
) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut handles = Vec::new();
            let base_client = shared_test_service.connect_client().await;
            let cache_path = base_client.get_cache_path().to_str().unwrap().to_owned();
            for client_num in 0..number_of_clients {
                let test_service = shared_test_service.clone();
                let cache = cache_path.clone();
                handles.push(tokio::task::spawn_local(async move {
                    let mut test_client = test_service.connect_client_to_existing_hub(cache).await;

                    for file_num in 0..number_of_files {
                        let key = format!("key_{client_num}_{file_num}");
                        test_client.create_file(&key).await;

                        test_client.read_file(&key, ResultCode::Success).await;
                    }
                }));
            }

            for handle in handles {
                handle.await.unwrap();
            }
        })
        .await;
}

/// Sunny day multiple client test. Tests multiple clients reading and writing blocks to the same server.
/// Paramaterized on:
///     - Number of clients
///     - Number of Files
///     - Client Type
#[rstest]
#[case::two_clients(2)]
#[ignore]
#[case::ten_clients(10)]
#[ignore]
#[case::one_hundred_clients(100)]
#[ignore]
#[case::one_thousand_clients(1000)]
#[test_log::test(tokio::test)]
async fn multiple_clients_block_cache(
    shared_test_service: std::sync::Arc<impl TestService + 'static>,
    #[case] number_of_clients: u64,
    #[values(1, 10)] number_of_blocks: usize,
) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let size = 100;
            let mut handles = Vec::new();
            let base_client = shared_test_service.connect_client().await;
            let cache_path = base_client.get_cache_path().to_owned();
            for client_num in 0..number_of_clients {
                let test_service = shared_test_service.clone();
                let cache = cache_path.clone();
                handles.push(tokio::task::spawn_local(async move {
                    let test_client = test_service.connect_client_to_existing_hub(&cache).await;

                    let mut keys = Vec::with_capacity(number_of_blocks);
                    for block_num in 0..number_of_blocks {
                        keys.push(Arc::from(format!("key_{client_num}_{block_num}").as_bytes()));
                    }
                    let mut key_slice: Vec<&[u8]> = Vec::with_capacity(number_of_blocks);
                    let mut blocks = Vec::with_capacity(number_of_blocks);
                    for k in keys.iter().take(number_of_blocks) {
                        key_slice.push(k);
                        blocks.push(PutRequest::new(k, size, 0, None))
                    }

                    let block_handles = test_client
                        .put_open(blocks, &vec![ResultCode::Success; number_of_blocks])
                        .await;
                    test_client
                        .put_commit(&block_handles, &vec![ResultCode::Success; number_of_blocks])
                        .await;

                    let block_handles = test_client
                        .get_open(&key_slice, &vec![ResultCode::Success; number_of_blocks], 0)
                        .await;

                    test_client
                        .get_close(&block_handles, &vec![ResultCode::Success; number_of_blocks])
                        .await;
                }));
            }

            for handle in handles {
                handle.await.unwrap();
            }
        })
        .await;
}

/// Rainy day multiple client test. Tests multiple clients reading and writing to the same server, but the server crashes
/// Paramaterized on:
///     - Number of clients
///     - Number of Files
///     - Server Restart method
///     - Client Type
#[rstest]
#[case::two_clients(2)]
#[ignore]
#[case::ten_clients(10)]
#[ignore]
#[case::one_hundred_clients(100)]
#[ignore]
#[case::one_thousand_clients(1000)]
#[test_log::test(tokio::test)]
async fn multiple_clients_restart_open(
    shared_restart_test_service: Rc<RwLock<Option<impl TestService + 'static>>>,
    #[case] number_of_clients: u64,
    #[values(1, 10)] number_of_files: u64,
    #[values(RestartMethod::Api, RestartMethod::Kill)] restart_method: RestartMethod,
) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut handles = Vec::new();
            let mut server_owning_client = shared_restart_test_service
                .read()
                .await
                .as_ref()
                .unwrap()
                .connect_client()
                .await;
            let clients: Arc<RwLock<HashMap<u64, Option<TestClient>>>> = Arc::new(RwLock::new(HashMap::new()));
            for client_num in 0..number_of_clients {
                let test_service = shared_restart_test_service.clone();
                let clients_copy = clients.clone();
                let path = server_owning_client.get_cache_path().to_str().unwrap().to_owned();
                handles.push(tokio::task::spawn_local(async move {
                    // Create the client
                    let mut client = test_service
                        .read()
                        .await
                        .as_ref()
                        .unwrap()
                        .connect_client_to_existing_hub(path)
                        .await;

                    // Open some files
                    for file_num in 0..number_of_files {
                        let key = format!("key_{client_num}_{file_num}");

                        if file_num % 2 == 0 {
                            let (path, _) = client.write_file(&key, ResultCode::Success).await.unwrap();
                            let _ = fs::File::create(path).unwrap();
                        } else {
                            client.create_file(&key).await;
                            client.read_file(&key, ResultCode::Success).await;
                        }
                    }

                    clients_copy.write().await.insert(client_num, Some(client));
                }));
            }

            for handle in handles {
                handle.await.unwrap();
            }

            //Restart the server
            server_owning_client.restart(restart_method).await;

            handles = Vec::new();
            for client_num in 0..number_of_clients {
                let clients_copy = clients.clone();
                handles.push(tokio::task::spawn_local(async move {
                    let mut client = clients_copy.write().await.remove(&client_num).unwrap().unwrap();

                    for file_num in 0..number_of_files {
                        let key = format!("key_{client_num}_{file_num}");

                        if file_num % 2 == 0 {
                            client.read_file(&key, ResultCode::ErrorPendingWrite).await;

                            client.close(&key, ResultCode::Success).await;

                            client.read_file(&key, ResultCode::Success).await;
                        } else {
                            client.write_file(&key, ResultCode::ErrorAlreadyExists).await;
                            client.close(&key, ResultCode::Success).await;
                        }
                    }
                }));
            }

            for handle in handles {
                handle.await.unwrap();
            }
        })
        .await;
}

/// Rainy day multiple client test. Tests multiple clients reading and writing blocks to the same server, but the server crashes
/// Paramaterized on:
///     - Number of clients
///     - Number of Files
///     - Server Restart method
#[rstest]
#[case::two_clients(2)]
#[ignore]
#[case::ten_clients(10)]
#[ignore]
#[case::one_hundred_clients(100)]
#[ignore]
#[case::one_thousand_clients(1000)]
#[test_log::test(tokio::test)]
async fn multiple_clients_restart_open_block_cache(
    shared_restart_test_service: Rc<RwLock<Option<impl TestService + 'static>>>,
    #[case] number_of_clients: u64,
    #[values(1, 10)] number_of_blocks: usize,
    #[values(RestartMethod::Api, RestartMethod::Kill)] restart_method: RestartMethod,
) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let size = 100;
            let mut handles = Vec::new();
            let mut server_owning_client = shared_restart_test_service
                .read()
                .await
                .as_ref()
                .unwrap()
                .connect_client()
                .await;
            let clients: Arc<RwLock<HashMap<u64, Option<TestClient>>>> = Arc::new(RwLock::new(HashMap::new()));
            let open_handles: Arc<RwLock<HashMap<u64, Vec<u64>>>> = Arc::new(RwLock::new(HashMap::new()));
            for client_num in 0..number_of_clients {
                let test_service = shared_restart_test_service.clone();
                let clients_copy = clients.clone();
                let open_map = open_handles.clone();
                let path = server_owning_client.get_cache_path().to_str().unwrap().to_owned();
                handles.push(tokio::task::spawn_local(async move {
                    // Create the client
                    let client = test_service
                        .read()
                        .await
                        .as_ref()
                        .unwrap()
                        .connect_client_to_existing_hub(path)
                        .await;

                    // Open some blocks
                    let mut keys = Vec::with_capacity(number_of_blocks);
                    for block_num in 0..number_of_blocks {
                        keys.push(Arc::from(format!("key_{client_num}_{block_num}").as_bytes()));
                    }
                    let mut key_slice: Vec<&[u8]> = Vec::with_capacity(number_of_blocks);
                    let mut blocks = Vec::with_capacity(number_of_blocks);
                    for k in keys.iter().take(number_of_blocks) {
                        key_slice.push(k);
                        blocks.push(PutRequest::new(k, size, 0, None))
                    }

                    // Even client leave with an open put
                    if client_num % 2 == 0 {
                        let block_handles = client
                            .put_open(blocks, &vec![ResultCode::Success; number_of_blocks])
                            .await;
                        open_map.write().await.insert(client_num, block_handles);
                    } else {
                        // Odd client leave with an open get
                        let block_handles = client
                            .put_open(blocks, &vec![ResultCode::Success; number_of_blocks])
                            .await;
                        client
                            .put_commit(&block_handles, &vec![ResultCode::Success; number_of_blocks])
                            .await;

                        let block_handles = client
                            .get_open(&key_slice, &vec![ResultCode::Success; number_of_blocks], 0)
                            .await;
                        open_map.write().await.insert(client_num, block_handles);
                    }

                    clients_copy.write().await.insert(client_num, Some(client));
                }));
            }

            for handle in handles {
                handle.await.unwrap();
            }

            //Restart the server
            server_owning_client.restart(restart_method).await;

            handles = Vec::new();
            for client_num in 0..number_of_clients {
                let clients_copy = clients.clone();
                let open_map = open_handles.clone();
                handles.push(tokio::task::spawn_local(async move {
                    let client = clients_copy.write().await.remove(&client_num).unwrap().unwrap();

                    let mut keys = Vec::with_capacity(number_of_blocks);
                    for file_num in 0..number_of_blocks {
                        keys.push(Arc::from(format!("key_{client_num}_{file_num}").as_bytes()));
                    }
                    let mut key_slice: Vec<&[u8]> = Vec::with_capacity(number_of_blocks);
                    let mut blocks = Vec::with_capacity(number_of_blocks);
                    for k in keys.iter().take(number_of_blocks) {
                        key_slice.push(k);
                        blocks.push(PutRequest::new(k, size, 0, None))
                    }

                    // Even block, resume write and then read
                    if client_num % 2 == 0 {
                        client
                            .get_open(&key_slice, &vec![ResultCode::ErrorPendingWrite; number_of_blocks], 0)
                            .await;
                        let block_handles = open_map.read().await.get(&client_num).unwrap().to_owned();
                        client
                            .put_commit(&block_handles, &vec![ResultCode::Success; number_of_blocks])
                            .await;

                        let block_handles = client
                            .get_open(&key_slice, &vec![ResultCode::Success; number_of_blocks], 0)
                            .await;
                        client
                            .get_close(&block_handles, &vec![ResultCode::Success; number_of_blocks])
                            .await;
                    } else {
                        // Odd client, close the read
                        client
                            .put_open(blocks, &vec![ResultCode::ErrorAlreadyExists; number_of_blocks])
                            .await;
                        let block_handles = open_map.read().await.get(&client_num).unwrap().to_owned();
                        client
                            .get_close(&block_handles, &vec![ResultCode::Success; number_of_blocks])
                            .await;
                    }
                }));
            }

            for handle in handles {
                handle.await.unwrap();
            }
        })
        .await;
}

/// Rainy day multiple client test. Tests multiple clients reading and writing to the same server, but both the server and
/// client crash
/// Paramaterized on:
///     - Number of clients
///     - Number of Files
///     - Server Restart method
///     - Client Type
#[rstest]
#[case::two_clients(2)]
#[ignore]
#[case::ten_clients(10)]
#[ignore]
#[case::one_hundred_clients(100)]
#[ignore]
#[case::one_thousand_clients(1000)]
#[test_log::test(tokio::test)]
async fn multiple_clients_restart_client_crash(
    shared_restart_test_service: Rc<RwLock<Option<impl TestService + 'static>>>,
    #[case] number_of_clients: u64,
    #[values(1, 10)] number_of_files: u64,
    #[values(RestartMethod::Api, RestartMethod::Kill)] restart_method: RestartMethod,
) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut handles = Vec::new();
            let mut server_owning_client = shared_restart_test_service
                .read()
                .await
                .as_ref()
                .unwrap()
                .connect_client()
                .await;
            let clients: Arc<RwLock<HashMap<u64, Mutex<Option<TestClient>>>>> = Arc::new(RwLock::new(HashMap::new()));
            for client_num in 0..number_of_clients {
                let test_service = shared_restart_test_service.clone();
                let clients_copy = clients.clone();
                let path = server_owning_client.get_cache_path().to_str().unwrap().to_owned();
                handles.push(tokio::task::spawn_local(async move {
                    // Create the client
                    let mut client = test_service
                        .read()
                        .await
                        .as_ref()
                        .unwrap()
                        .connect_client_to_existing_hub(path)
                        .await;

                    for file_num in 0..number_of_files {
                        let key = format!("key_{client_num}_{file_num}");
                        client.create_file(&key).await;

                        client.read_file(&key, ResultCode::Success).await;
                    }

                    clients_copy.write().await.insert(client_num, Mutex::new(Some(client)));
                }));
            }

            for handle in handles {
                handle.await.unwrap();
            }

            // Restart the server
            // Clear all open files, we need to do this before we restart the server so when the messagepack client
            // automatically reconnect, they do so with empty open file lists
            for client_num in 0..number_of_clients {
                let read = clients.read().await;
                read.get(&client_num)
                    .unwrap()
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .clear_open_entries();
            }
            server_owning_client.restart(restart_method).await;

            handles = Vec::new();
            for client_num in 0..number_of_clients {
                let clients_copy = clients.clone();
                handles.push(tokio::task::spawn_local(async move {
                    let mut client = clients_copy
                        .write()
                        .await
                        .remove(&client_num)
                        .unwrap()
                        .lock()
                        .await
                        .take()
                        .unwrap();

                    for file_num in 0..number_of_files {
                        let key = format!("key_{client_num}_{file_num}");

                        client.write_file(&key, ResultCode::ErrorAlreadyExists).await;
                        client.close(&key, ResultCode::ErrorHandleNotFound).await;
                    }
                }));
            }

            for handle in handles {
                handle.await.unwrap();
            }
        })
        .await;
}

/// Rainy day multiple client test. Tests multiple clients reading and writing blocks to the same server, but both the server and
/// client crash
/// Paramaterized on:
///     - Number of clients
///     - Number of Files
///     - Server Restart method
#[rstest]
#[case::two_clients(2)]
#[ignore]
#[case::ten_clients(10)]
#[ignore]
#[case::one_hundred_clients(100)]
#[ignore]
#[case::one_thousand_clients(1000)]
#[test_log::test(tokio::test)]
async fn multiple_clients_restart_client_crash_block_cache(
    shared_restart_test_service: Rc<RwLock<Option<impl TestService + 'static>>>,
    #[case] number_of_clients: u64,
    #[values(1, 10)] number_of_blocks: usize,
    #[values(RestartMethod::Api, RestartMethod::Kill)] restart_method: RestartMethod,
) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let size = 100;
            let mut handles = Vec::new();
            let mut server_owning_client = shared_restart_test_service
                .read()
                .await
                .as_ref()
                .unwrap()
                .connect_client()
                .await;
            let clients: Arc<RwLock<HashMap<u64, Mutex<Option<TestClient>>>>> = Arc::new(RwLock::new(HashMap::new()));
            let open_handles: Arc<RwLock<HashMap<u64, Vec<u64>>>> = Arc::new(RwLock::new(HashMap::new()));
            for client_num in 0..number_of_clients {
                let test_service = shared_restart_test_service.clone();
                let clients_copy = clients.clone();
                let open_map = open_handles.clone();
                let path = server_owning_client.get_cache_path().to_str().unwrap().to_owned();
                handles.push(tokio::task::spawn_local(async move {
                    // Create the client
                    let client = test_service
                        .read()
                        .await
                        .as_ref()
                        .unwrap()
                        .connect_client_to_existing_hub(path)
                        .await;

                    let mut keys = Vec::with_capacity(number_of_blocks);
                    for file_num in 0..number_of_blocks {
                        keys.push(Arc::from(format!("key_{client_num}_{file_num}").as_bytes()));
                    }
                    let mut key_slice: Vec<&[u8]> = Vec::with_capacity(number_of_blocks);
                    let mut blocks = Vec::with_capacity(number_of_blocks);
                    for k in keys.iter().take(number_of_blocks) {
                        key_slice.push(k);
                        blocks.push(PutRequest::new(k, size, 0, None))
                    }

                    let block_handles = client
                        .put_open(blocks, &vec![ResultCode::Success; number_of_blocks])
                        .await;
                    client
                        .put_commit(&block_handles, &vec![ResultCode::Success; number_of_blocks])
                        .await;

                    let block_handles = client
                        .get_open(&key_slice, &vec![ResultCode::Success; number_of_blocks], 0)
                        .await;
                    open_map.write().await.insert(client_num, block_handles);

                    clients_copy.write().await.insert(client_num, Mutex::new(Some(client)));
                }));
            }

            for handle in handles {
                handle.await.unwrap();
            }

            // Restart the server
            // Clear all open files, we need to do this before we restart the server so when the messagepack client
            // automatically reconnect, they do so with empty open file lists
            for client_num in 0..number_of_clients {
                let read = clients.read().await;
                read.get(&client_num)
                    .unwrap()
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .clear_open_entries();
            }
            server_owning_client.restart(restart_method).await;

            handles = Vec::new();
            for client_num in 0..number_of_clients {
                let clients_copy = clients.clone();
                let open_map = open_handles.clone();
                handles.push(tokio::task::spawn_local(async move {
                    // Take the client for this client_num
                    let client = clients_copy
                        .write()
                        .await
                        .remove(&client_num)
                        .unwrap()
                        .lock()
                        .await
                        .take()
                        .unwrap();

                    let mut keys = Vec::with_capacity(number_of_blocks);
                    for file_num in 0..number_of_blocks {
                        keys.push(Arc::from(format!("key_{client_num}_{file_num}").as_bytes()));
                    }
                    let mut key_slice: Vec<&[u8]> = Vec::with_capacity(number_of_blocks);
                    let mut blocks = Vec::with_capacity(number_of_blocks);
                    for k in keys.iter().take(number_of_blocks) {
                        key_slice.push(k);
                        blocks.push(PutRequest::new(k, size, 0, None))
                    }

                    client
                        .put_open(blocks, &vec![ResultCode::ErrorAlreadyExists; number_of_blocks])
                        .await;
                    let block_handles = open_map.read().await.get(&client_num).unwrap().to_owned();
                    client
                        .get_close(&block_handles, &vec![ResultCode::ErrorHandleNotFound; number_of_blocks])
                        .await;
                }));
            }

            for handle in handles {
                handle.await.unwrap();
            }
        })
        .await;
}
