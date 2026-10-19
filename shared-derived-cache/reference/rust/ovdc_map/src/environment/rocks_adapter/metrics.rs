// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use ovdc_trace::{label, metrics::buckets_seconds};

label! {
    pub static LABEL_IO_READ = ["io": "rocksdb_read"; 1];
    pub static LABEL_IO_READ_TIER1 = ["io": "rocksdb_read_l1"; 1];
    pub static LABEL_IO_READ_TIER2 = ["io": "rocksdb_read_l2"; 1];
    pub static LABEL_IO_WRITE = ["io": "rocksdb_write"; 1];
    pub static LABEL_IO_DELETE = ["io": "rocksdb_delete"; 1];
    pub static LABEL_IO_MERGE = ["io": "rocksdb_merge"; 1];
}

ovdc_trace::meter! {
    static;
    pub QUERY: Count { };
    pub ANSWER: Count { };
    pub GET_BYTES: Count { };
    pub MISS: Count { };

    pub PUT: Count { };

    pub METADATA_UPDATE: Count { };
    pub PUT_DELAY: Count { };
    pub PUT_BYTES: Count { };
    pub BYTES_RETURNED: Count { };
    pub RDB_PUT_PERF: Count { };
    pub RDB_GET_PERF: Count { };
    pub RDB_RETAINS_PERF: Count { };
    pub ERROR: Count { };
    pub BLOB_BEGIN: Count { };
    pub BLOB_ABANDON: Count { };
    pub BLOB_DENY: Count { };
    pub ROWCACHE_INSERT: Count { };
    pub IO_TIME: HistFloat {
        buckets: buckets_seconds()
    };
    pub PUT_BYTE_HIST: Hist {
        buckets: vec![500.0, 1000.0, 5000.0, 10_000.0, 15_000.0, 50_000.0, 250_000.0, 500_000.0, 1_000_000.0, 1_500_000.0, 2_500_000.0, 5_000_000.0, 10_000_000.0, 15_000_000.0, 30_000_000.0, 60_000_000.0, 120_000_000.0, 250_000_000.0]
    };
}
