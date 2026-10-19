// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use ovcc_auth_cache::{Acquire, AuthorizationCache, ProbeOutcome};

fn authorized_hit(c: &mut Criterion) {
    let cache = Arc::new(AuthorizationCache::new(1_000_000));
    let key = [7_u8; 32];
    let Acquire::Probe(probe) = cache.acquire(key, 100) else {
        panic!("new credential did not reserve a probe");
    };
    probe.complete(ProbeOutcome::Authorized, 200, 100);

    c.bench_function("authorized_hit", |b| {
        b.iter(|| {
            assert!(matches!(
                cache.acquire(black_box(key), 101),
                Acquire::Authorized
            ))
        });
    });
}

criterion_group!(benches, authorized_hit);
criterion_main!(benches);
