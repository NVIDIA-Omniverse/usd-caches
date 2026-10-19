// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod api_error;
mod cache_routes;
mod health_routes;
mod service_routes;
mod shutdown_routes;

use std::sync::Arc;

use axum::Router;

use crate::Context;

pub(crate) fn api_routes() -> Router<Arc<Context>> {
    Router::new()
        .nest("/health", health_routes::health_routes().with_state(()))
        .nest("/1.0/shutdown", shutdown_routes::shutdown_routes())
        .nest("/1.0/cache", cache_routes::cache_routes())
        .nest("/1.0/service", service_routes::service_routes())
}
