// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-HIT platform costs: the directory-relative segment open and the
//! zero-copy file send over loopback.

use std::fs::File;
use std::hint::black_box;
use std::io::{Read as _, Write as _};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};

fn open_regular_at(c: &mut Criterion) {
    let temp = tempfile::tempdir().unwrap();
    File::create(temp.path().join("segment"))
        .unwrap()
        .write_all(b"bytes")
        .unwrap();
    let directory = ovcc_platform::open_directory(temp.path()).unwrap();
    c.bench_function("open_regular_at", |b| {
        b.iter(|| black_box(ovcc_platform::open_regular_at(&directory, "segment").unwrap()))
    });
    c.bench_function("stat_nofollow", |b| {
        b.iter(|| black_box(ovcc_platform::stat_nofollow(temp.path()).unwrap()))
    });
    // Baseline: a plain by-path open of the same file, which isolates host
    // costs (e.g. on-access antivirus) from the relative/by-id open itself.
    let path = temp.path().join("segment");
    c.bench_function("std_file_open_baseline", |b| {
        b.iter(|| black_box(File::open(&path).unwrap()))
    });
    #[cfg(windows)]
    {
        let file = File::open(&path).unwrap();
        c.bench_function("transmit_source_new", |b| {
            b.iter(|| black_box(ovcc_platform::TransmitSource::new(&file).unwrap()))
        });
    }
}

fn send_file(c: &mut Criterion) {
    const LENGTH: usize = 1024 * 1024;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("object");
    File::create(&path)
        .unwrap()
        .write_all(&vec![7u8; LENGTH])
        .unwrap();
    let file = File::open(&path).unwrap();
    c.bench_function("map_immutable_1MiB", |b| {
        b.iter(|| {
            // SAFETY: the benchmark never modifies this published file.
            let mapping = unsafe { ovcc_platform::ReadOnlyMapping::new(&file, LENGTH).unwrap() };
            black_box(mapping.as_bytes()[0]);
            black_box(mapping.as_bytes()[LENGTH - 1]);
        })
    });
    #[cfg(windows)]
    let source = ovcc_platform::TransmitSource::new(&file).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (server, client) = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (client, accepted) =
            tokio::join!(tokio::net::TcpStream::connect(address), listener.accept());
        (accepted.unwrap().0, client.unwrap())
    });
    // A blocking drain thread keeps the socket writable.
    let mut drain = client.into_std().unwrap();
    drain.set_nonblocking(false).unwrap();
    std::thread::spawn(move || {
        let mut sink = vec![0u8; 1 << 20];
        while drain.read(&mut sink).is_ok_and(|n| n > 0) {}
    });

    let mut group = c.benchmark_group("send_file");
    group.throughput(Throughput::Bytes(LENGTH as u64));
    group.bench_function("1MiB_loopback", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let mut sent = 0u64;
                while sent < LENGTH as u64 {
                    #[cfg(target_os = "linux")]
                    {
                        use std::os::fd::AsFd as _;
                        let remaining = LENGTH - sent as usize;
                        let mut offset = sent;
                        let n = server
                            .async_io(tokio::io::Interest::WRITABLE, || {
                                ovcc_platform::sendfile(
                                    server.as_fd(),
                                    &file,
                                    &mut offset,
                                    remaining,
                                )
                            })
                            .await
                            .unwrap();
                        sent += n as u64;
                    }
                    #[cfg(windows)]
                    {
                        sent += u64::from(
                            ovcc_platform::transmit_file(
                                &server,
                                &source,
                                sent,
                                (LENGTH as u64 - sent) as u32,
                            )
                            .await
                            .unwrap(),
                        );
                    }
                }
                black_box(sent)
            })
        })
    });
    group.finish();
}

criterion_group!(benches, open_regular_at, send_file);
criterion_main!(benches);
