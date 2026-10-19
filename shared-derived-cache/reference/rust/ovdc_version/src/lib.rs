// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Product version, baked in at compile time from `OVDC_VERSION` by
//! `build.rs`. Unset -- i.e. any build outside CI -- yields `dev`.
//! The embedded value stays fixed for the lifetime of the built binary.

include!(concat!(env!("OUT_DIR"), "/version.rs"));
