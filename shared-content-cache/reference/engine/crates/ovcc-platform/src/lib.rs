// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Operating-system primitives the OVCC engine needs beyond `std`, behind one
//! API with a Linux and a Windows backend.
//!
//! | Primitive | Linux | Windows |
//! |-----------|-------|---------|
//! | [`stat_nofollow`] identity | `lstat` `st_dev`/`st_ino` | `FILE_ID_INFO` volume serial + 128-bit file id |
//! | [`open_directory`] | `open(2)` | `CreateFileW` + `FILE_FLAG_BACKUP_SEMANTICS` |
//! | [`open_regular_at`] | `/proc/self/fd/N/name` + `O_NOFOLLOW` | `NtCreateFile` probe relative to the directory handle + `FILE_OPEN_REPARSE_POINT`, then `OpenFileById` |
//! | [`sync_file`] / [`sync_directory`] | `fsync` | `FlushFileBuffers` on a write handle |
//! | [`rename`] | `rename(2)` | `std::fs::rename` + transient-violation retry |
//! | [`rename_noreplace`] | `renameat2(RENAME_NOREPLACE)` | `MoveFileExW(MOVEFILE_WRITE_THROUGH)` + retry |
//! | [`exchange_paths`] | `renameat2(RENAME_EXCHANGE)` | three no-replace renames through a sibling (see its contract) |
//! | [`available_bytes`] | `statvfs` | `GetDiskFreeSpaceExW` |
//! | [`ReadOnlyMapping`] | `mmap`/`munmap` (Linux-only test primitive) | — |
//! | [`system_resolver_config`] | `/etc/resolv.conf` | native DNS client (used directly by `ovcc-upstream`) |
//! | `system_root_certificates` / `verify_system_certificate_chain` | (distro PEM bundle, read by the caller) | CryptoAPI `ROOT` store plus full-chain policy verification |
//! | zero-copy send | `sendfile(2)` ([`sendfile`]) | overlapped `TransmitFile` ([`transmit_file`]) from an overlapped reopen (`TransmitSource`) |
//! | [`send_tls_close_notify`] | non-blocking `sendmsg(2)` of a kernel TLS alert record | — (Windows TLS stays in rustls) |
//! | [`tcp_send_progress`] | `TCP_INFO` acknowledged bytes and unsent or unacknowledged data | — (no kernel record queue to wait out) |
//!
//! Every function is a thin, allocation-light wrapper: policy (what to rename,
//! when to sync, how to pace a send) stays in the calling crate.

use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};

pub mod local;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as sys;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as sys;

#[cfg(target_os = "linux")]
pub use linux::{
    ReadOnlyMapping, TcpSendProgress, send_tls_close_notify, sendfile, tcp_send_progress,
};
#[cfg(windows)]
pub use local::ReadOnlyMapping;
#[cfg(windows)]
pub use windows::{
    TransmitError, TransmitSource, transmit_file, transmit_file_with_inactivity_timeout,
};

/// Stable identity of a file or directory: equal identities name the same
/// on-disk object even across renames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    /// Filesystem (Linux `st_dev`) or volume serial number (Windows).
    pub device: u64,
    /// Inode (Linux) or 128-bit file id (Windows; ReFS ids exceed 64 bits).
    pub file: u128,
}

/// Metadata of a path without following a final symlink, paired with its
/// [`FileIdentity`].
#[derive(Debug)]
pub struct Stat {
    pub metadata: Metadata,
    pub identity: FileIdentity,
}

/// Change-detection stamp for a path, following symlinks (a Kubernetes Secret
/// mount swaps a `..data` symlink). Any field differing means the content may
/// have changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    pub identity: FileIdentity,
    pub length: u64,
    pub modified: Option<std::time::SystemTime>,
    /// Unix mode bits (Linux) or file attributes (Windows).
    pub permissions: u32,
}

/// `lstat`-equivalent metadata plus identity for `path`.
pub fn stat_nofollow(path: &Path) -> io::Result<Stat> {
    sys::stat_nofollow(path)
}

/// Identity of an open file or directory handle.
pub fn file_identity(file: &File) -> io::Result<FileIdentity> {
    sys::file_identity(file)
}

/// Change-detection stamp for `path`, following symlinks.
pub fn file_stamp(path: &Path) -> io::Result<FileStamp> {
    sys::file_stamp(path)
}

/// Opens a directory as a [`File`] handle usable with [`open_regular_at`],
/// [`File::metadata`], and [`file_identity`]. The handle does not prevent the
/// directory from being renamed or deleted.
pub fn open_directory(path: &Path) -> io::Result<File> {
    sys::open_directory(path)
}

/// Opens `name` read-only relative to `directory` without following a final
/// symlink. `name` must be a single path component. Resolving through the held
/// handle keeps the open inside that exact directory even if its path has been
/// renamed or replaced since it was opened. The returned handle never prevents
/// that directory from being renamed or deleted (on Windows it is opened by
/// file id for exactly this reason). Callers must still check the file type:
/// Linux opens a directory named `name`, Windows refuses it.
pub fn open_regular_at(directory: &File, name: &str) -> io::Result<File> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', ':', '\0']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "name must be a single path component",
        ));
    }
    sys::open_regular_at(directory, name)
}

/// Flushes an existing file's data and metadata to stable storage without
/// holding a handle from the writer. (Windows `FlushFileBuffers` needs write
/// access, so a read-only handle cannot be used for this there.)
pub fn sync_file(path: &Path) -> io::Result<()> {
    sys::sync_file(path)
}

/// Makes completed renames, creations, and deletions inside `path` durable.
pub fn sync_directory(path: &Path) -> io::Result<()> {
    sys::sync_directory(path)
}

/// [`std::fs::rename`] that, on Windows, rides out the brief sharing and
/// access violations foreign opens (antivirus, indexer) cause.
pub fn rename(source: &Path, destination: &Path) -> io::Result<()> {
    sys::rename(source, destination)
}

/// Renames `source` to `destination`, failing with
/// [`io::ErrorKind::AlreadyExists`] rather than replacing an existing entry.
pub fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    sys::rename_noreplace(source, destination)
}

/// Swaps two existing entries, which may be of unlike types (a file and a
/// directory).
///
/// [!] Atomic on Linux only. Windows has no exchange primitive, so it performs
/// `right -> <left>.exchange`, `left -> right`, `<left>.exchange -> left`,
/// rolling the first step back if the second fails. The returned path contains
/// the entry that occupied `right` before the exchange. Between steps `right` is
/// briefly absent, and a crash can leave the old `right` at
/// `<left>.exchange`. Callers must therefore (a) never let a reader treat
/// `right` as authoritative while an exchange is in flight and (b) keep `left`
/// in a directory their crash recovery sweeps.
pub fn exchange_paths(left: &Path, right: &Path) -> io::Result<PathBuf> {
    sys::exchange_paths(left, right)
}

/// Bytes available to this (unprivileged) process on the filesystem holding
/// `path`.
pub fn available_bytes(path: &Path) -> io::Result<u64> {
    sys::available_bytes(path)
}

/// Where [`system_resolver_config`] reads from, for error messages.
#[cfg(target_os = "linux")]
pub const SYSTEM_RESOLVER_SOURCE: &str = "/etc/resolv.conf";
#[cfg(windows)]
pub const SYSTEM_RESOLVER_SOURCE: &str = "the Windows DNS Client";

/// The Linux system DNS configuration in `resolv.conf` syntax. Windows DNS
/// policy cannot be represented in that format, so Windows returns
/// [`io::ErrorKind::Unsupported`] and callers must use the native resolver.
pub fn system_resolver_config() -> io::Result<String> {
    sys::system_resolver_config()
}

/// DER certificates of the Windows system trust store (`ROOT`), the Windows
/// counterpart of a distro CA bundle such as
/// `/etc/ssl/certs/ca-certificates.crt`. Only roots the Windows chain engine
/// trusts for TLS server authentication are returned, which applies EKU
/// restrictions, the `Disallowed` store, and Untrusted CTLs.
///
/// [!] Windows installs some roots lazily (Automatic Root Certificates
/// Update), so a root no CryptoAPI client on the host has used yet may be
/// absent; pin a PEM bundle when that matters.
#[cfg(windows)]
pub fn system_root_certificates() -> io::Result<Vec<Vec<u8>>> {
    windows::system_root_certificates()
}

/// Applies Windows certificate-chain and SSL policy to the complete chain
/// presented by an origin after rustls has performed its normal verification.
#[cfg(windows)]
pub fn verify_system_certificate_chain(
    end_entity: &[u8],
    intermediates: &[&[u8]],
    server_name: &str,
) -> io::Result<()> {
    windows::verify_system_certificate_chain(end_entity, intermediates, server_name)
}

/// Creates a FIFO at `path`, the fixture for proving that non-regular files
/// planted in the cache are rejected without blocking.
#[cfg(target_os = "linux")]
pub fn create_fifo(path: &Path) -> io::Result<()> {
    linux::create_fifo(path)
}

/// A process-unique key for an open handle, stable for its lifetime. Suitable
/// for side tables keyed by open file.
pub fn handle_key(file: &File) -> u64 {
    sys::handle_key(file)
}

/// Positional read that does not depend on (or, on Linux, move) the cursor.
/// Callers must not share one handle's cursor across threads on Windows,
/// where the positional read also moves it.
pub fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    sys::read_at(file, buffer, offset)
}

/// [`read_at`] until `buffer` is full, failing with
/// [`io::ErrorKind::UnexpectedEof`] if the file ends first.
pub fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buffer.is_empty() {
        match read_at(file, buffer, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => {
                buffer = &mut buffer[read..];
                offset += read as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
