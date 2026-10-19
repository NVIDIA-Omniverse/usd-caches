// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::api_error::ApiError;

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::get,
    routing::put,
    Json, Router,
};
use config::{Config, File, FileFormat};
use serde_json::{json, Value};
use std::{env, sync::Arc};
use tokio::fs;
use tokio_util::io::ReaderStream;
use tracing::error;

use crate::support_bundle::SupportBundle;
use crate::Context;
use crate::Result;
use hub_util::settings::{Hub, SerializeToToml, Settings};
use hub_util::settings::{CONFIG_DIR, OMNIVERSE_TOML};

pub(crate) fn service_routes() -> Router<Arc<Context>> {
    Router::new()
        .route("/args", get(args))
        .route("/config", get(config))
        .route("/config", put(update_config))
        .route("/reset", get(reset))
        .route("/status", get(status))
        .route("/supportbundle", get(support_bundle))
}

/// get the hub startup arguments in json form
async fn args(State(context): State<Arc<Context>>) -> Json<Value> {
    json!(&context.args).into()
}

/// get the hub config / settings in json form
async fn config(State(context): State<Arc<Context>>) -> Json<Value> {
    json!(&context.settings.read().hub).into()
}

/// update hub settings (write hub.toml), soft restart service to reload settings
async fn update_config(
    State(context): State<Arc<Context>>,
    Json(payload): Json<serde_json::Value>,
) -> Result<(), ApiError> {
    // Use the provided hub.toml if available, otherwise fall back to the default
    let hub_toml = match &context.args.hub_config {
        Some(hub) => hub.clone(),
        None => Settings::default_hub_config(),
    };

    let mut builder = Config::builder();

    // Build the settings from the current hub.toml config, and then merge in the new settings provided.
    builder = builder.add_source(File::from(hub_toml.clone()).required(false));
    builder = builder.add_source(File::from_str(payload.to_string().as_str(), FileFormat::Json));

    let settings: Hub = match builder.build() {
        Ok(config) => match config.try_deserialize() {
            Ok(s) => s,
            Err(e) => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("Failed to deserialize settings: {e}"),
                ))
            },
        },
        Err(e) => {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build settings: {e}"),
            ))
        },
    };

    let contents = settings.serialize_to_toml(false);
    if let Err(e) = fs::write(hub_toml, contents.join("\n")).await {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to write toml file: {e}"),
        ));
    }

    // Reset Hub so the config changes take effect
    context.reset_notify.notify_one();

    Ok(())
}

/// reset hub settings to default, soft restart service to reload settings
async fn reset(State(context): State<Arc<Context>>) -> Result<(), ApiError> {
    // Use the provided hub.toml if available, otherwise fall back to the default
    let hub_toml = match &context.args.hub_config {
        Some(hub) => hub.clone(),
        None => Settings::default_hub_config(),
    };

    // attempt to remove file, file not found, we are already using defaults
    // besides any command line args, so just ignore and reset
    match hub_util::fs::remove_file(&hub_toml) {
        Ok(_) => {
            // reset hub, reloading config
            context.reset_notify.notify_one();
            Ok(())
        },
        Err(e) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to remove '{hub_toml:?}': {e}"),
        )),
    }
}

/// get service status
fn json_status(context: Arc<Context>) -> Result<Value> {
    Ok(json!({
        "exe_path": env::current_exe()?,
        "config": &context.settings.read().hub,
    }))
}

/// get service status
async fn status(State(context): State<Arc<Context>>) -> impl IntoResponse {
    let json_status = match json_status(context) {
        Ok(json_status) => json_status,
        Err(e) => {
            error!(?e, "unable to get service status");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        },
    };

    Ok(Json(json_status))
}

/// get support bundle, return zip file of various status, log files, database dump
async fn support_bundle(State(context): State<Arc<Context>>) -> impl IntoResponse {
    let json_status = match json_status(context.clone()) {
        Ok(json_status) => json_status,
        Err(e) => {
            error!(?e, "unable to get service status");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        },
    };

    let status_bytes = match serde_json::to_vec_pretty(&json_status) {
        Ok(status_bytes) => status_bytes,
        Err(e) => {
            error!(?e, "unable to convert JSON status to bytes");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        },
    };

    // TODO: add streaming of cache databases: index/system to bundle?

    // Use the provided hub.toml if available, otherwise fall back to the default
    let hub_toml = match &context.args.hub_config {
        Some(hub) => hub.clone(),
        None => Settings::default_hub_config(),
    };

    let mut builder = SupportBundle::builder()
        .with_comment("Omniverse Hub Support Bundle")
        .add_bytes(status_bytes, "status.json")
        .add_file(hub_toml, "hub.toml")
        .add_file(CONFIG_DIR.join(OMNIVERSE_TOML.as_path()), "omniverse.toml");

    if let Some(logging_path) = context.settings.read().hub.logging.path.as_ref() {
        builder = builder.add_directory(logging_path, "logs");
    }

    let bundle = match builder.build().await {
        Ok(bundle) => bundle,
        Err(e) => {
            error!(?e, "unable to make support bundle");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        },
    };

    // get the path to the newly built bundle zip file
    let path = bundle.path();

    let file = match fs::File::open(path).await {
        Ok(file) => file,
        Err(e) => {
            error!(?e, ?path, "unable to open support bundle file");
            return Err(StatusCode::NOT_FOUND);
        },
    };

    // convert the `AsyncRead` into a `Stream`
    let stream = ReaderStream::new(file);
    // convert the `Stream` into an `axum::body::HttpBody`
    let body = axum::body::Body::from_stream(stream);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip, application/octet-stream"),
    );

    let Some(file_name) = path.file_name() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let Ok(disposition) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        file_name.to_string_lossy().as_ref()
    )) else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    headers.insert(header::CONTENT_DISPOSITION, disposition);

    Ok((headers, body))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{context::TestContext, settings::from_args};

    use hub_util::settings::LogLevel;

    use super::*;
    use axum::{
        body::Body,
        http::{self, header, Request, StatusCode},
    };
    use tower::ServiceExt; // oneshot

    #[test_log::test(tokio::test)]
    async fn config() {
        let test_context = TestContext::new();
        let app = service_routes().with_state(test_context.context.clone());

        let response = app
            .oneshot(Request::builder().uri("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body, json!(test_context.context.settings.read().hub));
    }

    #[test_log::test(tokio::test)]
    async fn status() {
        let test_context = TestContext::new();
        let app = service_routes().with_state(test_context.context.clone());

        let response = app
            .oneshot(Request::builder().uri("/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["config"], json!(test_context.context.settings.read().hub));
        assert_eq!(body["exe_path"], json!(env::current_exe().unwrap()));
    }

    #[test_log::test(tokio::test)]
    async fn update_cache_config() {
        let test_context = TestContext::new_with_hub_toml();
        let app = service_routes().with_state(test_context.context.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/config")
                    .method(http::Method::PUT)
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "cache": {
                                "path": test_context.tmp_dir.path(),
                                "cleanup_threshold": 25,
                                "max_size": "55B",
                                "garbage_collection_interval": 100,
                            },
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Create a new settings from the generated toml and verify it
        let settings = from_args(test_context.context.args.clone()).unwrap();
        assert_eq!(settings.hub.cache.path.unwrap().as_path(), test_context.tmp_dir.path());
        assert_eq!(settings.hub.cache.cleanup_threshold, 25);
        assert_eq!(settings.hub.cache.max_size, bytesize::ByteSize::b(55));
        assert_eq!(settings.hub.cache.garbage_collection_interval, 100);
    }

    #[test_log::test(tokio::test)]
    async fn update_http_config() {
        let test_context = TestContext::new_with_hub_toml();
        let app = service_routes().with_state(test_context.context.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/config")
                    .method(http::Method::PUT)
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "http": {
                                "host": "10.20.30.40",
                                "port": 5555,
                            },
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Create a new settings from the generated toml and verify it
        let settings = from_args(test_context.context.args.clone()).unwrap();
        assert_eq!(settings.hub.http.host, "10.20.30.40");
        assert_eq!(settings.hub.http.port, 5555);
    }

    #[test_log::test(tokio::test)]
    async fn update_logging_config() {
        let test_context = TestContext::new_with_hub_toml();
        let app = service_routes().with_state(test_context.context.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/config")
                    .method(http::Method::PUT)
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "logging": {
                                "path": test_context.tmp_dir.path(),
                                "file": {
                                    "file": "log.txt",
                                    "enabled": true,
                                    "max_level": "warn",
                                    "json": true,
                                },
                                "stdout": {
                                    "enabled": true,
                                    "max_level": "warn",
                                    "color": true,
                                },
                                "stderr": {
                                    "enabled": true,
                                    "max_level": "warn",
                                    "color": true,
                                }
                            },
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Create a new settings from the generated toml and verify it
        let settings = from_args(test_context.context.args.clone()).unwrap();
        assert_eq!(
            settings.hub.logging.path.unwrap().as_path(),
            test_context.tmp_dir.path()
        );
        assert_eq!(settings.hub.logging.file.file, PathBuf::from("log.txt"));
        assert!(settings.hub.logging.file.enabled);
        assert!(settings.hub.logging.file.json);
        assert_eq!(settings.hub.logging.file.max_level, LogLevel::Warn);
        assert!(settings.hub.logging.stdout.enabled);
        assert!(settings.hub.logging.stdout.color);
        assert_eq!(settings.hub.logging.stdout.max_level, LogLevel::Warn);
        assert!(settings.hub.logging.stderr.enabled);
        assert!(settings.hub.logging.stderr.color);
        assert_eq!(settings.hub.logging.stderr.max_level, LogLevel::Warn);
    }

    #[test_log::test(tokio::test)]
    async fn update_runtime_config() {
        let test_context = TestContext::new_with_hub_toml();
        let app = service_routes().with_state(test_context.context.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/config")
                    .method(http::Method::PUT)
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "runtime": {
                                "num_worker_threads": 13,
                            },
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Create a new settings from the generated toml and verify it
        let settings = from_args(test_context.context.args.clone()).unwrap();
        assert_eq!(settings.hub.runtime.num_worker_threads, 13);
    }

    #[test_log::test(tokio::test)]
    async fn update_transport_config() {
        let test_context = TestContext::new_with_hub_toml();
        let app = service_routes().with_state(test_context.context.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/config")
                    .method(http::Method::PUT)
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "transport": {
                                "tcp": {
                                "host": "10.20.30.40",
                                "port": 5555,
                                "rpc_port": 6666,
                                "ext_port": 7777,
                                },
                            },
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Create a new settings from the generated toml and verify it
        let settings = from_args(test_context.context.args.clone()).unwrap();
        match settings.hub.transport {
            hub_util::settings::Transport::Tcp {
                host,
                rpc_port,
                ext_port,
                ..
            } => {
                assert_eq!(host, "10.20.30.40");
                assert_eq!(rpc_port, 6666);
                assert_eq!(ext_port, 7777);
            },
            _ => panic!("Type should be tcp"),
        }
    }

    #[test_log::test(tokio::test)]
    async fn update_combine_config() {
        let test_context = TestContext::new_with_hub_toml();
        let app = service_routes().with_state(test_context.context.clone());

        // Update runtime first
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/config")
                    .method(http::Method::PUT)
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "runtime": {
                                "num_worker_threads": 13,
                            },
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Create a new settings from the generated toml and verify it
        let settings = from_args(test_context.context.args.clone()).unwrap();
        assert_eq!(settings.hub.runtime.num_worker_threads, 13);

        // Now update http
        let app = service_routes().with_state(test_context.context.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/config")
                    .method(http::Method::PUT)
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "http": {
                                "host": "10.20.30.40",
                                "port": 5555,
                            },
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Create a new settings from the generated toml and verify it
        let settings = from_args(test_context.context.args.clone()).unwrap();
        assert_eq!(settings.hub.runtime.num_worker_threads, 13);
        assert_eq!(settings.hub.http.host, "10.20.30.40");
        assert_eq!(settings.hub.http.port, 5555);
    }

    #[test_log::test(tokio::test)]
    async fn update_overwrite_config() {
        let test_context = TestContext::new_with_hub_toml();
        let app = service_routes().with_state(test_context.context.clone());

        // Update cache first
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/config")
                    .method(http::Method::PUT)
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "cache": {
                                "path": test_context.tmp_dir.path(),
                                "cleanup_threshold": 25,
                                "max_size": "55B",
                                "garbage_collection_interval": 100,
                            },
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Create a new settings from the generated toml and verify it
        let settings = from_args(test_context.context.args.clone()).unwrap();
        assert_eq!(settings.hub.cache.path.unwrap().as_path(), test_context.tmp_dir.path());
        assert_eq!(settings.hub.cache.cleanup_threshold, 25);
        assert_eq!(settings.hub.cache.max_size, bytesize::ByteSize::b(55));
        assert_eq!(settings.hub.cache.garbage_collection_interval, 100);

        // Now override those values
        let app = service_routes().with_state(test_context.context.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/config")
                    .method(http::Method::PUT)
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "cache": {
                                "cleanup_threshold": 1,
                                "max_size": "2200B",
                                "garbage_collection_interval": 4444,
                            },
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Create a new settings from the generated toml and verify it
        let settings = from_args(test_context.context.args.clone()).unwrap();
        assert_eq!(settings.hub.cache.path.unwrap().as_path(), test_context.tmp_dir.path());
        assert_eq!(settings.hub.cache.cleanup_threshold, 1);
        assert_eq!(settings.hub.cache.max_size, bytesize::ByteSize::b(2200));
        assert_eq!(settings.hub.cache.garbage_collection_interval, 4444);
    }

    #[test_log::test(tokio::test)]
    async fn update_config_invalid_request() {
        let test_context = TestContext::new_with_hub_toml();
        let app = service_routes().with_state(test_context.context.clone());

        // An empty request
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/config")
                    .method(http::Method::PUT)
                    .header("Content-Type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let app = service_routes().with_state(test_context.context.clone());
        // An bogus request
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/config")
                    .method(http::Method::PUT)
                    .header("Content-Type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let app = service_routes().with_state(test_context.context.clone());
        // JSON that isn't a valid setting
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/config")
                    .method(http::Method::PUT)
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "cache": {
                                "cleanup_threshold" : "shouldnt_be_a_string",
                            },
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        let message = body["message"].as_str().unwrap();
        assert!(message.starts_with("Failed to deserialize settings"));
    }

    #[test_log::test(tokio::test)]
    async fn support_bundle() {
        let test_context = TestContext::new();
        let app = service_routes().with_state(test_context.context.clone());

        let response = app
            .oneshot(Request::builder().uri("/supportbundle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/zip, application/octet-stream"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(!body.is_empty());
    }
}
