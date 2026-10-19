// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Proxy-initiated client closes after a written response.
//!
//! Linux software kernel TLS can report a body's final records as written while
//! they still wait in its own transmit queue, which TCP accounting,
//! writability, and `SIOCOUTQ` cannot see. `close` frees those records and
//! `SHUT_WR` queues the FIN ahead of them, so the client reads an end of stream
//! without `close_notify`, short of `Content-Length`. A graceful kernel TLS
//! close therefore queues `close_notify` behind the body and never shuts down
//! the write side. Every graceful close then lingers, reading and discarding
//! client bytes until the client ends the connection or has acknowledged
//! everything, because closing over unread client bytes sends RST, which drops
//! whatever TCP still queues for the client. The owning model is
//! `skills/ovcc-connection-close/ConnectionClose.tla`.

use std::time::Duration;

use ovcc_telemetry::metrics::DownstreamCloseResult;
use tokio::net::TcpStream;

use super::{ClientStream, DOWNSTREAM_WRITE_STALL_TIMEOUT};

/// Bytes one discarding read takes, a maximal TLS record. The buffer lives on
/// the stack between awaits, so a close allocates none.
const DISCARD_BYTES: usize = 16 * 1024;
/// Reads that discard what already arrived before a close.
const DISCARD_READY_READS: usize = 64;

/// Bounds that end a lingering close.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
struct LingerBounds {
    /// Resets a client that acknowledges nothing for this long.
    stall: Duration,
    /// Closes once TCP has held nothing for the client this long.
    quiet: Duration,
    /// Spacing of the send-queue samples.
    poll: Duration,
}

/// Production bounds. A lingering client gets the inactivity bound a response
/// write gets, so a slow reader is never reset while it still acknowledges.
/// The quiet period leaves the kernel TLS transmit worker, which runs once an
/// acknowledgment frees TCP space, far longer than it needs to push the records
/// TCP accounting cannot see, so a TCP queue that stays empty that long means
/// kernel TLS holds nothing either.
#[cfg(target_os = "linux")]
const LINGER: LingerBounds = LingerBounds {
    stall: DOWNSTREAM_WRITE_STALL_TIMEOUT,
    quiet: Duration::from_secs(1),
    poll: Duration::from_millis(250),
};

/// Windows reports no send-queue progress to the linger, so it ends once the
/// client has sent nothing for this long, within the write stall bound overall.
/// Windows TLS has no kernel record queue, so that close stays graceful and
/// TCP keeps delivering what it queued.
#[cfg(windows)]
const WINDOWS_LINGER_QUIET: Duration = Duration::from_secs(1);

/// How a proxy-initiated close ends a client connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CloseMode {
    /// Delivers every written byte before the socket closes.
    Graceful,
    /// Discards unsent bytes with an RST, after a downstream write timeout or a
    /// failed body whose head carried no framing that could show the failure.
    Reset,
}

/// Closes a client connection after its response and records how the close
/// ended. Every outcome is an expected connection event, so none fails the
/// connection task. Process shutdown does not cut a lingering close short,
/// since `CONNECTION_SHUTDOWN_GRACE` already bounds it with the connection.
pub(super) async fn close_client(stream: ClientStream, mode: CloseMode) {
    let started = std::time::Instant::now();
    let result = match mode {
        CloseMode::Reset => {
            stream.reset_on_drop();
            DownstreamCloseResult::Reset
        }
        CloseMode::Graceful => graceful_close(stream).await,
    };
    ovcc_telemetry::metrics::record_downstream_close(result);
    tracing::debug!(
        event = "downstream_close",
        result = result.as_str(),
        close_seconds = started.elapsed().as_secs_f64(),
    );
}

async fn graceful_close(stream: ClientStream) -> DownstreamCloseResult {
    match stream {
        // Kernel TLS stays configured on the socket. Request bytes rustls
        // drained during the handshake were consumed by earlier requests or no
        // longer matter once the response is complete.
        #[cfg(target_os = "linux")]
        ClientStream::Tls(tls) => close_kernel_tls(tls.into_raw().1, LINGER).await,
        #[cfg(windows)]
        ClientStream::Tls(tls) => close_rustls(tls).await,
        ClientStream::Plain(tcp) => close_plain(tcp).await,
        ClientStream::Local(_) => DownstreamCloseResult::Closed,
    }
}

/// Half-closes a plaintext socket, which delivers every written byte ahead of
/// its FIN, then lingers so unread client bytes cannot turn the close into RST.
#[cfg(target_os = "linux")]
async fn close_plain(tcp: TcpStream) -> DownstreamCloseResult {
    close_plain_with(tcp, LINGER).await
}

#[cfg(target_os = "linux")]
async fn close_plain_with(mut tcp: TcpStream, bounds: LingerBounds) -> DownstreamCloseResult {
    if tokio::io::AsyncWriteExt::shutdown(&mut tcp).await.is_err() {
        return DownstreamCloseResult::Error;
    }
    let mut clock = SendClock::new(bounds, tokio::time::Instant::now());
    linger(&tcp, &mut clock, bounds).await
}

#[cfg(windows)]
async fn close_plain(mut tcp: TcpStream) -> DownstreamCloseResult {
    if tokio::io::AsyncWriteExt::shutdown(&mut tcp).await.is_err() {
        return DownstreamCloseResult::Error;
    }
    linger_blind(&tcp).await
}

/// Queues `close_notify` behind the response on a kernel TLS socket, then
/// lingers until the client ends the connection or has acknowledged
/// everything.
#[cfg(target_os = "linux")]
async fn close_kernel_tls(tcp: TcpStream, bounds: LingerBounds) -> DownstreamCloseResult {
    let mut clock = SendClock::new(bounds, tokio::time::Instant::now());
    if let Err(ended) = queue_close_notify(&tcp, &mut clock, bounds).await {
        return ended;
    }
    // Only an empty queue observed after close_notify counts as quiet.
    clock.restart_quiet();
    linger(&tcp, &mut clock, bounds).await
}

/// Queues `close_notify` without `SHUT_WR`, waiting for writability while the
/// send buffer is full. The wait discards client bytes, so a client that sends
/// before it reads cannot deadlock the close, and it ends early with the close
/// result when the client ends the connection or stops acknowledging.
#[cfg(target_os = "linux")]
async fn queue_close_notify(
    tcp: &TcpStream,
    clock: &mut SendClock,
    bounds: LingerBounds,
) -> Result<(), DownstreamCloseResult> {
    match send_close_notify(tcp) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(_) => return Err(DownstreamCloseResult::Error),
    }
    let mut samples = sample_interval(bounds.poll);
    loop {
        tokio::select! {
            // A raw `EAGAIN` leaves Tokio's write readiness set, so the retry
            // goes through `async_io`, which clears it before waiting.
            sent = tcp.async_io(tokio::io::Interest::WRITABLE, || send_close_notify(tcp)) => {
                return sent.map_err(|_| DownstreamCloseResult::Error);
            }
            ended = discard(tcp) => {
                if ended {
                    return Err(DownstreamCloseResult::PeerClosed);
                }
            }
            _ = samples.tick() => {
                let now = tokio::time::Instant::now();
                if clock.observe(sample(tcp), now) == Some(LingerEnd::Stalled) {
                    let _ = tcp.set_zero_linger();
                    return Err(DownstreamCloseResult::AlertTimeout);
                }
            }
        }
    }
}

/// One non-blocking `close_notify` attempt. A signal can interrupt it, because
/// the kernel takes the TLS transmit lock interruptibly while its transmit
/// worker pushes queued records, so that case retries at once.
#[cfg(target_os = "linux")]
fn send_close_notify(tcp: &TcpStream) -> std::io::Result<()> {
    use std::os::fd::AsFd as _;
    loop {
        match ovcc_platform::send_tls_close_notify(tcp.as_fd()) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            sent => return sent,
        }
    }
}

/// Discards client bytes until the client ends the connection, TCP has held
/// nothing for it through the quiet period, or it acknowledged nothing through
/// the stall bound. Only the stall resets, because a quiet socket already
/// delivered every byte and its FIN cannot overtake them.
#[cfg(target_os = "linux")]
async fn linger(
    tcp: &TcpStream,
    clock: &mut SendClock,
    bounds: LingerBounds,
) -> DownstreamCloseResult {
    let mut samples = sample_interval(bounds.poll);
    loop {
        tokio::select! {
            ended = discard(tcp) => {
                if ended {
                    return DownstreamCloseResult::PeerClosed;
                }
            }
            _ = samples.tick() => match clock.observe(sample(tcp), tokio::time::Instant::now()) {
                Some(LingerEnd::Quiet) => {
                    discard_ready(tcp);
                    return DownstreamCloseResult::Closed;
                }
                Some(LingerEnd::Stalled) => {
                    let _ = tcp.set_zero_linger();
                    return DownstreamCloseResult::LingerTimeout;
                }
                None => {}
            },
        }
    }
}

#[cfg(target_os = "linux")]
fn sample_interval(poll: Duration) -> tokio::time::Interval {
    let mut samples = tokio::time::interval(poll);
    samples.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    samples
}

#[cfg(target_os = "linux")]
fn sample(tcp: &TcpStream) -> Option<ovcc_platform::TcpSendProgress> {
    use std::os::fd::AsFd as _;
    ovcc_platform::tcp_send_progress(tcp.as_fd()).ok()
}

/// Why a lingering close stops waiting for the client.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LingerEnd {
    /// TCP held nothing for the client through the quiet period.
    Quiet,
    /// The client acknowledged nothing through the stall bound.
    Stalled,
}

/// Folds send-queue samples into the linger's bounds.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct SendClock {
    bounds: LingerBounds,
    acked: Option<u64>,
    progressed: tokio::time::Instant,
    quiet_since: Option<tokio::time::Instant>,
}

#[cfg(target_os = "linux")]
impl SendClock {
    fn new(bounds: LingerBounds, now: tokio::time::Instant) -> Self {
        Self {
            bounds,
            acked: None,
            progressed: now,
            quiet_since: None,
        }
    }

    fn restart_quiet(&mut self) {
        self.quiet_since = None;
    }

    /// Records one sample, `None` when the kernel reported none, and returns
    /// the bound it completes. An acknowledgment restarts the stall bound, and
    /// any byte waiting in TCP restarts the quiet period.
    fn observe(
        &mut self,
        sample: Option<ovcc_platform::TcpSendProgress>,
        now: tokio::time::Instant,
    ) -> Option<LingerEnd> {
        match sample {
            Some(progress) => {
                let advanced = self.acked.is_some_and(|acked| acked != progress.acked);
                if advanced {
                    self.progressed = now;
                }
                self.acked = Some(progress.acked);
                self.quiet_since = match (progress.pending, advanced) {
                    (true, _) => None,
                    (false, true) => Some(now),
                    (false, false) => Some(self.quiet_since.unwrap_or(now)),
                };
            }
            None => self.quiet_since = None,
        }
        if self
            .quiet_since
            .is_some_and(|since| now.duration_since(since) >= self.bounds.quiet)
        {
            return Some(LingerEnd::Quiet);
        }
        (now.duration_since(self.progressed) >= self.bounds.stall).then_some(LingerEnd::Stalled)
    }
}

/// Flushes rustls' buffered records and `close_notify` into the socket under
/// the write stall bound, then lingers on the raw socket, which delivers them
/// like plaintext.
#[cfg(windows)]
async fn close_rustls(mut tls: super::TlsClientStream) -> DownstreamCloseResult {
    let flushed = tokio::time::timeout(
        DOWNSTREAM_WRITE_STALL_TIMEOUT,
        tokio::io::AsyncWriteExt::shutdown(&mut tls),
    )
    .await;
    match flushed {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return DownstreamCloseResult::Error,
        Err(_) => {
            let _ = tls.get_ref().0.set_zero_linger();
            return DownstreamCloseResult::AlertTimeout;
        }
    }
    let (tcp, _) = (*tls).into_inner();
    linger_blind(&tcp).await
}

/// Discards client bytes until the client ends the connection or stops
/// sending, then closes gracefully. Without send-queue samples the linger
/// cannot tell a slow reader from a stalled one, and a graceful close leaves
/// TCP delivering what it queued either way.
#[cfg(windows)]
async fn linger_blind(tcp: &TcpStream) -> DownstreamCloseResult {
    let quiet = tokio::time::sleep(WINDOWS_LINGER_QUIET);
    let cap = tokio::time::sleep(DOWNSTREAM_WRITE_STALL_TIMEOUT);
    tokio::pin!(quiet, cap);
    loop {
        tokio::select! {
            ended = discard(tcp) => {
                if ended {
                    return DownstreamCloseResult::PeerClosed;
                }
                quiet
                    .as_mut()
                    .reset(tokio::time::Instant::now() + WINDOWS_LINGER_QUIET);
            }
            () = &mut quiet => break,
            () = &mut cap => break,
        }
    }
    discard_ready(tcp);
    DownstreamCloseResult::Closed
}

/// Waits for client bytes and discards them. Returns whether the client ended
/// its side of the connection by end of stream, a reset, or, on kernel TLS, an
/// alert record that a plain read surfaces as `EIO`. A client that half-closes
/// while it still reads looks the same, which the close model leaves out.
async fn discard(tcp: &TcpStream) -> bool {
    if tcp.readable().await.is_err() {
        return true;
    }
    let mut buffer = [0_u8; DISCARD_BYTES];
    match tcp.try_read(&mut buffer) {
        Ok(read) => read == 0,
        Err(error) => !matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ),
    }
}

/// Discards the client bytes already received, so the close that follows
/// sends FIN rather than RST.
fn discard_ready(tcp: &TcpStream) {
    let mut buffer = [0_u8; DISCARD_BYTES];
    for _ in 0..DISCARD_READY_READS {
        if !matches!(tcp.try_read(&mut buffer), Ok(read) if read > 0) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt as _;
    use tokio::net::TcpListener;

    async fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(address), listener.accept());
        (accepted.unwrap().0, client.unwrap())
    }

    #[tokio::test]
    async fn general_close_plaintext_graceful_ends_with_eof() {
        // Arrange.
        let (server, mut client) = pair().await;
        let reader = async move {
            let read = client.read(&mut [0_u8; 1]).await.unwrap();
            drop(client);
            read
        };

        // Act.
        let ((), read) = tokio::join!(
            close_client(ClientStream::Plain(server), CloseMode::Graceful),
            reader
        );

        // Assert.
        assert_eq!(read, 0);
    }

    #[tokio::test]
    async fn general_close_reset_discards_the_connection_with_rst() {
        // Arrange.
        let (server, mut client) = pair().await;

        // Act.
        close_client(ClientStream::Plain(server), CloseMode::Reset).await;

        // Assert.
        let error = client.read(&mut [0_u8; 1]).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
    }

    // A plain socket stands in for kernel TLS below. Without the TLS upper
    // layer the kernel delivers the alert's two bytes as data, which leaves the
    // alert, linger, and reset sequencing under test.
    #[cfg(target_os = "linux")]
    mod linger {
        use super::*;
        use std::time::Instant;
        use tokio::io::AsyncWriteExt as _;

        const GUARD: Duration = Duration::from_secs(5);

        fn bounds(stall: Duration, quiet: Duration) -> LingerBounds {
            LingerBounds {
                stall,
                quiet,
                poll: Duration::from_millis(10),
            }
        }

        fn progress(acked: u64, pending: bool) -> Option<ovcc_platform::TcpSendProgress> {
            Some(ovcc_platform::TcpSendProgress { acked, pending })
        }

        /// Fills the server's send path toward a client that does not read, until
        /// a send still blocks after a pause longer than the client's delayed ACK,
        /// and returns the bytes queued.
        async fn fill(server: &TcpStream, client: &TcpStream) -> usize {
            socket2::SockRef::from(server)
                .set_send_buffer_size(32 * 1024)
                .unwrap();
            socket2::SockRef::from(client)
                .set_recv_buffer_size(4096)
                .unwrap();
            let socket = socket2::SockRef::from(server);
            let chunk = [0_u8; 4096];
            let mut queued = 0;
            loop {
                while let Ok(sent) = socket.send(&chunk) {
                    queued += sent;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                match socket.send(&chunk) {
                    Ok(sent) => queued += sent,
                    Err(_) => return queued,
                }
            }
        }

        #[test]
        fn general_close_send_clock_closes_after_a_quiet_period() {
            // Arrange.
            let start = tokio::time::Instant::now();
            let quiet = Duration::from_millis(100);
            let mut clock = SendClock::new(bounds(GUARD, quiet), start);

            // Act.
            let ends = [0, 50, 99, 100].map(|elapsed| {
                clock.observe(progress(7, false), start + Duration::from_millis(elapsed))
            });

            // Assert.
            assert_eq!(ends, [None, None, None, Some(LingerEnd::Quiet)]);
        }

        #[test]
        fn general_close_send_clock_restarts_quiet_on_pending_bytes_or_missing_samples() {
            // Arrange.
            let start = tokio::time::Instant::now();
            let quiet = Duration::from_millis(100);
            let mut clock = SendClock::new(bounds(GUARD, quiet), start);
            let at = |elapsed| start + Duration::from_millis(elapsed);

            // Act.
            let ends = [
                clock.observe(progress(7, false), at(0)),
                clock.observe(progress(7, true), at(60)),
                clock.observe(progress(7, false), at(120)),
                clock.observe(None, at(180)),
                clock.observe(progress(7, false), at(240)),
                clock.observe(progress(7, false), at(339)),
                clock.observe(progress(7, false), at(340)),
            ];

            // Assert.
            assert_eq!(ends[..6], [None; 6]);
            assert_eq!(ends[6], Some(LingerEnd::Quiet));
        }

        #[test]
        fn general_close_send_clock_resets_only_without_acknowledgments() {
            // Arrange.
            let start = tokio::time::Instant::now();
            let stall = Duration::from_millis(100);
            let mut reading = SendClock::new(bounds(stall, GUARD), start);
            let mut stalled = SendClock::new(bounds(stall, GUARD), start);
            let at = |elapsed| start + Duration::from_millis(elapsed);

            // Act. One client acknowledges every 90 ms for far longer than the
            // stall bound, the other never acknowledges again.
            let reading_ends = (0..20)
                .map(|step| reading.observe(progress(step, true), at(step * 90)))
                .collect::<Vec<_>>();
            let stalled_ends =
                [0, 99, 100].map(|elapsed| stalled.observe(progress(3, true), at(elapsed)));

            // Assert.
            assert!(reading_ends.iter().all(Option::is_none), "{reading_ends:?}");
            assert_eq!(stalled_ends, [None, None, Some(LingerEnd::Stalled)]);
        }

        #[tokio::test]
        async fn general_close_linger_ends_when_the_client_closes() {
            // Arrange.
            let (server, mut client) = pair().await;
            let reader = tokio::spawn(async move {
                let mut alert = [0_u8; 2];
                client.read_exact(&mut alert).await.unwrap();
                alert
            });

            // Act.
            let result = close_kernel_tls(server, bounds(GUARD, GUARD)).await;

            // Assert.
            assert_eq!(result, DownstreamCloseResult::PeerClosed);
            assert_eq!(reader.await.unwrap(), [1, 0]);
        }

        #[tokio::test]
        async fn general_close_linger_closes_a_client_that_keeps_the_connection_open() {
            // Arrange.
            let (server, mut client) = pair().await;
            let reader = tokio::spawn(async move {
                let mut alert = [0_u8; 2];
                client.read_exact(&mut alert).await.unwrap();
                let after = client.read(&mut [0_u8; 1]).await;
                (alert, after.map_err(|error| error.kind()))
            });
            let quiet = Duration::from_millis(100);

            // Act.
            let started = Instant::now();
            let result = close_kernel_tls(server, bounds(GUARD, quiet)).await;

            // Assert. The client holds every byte, so the close ends with a FIN
            // rather than a reset.
            assert_eq!(result, DownstreamCloseResult::Closed);
            assert!(started.elapsed() >= quiet);
            let (alert, after) = reader.await.unwrap();
            assert_eq!(alert, [1, 0]);
            assert_eq!(after, Ok(0));
        }

        #[tokio::test]
        async fn general_close_linger_discards_a_late_upload_and_ends_with_fin() {
            // Arrange.
            let (server, mut client) = pair().await;
            let uploader = tokio::spawn(async move {
                let mut alert = [0_u8; 2];
                client.read_exact(&mut alert).await.unwrap();
                client.write_all(&[9_u8; 64 * 1024]).await.unwrap();
                client.shutdown().await.unwrap();
                let mut rest = Vec::new();
                let end = client.read_to_end(&mut rest).await;
                (alert, rest, end.map_err(|error| error.kind()))
            });

            // Act.
            let result = close_kernel_tls(server, bounds(GUARD, GUARD)).await;

            // Assert. The linger read the upload, so the close found no unread
            // byte and sent FIN rather than RST.
            assert_eq!(result, DownstreamCloseResult::PeerClosed);
            let (alert, rest, end) = uploader.await.unwrap();
            assert_eq!(alert, [1, 0]);
            assert!(rest.is_empty());
            assert_eq!(end, Ok(0));
        }

        #[tokio::test]
        async fn general_close_linger_keeps_a_client_that_still_reads() {
            // Arrange. The client drains the queued bytes in small reads for
            // several stall bounds, acknowledging well within each one.
            let (server, mut client) = pair().await;
            let queued = fill(&server, &client).await;
            let stall = Duration::from_millis(300);
            let reader = tokio::spawn(async move {
                let mut received = Vec::with_capacity(queued + 2);
                let mut chunk = [0_u8; 2048];
                while received.len() < queued + 2 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    let wanted = chunk.len().min(queued + 2 - received.len());
                    let read = client.read(&mut chunk[..wanted]).await.unwrap();
                    assert!(read > 0, "the close ended the stream early");
                    received.extend_from_slice(&chunk[..read]);
                }
                received
            });

            // Act.
            let started = Instant::now();
            let result = close_kernel_tls(server, bounds(stall, GUARD)).await;

            // Assert.
            assert_eq!(result, DownstreamCloseResult::PeerClosed);
            assert!(
                started.elapsed() > stall,
                "the drain finished within one stall bound, so the test proves nothing"
            );
            let received = reader.await.unwrap();
            assert_eq!(received.len(), queued + 2);
            assert_eq!(received[queued..], [1, 0]);
        }

        #[tokio::test]
        async fn general_close_linger_resets_a_client_that_stops_acknowledging() {
            // Arrange.
            let (server, mut client) = pair().await;
            fill(&server, &client).await;
            let stall = Duration::from_millis(100);
            let mut clock = SendClock::new(bounds(stall, GUARD), tokio::time::Instant::now());

            // Act.
            let started = Instant::now();
            let result = linger(&server, &mut clock, bounds(stall, GUARD)).await;
            drop(server);

            // Assert.
            assert_eq!(result, DownstreamCloseResult::LingerTimeout);
            assert!(started.elapsed() >= stall);
            let mut drained = Vec::new();
            let error = client.read_to_end(&mut drained).await.unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        }

        #[tokio::test]
        async fn general_close_alert_resets_a_client_that_stopped_reading() {
            // Arrange.
            let (server, mut client) = pair().await;
            fill(&server, &client).await;
            let stall = Duration::from_millis(100);

            // Act.
            let started = Instant::now();
            let result = close_kernel_tls(server, bounds(stall, GUARD)).await;

            // Assert.
            assert_eq!(result, DownstreamCloseResult::AlertTimeout);
            assert!(started.elapsed() >= stall);
            let mut drained = Vec::new();
            let error = client.read_to_end(&mut drained).await.unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        }

        #[tokio::test]
        async fn general_close_alert_reports_a_client_that_already_reset() {
            // Arrange.
            let (server, client) = pair().await;
            client.set_zero_linger().unwrap();
            drop(client);
            server.readable().await.unwrap();

            // Act.
            let result = close_kernel_tls(server, bounds(GUARD, GUARD)).await;

            // Assert.
            assert_eq!(result, DownstreamCloseResult::Error);
        }

        #[tokio::test]
        async fn general_close_plaintext_drains_a_late_request_body() {
            // Arrange.
            let (server, mut client) = pair().await;
            let uploader = tokio::spawn(async move {
                let first = client.read(&mut [0_u8; 1]).await.unwrap();
                client.write_all(b"late request body").await.unwrap();
                client.shutdown().await.unwrap();
                let end = client.read(&mut [0_u8; 1]).await;
                (first, end.map_err(|error| error.kind()))
            });

            // Act.
            let result = close_plain_with(server, bounds(GUARD, GUARD)).await;

            // Assert.
            assert_eq!(result, DownstreamCloseResult::PeerClosed);
            assert_eq!(uploader.await.unwrap(), (0, Ok(0)));
        }
    }
}
