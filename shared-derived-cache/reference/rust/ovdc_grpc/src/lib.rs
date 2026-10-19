// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use ovdc_bytes::keys;
use proto::{Key, S32Key};

use prost::bytes::{Buf, BufMut};

pub mod measuring_codec;

pub mod proto {
    tonic::include_proto!("ovdc");
}

impl From<keys::Key> for Key {
    fn from(value: keys::Key) -> Self {
        S32Key::from(value).into()
    }
}

impl From<S32Key> for Key {
    fn from(key: S32Key) -> Self {
        crate::proto::Key {
            inner: Some(crate::proto::key::Inner::FixedKey(key)),
        }
    }
}

impl From<[u8; 32]> for S32Key {
    fn from(buf: [u8; 32]) -> Self {
        let mut slice = &buf[..];
        S32Key {
            u0: slice.get_u64(),
            u1: slice.get_u64(),
            u2: slice.get_u64(),
            u3: slice.get_u64(),
        }
    }
}

impl From<&S32Key> for [u8; 32] {
    fn from(value: &S32Key) -> Self {
        let mut buf = [0u8; 32];

        {
            let mut slice = &mut buf[..];
            slice.put_slice(&value.u0.to_be_bytes());
            slice.put_slice(&value.u1.to_be_bytes());
            slice.put_slice(&value.u2.to_be_bytes());
            slice.put_slice(&value.u3.to_be_bytes());
        }

        buf
    }
}

impl From<keys::Key> for S32Key {
    fn from(value: keys::Key) -> Self {
        value.into_inner().into()
    }
}

mod helpers {
    use bytes::{Buf, Bytes};
    use ovdc_bytes::keys;
    use tokio_stream::Stream;
    use tonic::Request;

    use crate::proto::{
        put_request, GetRequest, PutDeclare, PutOpen, PutRequest, S32Key, TouchRequest,
    };

    struct SimplePutIterator {
        split: usize,
        bytes: Bytes,

        open: Option<PutRequest>,
        key: Option<keys::Key>,
    }

    impl Iterator for SimplePutIterator {
        type Item = PutRequest;

        fn next(&mut self) -> Option<Self::Item> {
            if let Some(open) = self.open.take() {
                return Some(open);
            }

            if !self.bytes.has_remaining() {
                return None;
            }

            if let Some(key) = self.key.take() {
                return Some(PutRequest {
                    kind: Some(put_request::Kind::Declare(PutDeclare {
                        key: Some(S32Key::from(key).into()),
                        size: self.bytes.remaining() as u64,
                        value: self
                            .bytes
                            .split_to(std::cmp::min(self.split, self.bytes.len()))
                            .to_vec(),
                    })),
                });
            }

            Some(PutRequest {
                kind: Some(put_request::Kind::Value(
                    self.bytes
                        .split_to(std::cmp::min(self.split, self.bytes.len()))
                        .to_vec(),
                )),
            })
        }
    }

    #[inline]
    pub fn put_request(key: keys::Key, value: Vec<u8>) -> impl Stream<Item = PutRequest> {
        tokio_stream::iter(SimplePutIterator {
            split: 1_000_000,
            bytes: value.into(),

            open: Some(PutRequest {
                kind: Some(put_request::Kind::Begin(PutOpen {
                    ttl: None,
                    durability: None,
                })),
            }),
            key: Some(key),
        })
    }

    #[inline]
    pub fn put_request_split_by(
        key: keys::Key,
        value: Vec<u8>,
        split: usize,
    ) -> impl Stream<Item = PutRequest> {
        tokio_stream::iter(SimplePutIterator {
            split,
            bytes: value.into(),

            open: Some(PutRequest {
                kind: Some(put_request::Kind::Begin(PutOpen {
                    ttl: None,
                    durability: None,
                })),
            }),
            key: Some(key),
        })
    }

    #[inline]
    pub fn touch_request(keys: impl IntoIterator<Item = keys::Key>) -> Request<TouchRequest> {
        Request::new(TouchRequest {
            keys: keys.into_iter().map(|y| S32Key::from(y).into()).collect(),
        })
    }

    #[inline]
    pub fn get_request(keys: impl IntoIterator<Item = keys::Key>) -> Request<GetRequest> {
        Request::new(GetRequest {
            keys: keys.into_iter().map(|y| S32Key::from(y).into()).collect(),
        })
    }

    #[inline]
    pub fn set_tenant<T>(tenant: &str, mut req: Request<T>) -> Request<T> {
        req.metadata_mut().append("tenant", tenant.parse().unwrap());
        req
    }

    #[inline]
    pub fn set_jwt<T>(jwt: &str, mut req: Request<T>) -> Request<T> {
        req.metadata_mut()
            .append("authorization", format!("Bearer {jwt}").parse().unwrap());
        req
    }
}

pub use helpers::{
    get_request, put_request, put_request_split_by, set_jwt, set_tenant, touch_request,
};
