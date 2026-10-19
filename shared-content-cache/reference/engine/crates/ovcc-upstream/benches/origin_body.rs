// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Loopback body polling with 16 KiB origin writes and real hyper framing.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct CountingAllocator;
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

const TOTAL_BYTES: usize = 256 * 1024 * 1024;
const FRAME_BYTES: usize = 16 * 1024;

async fn transfer(streams: usize) -> (u64, u64) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let bytes_per_stream = TOTAL_BYTES / streams;
    let server = tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..streams {
            let (mut socket, _) = listener.accept().await.unwrap();
            tasks.spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).await.unwrap();
                    head.push(byte[0]);
                }
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {bytes_per_stream}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                let frame = [7; FRAME_BYTES];
                for _ in 0..bytes_per_stream / FRAME_BYTES {
                    socket.write_all(&frame).await.unwrap();
                    tokio::task::yield_now().await;
                }
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    });
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let mut clients = tokio::task::JoinSet::new();
    for _ in 0..streams {
        clients.spawn(async move {
            let mut response =
                ovcc_upstream::open_fetch_with_headers(&format!("http://{address}/body"), &[])
                    .await
                    .unwrap();
            let mut bytes = 0;
            let mut frames = 0;
            while let Some(frame) = response.body.frame().await {
                if let Ok(data) = frame.unwrap().into_data() {
                    bytes += data.len();
                    frames += 1;
                }
            }
            assert_eq!(bytes, bytes_per_stream);
            frames
        });
    }
    let mut frames = 0;
    while let Some(result) = clients.join_next().await {
        frames += result.unwrap();
    }
    server.await.unwrap();
    (frames, ALLOCATIONS.load(Ordering::Relaxed) - before)
}

fn bench_origin_body(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("origin_body");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Bytes(TOTAL_BYTES as u64));
    for streams in [1, 32] {
        // Includes client and server allocations and reports observed frame counts,
        // since TCP can merge origin writes.
        runtime.block_on(transfer(streams));
        for sample in 0..5 {
            let (frames, allocations) = runtime.block_on(transfer(streams));
            eprintln!(
                "origin_body allocations streams={streams} sample={sample} frames={frames} allocations={allocations}"
            );
        }
        group.bench_with_input(
            BenchmarkId::from_parameter(streams),
            &streams,
            |b, &streams| {
                b.iter(|| runtime.block_on(transfer(streams)));
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_origin_body);
criterion_main!(benches);
