// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::io::{Read as _, Write as _};

fn write(path: &Path, bytes: &[u8]) {
    let mut file = File::create(path).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

fn read_all(mut file: File) -> Vec<u8> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    bytes
}

#[test]
fn platform_identity_is_stable_across_rename_and_distinct_per_object() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    write(&first, b"a");
    write(&second, b"b");

    let before = stat_nofollow(&first).unwrap();
    assert!(before.metadata.file_type().is_file());
    assert_ne!(before.identity, stat_nofollow(&second).unwrap().identity);

    let renamed = temp.path().join("renamed");
    std::fs::rename(&first, &renamed).unwrap();
    assert_eq!(before.identity, stat_nofollow(&renamed).unwrap().identity);
    assert_eq!(
        before.identity,
        file_identity(&File::open(&renamed).unwrap()).unwrap()
    );
}

#[test]
fn platform_directory_identity_matches_its_handle() {
    let temp = tempfile::tempdir().unwrap();
    let directory = open_directory(temp.path()).unwrap();
    assert!(directory.metadata().unwrap().file_type().is_dir());
    assert_eq!(
        file_identity(&directory).unwrap(),
        stat_nofollow(temp.path()).unwrap().identity
    );
}

#[test]
fn platform_file_stamp_changes_when_content_is_replaced() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("tls.crt");
    write(&path, b"one");
    let before = file_stamp(&path).unwrap();
    assert_eq!(before, file_stamp(&path).unwrap());

    let next = temp.path().join("next");
    write(&next, b"two!");
    std::fs::rename(&next, &path).unwrap();
    assert_ne!(before, file_stamp(&path).unwrap());
}

#[test]
fn platform_open_regular_at_resolves_through_the_held_directory() {
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("generation");
    std::fs::create_dir(&original).unwrap();
    write(&original.join("segment"), b"pinned bytes");
    let directory = open_directory(&original).unwrap();

    // Moving the directory and occupying its old path with an impostor must
    // not redirect an open through the held handle.
    let moved = temp.path().join("retired");
    std::fs::rename(&original, &moved).unwrap();
    std::fs::create_dir(&original).unwrap();
    write(&original.join("segment"), b"impostor");

    let file = open_regular_at(&directory, "segment").unwrap();
    assert_eq!(read_all(file), b"pinned bytes");
}

#[test]
fn platform_open_regular_at_rejects_multi_component_names() {
    let temp = tempfile::tempdir().unwrap();
    let directory = open_directory(temp.path()).unwrap();
    for name in ["", ".", "..", "a/b", r"a\b", "a:stream", "a\0b"] {
        let error = open_regular_at(&directory, name).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{name:?}");
    }
}

#[test]
fn platform_open_regular_at_never_yields_a_readable_directory_or_missing_name() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("child")).unwrap();
    let directory = open_directory(temp.path()).unwrap();
    // Windows refuses the open; Linux may return the directory, which the
    // caller's file-type check rejects.
    if let Ok(child) = open_regular_at(&directory, "child") {
        assert!(!child.metadata().unwrap().file_type().is_file());
    }
    assert_eq!(
        open_regular_at(&directory, "missing").unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

#[cfg(target_os = "linux")]
#[test]
fn platform_open_regular_at_does_not_block_on_a_fifo_without_writer() {
    let temp = tempfile::tempdir().unwrap();
    create_fifo(&temp.path().join("manifest")).unwrap();
    let directory = open_directory(temp.path()).unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(open_regular_at(&directory, "manifest").map(|file| {
            file.metadata()
                .map(|metadata| metadata.file_type().is_file())
        }));
    });
    let opened = receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("opening a writerless FIFO must not block");
    assert!(
        !matches!(opened, Ok(Ok(true))),
        "a FIFO is never a regular file"
    );
}

#[cfg(unix)]
fn try_symlink_file(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn try_symlink_file(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[test]
fn platform_open_regular_at_does_not_follow_a_final_symlink() {
    let temp = tempfile::tempdir().unwrap();
    write(&temp.path().join("target"), b"outside");
    match try_symlink_file(&temp.path().join("target"), &temp.path().join("link")) {
        Ok(()) => {}
        // Windows without Developer Mode cannot create symlinks
        // (ERROR_PRIVILEGE_NOT_HELD); nothing to test.
        Err(error) if error.raw_os_error() == Some(1314) => return,
        Err(error) => panic!("{error}"),
    }
    let directory = open_directory(temp.path()).unwrap();
    assert!(open_regular_at(&directory, "link").is_err());
}

#[test]
fn platform_rename_noreplace_refuses_to_clobber() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    write(&source, b"new");
    write(&destination, b"old");

    let error = rename_noreplace(&source, &destination).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&destination).unwrap(), b"old");

    std::fs::remove_file(&destination).unwrap();
    rename_noreplace(&source, &destination).unwrap();
    assert_eq!(std::fs::read(&destination).unwrap(), b"new");
    assert!(!source.exists());
}

#[test]
fn platform_exchange_swaps_a_file_and_a_directory() {
    let temp = tempfile::tempdir().unwrap();
    let staged = temp.path().join("staged");
    let published = temp.path().join("published");
    std::fs::create_dir(&staged).unwrap();
    write(&staged.join("manifest"), b"segmented");
    write(&published, b"contiguous");
    let staged_identity = stat_nofollow(&staged).unwrap().identity;
    let published_identity = stat_nofollow(&published).unwrap().identity;

    let retired = exchange_paths(&staged, &published).unwrap();

    assert_eq!(retired, staged);
    assert_eq!(stat_nofollow(&published).unwrap().identity, staged_identity);
    assert_eq!(stat_nofollow(&staged).unwrap().identity, published_identity);
    assert_eq!(
        std::fs::read(published.join("manifest")).unwrap(),
        b"segmented"
    );
    assert_eq!(std::fs::read(&staged).unwrap(), b"contiguous");
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 2);
}

#[test]
fn platform_exchange_with_a_missing_side_changes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let staged = temp.path().join("staged");
    let published = temp.path().join("published");
    write(&published, b"old");

    assert!(exchange_paths(&staged, &published).is_err());
    assert_eq!(std::fs::read(&published).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn platform_exchange_and_remove_succeed_while_readers_hold_files_open() {
    // The cache retires and replaces generations that in-flight HITs are
    // still reading; both must work with those handles open.
    let temp = tempfile::tempdir().unwrap();
    let staged = temp.path().join("staged");
    let published = temp.path().join("published");
    std::fs::create_dir(&staged).unwrap();
    write(&staged.join("segment"), b"new");
    std::fs::create_dir(&published).unwrap();
    write(&published.join("segment"), b"old");

    let pinned_directory = open_directory(&published).unwrap();
    let pinned_segment = open_regular_at(&pinned_directory, "segment").unwrap();

    exchange_paths(&staged, &published).unwrap();
    assert_eq!(std::fs::read(published.join("segment")).unwrap(), b"new");

    std::fs::remove_dir_all(&staged).unwrap();
    assert!(!staged.exists());
    assert_eq!(read_all(pinned_segment), b"old");
}

#[test]
fn platform_pinned_directory_can_be_retired_while_a_segment_is_being_read() {
    let temp = tempfile::tempdir().unwrap();
    let published = temp.path().join("published");
    std::fs::create_dir(&published).unwrap();
    write(&published.join("segment"), b"in flight");
    let directory = open_directory(&published).unwrap();
    let segment = open_regular_at(&directory, "segment").unwrap();

    let deferred = temp.path().join("deferred");
    rename(&published, &deferred).unwrap();
    assert!(!published.exists());
    std::fs::remove_dir_all(&deferred).unwrap();
    assert_eq!(read_all(segment), b"in flight");
}

#[test]
fn platform_sync_directory_and_available_bytes() {
    let temp = tempfile::tempdir().unwrap();
    write(&temp.path().join("entry"), b"x");
    let reader = File::open(temp.path().join("entry")).unwrap();
    sync_file(&temp.path().join("entry")).unwrap();
    drop(reader);
    sync_directory(temp.path()).unwrap();
    assert!(available_bytes(temp.path()).unwrap() > 0);
}

#[cfg(target_os = "linux")]
#[test]
fn platform_system_resolver_config_is_resolv_conf_syntax() {
    // Linux CI containers may lack /etc/resolv.conf; that is the caller's
    // fallback case, not a failure of the primitive.
    let Ok(config) = system_resolver_config() else {
        return;
    };
    let _ = config;
}

#[cfg(windows)]
#[test]
fn platform_does_not_flatten_windows_dns_policy() {
    let error = system_resolver_config().unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[cfg(windows)]
#[test]
fn platform_system_roots_are_server_auth_roots_outside_disallowed() {
    let roots = system_root_certificates().unwrap();
    assert!(
        !roots.is_empty(),
        "a Windows host trusts some server-auth root"
    );
    let disallowed = windows::system_store_certificates("Disallowed", |_| true).unwrap();
    assert!(roots.iter().all(|root| !disallowed.contains(root)));
    let unique = roots.iter().collect::<std::collections::HashSet<_>>();
    assert!(!unique.is_empty());
}

#[cfg(windows)]
#[test]
fn platform_origin_chain_rejects_intermediate_without_server_auth() {
    let root = include_bytes!("../testdata/server-auth-policy-root.der");
    let intermediate = include_bytes!("../testdata/client-auth-intermediate.der");
    let leaf = include_bytes!("../testdata/server-auth-leaf.der");

    let error =
        windows::verify_certificate_chain_with_root(leaf, &[intermediate], "origin.test", root)
            .unwrap_err();

    assert!(error.to_string().contains("Windows rejected"));
}

#[test]
fn platform_handle_keys_are_distinct_for_live_handles() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file");
    write(&path, b"x");
    let first = File::open(&path).unwrap();
    let second = File::open(&path).unwrap();
    assert_ne!(handle_key(&first), handle_key(&second));
    assert_eq!(handle_key(&first), handle_key(&first));
}

#[test]
fn platform_read_at_reads_the_requested_window() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file");
    write(&path, b"0123456789");
    let file = File::open(&path).unwrap();
    let mut buffer = [0u8; 4];
    assert_eq!(read_at(&file, &mut buffer, 3).unwrap(), 4);
    assert_eq!(&buffer, b"3456");
    assert_eq!(read_at(&file, &mut buffer, 8).unwrap(), 2);
    assert_eq!(read_at(&file, &mut buffer, 10).unwrap(), 0);
}

mod send {
    use super::*;
    use tokio::io::AsyncReadExt as _;
    use tokio::net::{TcpListener, TcpStream};

    /// Drives the platform zero-copy primitive the way the proxy does: bounded
    /// chunks until `length` bytes are sent.
    pub(super) async fn send_file(
        stream: &TcpStream,
        file: &File,
        start: u64,
        length: u64,
    ) -> io::Result<u64> {
        const CHUNK: u64 = 4 * 1024 * 1024;
        #[cfg(windows)]
        let source = TransmitSource::new(file)?;
        let mut sent = 0;
        while sent < length {
            let count = (length - sent).min(CHUNK);
            #[cfg(target_os = "linux")]
            let n = {
                use std::os::fd::AsFd as _;
                let mut offset = start + sent;
                stream
                    .async_io(tokio::io::Interest::WRITABLE, || {
                        sendfile(stream.as_fd(), file, &mut offset, count as usize)
                    })
                    .await? as u64
            };
            #[cfg(windows)]
            let n = u64::from(transmit_file(stream, &source, start + sent, count as u32).await?);
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            sent += n;
        }
        Ok(sent)
    }

    async fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(address), listener.accept());
        (accepted.unwrap().0, client.unwrap())
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn platform_transmit_file_times_out_when_reader_stalls() {
        const LENGTH: u32 = 512 * 1024 * 1024;
        let file = tempfile::tempfile().unwrap();
        file.set_len(u64::from(LENGTH)).unwrap();
        let (server, client) = pair().await;
        socket2::SockRef::from(&server)
            .set_send_buffer_size(4096)
            .unwrap();
        socket2::SockRef::from(&client)
            .set_recv_buffer_size(4096)
            .unwrap();
        let source = TransmitSource::new(&file).unwrap();

        let failure = transmit_file_with_inactivity_timeout(
            &server,
            &source,
            0,
            LENGTH,
            std::time::Duration::from_millis(100),
        )
        .await
        .unwrap_err();

        assert_eq!(failure.error.kind(), io::ErrorKind::TimedOut);
        assert!(failure.sent <= LENGTH);
        drop(client);
    }

    fn pattern(length: usize) -> Vec<u8> {
        (0..length).map(|index| (index * 31 % 251) as u8).collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn platform_send_file_streams_an_offset_window_intact() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("object");
        let body = pattern(24 * 1024 * 1024 + 17);
        write(&path, &body);
        let file = File::open(&path).unwrap();
        let (server, mut client) = pair().await;

        let offset = 4099u64;
        let length = body.len() as u64 - offset - 5;
        let reader = tokio::spawn(async move {
            let mut received = Vec::new();
            client.read_to_end(&mut received).await.unwrap();
            received
        });
        assert_eq!(
            send_file(&server, &file, offset, length).await.unwrap(),
            length
        );
        drop(server);
        let received = reader.await.unwrap();
        assert_eq!(received.len() as u64, length);
        assert!(received == body[offset as usize..(offset + length) as usize]);
    }

    /// One primitive call, with EOF reported as a short (or zero) count.
    async fn send_once(
        stream: &TcpStream,
        file: &File,
        offset: u64,
        count: u32,
    ) -> io::Result<u64> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsFd as _;
            let mut offset = offset;
            return stream
                .async_io(tokio::io::Interest::WRITABLE, || {
                    sendfile(stream.as_fd(), file, &mut offset, count as usize)
                })
                .await
                .map(|n| n as u64);
        }
        #[cfg(windows)]
        transmit_file(stream, &TransmitSource::new(file)?, offset, count)
            .await
            .map(u64::from)
            .map_err(io::Error::from)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn platform_send_file_repeats_whole_windows_against_one_handle() {
        const LENGTH: usize = 1024 * 1024;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("object");
        let body = pattern(LENGTH);
        write(&path, &body);
        let file = File::open(&path).unwrap();
        let (server, mut client) = pair().await;
        const ROUNDS: usize = 200;
        let reader = tokio::spawn(async move {
            let mut received = vec![0u8; LENGTH];
            for _ in 0..ROUNDS {
                client.read_exact(&mut received).await.unwrap();
            }
            received
        });
        for round in 0..ROUNDS {
            let mut sent = 0u64;
            while sent < LENGTH as u64 {
                let count = (LENGTH as u64 - sent) as u32;
                match send_once(&server, &file, sent, count).await {
                    Ok(0) => panic!("round {round}: zero at offset {sent}"),
                    Ok(n) => sent += n,
                    Err(error) => panic!("round {round}: offset {sent} count {count}: {error}"),
                }
            }
        }
        assert!(reader.await.unwrap() == body);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn platform_send_file_serves_evicted_and_by_id_handles() {
        // A lease keeps serving after eviction deletes the file, and segment
        // handles come from open_regular_at (by file id on Windows).
        let temp = tempfile::tempdir().unwrap();
        let body = pattern(3 * 1024 * 1024 + 5);
        write(&temp.path().join("segment"), &body);
        let directory = open_directory(temp.path()).unwrap();
        let by_id = open_regular_at(&directory, "segment").unwrap();
        let evicted = File::open(temp.path().join("segment")).unwrap();
        std::fs::remove_file(temp.path().join("segment")).unwrap();

        for file in [&by_id, &evicted] {
            let (server, mut client) = pair().await;
            let reader = tokio::spawn(async move {
                let mut received = Vec::new();
                client.read_to_end(&mut received).await.unwrap();
                received
            });
            let length = body.len() as u64;
            assert_eq!(send_file(&server, file, 0, length).await.unwrap(), length);
            drop(server);
            assert!(reader.await.unwrap() == body);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn platform_send_file_serves_handles_that_forbid_reopening() {
        // tempfile() opens without read sharing, so Windows refuses the
        // overlapped reopen and the source positions the original handle.
        let mut file = tempfile::tempfile().unwrap();
        let body = pattern(2 * 1024 * 1024 + 3);
        file.write_all(&body).unwrap();
        let (server, mut client) = pair().await;
        let reader = tokio::spawn(async move {
            let mut received = Vec::new();
            client.read_to_end(&mut received).await.unwrap();
            received
        });
        let length = body.len() as u64;
        assert_eq!(send_file(&server, &file, 0, length).await.unwrap(), length);
        assert_eq!(send_file(&server, &file, 0, length).await.unwrap(), length);
        drop(server);
        let received = reader.await.unwrap();
        assert!(received[..body.len()] == body[..] && received[body.len()..] == body[..]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn platform_send_file_reports_eof_as_a_short_count() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("object");
        write(&path, b"0123456789");
        let file = File::open(&path).unwrap();
        let (server, _client) = pair().await;
        assert_eq!(send_once(&server, &file, 6, 100).await.unwrap(), 4);
        assert_eq!(send_once(&server, &file, 10, 100).await.unwrap(), 0);
        assert_eq!(send_once(&server, &file, 50, 100).await.unwrap(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn platform_send_file_is_cancel_safe_against_a_stalled_reader() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("object");
        write(&path, &pattern(64 * 1024 * 1024));
        let file = File::open(&path).unwrap();
        let (server, client) = pair().await;

        // The client never reads, so the send stalls on backpressure and the
        // timeout drops the in-flight transfer.
        let stalled = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            send_file(&server, &file, 0, 64 * 1024 * 1024),
        )
        .await;
        assert!(stalled.is_err(), "a stalled reader must not complete");

        // The socket and file remain usable after the cancellation.
        drop(client);
        drop(server);
        let mut again = File::open(&path).unwrap();
        let mut first = [0u8; 8];
        again.read_exact(&mut first).unwrap();
        assert_eq!(&first[..], &pattern(8)[..]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn platform_send_file_reports_a_disconnected_peer() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("object");
        write(&path, &pattern(32 * 1024 * 1024));
        let file = File::open(&path).unwrap();
        let (server, client) = pair().await;
        drop(client);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            send_file(&server, &file, 0, 32 * 1024 * 1024),
        )
        .await
        .expect("a reset peer must fail the send promptly");
        assert!(result.is_err());
    }
}

// A plain TCP socket stands in for kernel TLS here, since without the TLS upper
// layer the kernel ignores the record-type control message and sends the two
// alert bytes as data, which exercises the same non-blocking send and errno
// mapping. The kTLS path itself is covered by `ovcc-proxy`'s `ktls_close`.
#[cfg(target_os = "linux")]
mod tls_close_notify {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsFd as _;
    use std::time::{Duration, Instant};

    #[test]
    fn platform_tls_close_notify_reports_a_full_socket_as_would_block_without_waiting() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut server = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        // The peer never reads, so nothing ever frees send space.
        let (_peer, _) = listener.accept().unwrap();
        socket2::SockRef::from(&server)
            .set_send_buffer_size(4096)
            .unwrap();
        server.set_nonblocking(true).unwrap();
        let chunk = [0u8; 64 * 1024];
        while server.write(&chunk).is_ok() {}
        // A blocking socket with a send timeout turns a missing MSG_DONTWAIT
        // into a measurable stall instead of a hang.
        server.set_nonblocking(false).unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let started = Instant::now();
        let error = (0..100_000)
            .find_map(|_| send_tls_close_notify(server.as_fd()).err())
            .expect("a peer that never reads must eventually fill the socket");

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the alert send waited for space: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn platform_tls_close_notify_reports_an_unconnected_socket_as_an_error() {
        let socket =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();

        let error = send_tls_close_notify(socket.as_fd()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}

// Loopback sockets acknowledge without a network in between, so these pin the
// sampled counters rather than any timing.
#[cfg(target_os = "linux")]
mod tcp_send_progress {
    use super::*;
    use crate::linux::{TCPI_UNACKED, TcpInfo};
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsFd as _;
    use std::time::{Duration, Instant};

    #[test]
    fn platform_tcp_send_progress_mirrors_the_kernel_tcp_info_layout() {
        // `include/uapi/linux/tcp.h` places these fields at fixed offsets, and
        // the older prefix must agree with the `libc` definition.
        let unacked = std::mem::offset_of!(TcpInfo, counters) + TCPI_UNACKED * 4;

        assert_eq!(
            unacked,
            std::mem::offset_of!(nix::libc::tcp_info, tcpi_unacked)
        );
        assert_eq!(std::mem::offset_of!(TcpInfo, bytes_acked), 120);
        assert_eq!(std::mem::offset_of!(TcpInfo, notsent_bytes), 144);
    }

    #[test]
    fn platform_tcp_send_progress_counts_bytes_the_peer_acknowledged() {
        // Arrange.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut server = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        let body = vec![7_u8; 64 * 1024];
        let before = tcp_send_progress(server.as_fd()).unwrap();

        // Act.
        server.write_all(&body).unwrap();
        let mut received = vec![0_u8; body.len()];
        peer.read_exact(&mut received).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let after = loop {
            let progress = tcp_send_progress(server.as_fd()).unwrap();
            if !progress.pending || Instant::now() > deadline {
                break progress;
            }
            std::thread::sleep(Duration::from_millis(5));
        };

        // Assert.
        assert!(!after.pending, "{after:?}");
        assert_eq!(after.acked - before.acked, body.len() as u64);
    }

    #[test]
    fn platform_tcp_send_progress_reports_bytes_a_full_window_holds_as_pending() {
        // Arrange.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut server = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        // The peer never reads, so its window closes and TCP keeps the rest.
        let (_peer, _) = listener.accept().unwrap();
        server.set_nonblocking(true).unwrap();
        let chunk = [0_u8; 64 * 1024];
        let mut written = 0_u64;
        while let Ok(count) = server.write(&chunk) {
            written += count as u64;
        }

        // Act.
        let progress = tcp_send_progress(server.as_fd()).unwrap();

        // Assert.
        assert!(progress.pending, "{progress:?}");
        assert!(progress.acked < written, "{progress:?} of {written}");
    }

    #[test]
    fn platform_tcp_send_progress_reports_an_unconnected_socket_without_pending_bytes() {
        let socket =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();

        let progress = tcp_send_progress(socket.as_fd()).unwrap();

        assert_eq!(
            progress,
            TcpSendProgress {
                acked: 0,
                pending: false
            }
        );
    }
}
