// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Converts a string with time units to a Duration.

use std::ops::Deref;
use std::time::Duration;

use serde::{de::Visitor, Deserialize, Deserializer};

/// A time interval stored as milliseconds.
#[repr(transparent)]
#[derive(serde::Deserialize, serde::Serialize, Debug, Clone, Copy)]
#[serde(transparent)]
pub struct Interval(i64);

impl From<Duration> for Interval {
    fn from(value: Duration) -> Self {
        Self(value.as_millis() as _)
    }
}
impl From<i64> for Interval {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl Interval {
    /// Returns the interval as a [`Duration`], or `None` if the value is negative.
    pub fn dur(self) -> Option<Duration> {
        if self.0 < 0 {
            return None;
        }
        Some(Duration::from_millis(self.0 as _))
    }
}

fn parse_duration(duration_str: &str) -> Option<Duration> {
    let value = super::split(duration_str)?;
    parse_split(value)
}

pub(super) fn is_time(units: &str) -> bool {
    matches!(units, "ms" | "s" | "m" | "h" | "d" | "ns" | "us" | "µs")
}

pub(super) fn parse_split(split: (&str, &str)) -> Option<Duration> {
    let (left, right) = split;

    if right.is_empty() && !left.is_empty() {
        // No unit specified, treat as milliseconds
        let x = left.parse::<u64>().ok()?;
        return Some(Duration::from_millis(x));
    }

    let value = left.parse::<u64>().ok()?;

    let duration = match right {
        "ns" => Duration::from_nanos(value),
        "us" | "µs" => Duration::from_micros(value),
        "ms" => Duration::from_millis(value),
        "s" => Duration::from_secs(value),
        "m" => Duration::from_secs(value * 60),
        "h" => Duration::from_secs(value * 3600),
        "d" => Duration::from_secs(value * 86400),
        _ => return None,
    };

    Some(duration)
}

/// A duration descriptor such as 100, 100s, 100ms, 100m, 100h, 100d, etc.
///
/// Deserializes from either a unit string or a bare number of milliseconds.
/// Serializes as a bare number of milliseconds, mirroring the bare-number form
/// the deserializer accepts (see [`DurationStrVisitor::visit_u64`]).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HumanDuration(pub Duration);

impl serde::Serialize for HumanDuration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.0.as_millis() as u64)
    }
}

impl From<Duration> for HumanDuration {
    fn from(value: Duration) -> Self {
        Self(value)
    }
}

impl Deref for HumanDuration {
    type Target = Duration;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

struct DurationStrVisitor;
impl Visitor<'_> for DurationStrVisitor {
    type Value = Duration;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str(
            "duration that is a number optionally followed by unit (valid units: ns, us, ms, s, m, h, d)",
        )
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if v < 0 {
            return Err(E::custom("duration cannot be negative"));
        }

        Ok(Duration::from_millis(v as _))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        // Treat bare numbers as milliseconds
        Ok(Duration::from_millis(v))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        parse_duration(v).ok_or_else(|| E::custom("invalid duration string"))
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Use deserialize_any to handle both strings and numbers
        // If deserialize_any is not available, this will fall back gracefully
        let value = deserializer.deserialize_any(DurationStrVisitor)?;
        Ok(HumanDuration(value))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_duration;
    use std::time::Duration;

    #[test]
    fn test_parse_duration_sub_ms() {
        assert_eq!(parse_duration("1ns"), Some(Duration::from_nanos(1)));
        assert_eq!(parse_duration("100ns"), Some(Duration::from_nanos(100)));
        assert_eq!(parse_duration("1us"), Some(Duration::from_micros(1)));
        assert_eq!(parse_duration("500us"), Some(Duration::from_micros(500)));
        // NOTE: `is_time` lists "µs" (time.rs), but `super::split` rejects any
        // non-ASCII suffix before it is reached, so "500µs" parses as None. That
        // branch is unreachable. Left as-is here: this change is deletion-only.
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("100s").unwrap(), Duration::from_secs(100));
        assert_eq!(parse_duration("100h").unwrap(), Duration::from_secs(360000));
        assert_eq!(parse_duration("100ms").unwrap(), Duration::from_millis(100));
        assert_eq!(parse_duration("100m").unwrap(), Duration::from_secs(6000));
        assert_eq!(
            parse_duration("100d").unwrap(),
            Duration::from_secs(8640000)
        );
        assert_eq!(parse_duration("100").unwrap(), Duration::from_millis(100));
        assert_eq!(parse_duration("100ns"), Some(Duration::from_nanos(100)));
    }
}
