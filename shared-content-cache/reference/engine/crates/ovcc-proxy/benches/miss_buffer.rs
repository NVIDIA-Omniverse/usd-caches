// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use hyper::body::{Body, Frame};
use std::{
    convert::Infallible,
    hint::black_box,
    pin::Pin,
    task::{Context, Poll},
};

struct Frames {
    remaining: usize,
    frame_size: usize,
}

impl Body for Frames {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        let len = self.frame_size.min(self.remaining);
        self.remaining -= len;
        Poll::Ready(Some(Ok(Frame::data(Bytes::from(vec![42; len])))))
    }
}

fn bench_miss_buffer(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("miss_buffer");
    for (name, size, frame_size) in [
        ("16k_frames", 64 * 1024, 16 * 1024),
        ("16k_frames", 1024 * 1024, 16 * 1024),
        ("16k_frames", 16 * 1024 * 1024, 16 * 1024),
        ("single_frame", 16 * 1024 * 1024, 16 * 1024 * 1024),
        ("64b_frames", 1024 * 1024, 64),
    ] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new(name, size), &size, |b, &size| {
            b.iter(|| {
                black_box(
                    runtime
                        .block_on(ovcc_proxy::collect_miss_body(
                            Frames {
                                remaining: size,
                                frame_size,
                            },
                            size,
                        ))
                        .unwrap(),
                )
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_miss_buffer);
criterion_main!(benches);
