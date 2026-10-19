// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

fn main() {
    // `--cfg loom` swaps every synchronization primitive for Loom's
    // model-checked version; see `src/sync.rs`.
    println!("cargo::rustc-check-cfg=cfg(loom)");
}
