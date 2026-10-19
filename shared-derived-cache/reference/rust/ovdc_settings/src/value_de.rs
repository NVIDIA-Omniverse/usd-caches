// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Value deserializer implementation for internal use.

use serde::de::{
    EnumAccess, Error as SerdeError, IntoDeserializer, MapAccess, SeqAccess, VariantAccess, Visitor,
};
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;

/// Errors that can occur when deserializing a type.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Error {
    message: String,
    /// Path to the failing value, innermost key first. Built up as the error
    /// propagates back out through the enclosing tables.
    keys: Vec<String>,
}

impl Error {
    /// Records `key` as the next step out on the path to the failing value.
    fn with_key(mut self, key: &str) -> Self {
        self.keys.push(key.to_owned());
        self
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (depth, key) in self.keys.iter().rev().enumerate() {
            match depth {
                0 => f.write_str(key)?,
                _ => write!(f, ".{key}")?,
            }
        }
        if !self.keys.is_empty() {
            f.write_str(": ")?;
        }
        self.message.fmt(f)
    }
}

impl serde::de::StdError for Error {}
impl SerdeError for Error {
    fn custom<T>(msg: T) -> Self
    where
        T: std::fmt::Display,
    {
        Self {
            message: format!("{msg}"),
            keys: Vec::new(),
        }
    }
}

/// A parsed TOML value, used as the untyped intermediate that typed config
/// sections are deserialized out of.
#[derive(Default, Debug)]
pub enum ConfigValue {
    /// No value. This is the default so that a settings container can be
    /// constructed before a config file has been read.
    #[default]
    None,

    /// A TOML integer.
    Int(i64),
    /// A TOML float.
    Float(f64),
    /// A TOML boolean.
    Bool(bool),
    /// A TOML string, kept exactly as written. Human sizes and durations are
    /// resolved on read by [`ConfigValue::as_i64`], not at parse time.
    String(String),

    /// A TOML table.
    Map(HashMap<String, ConfigValue>),
    /// A TOML array.
    List(Vec<ConfigValue>),
}

impl ConfigValue {
    /// Reads this value as an integer.
    ///
    /// Accepts an integer, a float with no fractional part that fits in an
    /// `i64`, and a string holding a human size or duration (`"32G"`, `"10s"`).
    /// Returns `None` for anything else, which surfaces as a deserialization
    /// error rather than a silently wrong number.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            // Range-guard the cast: `as` saturates, so an out-of-range float
            // would silently become i64::MAX/MIN instead of failing. Returning
            // None here routes to deserialize_any, which reports an error.
            //
            // The upper bound is strict because `i64::MAX as f64` rounds up to
            // 2^63; `<=` would admit exactly 2^63 and saturate it back down to
            // i64::MAX. `i64::MIN as f64` is -2^63 exactly, so `>=` is correct.
            Self::Float(x) if x.fract() == 0.0 && *x >= i64::MIN as f64 && *x < i64::MAX as f64 => {
                Some(*x as i64)
            }
            Self::Int(x) => Some(*x),
            // Numeric fields still accept human sizes and durations ("32G",
            // "10s"); the coercion is applied here rather than at parse time so
            // it cannot affect String-typed fields.
            Self::String(x) => ovdc_bytes::human_serde::parse_number(x),
            _ => None,
        }
    }
    /// Reads this value as a string, or `None` if it is not a
    /// [`ConfigValue::String`].
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(x) => Some(x),
            _ => None,
        }
    }

    /// Reads this value as a table, or `None` if it is not a
    /// [`ConfigValue::Map`].
    pub fn as_table(&self) -> Option<&HashMap<String, Self>> {
        match self {
            Self::Map(x) => Some(x),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for ConfigValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ConfigValueVisitor;

        impl<'de> Visitor<'de> for ConfigValueVisitor {
            type Value = ConfigValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("any valid TOML value")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(ConfigValue::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(ConfigValue::Int(value))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                i64::try_from(value)
                    .map(ConfigValue::Int)
                    .map_err(|_| E::custom(format!("integer {value} does not fit in i64")))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(ConfigValue::Float(value))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                // Strings are kept verbatim. Human-number coercion happens on
                // the way *out*, in `as_i64`, so it applies only when the target
                // field is actually numeric -- a String-typed field round-trips
                // exactly. Consumers with a stricter requirement convert at
                // their own boundary (see ovdc_map's rocks_adapter OptionValue).
                Ok(ConfigValue::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_str(&value)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut vec = Vec::new();
                while let Some(elem) = seq.next_element()? {
                    vec.push(elem);
                }
                Ok(ConfigValue::List(vec))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut hash_map = HashMap::new();
                while let Some((key, value)) = map.next_entry::<String, ConfigValue>()? {
                    hash_map.insert(key, value);
                }
                Ok(ConfigValue::Map(hash_map))
            }
        }

        deserializer.deserialize_any(ConfigValueVisitor)
    }
}

/// Emits a `deserialize_<int>` method per named integer type. Each tries the
/// given accessor on the inner value, converts with `TryFrom`, and falls back
/// to `deserialize_any` when either step fails.
macro_rules! forward_int_via {
    ($fn:ident = $($k:ident),*) => {
        pastey::paste!{
            $(
                fn [< deserialize_ $k >]<V>(self, visitor: V) -> Result<V::Value, Self::Error>
                where V: Visitor<'de> {
                    if let Some(x) = self.inner.$fn() {
                        #[allow(irrefutable_let_patterns)]
                        if let Ok(x) = $k::try_from(x) {
                            return visitor.[< visit_ $k >](x);
                        }
                    }
                    self.deserialize_any(visitor)
                }
            )*

        }
    };
}

struct ConfigValueDeserializer<'a> {
    inner: &'a ConfigValue,
}

impl<'de> Deserializer<'de> for ConfigValueDeserializer<'de> {
    type Error = Error;

    serde::forward_to_deserialize_any! {
        bool f32 f64 char str seq
        bytes byte_buf map unit
        ignored_any unit_struct tuple_struct tuple identifier
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let new_string = match self.inner {
            ConfigValue::Int(x) => x.to_string(),
            ConfigValue::Float(x) => x.to_string(),
            ConfigValue::Bool(true) => return visitor.visit_str("true"),
            ConfigValue::Bool(false) => return visitor.visit_str("false"),
            ConfigValue::String(x) => x.to_string(),
            _ => return self.deserialize_any(visitor),
        };
        visitor.visit_string(new_string)
    }

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        match self.inner {
            ConfigValue::None => visitor.visit_unit(),
            ConfigValue::Int(i) => visitor.visit_i64(*i),
            ConfigValue::Float(f) => visitor.visit_f64(*f),
            ConfigValue::Bool(b) => visitor.visit_bool(*b),
            ConfigValue::String(s) => visitor.visit_str(s),
            ConfigValue::Map(hash_map) => visitor.visit_map(MapDeserializer::new(hash_map)),
            ConfigValue::List(value2s) => visitor.visit_seq(SeqDeserializer::new(value2s)),
        }
    }

    forward_int_via!(as_i64 = u64, u32, u16, u8, i64, i32, i16, i8);

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        match self.inner {
            ConfigValue::Map(hash_map) => visitor.visit_map(MapDeserializer::new(hash_map)),
            _ => Err(serde::de::Error::custom("expected a map for struct")),
        }
    }

    fn deserialize_enum<V>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        match self.inner {
            ConfigValue::String(s) => visitor.visit_enum(s.as_str().into_deserializer()),
            ConfigValue::Map(hash_map) => {
                if hash_map.len() != 1 {
                    return Err(serde::de::Error::custom(
                        "expected a map with single key for enum",
                    ));
                }
                visitor.visit_enum(EnumDeserializer::new(hash_map))
            }
            _ => Err(serde::de::Error::custom(
                "expected a string or map for enum",
            )),
        }
    }
}

struct MapDeserializer<'a> {
    iter: std::vec::IntoIter<(&'a String, &'a ConfigValue)>,
    value: Option<(&'a String, &'a ConfigValue)>,
}

impl<'a> MapDeserializer<'a> {
    fn new(map: &'a HashMap<String, ConfigValue>) -> Self {
        Self {
            iter: map.iter().collect::<Vec<_>>().into_iter(),
            value: None,
        }
    }
}

impl<'de> MapAccess<'de> for MapDeserializer<'de> {
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: serde::de::DeserializeSeed<'de>,
    {
        if let Some((key, value)) = self.iter.next() {
            self.value = Some((key, value));
            seed.deserialize(key.as_str().into_deserializer()).map(Some)
        } else {
            Ok(None)
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::DeserializeSeed<'de>,
    {
        let (key, value) = self
            .value
            .take()
            .expect("called next_value before next_key");

        // Tag failures with the key they came from, so a nested config error
        // reports the path to the offending value rather than just the type
        // mismatch at the bottom of it.
        seed.deserialize(ConfigValueDeserializer { inner: value })
            .map_err(|error| error.with_key(key))
    }
}

struct SeqDeserializer<'a> {
    iter: std::slice::Iter<'a, ConfigValue>,
}

impl<'a> SeqDeserializer<'a> {
    fn new(vec: &'a [ConfigValue]) -> Self {
        Self { iter: vec.iter() }
    }
}

impl<'de> SeqAccess<'de> for SeqDeserializer<'de> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: serde::de::DeserializeSeed<'de>,
    {
        if let Some(value) = self.iter.next() {
            seed.deserialize(ConfigValueDeserializer { inner: value })
                .map(Some)
        } else {
            Ok(None)
        }
    }
}

struct EnumDeserializer<'a> {
    variant: &'a String,
    value: Option<&'a ConfigValue>,
}

impl<'a> EnumDeserializer<'a> {
    fn new(map: &'a HashMap<String, ConfigValue>) -> Self {
        let (variant, value) = map.iter().next().expect("map should have one entry");
        Self {
            variant,
            value: Some(value),
        }
    }
}

impl<'de> EnumAccess<'de> for EnumDeserializer<'de> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self::Variant), Self::Error>
    where
        V: serde::de::DeserializeSeed<'de>,
    {
        let variant = seed.deserialize(self.variant.as_str().into_deserializer())?;
        Ok((variant, self))
    }
}

impl<'de> VariantAccess<'de> for EnumDeserializer<'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, seed: T) -> Result<T::Value, Self::Error>
    where
        T: serde::de::DeserializeSeed<'de>,
    {
        let value = self.value.expect("variant should have a value");
        seed.deserialize(ConfigValueDeserializer { inner: value })
    }

    fn tuple_variant<V>(self, _len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let value = self.value.expect("variant should have a value");
        match value {
            ConfigValue::List(vec) => visitor.visit_seq(SeqDeserializer::new(vec)),
            _ => Err(serde::de::Error::custom(
                "expected a list for tuple variant",
            )),
        }
    }

    fn struct_variant<V>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let value = self.value.expect("variant should have a value");
        match value {
            ConfigValue::Map(map) => visitor.visit_map(MapDeserializer::new(map)),
            _ => Err(serde::de::Error::custom(
                "expected a map for struct variant",
            )),
        }
    }
}

/// Deserializes `T` out of a parsed [`ConfigValue`].
pub fn der<'de, T: Deserialize<'de>>(input: &'de ConfigValue) -> Result<T, Error> {
    T::deserialize(ConfigValueDeserializer { inner: input })
}
