// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Zero-copy regression guard for the HIT path. The engine's whole point is
//! serving a cached body straight from the page cache to the socket with no
//! userspace copy: `sendfile(2)` (over kTLS in production) on Linux, an
//! overlapped `TransmitFile` on Windows. This bench measures that
//! file-to-socket primitive so a regression in the hot path shows up.
//! Correctness of the full kTLS pipeline is covered by `tests/ktls_sendfile.rs`.

use std::fs::File;
use std::hint::black_box;
use std::io::Read;
use std::net::{TcpListener, TcpStream};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

/// One blocking zero-copy send of up to `len` bytes of `file` at `offset`.
#[cfg(target_os = "linux")]
fn send_window(stream: &TcpStream, file: &File, offset: usize, len: usize) -> usize {
    use std::os::fd::AsFd as _;
    let mut offset = offset as u64;
    ovcc_platform::sendfile(stream.as_fd(), file, &mut offset, len).expect("sendfile")
}

/// One overlapped `TransmitFile`, awaited on a private current-thread runtime.
#[cfg(windows)]
fn send_window(stream: &TcpStream, file: &File, offset: usize, len: usize) -> usize {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("bench runtime")
    });
    let len = u32::try_from(len).expect("bench window fits u32");
    // The proxy reopens once per body; paying it per window here bounds that
    // overhead from above.
    let source = ovcc_platform::TransmitSource::new(file).expect("overlapped reopen");
    runtime
        .block_on(ovcc_platform::transmit_file(
            stream,
            &source,
            offset as u64,
            len,
        ))
        .expect("TransmitFile") as usize
}

fn send_published_frontiers(stream: &TcpStream, file: &File, frontiers: &[usize]) -> usize {
    let mut follower_offset = 0usize;
    for &published in frontiers {
        assert!(published >= follower_offset);
        while follower_offset < published {
            let sent = send_window(stream, file, follower_offset, published - follower_offset);
            if sent == 0 {
                break;
            }
            follower_offset += sent;
        }
    }
    follower_offset
}

/// Zero-copy sends a cached-object-sized file to a drained loopback socket, the
/// same file-to-socket copy the HIT driver performs.
fn bench_sendfile(c: &mut Criterion) {
    const SIZE: usize = 256 * 1024;

    let path = std::env::temp_dir().join(format!("ovcc-sendfile-bench-{}.bin", std::process::id()));
    std::fs::write(&path, vec![0u8; SIZE]).expect("write bench file");
    let file = File::open(&path).expect("open bench file");

    // A background reader keeps the socket drained so `sendfile` makes progress
    // instead of blocking on a full send buffer.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let drainer = std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = vec![0u8; 64 * 1024];
            while matches!(sock.read(&mut buf), Ok(n) if n > 0) {}
        }
    });
    let stream = TcpStream::connect(addr).expect("connect loopback");

    let mut group = c.benchmark_group("sendfile");
    group.throughput(Throughput::Bytes(SIZE as u64));
    group.bench_function("file_to_socket_256k", |b| {
        b.iter(|| {
            let mut sent = 0usize;
            while sent < SIZE {
                let n = send_window(&stream, &file, sent, SIZE - sent);
                if n == 0 {
                    break;
                }
                sent += n;
            }
            black_box(sent)
        })
    });
    group.finish();

    // Compare one committed-file send with the exact frontier-bounded windows
    // used by same-replica followers. The late case joins after three quarters
    // are already published, catches up from offset zero, then tails one final
    // window.
    let mut frontier = c.benchmark_group("sendfile_frontier");
    frontier.throughput(Throughput::Bytes(SIZE as u64));
    for (name, published_frontiers) in [
        ("committed_file", vec![SIZE]),
        (
            "in_progress_64k_windows",
            vec![64 * 1024, 128 * 1024, 192 * 1024, SIZE],
        ),
        ("late_catch_up", vec![192 * 1024, SIZE]),
    ] {
        frontier.bench_with_input(
            BenchmarkId::new("file_to_socket_256k", name),
            &published_frontiers,
            |b, published_frontiers| {
                b.iter(|| {
                    black_box(send_published_frontiers(
                        &stream,
                        &file,
                        published_frontiers,
                    ))
                })
            },
        );
    }
    frontier.finish();

    let chain_root =
        std::env::temp_dir().join(format!("ovcc-sendfile-chain-{}", std::process::id()));
    std::fs::create_dir_all(&chain_root).expect("create chain bench directory");
    let mut chain = c.benchmark_group("sendfile_chain");
    for &count in &[1usize, 16, 256, 1_969] {
        let segment_size = SIZE.div_ceil(count);
        let mut paths = Vec::with_capacity(count);
        for index in 0..count {
            let remaining = SIZE.saturating_sub(index * segment_size);
            let size = remaining.min(segment_size);
            let path = chain_root.join(format!("{count}-{index}"));
            std::fs::write(&path, vec![index as u8; size]).expect("write segment");
            paths.push((path, size));
        }
        chain.throughput(Throughput::Bytes(SIZE as u64));
        chain.bench_with_input(BenchmarkId::new("bounded_fd", count), &paths, |b, paths| {
            b.iter(|| {
                let mut sent_total = 0usize;
                for (path, size) in paths {
                    let file = File::open(path).expect("open current segment");
                    let mut sent = 0usize;
                    while sent < *size {
                        let n = send_window(&stream, &file, sent, *size - sent);
                        if n == 0 {
                            break;
                        }
                        sent += n;
                    }
                    sent_total += sent;
                }
                black_box(sent_total)
            })
        });
    }
    chain.finish();

    drop(stream);
    let _ = drainer.join();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&chain_root);
}

criterion_group!(benches, bench_sendfile);
criterion_main!(benches);
