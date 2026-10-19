// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Helpers to make reading/writing human readable values with serde simpler.

mod f64;
#[allow(unused)]
mod size;
mod time;

pub use f64::HumanF64;
pub use size::HumanByteSize;
pub use time::{HumanDuration, Interval};

fn split(duration_str: &str) -> Option<(&str, &str)> {
    let duration_str = duration_str.trim().trim_matches('"');

    let mut moved_over = duration_str.chars();
    let mut right = moved_over.as_str();
    while let Some(value) = moved_over.next() {
        if !value.is_ascii_digit() && value != '-' {
            break;
        }
        right = moved_over.as_str();
    }
    if !right.is_empty() && !right.is_ascii() {
        return None;
    }
    let left = &duration_str[..duration_str.len() - right.len()];
    Some((left, right))
}

/// Try read input as bytes, time or i64.
pub fn parse_number(input: &str) -> Option<i64> {
    let (left, right) = split(input)?;
    if time::is_time(right) {
        return time::parse_split((left, right)).map(|y| y.as_millis() as i64);
    }
    if size::is_size(right) {
        size::parse_split((left, right)).map(|y| y as i64)
    } else if right.is_empty() {
        left.parse().ok()
    } else {
        None
    }
}
