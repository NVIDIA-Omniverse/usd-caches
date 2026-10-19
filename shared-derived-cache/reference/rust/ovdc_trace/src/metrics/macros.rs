// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{
    borrow::Cow,
    time::{Duration, SystemTime},
};

use opentelemetry::{
    metrics::{Counter, Gauge as OtelGauge, Histogram, UpDownCounter},
    KeyValue,
};

pub type OtelValue = opentelemetry::Value;
pub type MargsBorrow<'a> = &'a [KeyValue];

pub trait Measure {
    type Input;
    fn measure(&self, value: Self::Input, attributes: MargsBorrow);
}

#[derive(Default)]
pub struct Props {
    pub name: Cow<'static, str>,
    pub desc: &'static str,
    pub unit: &'static str,
    pub buckets: Vec<f64>,
}

#[macro_export]
macro_rules! label {
    (
        $(
        $v:vis static $static:ident =
        [$($name:literal: $value:literal),*; $count: literal];
        )*
    ) => {
        $(
        $v static $static: std::sync::LazyLock<[$crate::metrics::KeyValue; $count]> = std::sync::LazyLock::new(|| {
            [$(
                {
                    let value = $crate::metrics::OtelValue::from($value);
                    $crate::metrics::KeyValue::new::<&'static str, _>($name, value)
                }
            ),*]
        });
        )*
    };
    (
        $($name:literal: $value:expr),*
    ) =>{
        [$(
            {
                let value = $crate::metrics::OtelValue::from($value);
                $crate::metrics::KeyValue::new::<&'static str, _>($name, value)
            }
        ),*]
    };
}

macro_rules! builder_kind {
    (@Histogram, $name:ident, $props:ident) => {
        $name.description = ($props.desc != "").then_some($props.desc.into());
        $name.unit = ($props.unit != "").then_some($props.unit.into());
        $name.boundaries = (!$props.buckets.is_empty()).then_some($props.buckets.clone());
    };
    (@$_x:ident, $name:ident, $props:ident) => {
        $name.description = ($props.desc != "").then_some($props.desc.into());
        $name.unit = ($props.unit != "").then_some($props.unit.into());
    };
}

macro_rules! kinds {
    {
        $(
            $name:ident: $metric_struct:ident::<$value:ident>::$method:ident() as $method_name:ident;
        )+
    } =>{
        pastey::paste!{
        $(
        #[allow(unused)]
        pub struct $name {
            props: $crate::metrics::Props,
            inner: std::sync::RwLock<(u64, $metric_struct<$value>)>,
        }
        #[allow(unused)]
        pub type [< TyOf $name >] = $value;

        impl $name {
            fn build_instrument(props: &$crate::metrics::Props) -> $metric_struct<$value> {
                super::with_meter(move |y| {
                    let mut builder = y.$method_name(props.name.clone());
                    builder_kind!(@$metric_struct, builder, props);
                    builder.build()
                }).expect("no meter/metric provider setup")
            }
        }

        impl From<$crate::metrics::Props> for $name {
            fn from(props: $crate::metrics::Props) -> Self {
                let gen = super::METER_GENERATION.load(std::sync::atomic::Ordering::Relaxed);
                let instrument = Self::build_instrument(&props);
                $name {
                    inner: std::sync::RwLock::new((gen, instrument)),
                    props,
                }
            }
        }

        impl $crate::metrics::Measure for $name {
            type Input = $value;
            fn measure(&self, value: $value, attributes: MargsBorrow) {
                let gen = super::METER_GENERATION.load(std::sync::atomic::Ordering::Relaxed);
                {
                    let guard = self.inner.read().unwrap();
                    if guard.0 == gen {
                        guard.1.$method(value, attributes);
                        return;
                    }
                }
                let mut guard = self.inner.write().unwrap();
                if guard.0 != gen {
                    guard.1 = Self::build_instrument(&self.props);
                    guard.0 = gen;
                }
                guard.1.$method(value, attributes);
            }
        }
        )*
        }
    };
}

#[macro_export]
macro_rules! meter {
    {
        static;
        $(
        $(#[$attr:meta])*
        $v:vis $name:ident: $mkind:ident {
            $($p_name:ident: $p_value:expr),*
        };
        )*
    } => {
        $crate::___paste!{
        $(

        $(#[$attr])*
        $v static $name: std::sync::LazyLock<$crate::metrics::$mkind> = std::sync::LazyLock::new(|| {
            $crate::meter! {
                $name: $mkind {
                    $($p_name: $p_value),*
                };
            }
        });

        )*
        }
    };
    {
        $name:ident: $mkind:ident {
            $($p_name:ident: $p_value:expr),*
        };
    } => {
        $crate::___paste!{
        {
            let mut props = $crate::metrics::Props {
                $(
                $p_name: $p_value.into(),
                )*
                ..Default::default()
            };

            if props.name == "" {
                props.name = stringify!([< $name:lower >]).replace("_", ".").into();
            }
            $crate::metrics::$mkind::from(props)
        }
        }
    };
}

pub struct OnDropTimer<'a, const N: usize> {
    pub inner: &'a HistFloat,
    pub on_drop: Option<SystemTime>,
    pub args: [KeyValue; N],
}

pub fn timer<'a, const N: usize>(inner: &'a HistFloat, args: [KeyValue; N]) -> OnDropTimer<'a, N> {
    OnDropTimer {
        inner,
        args,
        on_drop: Some(SystemTime::now()),
    }
}

impl<'a, const N: usize> OnDropTimer<'a, N> {
    pub fn stop(self) -> Duration {
        self.on_drop
            .unwrap()
            .elapsed()
            .expect("Time went backwards")
    }
}

impl<'a, const N: usize> Drop for OnDropTimer<'a, N> {
    fn drop(&mut self) {
        let Some(value) = self.on_drop.take() else {
            return;
        };
        let value = value.elapsed().expect("Time went backwards").as_micros() as f64;
        self.inner.measure(value / 1_000_000.0, &self.args);
    }
}

kinds! {
    Count: Counter::<u64>::add() as u64_counter;
    CountFloat: Counter::<f64>::add() as f64_counter;
    UpDown: UpDownCounter::<i64>::add() as i64_up_down_counter;
    UpDownFloat: UpDownCounter::<f64>::add() as f64_up_down_counter;
    Hist: Histogram::<u64>::record() as u64_histogram;
    HistFloat: Histogram::<f64>::record() as f64_histogram;
    Gauge: OtelGauge::<u64>::record() as u64_gauge;
    GaugeFloat: OtelGauge::<f64>::record() as f64_gauge;
}
