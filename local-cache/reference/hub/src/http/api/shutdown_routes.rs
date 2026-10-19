// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Router};

use crate::context::Context;

pub(crate) fn shutdown_routes() -> Router<Arc<Context>> {
    Router::new().route("/", get(shutdown))
}

async fn shutdown(State(context): State<Arc<Context>>) -> impl IntoResponse {
    context.shutdown_notify.notify_one();
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::context::TestContext;

    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tokio::time::timeout;
    use tower::ServiceExt; // oneshot

    #[tokio::test]
    async fn test_shutdown() {
        let test_context = TestContext::new();
        let app = shutdown_routes().with_state(test_context.context.clone());

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        timeout(
            Duration::from_millis(100),
            test_context.context.shutdown_notify.notified(),
        )
        .await
        .unwrap();
    }
}
