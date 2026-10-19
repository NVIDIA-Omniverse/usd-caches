// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Result},
    routing::{delete, get},
    Json, Router,
};
use serde_json::json;
use tracing::error;

use crate::context::Context;

pub(crate) fn cache_routes() -> Router<Arc<Context>> {
    Router::new()
        .route("/clear", delete(cache_clear))
        .route("/dumpdb", get(cache_dumpdb))
        .route("/stats", get(cache_stats))
        .route("/stats/reset", delete(reset_cache_stats))
}

async fn cache_dumpdb(State(context): State<Arc<Context>>) -> Result<impl IntoResponse, StatusCode> {
    context.cache.dump_database().await.map_err(|e| {
        error!("Unable to dump cache database: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(StatusCode::OK)
}

async fn cache_clear(State(context): State<Arc<Context>>) -> Result<impl IntoResponse, StatusCode> {
    context.cache.clear().await.map_err(|e| {
        error!("Unable to clear cache: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(StatusCode::OK)
}

async fn cache_stats(State(context): State<Arc<Context>>) -> impl IntoResponse {
    let mut value = json!(context.cache.stats());

    // TODO: deprecate/remove - fix for launcher web ui not default missing values to zero
    value["current_size"] = json!(0u64);
    value["hit_rate"] = json!(0u64);
    value["number_of_files"] = json!(0u64);

    Json(value)
}

async fn reset_cache_stats(State(context): State<Arc<Context>>) -> Result<impl IntoResponse, StatusCode> {
    context.cache.clear_stats();
    Ok(StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::OpenOptions;
    use std::io::Write;

    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
    };
    use rstest::{fixture, rstest};
    use serde_json::Value;
    use tower::ServiceExt; // oneshot
    use uuid::Uuid;

    use crate::context::TestContext;

    use hub_cache::block::PutRequest;

    struct TestCase {
        client_uuid: Uuid,
        test_context: TestContext,
    }

    impl TestCase {
        pub fn new() -> Self {
            Self {
                client_uuid: Uuid::new_v4(),
                test_context: TestContext::new(),
            }
        }
    }

    #[fixture]
    fn test_case() -> TestCase {
        TestCase::new()
    }

    fn populate_cache(test_case: &TestCase, cache_entries: u64) {
        for n in 0..cache_entries {
            let key = format!("key_{n}");
            let extension: Arc<str> = Arc::from("txt");

            let request = PutRequest::new(key.as_bytes(), 10, 9, Some(extension));
            let file_path = test_case
                .test_context
                .context
                .cache
                .file_put(&request, &test_case.client_uuid)
                .unwrap();
            assert!(file_path.exists());

            let mut f = OpenOptions::new()
                .read(false)
                .write(true)
                .create(false)
                .open(&file_path)
                .unwrap();
            write!(f, "test").unwrap();

            test_case
                .test_context
                .context
                .cache
                .file_close_deprecated(&request.key)
                .unwrap();
            test_case.test_context.context.cache.flush().unwrap();
        }
    }

    #[rstest]
    #[case(0)]
    #[case(1)]
    #[case(2)]
    #[case(10)]
    #[case(100)]
    #[test_log::test(tokio::test)]
    async fn clear(test_case: TestCase, #[case] cache_entries: u64) {
        populate_cache(&test_case, cache_entries);

        let stats = test_case.test_context.context.cache.stats();
        assert_eq!(stats.cache_entries, cache_entries);

        let app = cache_routes().with_state(test_case.test_context.context.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method(axum::http::method::Method::DELETE)
                    .uri("/clear")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let stats = test_case.test_context.context.cache.stats();
        assert_eq!(stats.cache_entries, 0);
    }

    #[rstest]
    #[case(0)]
    #[case(1)]
    #[case(2)]
    #[case(10)]
    #[case(100)]
    #[test_log::test(tokio::test)]
    async fn stats(test_case: TestCase, #[case] cache_entries: u64) {
        populate_cache(&test_case, cache_entries);

        let stats = test_case.test_context.context.cache.stats();
        assert_eq!(stats.cache_entries, cache_entries);

        let app = cache_routes().with_state(test_case.test_context.context.clone());

        let response = app
            .oneshot(Request::builder().uri("/stats").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["cache_entries"].as_u64().unwrap(), cache_entries);
    }
}
