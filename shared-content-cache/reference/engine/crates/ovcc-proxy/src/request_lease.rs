// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;

pub(super) enum RequestLease {
    File(Lease),
    Path(ovcc_storage::PathCandidate),
}

impl RequestLease {
    pub(super) fn meta(&self) -> &StoredMeta {
        match self {
            Self::File(lease) => &lease.meta,
            Self::Path(path) => &path.meta,
        }
    }

    pub(super) fn into_body(self) -> BodySource {
        match self {
            Self::File(lease) => BodySource::Sendfile(lease),
            Self::Path(path) => BodySource::LocalPath(path),
        }
    }
}

impl From<Lease> for RequestLease {
    fn from(value: Lease) -> Self {
        Self::File(value)
    }
}

#[expect(
    clippy::large_enum_variant,
    reason = "keeps ordinary HTTP hits allocation-free"
)]
pub(super) enum Lookup {
    Hit(RequestLease),
    SoftMiss(()),
    Miss,
}

impl From<ovcc_storage::Lookup> for Lookup {
    fn from(value: ovcc_storage::Lookup) -> Self {
        match value {
            ovcc_storage::Lookup::Hit(lease) => Self::Hit(lease.into()),
            ovcc_storage::Lookup::SoftMiss(_) => Self::SoftMiss(()),
            ovcc_storage::Lookup::Miss => Self::Miss,
        }
    }
}

impl From<ovcc_storage::LocalPathLookup> for Lookup {
    fn from(value: ovcc_storage::LocalPathLookup) -> Self {
        match value {
            ovcc_storage::LocalPathLookup::Hit(path) => Self::Hit(RequestLease::Path(path)),
            ovcc_storage::LocalPathLookup::SoftMiss(_) => Self::SoftMiss(()),
            ovcc_storage::LocalPathLookup::Miss => Self::Miss,
        }
    }
}
