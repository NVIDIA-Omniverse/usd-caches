// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use axum::{extract::State, routing::get, Json, Router};
use serde_json::{json, Value};
use tokio::time::Instant;

pub(crate) fn health_routes() -> Router {
    Router::new().route("/", get(health)).with_state(Instant::now())
}

async fn health(State(start_time): State<Instant>) -> Json<Value> {
    let uptime = Instant::now().duration_since(start_time).as_secs();
    Json(json!({ "health": "ok", "uptime": uptime }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
    };
    use tower::ServiceExt; // oneshot

    #[tokio::test]
    async fn test_health() {
        let app = health_routes();

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["health"].as_str().unwrap(), "ok");
    }
}
