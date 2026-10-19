// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use bytes::{Bytes, BytesMut};
use std::io;
use std::marker::PhantomData;
use std::pin::Pin;
use tokio_serde::{Deserializer, Serializer};

#[derive(Default)]
pub struct ProtoCodec<Item, SinkItem> {
    ghost: PhantomData<(Item, SinkItem)>,
}

pub type SymmetricalProtoCodec<T> = ProtoCodec<T, T>;

impl<Item, SinkItem> Deserializer<Item> for ProtoCodec<Item, SinkItem>
where
    for<'a> Item: Deserialize<'a>,
    Item: std::default::Default,
    Item: prost::Message,
{
    type Error = prost::DecodeError;

    fn deserialize(self: Pin<&mut Self>, src: &BytesMut) -> Result<Item, Self::Error> {
        Item::decode(std::io::Cursor::new(&src))
    }
}

impl<Item, SinkItem> Serializer<SinkItem> for ProtoCodec<Item, SinkItem>
where
    SinkItem: Serialize,
    SinkItem: prost::Message,
{
    type Error = io::Error;

    fn serialize(self: Pin<&mut Self>, item: &SinkItem) -> Result<Bytes, Self::Error> {
        let mut buf = BytesMut::new();
        buf.reserve(item.encoded_len());
        item.encode(&mut buf).unwrap();
        Ok(buf.freeze())
    }
}
