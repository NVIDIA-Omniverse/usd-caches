// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Same-user local IPC with bounded control frames.

use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(target_os = "linux")]
#[path = "local_linux.rs"]
mod sys;
#[cfg(windows)]
#[path = "local_windows.rs"]
mod sys;
#[cfg(windows)]
pub use sys::ReadOnlyMapping;
pub use sys::{Connection, Listener};

/// Bounds protocol allocations independently of caller-provided lengths.
pub const MAX_FRAME: usize = 64 * 1024;

impl Connection {
    /// Discovers the current user's local server without creating directories.
    pub async fn connect_default() -> io::Result<Self> {
        Self::connect(&sys::default_endpoint()?).await
    }

    pub async fn read_frame(&mut self) -> io::Result<Vec<u8>> {
        let length = self.stream.read_u32().await? as usize;
        if length > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "local frame exceeds 64 KiB",
            ));
        }
        let mut bytes = vec![0; length];
        self.stream.read_exact(&mut bytes).await?;
        Ok(bytes)
    }

    pub async fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "local frame exceeds 64 KiB",
            ));
        }
        self.stream.write_u32(bytes.len() as u32).await?;
        self.stream.write_all(bytes).await?;
        self.stream.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use std::path::PathBuf;
    use std::time::Duration;

    #[tokio::test]
    async fn local_get_frame_big_endian_length_is_stable() -> io::Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        #[cfg(target_os = "linux")]
        let endpoint = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))?;
            root.path().join("frame.sock")
        };
        #[cfg(windows)]
        let endpoint = PathBuf::from(format!(
            r"\\.\pipe\ovcc-frame-{}-{}",
            std::process::id(),
            root.path()
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        ));
        let mut listener = Listener::bind(&endpoint)?;
        let server = async {
            let mut connection = listener.accept().await?;
            connection.write_frame(&[1, 2, 3]).await?;
            connection.read_frame().await
        };
        let client = async {
            let mut connection = Connection::connect(&endpoint).await?;
            let mut bytes = [0; 7];
            connection.stream.read_exact(&mut bytes).await?;
            connection.stream.write_all(&[0, 0, 0, 2, 9, 8]).await?;
            Ok::<_, io::Error>(bytes)
        };
        // Act.
        let (received, sent) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(server, client)
        })
        .await
        .map_err(io::Error::other)?;
        // Assert.
        assert_eq!(sent?, [0, 0, 0, 3, 1, 2, 3]);
        assert_eq!(received?, [9, 8]);
        Ok(())
    }
}
