// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use ovcc_blocklist::{
    BlockId, BlockMetadata, BlockMetadataSource, parse_block_list, plan_segments,
};

/// Builds a Put Block List XML body naming `n` synthetic `<Latest>` blocks.
fn block_list_xml(n: usize) -> String {
    let mut xml = String::from("<?xml version=\"1.0\"?><BlockList>");
    for i in 0..n {
        xml.push_str("<Latest>");
        xml.push_str(&format!("block-{i:08}"));
        xml.push_str("</Latest>");
    }
    xml.push_str("</BlockList>");
    xml
}

/// In-memory staged metadata keeps the benchmark on pure planning overhead.
struct MemMetadata(HashMap<String, u64>);

impl MemMetadata {
    fn with_blocks(n: usize) -> Self {
        Self((0..n).map(|i| (format!("block-{i:08}"), 16)).collect())
    }
}

impl BlockMetadataSource for MemMetadata {
    fn metadata(&self, id: &BlockId) -> Option<BlockMetadata> {
        self.0.get(id.as_str()).copied().map(BlockMetadata::new)
    }
}

/// Parsing the commit XML scales with block count; this is the commit-time cost
/// before any fragment is touched.
fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_block_list");
    for &n in &[1usize, 100, 10_000] {
        let xml = block_list_xml(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &xml, |b, xml| {
            b.iter(|| parse_block_list(black_box(xml.as_bytes())))
        });
    }
    group.finish();
}

/// Planning resolves unique metadata and materializes each ordered occurrence.
fn bench_plan(c: &mut Criterion) {
    let mut group = c.benchmark_group("plan_segments");
    for &n in &[1usize, 16, 256, 1_969] {
        let source = MemMetadata::with_blocks(n);
        let list: Vec<BlockId> = (0..n)
            .map(|i| BlockId::new(format!("block-{i:08}")))
            .collect();
        group.bench_with_input(BenchmarkId::from_parameter(n), &list, |b, list| {
            b.iter(|| plan_segments(black_box(list), black_box(&source)).unwrap())
        });
    }
    group.finish();
}

criterion_group!(benches, bench_parse, bench_plan);
criterion_main!(benches);
