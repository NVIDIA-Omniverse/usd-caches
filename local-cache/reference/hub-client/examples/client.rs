// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use hub_client::utils::PutRequest;
use hub_client::Client;
use hub_rpc::ResultCode;
use positioned_io::{RandomAccessFile, ReadAt, WriteAt};
use rand::Rng;
use std::fs;
use std::io::Write;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    std::env::set_var("OMNICLIENT_HUB_EXE", "target/debug/hub");
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let client = Client::builder("example_client")
        .cache_path(tmp_dir.path())
        .mode(hub_client::Mode::Exclusive)
        .build()
        .unwrap();
    let version = client.cache_aside.get_server_api_version().await.unwrap();
    println!("Version: {version:?}");

    let write_res = client
        .cache_aside
        .cache_open_for_write("key", "provider", "exe", 10)
        .await
        .unwrap();

    println!("Write Result: {write_res:?}");

    {
        let mut f = fs::File::create(write_res.local_file.as_ref().unwrap().clone()).unwrap();
        writeln!(f, "{}", write_res.local_file.as_ref().unwrap().display()).unwrap();
    }

    let write_handle = write_res.handle.unwrap();
    let close_res = client.cache_aside.close(write_handle).await.unwrap();

    println!("Close Result: {close_res:?}");

    let read_res = client
        .cache_aside
        .cache_open_for_read("key", "provider", "exe")
        .await
        .unwrap();

    println!("Read Result: {read_res:?}");

    let read_handle = read_res.handle.unwrap();
    let remove_res = client.cache_aside.remove(read_handle).await.unwrap();

    println!("Remove Result: {remove_res:?}");

    for i in 0..100 {
        let mut rng = rand::thread_rng();
        let key_count = rng.gen_range(10..100);
        println!("Starting block cache loop {i:?} with {key_count} keys");
        let mut key_data = Vec::with_capacity(key_count);
        let mut blocks = Vec::with_capacity(key_count);
        let mut key_slices = Vec::with_capacity(key_count);
        let mut value_data = Vec::with_capacity(key_count);

        for _ in 0..key_count {
            let key_size = rng.gen_range(1..512);
            let data_size = rng.gen_range(1..65536);
            let mut k_d: Vec<u8> = Vec::with_capacity(key_size);
            let mut v_d: Vec<u8> = Vec::with_capacity(data_size);
            for _ in 0..key_size {
                k_d.push(rng.gen());
            }
            for _ in 0..data_size {
                v_d.push(rng.gen());
            }
            key_data.push(k_d);
            value_data.push(v_d);
        }

        for i in 0..key_count {
            key_slices.push(key_data[i].as_slice());
            blocks.push(PutRequest::new(
                key_data[i].as_slice(),
                value_data[i].len() as u64,
                0,
                None,
            ))
        }

        let blocks = hub_client::utils::put_open(&client, blocks).await.unwrap();

        // Write the data
        let mut handles = Vec::new();
        assert_eq!(blocks.len(), value_data.len());
        for (index, block) in blocks.iter().enumerate() {
            match block.result {
                ResultCode::Success => {
                    let b = block.block.as_ref().unwrap();
                    assert_eq!(b.size, value_data[index].len() as u64);
                    let mut f = RandomAccessFile::try_new(
                        std::fs::OpenOptions::new()
                            .write(true)
                            .create(false)
                            .open(&b.file_path)
                            .unwrap(),
                    )
                    .unwrap();
                    assert_eq!(
                        b.size,
                        f.write_at(b.file_offset, value_data[index].as_slice()).unwrap() as u64
                    );
                    handles.push(b.handle);
                },
                _ => panic!("Failed put open for {:?}", key_slices[index]),
            }
        }

        let commit_res = hub_client::utils::put_commit(&client, &handles).await.unwrap();
        for res in commit_res {
            assert_eq!(res, ResultCode::Success);
        }

        let blocks = hub_client::utils::get_open(&client, &key_slices, 0).await.unwrap();
        // read the data
        let mut handles = Vec::new();
        assert_eq!(blocks.len(), value_data.len());
        for (index, block) in blocks.iter().enumerate() {
            match block.result {
                ResultCode::Success => {
                    let b = block.block.as_ref().unwrap();
                    assert_eq!(b.size, value_data[index].len() as u64);
                    let f = RandomAccessFile::open(&b.file_path).unwrap();
                    let mut buf: Vec<u8> = vec![0; b.size as usize];
                    assert_eq!(b.size, f.read_at(b.file_offset, buf.as_mut_slice()).unwrap() as u64);
                    assert_eq!(buf, value_data[index]);
                    handles.push(b.handle);
                },
                _ => panic!("Failed put open for {:?}", key_slices[index]),
            }
        }

        let close_res = hub_client::utils::get_close(&client, &handles).await.unwrap();
        for res in close_res {
            assert_eq!(res, ResultCode::Success);
        }

        let stat_res = hub_client::utils::stat(&client, &key_slices, 0).await.unwrap();
        for (index, result) in stat_res.iter().enumerate() {
            match result.result {
                ResultCode::Success => {
                    assert_eq!(result.metadata.as_ref().unwrap().size, value_data[index].len() as u64);
                },
                ResultCode::ErrorNotFound => {
                    panic!("key not found in stat {:?}", key_slices[index]);
                },
                ResultCode::ErrorPendingWrite => {
                    panic!("key pending write in stat {:?}", key_slices[index]);
                },
                _ => panic!("Invalid type"),
            }
        }
    }

    client.shutdown();
}
