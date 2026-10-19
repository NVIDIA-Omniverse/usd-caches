// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Prost [`tonic::codec::Codec`] that reports serialized protobuf message sizes to a process-wide hook.
//!
//! The hook is installed from the binary crate with [`init_byte_counter`]. Until then, encoding
//! behaves like [`tonic_prost::ProstCodec`] with no extra overhead beyond length checks.

use std::{
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use bytes::Buf;
use prost::Message;
use tonic::codec::{BufferSettings, Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::Status;
use tonic_prost::{ProstDecoder, ProstEncoder};

/// Hook invoked with `(is_encode, byte_len)` for every serialized protobuf message.
pub type ByteHook = Arc<dyn Fn(bool, usize) + Send + Sync>;

static BYTE_HOOK: Mutex<Option<ByteHook>> = Mutex::new(None);

/// Install the callback used to accumulate per-span protobuf byte totals (`sent` / `recv` on the wire).
///
/// `sent == true` means an **outbound** message from this process (server: response); `false` is **inbound**
/// (server: request). `n` is the serialized protobuf length for that message.
///
/// The hook is process-wide and later calls replace it, so every installed hook must attribute
/// bytes by the current span rather than by which server installed it -- otherwise a process
/// hosting more than one server (notably the test suite) misattributes across them.
pub fn init_byte_counter(hook: ByteHook) {
    *BYTE_HOOK.lock().expect("grpc byte hook mutex poisoned") = Some(hook);
}

#[inline]
fn fire_hook(sent: bool, n: usize) {
    if n == 0 {
        return;
    }
    let guard = BYTE_HOOK.lock().expect("grpc byte hook mutex poisoned");
    if let Some(h) = guard.as_ref() {
        h(sent, n);
    }
}

/// [`Codec`] compatible with `tonic-prost-build` [`codec_path`](tonic_prost_build::Builder::codec_path).
#[derive(Debug, Clone, Default)]
pub struct MeasuringProstCodec<T, U> {
    _pd: PhantomData<(T, U)>,
}

impl<T, U> MeasuringProstCodec<T, U> {
    /// Creates a new codec (same as [`Default`]).
    pub fn new() -> Self {
        Self { _pd: PhantomData }
    }
}

impl<T, U> Codec for MeasuringProstCodec<T, U>
where
    T: Message + Send + 'static,
    U: Message + Default + Send + 'static,
{
    type Encode = T;
    type Decode = U;
    type Encoder = MeasuringEncoder<T>;
    type Decoder = MeasuringDecoder<U>;

    fn encoder(&mut self) -> Self::Encoder {
        MeasuringEncoder {
            inner: ProstEncoder::new(BufferSettings::default()),
        }
    }

    fn decoder(&mut self) -> Self::Decoder {
        MeasuringDecoder {
            inner: ProstDecoder::new(BufferSettings::default()),
        }
    }
}

/// Prost encoder that records [`Message::encoded_len`] before writing.
pub struct MeasuringEncoder<T> {
    inner: ProstEncoder<T>,
}

impl<T: Message> Encoder for MeasuringEncoder<T> {
    type Item = T;
    type Error = Status;

    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        fire_hook(true, item.encoded_len());
        self.inner.encode(item, dst)
    }

    fn buffer_settings(&self) -> BufferSettings {
        self.inner.buffer_settings()
    }
}

/// Prost decoder that records the gRPC message payload length before decoding.
pub struct MeasuringDecoder<U> {
    inner: ProstDecoder<U>,
}

impl<U: Message + Default> Decoder for MeasuringDecoder<U> {
    type Item = U;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        fire_hook(false, src.remaining());
        self.inner.decode(src)
    }

    fn buffer_settings(&self) -> BufferSettings {
        self.inner.buffer_settings()
    }
}
