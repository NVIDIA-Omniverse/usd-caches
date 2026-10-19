// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#[macro_use]
mod common;

use crate::common::*;

use std::{env, fs};

use curl::easy::{self, Easy2};
use hub_rpc::ResultCode;
use rstest::rstest;
use serde_json::{json, Value};

// NOTE: These tests cover the HTTP API, and so the RPC protocol shouldn't
//       actually matter. So we'll just use the MessagePack interface. To test
//       both, we'd need to specify separate locations for the hub.port file
//       or specify that the tests should be run in #[serial].

/// Tests the Hub Server writes out the HTTP port file
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[test_log::test(tokio::test)]
async fn port_file(#[case] test_service: impl TestService) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let test_client = test_service.connect_client().await;
            test_client.wait_for_api();

            let port_file = test_client.get_cache_path().join("hub.port");
            assert!(port_file.exists());
            let read_port = fs::read_to_string(port_file).expect("Unable to read port file");
            assert_eq!(read_port, test_client.get_http_connection().1.to_string());
            assert_eq!(read_port, test_client.hub_client.get_hub_http_port().to_string());
        })
        .await;
}

/// Tests the Hub Server health API
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[test_log::test(tokio::test)]
async fn health(#[case] test_service: impl TestService) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let test_client = test_service.connect_client().await;
            test_client.wait_for_api();

            // Perform a health check
            let mut easy = Easy2::new(Collector::new());
            let (host, http_port) = test_client.get_http_connection();
            easy.url(format!("{host}:{http_port}/api/health").as_str()).unwrap();
            easy.perform().unwrap();
            assert_eq!(easy.response_code().unwrap(), 200);

            let contents = easy.get_ref();
            let health = serde_json::from_slice::<Value>(&contents.data).unwrap();
            assert_eq!(health["health"].as_str().unwrap(), "ok");
        })
        .await;
}

/// Tests the Hub Server status API
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[test_log::test(tokio::test)]
async fn status(#[case] test_service: impl TestService) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let test_client = test_service.connect_client().await;
            test_client.wait_for_api();

            // get the status
            let mut easy = Easy2::new(Collector::new());
            let (host, http_port) = test_client.get_http_connection();
            easy.url(format!("{host}:{http_port}/api/1.0/service/status").as_str())
                .unwrap();
            easy.perform().unwrap();
            assert_eq!(easy.response_code().unwrap(), 200);

            let contents = easy.get_ref();
            let body = serde_json::from_slice::<Value>(&contents.data).unwrap();
            assert_eq!(body["config"]["cache"]["path"], json!(test_client.get_cache_path()));
            assert_eq!(body["exe_path"], json!(env!("CARGO_BIN_EXE_hub")));
        })
        .await;
}

/// Tests the Hub Server stats API
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[test_log::test(tokio::test)]
async fn cache_stats(#[case] test_service: impl TestService) {
    let file_data = "some cool data to write to a file";

    // Perform a bunch of database operations
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut test_client = test_service.connect_client().await;

            // Try to read the open file, which fail because it doesn't exist
            test_client.read_file("key", ResultCode::ErrorNotFound).await;

            // Open the file for writing
            let (path, _) = test_client.write_file("key", ResultCode::Success).await.unwrap();

            // create file (otherwise close() will fail)
            fs::write(path, file_data).unwrap();

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

            // Get the cache stats
            let mut easy = Easy2::new(Collector::new());
            let (host, http_port) = test_client.get_http_connection();
            easy.url(format!("{host}:{http_port}/api/1.0/cache/stats").as_str())
                .unwrap();
            easy.perform().unwrap();
            assert_eq!(easy.response_code().unwrap(), 200);

            let result = serde_json::from_slice::<Value>(&easy.get_ref().data).unwrap();
            assert_eq!(result["cache_entries"].as_u64().unwrap(), 1);
            assert_eq!(result["cache_size"].as_u64().unwrap(), file_data.len() as u64);
            assert_eq!(result["cache_hits"].as_u64().unwrap(), 1);
            assert_eq!(result["cache_misses"].as_u64().unwrap(), 2);
            assert_eq!(result["total_stored_data"].as_u64().unwrap(), file_data.len() as u64);
            assert_eq!(result["reads_during_file_write"].as_u64().unwrap(), 1);
            assert_eq!(result["writes_during_file_write"].as_u64().unwrap(), 1);
        })
        .await;
}

/// Tests updating the Hub configuration via the config API
#[ignore = "We need the ability to provide the hub_toml command line argument in the tests, otherwise this test tries to write over the user's hub.toml"]
#[rstest]
#[case::msgpack(msgpack_test_service())]
#[test_log::test(tokio::test)]
async fn update_config(#[case] test_service: impl TestService) {
    // Do a quick version check to test connection
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut client = test_service.connect_client().await;
            let returned_version = client.get_server_api_version().await;
            assert_eq!(1, returned_version.major);

            // Preremove the hub port file so we can wait for it to be recreated
            fs::remove_file(client.get_cache_path().join("hub.port")).unwrap();

            let gc_interval = 12345;
            // Update a config option to force a hub reset
            let json_data = json!({
                "cache": {
                    "garbage_collection_interval": gc_interval,
                },
            })
            .to_string();

            let mut easy = Easy2::new(Collector::new_upload(json_data.clone()));
            let (host, http_port) = client.get_http_connection();
            easy.url(format!("{host}:{http_port}/api/1.0/service/config").as_str())
                .unwrap();
            easy.upload(true).unwrap();
            let mut headers = easy::List::new();
            headers.append("Content-Type: application/json").unwrap();
            easy.http_headers(headers).unwrap();
            easy.in_filesize(json_data.len() as u64).unwrap();
            easy.perform().unwrap();
            assert_eq!(easy.response_code().unwrap(), 200);

            // Reconnect to the test service to the new ports
            client.reconnect_to_server().await;

            let returned_version = client.get_server_api_version().await;
            assert_eq!(1, returned_version.major);

            // Check updated config value
            let mut easy = Easy2::new(Collector::new());
            let (host, http_port) = client.get_http_connection();
            easy.url(format!("{host}:{http_port}/api/1.0/service/config").as_str())
                .unwrap();
            easy.perform().unwrap();
            assert_eq!(easy.response_code().unwrap(), 200);

            let contents = easy.get_ref();
            let config = serde_json::from_slice::<Value>(&contents.data).unwrap();
            let reported_interval = config["cache"]["garbage_collection_interval"].as_u64().unwrap();
            assert_eq!(gc_interval, reported_interval);
        })
        .await;
}
