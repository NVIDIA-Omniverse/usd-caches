// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use ovdc_trace::metrics::Measure;

fn grpc_server_buckets_bytes() -> Vec<f64> {
    vec![
        0.0,
        1024.0,
        2048.0,
        4096.0,
        16384.0,
        65536.0,
        262144.0,
        1048576.0,
        4194304.0,
        16777216.0,
        67108864.0,
        268435456.0,
        1073741824.0,
        4294967296.0,
    ]
}

ovdc_trace::meter! {
    static;
    pub API_KEYS_TOTAL: Count { };
    pub API_ERRORS_TOTAL: Count { };
    pub API_REQUEST_TOTAL: Count { };
    pub API_CONNECTIONS: UpDown { };
    pub API_UPTIME_SECS: Gauge { };
    pub API_REQUEST_TIME: HistFloat {
        buckets: ovdc_trace::metrics::buckets_seconds()
    };
    pub API_WRITE_QUEUE_TIME: HistFloat {
        buckets: ovdc_trace::metrics::buckets_seconds()
    };
    pub API_WRITES_WAITING: UpDown { };
    pub API_TCP_RX_TOTAL: Count { };
    pub API_TCP_TX_TOTAL: Count { };

    pub GRPC_SERVER_CALL_STARTED: Count {
        name: "grpc.server.call.started"
    };
    pub GRPC_SERVER_CALL_DURATION: HistFloat {
        name: "grpc.server.call.duration",
        unit: "s",
        buckets: ovdc_trace::metrics::buckets_seconds()
    };
    pub GRPC_SERVER_CALL_SENT_BYTES: HistFloat {
        name: "grpc.server.call.sent_total_compressed_message_size",
        unit: "By",
        buckets: grpc_server_buckets_bytes()
    };
    pub GRPC_SERVER_CALL_RCVD_BYTES: HistFloat {
        name: "grpc.server.call.rcvd_total_compressed_message_size",
        unit: "By",
        buckets: grpc_server_buckets_bytes()
    };
}

/// Guard that decrements WRITES_WAITING by 1 on drop.
pub struct WritesWaitingGuard;

/// Records WRITES_WAITING=1 and returns a guard that decrements by 1 on drop.
pub fn writes_waiting_guard() -> WritesWaitingGuard {
    API_WRITES_WAITING.measure(1, &[]);
    WritesWaitingGuard
}

impl Drop for WritesWaitingGuard {
    fn drop(&mut self) {
        API_WRITES_WAITING.measure(-1, &[]);
    }
}
