// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Approximate conversion for f64

use std::ops::Deref;

use serde::{Deserialize, Serialize};

/// Allows Approximation of an f64 with 1e-9 precision.
#[derive(Default, Clone, Copy)]
pub struct HumanF64(f64);

impl From<f64> for HumanF64 {
    fn from(value: f64) -> Self {
        Self((value / Self::PRECISION).round() * Self::PRECISION)
    }
}

impl HumanF64 {
    const PRECISION: f64 = 1e-9;
}

impl Deref for HumanF64 {
    type Target = f64;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<HumanF64> for f64 {
    fn from(value: HumanF64) -> Self {
        *value
    }
}

impl Serialize for HumanF64 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for HumanF64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        Ok(value.into())
    }
}

// This PartialEq implementation makes NaN == NaN return true, which violates
// IEEE 754 semantics. This may cause unexpected behavior in collections or comparisons.
impl PartialEq for HumanF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0 || (self.0.is_nan() && other.0.is_nan())
    }
}

impl Eq for HumanF64 {}

impl std::hash::Hash for HumanF64 {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Hash the rounded value as an integer representation
        // Convert to integer by scaling up and rounding
        // Handle special cases: NaN and infinity
        if self.0.is_nan() {
            state.write_u8(0);
            return;
        }
        if self.0.is_infinite() {
            state.write_u8(if self.0.is_sign_positive() { 1 } else { 2 });
            return;
        }
        // For normal values, scale to integer representation
        let scaled = (self.0 / HumanF64::PRECISION).round() as i64;
        scaled.hash(state);
    }
}
