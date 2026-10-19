// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Linux backend.

use std::ffi::CString;
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::libc;

use crate::{FileIdentity, FileStamp, Stat};

fn identity(metadata: &Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        file: u128::from(metadata.ino()),
    }
}

pub(crate) fn stat_nofollow(path: &Path) -> io::Result<Stat> {
    let metadata = std::fs::symlink_metadata(path)?;
    Ok(Stat {
        identity: identity(&metadata),
        metadata,
    })
}

pub(crate) fn file_identity(file: &File) -> io::Result<FileIdentity> {
    Ok(identity(&file.metadata()?))
}

pub(crate) fn file_stamp(path: &Path) -> io::Result<FileStamp> {
    let metadata = std::fs::metadata(path)?;
    Ok(FileStamp {
        identity: identity(&metadata),
        length: metadata.len(),
        modified: metadata.modified().ok(),
        permissions: metadata.mode(),
    })
}

pub(crate) fn open_directory(path: &Path) -> io::Result<File> {
    File::open(path)
}

pub(crate) fn open_regular_at(directory: &File, name: &str) -> io::Result<File> {
    // The magic link resolves through the held descriptor, so the lookup stays
    // inside that directory even if its path was renamed or exchanged.
    // O_NONBLOCK keeps a FIFO or device planted under the name from blocking
    // the open; it does not affect regular files, and callers reject the
    // non-regular handle by type.
    let path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name);
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
}

pub(crate) fn sync_file(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn renameat2(source: &Path, destination: &Path, flags: libc::c_uint) -> io::Result<()> {
    let source = CString::new(source.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: both C strings stay alive for the complete syscall, which only
    // reads the NUL-terminated paths behind their pointers.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            flags,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn rename(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

pub(crate) fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    renameat2(source, destination, libc::RENAME_NOREPLACE)
}

pub(crate) fn exchange_paths(left: &Path, right: &Path) -> io::Result<PathBuf> {
    // RENAME_EXCHANGE atomically swaps unlike path types, including a legacy
    // contiguous file and a segmented directory.
    renameat2(left, right, libc::RENAME_EXCHANGE)?;
    Ok(left.to_path_buf())
}

pub(crate) fn available_bytes(path: &Path) -> io::Result<u64> {
    let stats = nix::sys::statvfs::statvfs(path).map_err(io::Error::from)?;
    Ok(stats
        .blocks_available()
        .saturating_mul(stats.fragment_size()))
}

pub(crate) fn system_resolver_config() -> io::Result<String> {
    std::fs::read_to_string(crate::SYSTEM_RESOLVER_SOURCE)
}

pub(crate) fn create_fifo(path: &Path) -> io::Result<()> {
    use nix::sys::stat::Mode;
    nix::unistd::mkfifo(path, Mode::S_IRUSR | Mode::S_IWUSR).map_err(io::Error::from)
}

pub(crate) fn handle_key(file: &File) -> u64 {
    // Descriptors are non-negative while open.
    file.as_raw_fd() as u64
}

pub(crate) fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    file.read_at(buffer, offset)
}

/// Read-only file mapping used to check that cached bytes survive eviction.
#[derive(Debug)]
pub struct ReadOnlyMapping {
    address: std::ptr::NonNull<u8>,
    length: usize,
}

// SAFETY: the constructor requires immutable backing bytes for the entire view
// lifetime. Mapping addresses are process-wide and Drop exclusively owns unmap.
unsafe impl Send for ReadOnlyMapping {}
unsafe impl Sync for ReadOnlyMapping {}

impl ReadOnlyMapping {
    /// Maps `length` bytes from the start of `file`.
    ///
    /// # Safety
    ///
    /// The caller must keep the file at least `length` bytes long and avoid
    /// modifying it while the mapping is read.
    pub unsafe fn new(file: &File, length: usize) -> io::Result<Self> {
        if length == 0 || length > isize::MAX as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty mapping"));
        }
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let Some(address) = std::ptr::NonNull::new(address.cast::<u8>()) else {
            unsafe { libc::munmap(address, length) };
            return Err(io::Error::other("mmap returned a null address"));
        };
        Ok(Self { address, length })
    }

    /// Returns the mapped bytes while the mapping is live.
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.address.as_ptr(), self.length) }
    }
}

impl Drop for ReadOnlyMapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.address.as_ptr().cast(), self.length) };
    }
}

/// One non-blocking `sendfile(2)` of up to `count` bytes from `file` at
/// `*offset` to `socket`, advancing `*offset` by the bytes sent. A full socket
/// buffer surfaces as [`io::ErrorKind::WouldBlock`] so the caller can await
/// writability and retry.
pub fn sendfile(
    socket: BorrowedFd<'_>,
    file: &File,
    offset: &mut u64,
    count: usize,
) -> io::Result<usize> {
    let mut native: libc::off_t = (*offset)
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset exceeds off_t"))?;
    let result = nix::sys::sendfile::sendfile(socket, file, Some(&mut native), count);
    // The kernel advances the offset even on a partial send.
    *offset = native as u64;
    match result {
        Ok(sent) => Ok(sent),
        Err(Errno::EAGAIN) => Err(io::ErrorKind::WouldBlock.into()),
        Err(error) => Err(error.into()),
    }
}

/// One `SOL_TLS` control message selecting the record type of a `sendmsg`.
/// The C layout pads it to `CMSG_SPACE(1)`.
#[repr(C)]
struct RecordTypeMessage {
    header: libc::cmsghdr,
    record_type: u8,
}

/// TLS `alert` content type.
const TLS_RECORD_TYPE_ALERT: u8 = 21;
/// A warning-level `close_notify` alert body.
const CLOSE_NOTIFY_ALERT: [u8; 2] = [1, 0];

/// Queues a TLS `close_notify` alert on a kernel TLS socket without blocking.
/// A full socket buffer surfaces as [`io::ErrorKind::WouldBlock`] so the
/// caller can await writability and retry, and a broken connection returns an
/// error rather than raising `SIGPIPE`.
///
/// [!] Success means kernel TLS accepted the alert record, not that it or the
/// records before it reached TCP. Kernel TLS can hold encrypted records that
/// neither TCP accounting nor writability report, and `shutdown(SHUT_WR)` or
/// `close` discards them, so the caller must keep the socket open until the
/// peer has read everything. Without kernel TLS transmit configured, the kernel
/// ignores the record type and sends the two alert bytes as plain data.
pub fn send_tls_close_notify(socket: BorrowedFd<'_>) -> io::Result<()> {
    let mut alert = CLOSE_NOTIFY_ALERT;
    let mut payload = libc::iovec {
        iov_base: alert.as_mut_ptr().cast(),
        iov_len: alert.len(),
    };
    let mut control = RecordTypeMessage {
        header: libc::cmsghdr {
            cmsg_len: std::mem::offset_of!(RecordTypeMessage, record_type) + 1,
            cmsg_level: libc::SOL_TLS,
            cmsg_type: libc::TLS_SET_RECORD_TYPE,
        },
        record_type: TLS_RECORD_TYPE_ALERT,
    };
    let message = libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: &mut payload,
        msg_iovlen: 1,
        msg_control: (&raw mut control).cast(),
        msg_controllen: std::mem::size_of::<RecordTypeMessage>(),
        msg_flags: 0,
    };
    // SAFETY: the message, its single payload vector, and its control buffer
    // all outlive the call, and the kernel only reads through them.
    let sent = unsafe {
        libc::sendmsg(
            socket.as_raw_fd(),
            &message,
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        )
    };
    // `EAGAIN` already decodes as `WouldBlock`.
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// How far the peer of a connected TCP socket has acknowledged what TCP queued
/// for it, sampled from `TCP_INFO`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpSendProgress {
    /// Bytes the peer acknowledged over the connection's life. Only new data
    /// advances it, so zero-window probes leave it unchanged.
    pub acked: u64,
    /// Whether a byte or a FIN still waits in TCP for transmission or
    /// acknowledgment.
    pub pending: bool,
}

/// The leading fields of the kernel's `struct tcp_info` through
/// `tcpi_notsent_bytes`, which Linux 4.6 added. glibc and the `libc` crate stop
/// before `tcpi_bytes_acked`, and the kernel copies only the prefix a caller
/// asks for.
#[repr(C)]
#[derive(Default)]
pub(crate) struct TcpInfo {
    /// `tcpi_state` through the byte holding the delivery-rate bit fields.
    pub(crate) flags: [u8; 8],
    /// `tcpi_rto` through `tcpi_total_retrans`, in declaration order.
    pub(crate) counters: [u32; 24],
    pub(crate) pacing_rate: u64,
    pub(crate) max_pacing_rate: u64,
    pub(crate) bytes_acked: u64,
    pub(crate) bytes_received: u64,
    pub(crate) segs_out: u32,
    pub(crate) segs_in: u32,
    pub(crate) notsent_bytes: u32,
}

/// `tcpi_unacked` within [`TcpInfo::counters`], the segments in flight.
pub(crate) const TCPI_UNACKED: usize = 4;

/// Samples how far the peer acknowledged the socket's sent bytes.
///
/// [!] The sample covers TCP only. Kernel TLS can hold encrypted records above
/// TCP that no counter here reports, so an empty TCP queue proves nothing about
/// them at the instant of the sample.
pub fn tcp_send_progress(socket: BorrowedFd<'_>) -> io::Result<TcpSendProgress> {
    let mut info = TcpInfo::default();
    let mut length = std::mem::size_of::<TcpInfo>() as libc::socklen_t;
    // SAFETY: `info` is plain data that outlives the call, and `length` bounds
    // how many bytes the kernel writes into it.
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            (&raw mut info).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let required = std::mem::offset_of!(TcpInfo, notsent_bytes) + std::mem::size_of::<u32>();
    if (length as usize) < required {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "TCP_INFO lacks tcpi_notsent_bytes",
        ));
    }
    Ok(TcpSendProgress {
        acked: info.bytes_acked,
        pending: info.notsent_bytes != 0 || info.counters[TCPI_UNACKED] != 0,
    })
}
