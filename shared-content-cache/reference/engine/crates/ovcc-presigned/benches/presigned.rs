// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ovcc_presigned::classify;

fn bench_classify(c: &mut Criterion) {
    // Realistic AWS SigV4 presigned GET query (signature shortened).
    let query = "X-Amz-Algorithm=AWS4-HMAC-SHA256\
                 &X-Amz-Credential=AKIDEXAMPLE%2F20250101%2Fus-west-2%2Fs3%2Faws4_request\
                 &X-Amz-Date=20250101T000000Z&X-Amz-Expires=3600&X-Amz-SignedHeaders=host\
                 &X-Amz-Signature=deadbeefcafef00ddeadbeefcafef00ddeadbeefcafef00ddeadbeefcafef00d";
    c.bench_function("classify/s3_presigned", |b| {
        b.iter(|| classify(black_box(query)))
    });
}

criterion_group!(benches, bench_classify);
criterion_main!(benches);
