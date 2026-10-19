// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Regression guard for the transparent relay's per-upload framing decision and
//! HTTP/1.1 chunked decode.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ovcc_proxy::{ChunkedDecoder, TransparentFraming, transparent_body_framing};

fn hdr(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn encode_chunked(object: &[u8], chunk_size: usize) -> Vec<u8> {
    let mut wire = Vec::with_capacity(object.len() + object.len() / chunk_size * 8);
    for chunk in object.chunks(chunk_size) {
        wire.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        wire.extend_from_slice(chunk);
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend_from_slice(b"0\r\n\r\n");
    wire
}

/// Per-upload framing decision on the transparent path. Pure; runs once before I/O.
fn bench_transparent_framing(c: &mut Criterion) {
    let fixed = hdr(&[
        ("Content-Type", "text/plain"),
        ("Content-Length", "1048576"),
    ]);
    let te_chunked = hdr(&[("Transfer-Encoding", "chunked")]);
    let aws_verbatim = hdr(&[
        ("Content-Encoding", "aws-chunked"),
        ("Content-Length", "42"),
    ]);
    let aws_chunked = hdr(&[
        ("Transfer-Encoding", "chunked"),
        ("Content-Encoding", "aws-chunked"),
    ]);
    let streaming = hdr(&[
        ("Transfer-Encoding", "chunked"),
        ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
    ]);

    let mut group = c.benchmark_group("transparent_framing");
    group.bench_function("fixed", |b| {
        b.iter(|| transparent_body_framing(black_box(&fixed), black_box(1_048_576)))
    });
    group.bench_function("te_chunked", |b| {
        b.iter(|| transparent_body_framing(black_box(&te_chunked), black_box(0)))
    });
    group.bench_function("aws_verbatim", |b| {
        b.iter(|| transparent_body_framing(black_box(&aws_verbatim), black_box(42)))
    });
    group.bench_function("reject_aws_chunked", |b| {
        b.iter(|| black_box(transparent_body_framing(black_box(&aws_chunked), black_box(0)).err()))
    });
    group.bench_function("reject_streaming_sig", |b| {
        b.iter(|| black_box(transparent_body_framing(black_box(&streaming), black_box(0)).err()))
    });
    group.finish();
}

/// Chunked decode on the transparent upload feed.
fn bench_chunked_decoder(c: &mut Criterion) {
    const OBJECT_1M: usize = 1024 * 1024;
    let object_1m = vec![b'.'; OBJECT_1M];
    let wire_64k = encode_chunked(&object_1m, 64 * 1024);
    let wire_16k = encode_chunked(&object_1m, 16 * 1024);
    let wire_small = encode_chunked(b"helloworld", 5);

    let mut group = c.benchmark_group("chunked_decoder");
    group.throughput(Throughput::Bytes(OBJECT_1M as u64));

    group.bench_function("decode_1m_64k_chunks_single_push", |b| {
        b.iter(|| {
            let mut decoder = ChunkedDecoder::new();
            let mut out = Vec::new();
            let done = decoder
                .push(black_box(&wire_64k), black_box(&mut out))
                .expect("valid chunked");
            black_box((done, out.len()))
        })
    });

    group.bench_function("decode_1m_64k_chunks_split_4k_reads", |b| {
        b.iter(|| {
            let mut decoder = ChunkedDecoder::new();
            let mut out = Vec::new();
            for piece in wire_64k.chunks(4096) {
                decoder
                    .push(black_box(piece), black_box(&mut out))
                    .expect("valid chunked");
            }
            black_box((decoder.finished(), out.len()))
        })
    });

    group.bench_function("decode_1m_16k_chunks_single_push", |b| {
        b.iter(|| {
            let mut decoder = ChunkedDecoder::new();
            let mut out = Vec::new();
            let done = decoder
                .push(black_box(&wire_16k), black_box(&mut out))
                .expect("valid chunked");
            black_box((done, out.len()))
        })
    });

    group.throughput(Throughput::Bytes(10));
    group.bench_function("decode_small_two_chunks", |b| {
        b.iter(|| {
            let mut decoder = ChunkedDecoder::new();
            let mut out = Vec::new();
            let done = decoder
                .push(black_box(&wire_small), black_box(&mut out))
                .expect("valid chunked");
            black_box((done, out))
        })
    });

    group.finish();
}

/// Framing plus decode on a 1 MiB chunked upload.
fn bench_framing_then_decode(c: &mut Criterion) {
    const OBJECT_1M: usize = 1024 * 1024;
    let headers = hdr(&[("Transfer-Encoding", "chunked")]);
    let wire = encode_chunked(&vec![b'.'; OBJECT_1M], 64 * 1024);

    let mut group = c.benchmark_group("transparent_chunked_upload");
    group.throughput(Throughput::Bytes(OBJECT_1M as u64));
    group.bench_function("classify_and_decode_1m", |b| {
        b.iter(|| {
            let framing =
                transparent_body_framing(black_box(&headers), black_box(0)).expect("chunked");
            assert_eq!(framing, TransparentFraming::Chunked);
            let mut decoder = ChunkedDecoder::new();
            let mut out = Vec::new();
            let done = decoder
                .push(black_box(&wire), black_box(&mut out))
                .expect("valid chunked");
            black_box((done, out.len()))
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_transparent_framing,
    bench_chunked_decoder,
    bench_framing_then_decode
);
criterion_main!(benches);
