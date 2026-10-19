// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{Semaphore, SemaphorePermit};

use crate::{Client, LeaseRequest, LeaseResult};

/// Portable starting cap from the warm-object matrix, not a universal optimum.
pub const DEFAULT_MAX_CONNECTIONS: usize = 4;

/// Cloneable path client with a private, bounded pool of dedicated connections.
/// Calls own their complete exchange; returned metadata owns no pool capacity.
#[derive(Clone)]
pub struct SharedClient {
    inner: Arc<Pool>,
}

struct Pool {
    endpoint: Option<PathBuf>,
    capacity: Semaphore,
    idle: Mutex<Vec<Client>>,
}

impl Pool {
    fn idle(&self) -> MutexGuard<'_, Vec<Client>> {
        // Only bounded vector operations hold this lock, never I/O or user code.
        self.idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for SharedClient {
    fn default() -> Self {
        Self::build(None, DEFAULT_MAX_CONNECTIONS)
    }
}

impl SharedClient {
    /// Creates a lazy same-user client. Clones share the same connection bound.
    pub fn new(max_connections: usize) -> io::Result<Self> {
        Self::configured(None, max_connections)
    }

    /// Overrides discovery for isolated servers and tests.
    pub fn with_endpoint(endpoint: &Path, max_connections: usize) -> io::Result<Self> {
        Self::configured(Some(endpoint.to_owned()), max_connections)
    }

    fn configured(endpoint: Option<PathBuf>, max_connections: usize) -> io::Result<Self> {
        if max_connections == 0 || max_connections > Semaphore::MAX_PERMITS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid connection limit",
            ));
        }
        Ok(Self::build(endpoint, max_connections))
    }

    fn build(endpoint: Option<PathBuf>, max_connections: usize) -> Self {
        Self {
            inner: Arc::new(Pool {
                endpoint,
                capacity: Semaphore::new(max_connections),
                idle: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Waits for capacity, connects if needed, and performs exactly one exchange.
    /// Cancellation releases capacity and discards any interrupted connection.
    /// Callers may wrap this future in a timeout covering all three stages.
    pub async fn lease(&self, request: &LeaseRequest) -> io::Result<LeaseResult> {
        let permit = self
            .inner
            .capacity
            .acquire()
            .await
            .map_err(io::Error::other)?;
        let mut slot = Checkout {
            pool: &self.inner,
            client: self.inner.idle().pop(),
            _permit: permit,
        };
        if slot.client.is_none() {
            slot.client = Some(match &self.inner.endpoint {
                Some(path) => Client::connect(path).await?,
                None => Client::connect_local().await?,
            });
        }
        slot.client
            .as_mut()
            .ok_or_else(|| io::Error::other("missing connection"))?
            .lease(request)
            .await
    }
}

struct Checkout<'a> {
    pool: &'a Pool,
    client: Option<Client>,
    _permit: SemaphorePermit<'a>,
}

impl Drop for Checkout<'_> {
    fn drop(&mut self) {
        if let Some(client) = self.client.take().filter(Client::is_usable) {
            // Makes the connection available before a waiting call receives capacity.
            self.pool.idle().push(client);
        }
    }
}

#[cfg(test)]
mod tests;
