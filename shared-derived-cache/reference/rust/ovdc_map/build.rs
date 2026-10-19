// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let settings_path = out_dir.join("default_settings");
    let build_env = out_dir.join("_build_rdb_env/");

    {
        let mut opt = rocksdb::Options::default();
        opt.create_if_missing(true);
        rocksdb::DBWithThreadMode::<rocksdb::MultiThreaded>::open(&opt, &build_env).unwrap();
    }

    let mut count = -1i32;
    let mut best = PathBuf::default();
    for d in std::fs::read_dir(&build_env).unwrap() {
        let Ok(d) = d else {
            continue;
        };
        let path = d.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name() else {
            continue;
        };

        let Some(count_str) = file_name.to_str().unwrap().strip_prefix("OPTIONS-") else {
            continue;
        };
        let i_count_str = count_str.parse::<i32>().unwrap();
        if count < i_count_str {
            count = i_count_str;
            best = path;
        }
    }

    std::fs::rename(&best, &settings_path).unwrap();

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/environment/rocks_adapter/config.rs");
}
