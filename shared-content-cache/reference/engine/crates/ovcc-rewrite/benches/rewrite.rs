// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ovcc_rewrite::{RewriteRule, Substitute, apply};

fn pip_rule() -> RewriteRule {
    RewriteRule {
        uri_starts_with: "/simple/".to_owned(),
        types: vec!["text/html".to_owned()],
        substitute: Substitute {
            match_: "https://files.pythonhosted.org".to_owned(),
            replace: "http://files.pythonhosted.org.cache.svc.cluster.local:14128".to_owned(),
        },
    }
}

// A realistic simple-index body: many links, each pointing at the artifact host.
fn index_body(links: usize) -> Vec<u8> {
    let mut body = String::from("<!DOCTYPE html><html><body>\n");
    for i in 0..links {
        body.push_str(&format!(
            "<a href=\"https://files.pythonhosted.org/packages/ab/cd/pkg-{i}.whl#sha256=deadbeef\">pkg-{i}.whl</a>\n"
        ));
    }
    body.push_str("</body></html>\n");
    body.into_bytes()
}

fn bench_apply(c: &mut Criterion) {
    let rules = [pip_rule()];
    let mut group = c.benchmark_group("apply");

    // The hot case the engine actually runs: a many-link index gets every link
    // rewritten on the cache-fill path.
    let many = index_body(200);
    group.bench_function("index_200_links", |b| {
        b.iter(|| {
            apply(
                black_box(&rules),
                black_box("/simple/idna/"),
                black_box("text/html"),
                black_box(&many),
            )
        })
    });

    // The fast reject: a body whose content type is not eligible never scans.
    group.bench_function("content_type_reject", |b| {
        b.iter(|| {
            apply(
                black_box(&rules),
                black_box("/simple/idna/"),
                black_box("application/octet-stream"),
                black_box(&many),
            )
        })
    });

    // The scan-but-no-match case: eligible body with no occurrence of the match.
    let no_match = vec![b'.'; 8192];
    group.bench_function("scan_no_match", |b| {
        b.iter(|| {
            apply(
                black_box(&rules),
                black_box("/simple/idna/"),
                black_box("text/html"),
                black_box(&no_match),
            )
        })
    });

    group.finish();
}

criterion_group!(benches, bench_apply);
criterion_main!(benches);
