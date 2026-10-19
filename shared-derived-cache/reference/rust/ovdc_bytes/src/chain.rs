// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Allows multiple buffers to be chained into a single [Buf].

use std::collections::VecDeque;

use bytes::{Buf, BufMut, Bytes};

/// An implementation of [Segmented].
#[derive(Default, Debug, Clone)]
pub struct Segments {
    buffers: VecDeque<bytes::Bytes>,
}

impl Segments {
    /// Copy the current value in to a single [Bytes]
    pub fn into_bytes(mut self) -> Bytes {
        if self.buffers.len() == 1 {
            return self.buffers.pop_back().unwrap();
        } else if self.buffers.is_empty() {
            Default::default()
        }

        self.copy_to_bytes(self.remaining())
    }

    /// Take the first buffer segment.
    pub fn pop(&mut self) -> Option<Bytes> {
        self.buffers.remove(0)
    }
}

impl From<Vec<bytes::Bytes>> for Segments {
    fn from(value: Vec<bytes::Bytes>) -> Self {
        Segments {
            buffers: value.into(),
        }
    }
}

impl From<&[bytes::Bytes]> for Segments {
    fn from(value: &[bytes::Bytes]) -> Self {
        Segments {
            buffers: value.to_vec().into(),
        }
    }
}

impl From<bytes::Bytes> for Segments {
    fn from(value: bytes::Bytes) -> Self {
        Segments {
            buffers: vec![value].into(),
        }
    }
}

impl Buf for Segments {
    fn remaining(&self) -> usize {
        self.buffers.iter().map(|y| y.len()).sum()
    }

    fn chunk(&self) -> &[u8] {
        self.buffers.front().map(|y| y.chunk()).unwrap_or(&[])
    }

    fn advance(&mut self, mut cnt: usize) {
        while cnt > 0 && !self.buffers.is_empty() {
            let mut buffer = self
                .buffers
                .pop_front()
                .expect("stream contains no buffers");

            let len = std::cmp::min(buffer.len(), cnt);
            cnt -= len;

            buffer.advance(len);

            if !buffer.is_empty() {
                self.buffers.push_front(buffer);
            }
        }
    }
}

/// Allows a multi segmented buffer to be consumed as a single [Buf].
///
/// Many arrays, vectors, or other byte containers can be used and represented as a single [Buf].
///
/// this is useful because network data may be framed and therefore a stream of data is not a
/// single continuous slice of bytes, but rather many.
pub trait Segmented: Buf {
    /// Adds a [Bytes] segment to the current [Segmented] implementation.
    fn push(&mut self, next: Bytes);

    /// Combines this segmented object with another.
    fn combine(&mut self, mut other: Self)
    where
        Self: Sized,
    {
        self.combine_by_ref(&mut other);
    }

    /// Combines this segmented object with another.
    fn combine_by_ref(&mut self, other: &mut Self);

    /// Splits the current segment until the given index is reached.
    fn split_to(&mut self, index: usize) -> Segments;

    /// Takes all of the data from self.
    fn take_all(&mut self) -> Segments;
}

impl Segmented for &mut Segments {
    #[inline]
    fn push(&mut self, next: Bytes) {
        (*self).push(next)
    }

    #[inline]
    fn combine_by_ref(&mut self, other: &mut Self) {
        (*self).combine_by_ref(other)
    }

    #[inline]
    fn split_to(&mut self, index: usize) -> Segments {
        (*self).split_to(index)
    }

    #[inline]
    fn take_all(&mut self) -> Segments {
        (*self).take_all()
    }
}

impl Segmented for Segments {
    fn push(&mut self, next: Bytes) {
        debug_assert!(!next.chunk().is_empty());
        self.buffers.push_back(next);
    }

    fn combine_by_ref(&mut self, other: &mut Self) {
        self.buffers.extend(other.buffers.drain(..))
    }

    fn split_to(&mut self, mut index: usize) -> Segments {
        let mut new_stream = Segments {
            buffers: Default::default(),
        };

        while index > 0 && !self.buffers.is_empty() {
            let mut buffer = self
                .buffers
                .pop_front()
                .expect("stream contains no buffers");

            if index >= buffer.len() {
                // The buffer is not smaller than the index.
                index -= buffer.len();
                new_stream.buffers.push_back(buffer);
            } else {
                // The buffer contains the index.
                let split = buffer.split_to(index);
                new_stream.buffers.push_back(split);

                self.buffers.push_front(buffer);

                break;
            }
        }

        new_stream
    }

    fn take_all(&mut self) -> Segments {
        Segments {
            buffers: self.buffers.drain(..).collect(),
        }
    }
}

impl Segmented for bytes::BytesMut {
    fn push(&mut self, next: Bytes) {
        self.put_slice(next.chunk());
    }

    fn combine(&mut self, next: Self) {
        self.put_slice(next.chunk());
    }

    fn combine_by_ref(&mut self, other: &mut Self) {
        self.put_slice(other.chunk());
    }

    fn split_to(&mut self, index: usize) -> Segments {
        let split = bytes::BytesMut::split_to(self, index);
        Segments {
            buffers: vec![split.freeze()].into(),
        }
    }

    fn take_all(&mut self) -> Segments {
        Segments {
            buffers: vec![self.split().into()].into(),
        }
    }
}

#[test]
fn test_segments() {
    let mut segments = Segments::default();
    segments.push(vec![1, 2, 3].into());
    segments.push(vec![4, 5, 6].into());

    let mut collected = vec![];
    while segments.remaining() > 0 {
        let len = segments.chunk().len();
        collected.extend_from_slice(segments.chunk());
        segments.advance(len);
    }

    assert_eq!(collected, &[1, 2, 3, 4, 5, 6]);

    segments.push(vec![1, 2, 3].into());
    segments.push(vec![4, 5, 6].into());

    let mut new = segments.split_to(4);
    assert_eq!(new.chunk(), &[1, 2, 3]);
    new.advance(3);
    assert_eq!(new.chunk(), &[4]);
}
