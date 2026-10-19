// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

// Not all tests will use all functions in this file, so allow dead code
#![allow(dead_code, unused_macros)]

/// This file contains all of the common infrastructure used for the integration tests. It provides a TestService
/// struct that serves as the fixture for the tests, and then MessagePack test client.
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Child,
    thread::sleep,
    time::Duration,
};

use curl::easy::{Easy2, Handler, WriteError};
use hub_client::{Client, Mode, TcpCodec};
use hub_rpc::ResultCode;
use rstest::fixture;

/// Number of retries to perform when waiting for things like process launching or port file writing.
/// This is a large number because aarch64 tests can be very slow.
static RETRY_COUNT: u64 = 2000;

////////////////////////////////////////////////////////////////////////////////
// TestService
////////////////////////////////////////////////////////////////////////////////

#[allow(async_fn_in_trait)]
pub trait TestService {
    async fn connect_client(&self) -> TestClient;
    async fn connect_client_to_existing_hub(&self, cache_path: impl AsRef<Path>) -> TestClient;
}

////////////////////////////////////////////////////////////////////////////////
// MsgPackTestService
////////////////////////////////////////////////////////////////////////////////

pub struct MsgPackTestService {}

impl Default for MsgPackTestService {
    fn default() -> Self {
        Self::new()
    }
}

impl MsgPackTestService {
    pub fn new() -> Self {
        #[cfg(unix)]
        {
            // Increase open file rlimit for larger tests
            let desired_nofile = 100000;
            rlimit::increase_nofile_limit(desired_nofile).unwrap();
            let (soft, _) = rlimit::Resource::NOFILE.get().unwrap();
            if soft < desired_nofile {
                panic!("Could not set NOFILE limit");
            }
        }

        Self {}
    }
}

impl TestService for MsgPackTestService {
    async fn connect_client(&self) -> TestClient {
        TestClient::new(Mode::Exclusive, TcpCodec::MessagePack).await
    }

    async fn connect_client_to_existing_hub(&self, cache_path: impl AsRef<Path>) -> TestClient {
        TestClient::new_with_existing_cache_path(cache_path, Mode::Exclusive, TcpCodec::MessagePack).await
    }
}

#[fixture]
pub fn msgpack_test_service() -> MsgPackTestService {
    MsgPackTestService::new()
}

////////////////////////////////////////////////////////////////////////////////
// ProtobufTestService
////////////////////////////////////////////////////////////////////////////////

pub struct ProtobufTestService {}

impl Default for ProtobufTestService {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtobufTestService {
    pub fn new() -> Self {
        #[cfg(unix)]
        {
            // Increase open file rlimit for larger tests
            let desired_nofile = 100000;
            rlimit::increase_nofile_limit(desired_nofile).unwrap();
            let (soft, _) = rlimit::Resource::NOFILE.get().unwrap();
            if soft < desired_nofile {
                panic!("Could not set NOFILE limit");
            }
        }

        Self {}
    }
}

impl TestService for ProtobufTestService {
    async fn connect_client(&self) -> TestClient {
        TestClient::new(Mode::Exclusive, TcpCodec::Protobuf).await
    }

    async fn connect_client_to_existing_hub(&self, cache_path: impl AsRef<Path>) -> TestClient {
        TestClient::new_with_existing_cache_path(cache_path, Mode::Exclusive, TcpCodec::Protobuf).await
    }
}

#[fixture]
pub fn protobuf_test_service() -> ProtobufTestService {
    ProtobufTestService::new()
}

////////////////////////////////////////////////////////////////////////////////
// TestClient
////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Clone, Copy)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

/// MessagePack client wrapper. Because the MessagePack client provides automatic Hub process
/// management, this class doesn't have to.
pub struct TestClient {
    pub handle_map: HashMap<String, u64>,
    pub provider: String,
    pub hub_client: Client,
    pub tmp_dir: Option<tempfile::TempDir>,
    pub host: String,
    pub http_port: Option<u16>,
    pub cmd: Option<Child>,
}

impl TestClient {
    /// Creates a new MessagePack client.
    /// This creates the cache directory for the test and then initializes the MessagePack client.
    pub async fn new(mode: Mode, codec: TcpCodec) -> Self {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let hub_client = TestClient::connect_to_server(tmp_dir.path(), mode, codec).await;

        // Get the HTTP port from the cache directory
        let http_port = wait_for_port(tmp_dir.path());

        // Wait for the server to actually be running before starting the tests
        let mut cmd: Option<Child> = None;
        for _ in 0..RETRY_COUNT {
            if let Some(child) = hub_client.get_hub_process() {
                cmd = Some(child);
                break;
            } else {
                sleep(Duration::from_millis(25));
            }
        }
        assert!(cmd.is_some(), "Hub Subprocess never launched");
        Self {
            handle_map: HashMap::new(),
            provider: "provider".to_owned(),
            hub_client,
            tmp_dir: Some(tmp_dir),
            host: "127.0.0.1".to_owned(),
            http_port: Some(http_port),
            cmd,
        }
    }

    /// Creates a MessagePack client with an existing cache directory
    pub async fn new_with_existing_cache_path(tmp_dir: impl AsRef<Path>, mode: Mode, codec: TcpCodec) -> Self {
        let hub_client = TestClient::connect_to_server(tmp_dir.as_ref(), mode, codec).await;
        let http_port = wait_for_port(tmp_dir.as_ref());
        let mut cmd: Option<Child> = None;
        for _ in 0..RETRY_COUNT {
            if let Some(child) = hub_client.get_hub_process() {
                cmd = Some(child);
                break;
            } else {
                sleep(Duration::from_millis(25));
            }
        }
        assert!(cmd.is_some(), "Hub Subprocess never launched");
        Self {
            handle_map: HashMap::new(),
            provider: "provider".to_owned(),
            hub_client,
            tmp_dir: None,
            host: "127.0.0.1".to_owned(),
            http_port: Some(http_port),
            cmd,
        }
    }

    async fn connect_to_server(tmp_dir: impl AsRef<Path>, mode: Mode, codec: TcpCodec) -> Client {
        std::env::set_var("OMNICLIENT_HUB_EXE", env!("CARGO_BIN_EXE_hub"));
        Client::builder("test_msgpack_client")
            .mode(mode)
            .tcp_codec(codec)
            .cache_path(tmp_dir)
            .build()
            .unwrap()
    }

    pub fn populate_file(&self, path: PathBuf) {
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{}", path.display()).unwrap();
        for i in 0..100 {
            writeln!(f, "value_{i:02}").unwrap();
        }
    }

    pub fn kill_hub_api(&mut self) {
        // Call the shutdown endpoint
        let mut easy = Easy2::new(Collector::new());
        easy.url(format!("{}:{}/api/1.0/shutdown", self.host, self.http_port.unwrap()).as_str())
            .unwrap();
        easy.perform().unwrap();

        self.wait_for_hub_exit();
    }

    fn kill_hub(&mut self) {
        self.cmd.as_mut().unwrap().kill().unwrap();
        self.wait_for_hub_exit();
    }

    fn wait_for_hub_exit(&mut self) -> bool {
        for _ in 0..RETRY_COUNT {
            match self.cmd.as_mut().unwrap().try_wait() {
                Ok(Some(status)) => {
                    println!("Hub Subprocess Exit status: {status}");
                    self.cmd = None;
                    return true;
                },
                Ok(None) => {
                    sleep(Duration::from_millis(10));
                },
                Err(e) => panic!("error attempting to wait: {e}"),
            }
        }
        panic!("Hub command did not exit after being killed");
    }

    pub async fn get_open(&self, keys: &[&[u8]], expected_results: &[ResultCode], timeout_us: u64) -> Vec<u64> {
        let results = hub_client::utils::get_open(&self.hub_client, keys, timeout_us)
            .await
            .unwrap();
        let mut handles = Vec::new();
        for (i, b) in results.iter().enumerate() {
            assert_eq!(b.result, expected_results[i]);
            if b.result == ResultCode::Success {
                handles.push(b.block.as_ref().unwrap().handle);
            }
        }
        handles
    }

    pub async fn get_close(&self, handles: &[u64], expected_results: &[ResultCode]) {
        let results = hub_client::utils::get_close(&self.hub_client, handles).await.unwrap();
        for (i, r) in results.iter().enumerate() {
            assert_eq!(*r, expected_results[i]);
        }
    }

    pub async fn put_open(
        &self,
        blocks: Vec<hub_client::utils::PutRequest>,
        expected_results: &[ResultCode],
    ) -> Vec<u64> {
        let results = hub_client::utils::put_open(&self.hub_client, blocks).await.unwrap();
        let mut handles = Vec::new();
        for (i, b) in results.iter().enumerate() {
            assert_eq!(b.result, expected_results[i]);
            if b.result == ResultCode::Success {
                handles.push(b.block.as_ref().unwrap().handle);
            }
        }
        handles
    }

    pub async fn put_commit(&self, handles: &[u64], expected_results: &[ResultCode]) {
        let results = hub_client::utils::put_commit(&self.hub_client, handles).await.unwrap();
        for (i, r) in results.iter().enumerate() {
            assert_eq!(*r, expected_results[i]);
        }
    }

    pub async fn put_abort(&self, handles: &[u64], expected_results: &[ResultCode]) {
        let results = hub_client::utils::put_abort(&self.hub_client, handles).await.unwrap();
        for (i, r) in results.iter().enumerate() {
            assert_eq!(*r, expected_results[i]);
        }
    }

    pub async fn connect(&mut self) {}

    pub async fn create_file(&mut self, key: &str) {
        let write_response = self
            .hub_client
            .cache_aside
            .cache_open_for_write(key, &self.provider, "extension", 10)
            .await
            .unwrap();
        assert_eq!(write_response.result, ResultCode::Success);
        assert!(write_response.local_file.is_some());

        self.populate_file(write_response.local_file.as_ref().unwrap().clone());

        let close_response = self
            .hub_client
            .cache_aside
            .close(write_response.handle.unwrap())
            .await
            .unwrap();
        assert_eq!(close_response.result, ResultCode::Success);
    }

    pub async fn read_file(&mut self, key: &str, expected_result: ResultCode) {
        let read_response = self
            .hub_client
            .cache_aside
            .cache_open_for_read(key, &self.provider, "extension")
            .await
            .unwrap();

        assert_eq!(read_response.result, expected_result, "key {key}");

        if expected_result == ResultCode::Success {
            assert!(read_response.local_file.is_some());
            let read_handle = read_response.handle.unwrap();
            self.handle_map.insert(key.to_owned(), read_handle);
        } else {
            // Any non-success result should not have a local file
            assert!(read_response.local_file.is_none());
        }
    }

    pub async fn write_file(&mut self, key: &str, expected_result: ResultCode) -> Option<(PathBuf, u64)> {
        let write_response = self
            .hub_client
            .cache_aside
            .cache_open_for_write(key, &self.provider, "extension", 10)
            .await
            .unwrap();
        assert_eq!(write_response.result, expected_result);

        if expected_result == ResultCode::Success {
            assert!(write_response.local_file.is_some());
            let write_handle = write_response.handle.unwrap();
            self.handle_map.insert(key.to_owned(), write_handle);
        }

        if let (Some(local_file), Some(handle)) = (write_response.local_file, write_response.handle) {
            Some((local_file, handle))
        } else {
            None
        }
    }

    pub async fn close(&mut self, key: &str, expected_result: ResultCode) {
        let handle = self.handle_map.get(key).unwrap();
        self.close_handle(*handle, expected_result).await;
        if expected_result == ResultCode::Success {
            self.handle_map.remove(key);
        }
    }

    pub async fn close_handle(&self, handle: u64, expected_result: ResultCode) {
        let close_response = self.hub_client.cache_aside.close(handle).await.unwrap();
        assert_eq!(close_response.result, expected_result);
    }

    pub async fn remove(&mut self, key: &str, expected_result: ResultCode) {
        let handle = self.handle_map.get(key).unwrap();

        self.remove_handle(*handle, expected_result).await;

        if expected_result == ResultCode::Success {
            self.handle_map.remove(key);
        }
    }

    pub async fn remove_handle(&self, handle: u64, expected_result: ResultCode) {
        let remove_response = self.hub_client.cache_aside.remove(handle).await.unwrap();
        assert_eq!(remove_response.result, expected_result);
    }

    pub async fn get_server_api_version(&mut self) -> SemVer {
        // Check cache aside Api version information
        let current_version = self
            .hub_client
            .cache_aside
            .get_server_api_version()
            .await
            .unwrap()
            .version;
        SemVer {
            major: current_version.major,
            minor: current_version.minor,
            patch: current_version.patch,
        }
    }

    pub async fn wait_for_close_on_disconnect(&mut self, key: &str) {
        // Loop until we see the keys have been closed. Windows can be slow to detect the closed connection
        // so this loop keeps trying to wait for the key to be closed.
        let mut retries = 0;
        loop {
            let read_response = self
                .hub_client
                .cache_aside
                .cache_open_for_read(format!("{}_{}", key, 0), &self.provider, "extension")
                .await
                .unwrap();

            if read_response.result != ResultCode::ErrorNotFound {
                retries += 1;
                assert!(retries < RETRY_COUNT, "File was not closed on disconnect");
                sleep(Duration::from_millis(20 * (retries)));
                continue;
            } else {
                break;
            }
        }
    }

    pub async fn restart(&mut self, method: RestartMethod) {
        if self.tmp_dir.is_some() {
            // Remove the old port file so we wait for the new one on relaunch
            let port_file = self.tmp_dir.as_ref().unwrap().path().join("hub.port");
            fs::remove_file(port_file).unwrap();

            match method {
                RestartMethod::Kill => self.kill_hub(),
                RestartMethod::Api => self.kill_hub_api(),
            };
        }
        println!("Hub has been killed");
    }

    // Loops until successful health API call or timeout
    pub fn wait_for_api(&self) {
        wait_for_api(&self.host, self.http_port.unwrap());
    }

    pub fn get_http_connection(&self) -> (String, u16) {
        (self.host.clone(), self.http_port.unwrap())
    }

    pub fn get_cache_path(&self) -> &Path {
        self.tmp_dir.as_ref().unwrap().path()
    }

    pub async fn reconnect_to_server(&mut self) {
        self.http_port = Some(wait_for_port(self.tmp_dir.as_ref().unwrap().path()));
    }

    pub fn clear_open_entries(&self) {
        self.hub_client.cache_aside.clear_open_files();
        self.hub_client.block_cache.clear_open_blocks();
    }
}

#[derive(Clone, Copy)]
pub enum RestartMethod {
    Kill,
    Api,
}

// Waits for the hub.port file to exist and returns the port
fn wait_for_port(cache_path: &Path) -> u16 {
    let mut retries = 0;
    let port_file = cache_path.join("hub.port");
    loop {
        if port_file.exists() {
            let read_port = fs::read_to_string(port_file).expect("Unable to read port file");
            println!("Read port {read_port} from file");
            return read_port.parse().unwrap();
        } else {
            retries += 1;
            assert!(
                retries < RETRY_COUNT,
                "Failed to read port file from cache directory: File doesn't exist"
            );
            sleep(Duration::from_millis(10));
            continue;
        }
    }
}

fn wait_for_api(host: &String, http_port: u16) {
    let mut retries = 0;
    let mut easy = Easy2::new(Collector::new());
    easy.url(format!("{host}:{http_port}/api/health").as_str()).unwrap();
    loop {
        match easy.perform() {
            Ok(_) => return,
            Err(_) => {
                retries += 1;
                assert!(retries < RETRY_COUNT, "Failed to connect to Hub API");
                sleep(Duration::from_millis(10));
                continue;
            },
        }
    }
}

pub struct Collector {
    pub data: Vec<u8>,
    pub upload: String,
}

impl Collector {
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            upload: String::from(""),
        }
    }

    pub fn new_upload(upload: String) -> Self {
        Self {
            data: Vec::new(),
            upload,
        }
    }
}

impl Handler for Collector {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        self.data.extend_from_slice(data);
        Ok(data.len())
    }

    fn read(&mut self, data: &mut [u8]) -> Result<usize, curl::easy::ReadError> {
        let (write, _) = data.split_at_mut(self.upload.len());
        write.copy_from_slice(self.upload.as_bytes());
        Ok(self.upload.len())
    }
}
