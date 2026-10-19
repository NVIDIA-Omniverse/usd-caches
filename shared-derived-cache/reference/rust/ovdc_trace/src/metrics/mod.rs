// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod exporter;
mod macros;
mod prometheus;

pub use macros::{
    timer, Count, CountFloat, Gauge, GaugeFloat, Hist, HistFloat, MargsBorrow, Measure,
    OnDropTimer, OtelValue, Props, TyOfCount, TyOfCountFloat, TyOfGauge, TyOfGaugeFloat, TyOfHist,
    TyOfHistFloat, TyOfUpDown, TyOfUpDownFloat, UpDown, UpDownFloat,
};

pub use exporter::MeterReader;
use opentelemetry::metrics::Meter;
pub use opentelemetry::KeyValue;
pub use opentelemetry::Value;
use opentelemetry_sdk::metrics::SdkMeterProvider;
pub use prometheus::collect_openmetrics;
pub use prometheus::M64;

use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

pub static METER_GENERATION: AtomicU64 = AtomicU64::new(0);

struct Global {
    provider: Option<SdkMeterProvider>,
    meter: Option<Meter>,
}

static GLOBAL: RwLock<Global> = RwLock::new(Global {
    provider: None,
    meter: None,
});

pub fn with_meter<O>(f: impl FnOnce(&Meter) -> O) -> Option<O> {
    if let Ok(guard) = GLOBAL.read() {
        if let Some(meter) = guard.meter.as_ref() {
            return Some(f(meter));
        }
    }

    use opentelemetry::metrics::MeterProvider;

    let mut guard = GLOBAL.write().unwrap();

    // Another thread may have already set up a meter provider.
    if let Some(meter) = guard.meter.as_ref() {
        return Some(f(meter));
    }

    let provider = SdkMeterProvider::default();
    guard.provider = Some(provider.clone());

    let meter = provider.meter("unset");

    let val = Some(f(&meter));
    guard.meter = Some(meter);
    val
}

pub fn set(meter: Meter, provider: SdkMeterProvider) {
    let mut guard = GLOBAL.write().unwrap();
    let last = mem::replace(
        &mut *guard,
        Global {
            provider: Some(provider),
            meter: Some(meter),
        },
    );
    let Some(meter) = last.meter else { return };
    drop(meter);
    let Some(provider) = last.provider else {
        return;
    };
    METER_GENERATION.fetch_add(1, Ordering::Relaxed);
    std::thread::spawn(move || {
        if let Err(e) = provider.shutdown() {
            println!("ovdc_trace: failed to shutdown metrics provider: {e}");
        }
    });
}

pub fn bump_meter_generation() {
    METER_GENERATION.fetch_add(1, Ordering::Relaxed);
}

pub fn buckets_millis() -> Vec<f64> {
    vec![
        0.001, 0.002, 0.003, 0.005, 0.009, 0.01, 0.02, 0.05, 0.09, 0.1, 0.3, 0.5, 0.9, 1.0, 1.5,
        2.0, 5.0, 7.0, 10.0,
    ]
}

pub fn buckets_seconds() -> Vec<f64> {
    vec![
        0.001, 0.002, 0.005, 0.010, 0.025, 0.050, 0.075, 0.100, 0.250, 0.500, 0.750, 1.0, 2.5, 5.0,
        7.5, 10.0,
    ]
}
