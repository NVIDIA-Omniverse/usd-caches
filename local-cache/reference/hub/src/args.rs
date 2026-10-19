// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use clap::{Parser, ValueEnum};
use clap_num::maybe_hex;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, ValueEnum)]
pub enum Mode {
    /// launch hub exclusively, hub process lifetime tied to launching process (parent)
    #[default]
    Exclusive,
    /// launch hub shared, if hub isn't already running start it, if hub already running return
    Shared,
}

#[derive(Debug, Default, Parser, Serialize)]
#[command(author, version, about, long_about)]
pub struct Args {
    /// path to hub.toml to load exclusively
    #[arg(long)]
    pub hub_config: Option<PathBuf>,

    /// path omniverse.toml to load exclusively
    #[arg(long)]
    pub omniverse_config: Option<PathBuf>,

    /// dump config and exit
    #[serde(skip)]
    #[arg(long)]
    pub show_config: bool,

    /// unique identifier of this instanced of Hub
    ///
    /// DEPRECATED: This option should no longer be used because Hub Client manages process launching
    #[serde(skip)]
    #[arg(long, value_parser=maybe_hex::<u64>)]
    pub launch_id: Option<u64>,

    /// mode to launch Hub in @see Mode
    #[arg(long, value_enum, default_value_t = Mode::Exclusive)]
    pub mode: Mode,

    /// path to write the current Hub configuration (used by launching process to figure out how to bootstrap)
    #[serde(skip)]
    #[arg(long)]
    pub write_config: Option<PathBuf>,

    /// search for a Hub instance and return info if prompted, but do not start a new Hub instance if Hub is not running
    #[serde(skip)]
    #[arg(long, env = "HUB__ARGS__DETECT_ONLY")]
    pub detect_only: bool,

    /// attempt to connect to the running Hub instance and shutdown instance
    /// returns success (0) if Hub is not running or a successful shutdown API was made, failure otherwise
    #[serde(skip)]
    #[arg(long)]
    pub shutdown: bool,

    /// override any settings loaded from TOML or environment via key=value pairs
    #[serde(skip)]
    #[arg(last = true)]
    pub overrides: Vec<String>,
}

impl Args {
    pub fn new<I, T>(args: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        Self::parse_from(args)
    }
}
