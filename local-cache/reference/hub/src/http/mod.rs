// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod api;

use crate::Context;
use axum::{routing::get_service, Router};
use std::{path::PathBuf, sync::Arc};
use tower_http::services::ServeDir;
use tracing::info;

pub(crate) fn new_app(context: Arc<Context>) -> Router {
    let web_dir = web_dir();
    info!(?web_dir, "serving static files");

    Router::new()
        .nest("/api", api::api_routes())
        .nest_service("/", get_service(ServeDir::new(web_dir.as_path())))
        .with_state(context.clone())
}

fn web_dir() -> PathBuf {
    match std::env::current_exe() {
        Ok(p) => match p.parent() {
            Some(parent) => {
                let webdir = parent.join("web");
                if webdir.is_dir() {
                    webdir
                } else {
                    PathBuf::from("web")
                }
            },
            None => PathBuf::from("web"),
        },
        Err(_) => PathBuf::from("web"),
    }
}
