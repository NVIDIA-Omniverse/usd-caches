// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod args;
use crate::args::Args;

mod context;
use crate::context::Context;

mod error;
use crate::error::Error;

mod logging;
use crate::logging::Logging;

mod http;

mod rpc;
use crate::rpc::server_msgpack::RpcServer;
use crate::rpc::server_protobuf::ProtobufServer;

mod settings;
use crate::settings::from_args;

mod support_bundle;

use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::prelude::*;
use std::io::SeekFrom;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use fd_lock::RwLock;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use tokio::net::TcpListener;
use tokio::runtime::Builder;
use tokio::signal;
use tokio::task;
use tracing::{debug, error, info, instrument, warn};
use tracing_subscriber::filter::{EnvFilter, LevelFilter};

#[instrument(level = "info", ret)]
fn main() -> Result<()> {
    // setup default tracing subscriber for early logging to stdout
    // actual logging configuration will be setup in Logging::new() with `tracing::subscriber::DefaultGuard`
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    tracing_subscriber::fmt::Subscriber::builder()
        .with_file(true)
        .with_level(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_target(true)
        .with_ansi(false)
        .with_env_filter(env_filter)
        .try_init()
        .expect("Unable to install default tracing subscriber");

    // set process name with OS
    set_process_name();

    let args = Arc::new(Args::new(env::args_os()));

    // Indicates if we should reset or shutdown
    let keep_running = Arc::new(AtomicBool::new(true));

    while keep_running.load(Ordering::SeqCst) {
        let settings = from_args(args.clone())?;

        // if user requested dump of settings, print them here to console
        // using pretty printer and exit
        if args.show_config {
            println!("{}", serde_json::to_string_pretty(&settings.hub)?);
            return Ok(());
        }

        // when logging is dropped, workerguard will flush any non-blocking logs
        let logging = Logging::new(&settings)?;

        info!(
            name = env!("CARGO_PKG_NAME"),
            description = env!("CARGO_PKG_DESCRIPTION"),
            version = env!("CARGO_PKG_VERSION"),
            license = env!("CARGO_PKG_LICENSE"),
            homepage = env!("CARGO_PKG_HOMEPAGE"),
            respository = env!("CARGO_PKG_REPOSITORY"),
        );

        // Calculate path to write the port file to
        let cache_dir = settings
            .hub
            .cache
            .path
            .clone()
            .ok_or(Error::Configuration("Hub cache path not set or calculated".to_owned()))?;

        let port_temp_path = cache_dir.join(".hub.port");
        let port_final_path = cache_dir.join("hub.port");

        // ensure cache base directory exists
        fs::create_dir_all(&cache_dir)?;

        let mut hub_lock_file = RwLock::new(
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(cache_dir.join("hub.lock"))?,
        );

        // check if hub is already running, if so, write out config if command line arg is present
        let Ok(mut hub_lock) = hub_lock_file.try_write() else {
            info!(?hub_lock_file, ?args.write_config, args.shutdown, "Hub is already running");

            // read port file and attempt to get config from http api
            let port = wait_for_port(port_final_path.as_path());
            info!(port, "Found hub port");

            // request to shutdown hub
            if args.shutdown {
                info!("Requesting shutdown");
                let status = reqwest::blocking::get(format!("http://127.0.0.1:{port}/api/1.0/shutdown"))?.status();

                match status {
                    reqwest::StatusCode::OK => return Ok(()),
                    _ => {
                        error!(?status, "Failed to shutdown hub via the HTTP API");
                        return Err(Error::HttpApi(status.as_u16()).into());
                    },
                }
            }

            if let Some(config_path) = &args.write_config {
                info!("Requesting configuration");
                // grab config from API and write it out
                let config =
                    reqwest::blocking::get(format!("http://127.0.0.1:{port}/api/1.0/service/config"))?.text()?;
                write_config(config_path, config);
            }

            return Ok(());
        };

        if args.detect_only {
            return Err(anyhow::anyhow!("Hub is not running"));
        }

        // hub is not running, but we are asking for shutdown, so just exit
        if args.shutdown {
            warn!("Hub shutdown requested, but Hub is not running");
            return Ok(());
        }

        // write current pid into lock file
        hub_lock.seek(SeekFrom::Start(0))?;
        hub_lock.set_len(0)?; // truncate the file
        write!(hub_lock, "{}", std::process::id())?;

        let context = Arc::new(Context::new(settings, args.clone(), logging)?);
        let rpc_service = RpcServer::new();
        let protobuf_service = ProtobufServer::new();

        let runtime = Builder::new_multi_thread()
            .enable_all()
            .worker_threads(context.settings.read().hub.runtime.num_worker_threads)
            .thread_name_fn(|| {
                static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
                let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
                format!("hub-{id:03}")
            })
            .build()?;

        let http_addr = SocketAddr::new(
            IpAddr::from_str(context.settings.read().hub.http.host.as_str())?,
            context.settings.read().hub.http.port,
        );

        let keep_running_clone = keep_running.clone();
        runtime.block_on(async move {
            let mut tasks = task::JoinSet::new();
            let mut api = task::JoinSet::new();

            // startup api
            let http_listener = TcpListener::bind(&http_addr).await.unwrap();
            let http_assigned_port = http_listener.local_addr().unwrap().port();
            info!("HTTP server bound to port {} ", http_assigned_port);

            let http_app = http::new_app(context.clone());
            let http_service = axum::serve(http_listener, http_app.into_make_service());

            // Update the RPC port in the settings
            context.settings.write().hub.http.port = http_assigned_port;

            // Write the HTTP port to a file in the cache directory so it's discoverable.
            // Do this in two steps so it's atomic.
            fs::write(&port_temp_path, http_assigned_port.to_string())
                .unwrap_or_else(|_| panic!("Failed to write port {} to {:?}", http_assigned_port, &port_temp_path));
            fs::rename(&port_temp_path, &port_final_path)
                .unwrap_or_else(|_| panic!("Failed to rename file: {:?} -> {:?}", &port_temp_path, &port_final_path));

            api.spawn(async move { http_service.await });

            // startup block cache service (gc, other background tasks)
            let mut cache_tasks = context.cache.run()?;

            // startup cache service (garbage collection, background tasks)
            // tasks.spawn(async move { cache_service.run().await });

            let rpc_context = context.clone();
            tasks.spawn(async move { rpc_service.run_server(rpc_context).await });

            let protobuf_context = context.clone();
            tasks.spawn(async move { protobuf_service.run_server(protobuf_context).await });

            tokio::spawn({
                let context = context.clone();

                async move {
                    if let Some(config_path) = &context.args.write_config {
                        let ready_futures = FuturesUnordered::new();
                        ready_futures.push(context.msgpack_ready.notified());
                        ready_futures.push(context.protobuf_ready.notified());
                        let _: Vec<_> = ready_futures.collect().await;
                        // write out config now that the bound ports are known
                        let config = serde_json::to_string_pretty(&context.settings.read().hub)
                            .expect("Failed to serialize Hub config JSON");
                        write_config(config_path, config);
                    }
                }
            });

            keep_running_clone.store(false, Ordering::SeqCst);
            tokio::select! {
                res = signal::ctrl_c() => {
                    info!(?res, "Interrupted!");
                }
                res = tasks.join_next() => {
                    info!(?res, "Task exited");
                }
                res = cache_tasks.join_next() => {
                    info!(?res, "Cache task exited");
                }
                res = api.join_next() => {
                    info!(?res, "API task exited");
                }
                _ = context.shutdown_notify.notified() => {
                    info!("Shutdown called");
                }
                _ = context.reset_notify.notified() => {
                    info!("Reset called. Hub will restart");
                    keep_running_clone.store(true, Ordering::SeqCst);
                }
            }

            info!(
                tasks = tasks.len(),
                api = api.len(),
                "shutting down any remaining tasks"
            );
            api.shutdown().await;
            tasks.shutdown().await;
            cache_tasks.shutdown().await;
            Ok::<(), Error>(())
        })?;
    }

    info!("exiting...");

    Ok(())
}

// Waits for the hub.port file to exist and returns the port
#[instrument(level = "info")]
fn wait_for_port(port_file: &Path) -> u16 {
    let mut retries = 0;
    loop {
        if port_file.exists() {
            let port =
                fs::read_to_string(port_file).unwrap_or_else(|_| panic!("Unable to read port file: {port_file:?}"));
            debug!(port, ?port_file);
            return port.parse().unwrap();
        } else {
            retries += 1;
            assert!(
                retries < 200,
                "Failed to read port file from cache directory: File doesn't exist: {port_file:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

// write out current config to `path`
#[instrument(level = "info", skip(config))]
fn write_config(path: &Path, config: String) {
    let parent = path.parent().unwrap_or_else(|| panic!("Invalid path: {path:?}"));

    // temp files cannot be persisted across filesystems, so make sure the temp file
    // is created in the parent directory of the final path
    let named_tempfile = tempfile::Builder::new()
        .suffix(".hub-config")
        .rand_bytes(8)
        .tempfile_in(parent)
        .unwrap_or_else(|e| panic!("Failed to create temporary file: {e:?}"));

    let mut tmp_file = named_tempfile.as_file();
    write!(tmp_file, "{config}").unwrap_or_else(|e| panic!("Failed to write config file '{tmp_file:?}': {e:?}"));

    // if a file exists at the target path, persist will atomically replace it
    named_tempfile
        .persist(path)
        .unwrap_or_else(|e| panic!("Error persisting config file: {e:?}"));
}

#[cfg(target_os = "linux")]
fn set_process_name() {
    if let Err(e) = prctl::set_name("Omniverse Hub") {
        eprintln!("Failed to set process name. errno: {e}");
    }
}

// windows the process name is set through resources file
#[cfg(not(target_os = "linux"))]
fn set_process_name() {}
