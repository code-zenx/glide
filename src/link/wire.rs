//! Moving messages over a live connection: the reliable stream, a stream
//! per clipboard item, datagrams, and the pings that time them.

use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use quinn::{Connection, RecvStream, SendStream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info};

use super::{MAX_BULK, MAX_FRAME, PING_INTERVAL, SAMPLE_WINDOW, STATS_INTERVAL};
use crate::proto::{Msg, Route};
use crate::status::Status;

/// Put every outgoing message on the channel it belongs to. Clipboard items go
/// out on streams of their own, spawned, so a large one never delays the key
/// press queued behind it.
pub(super) async fn write_loop(conn: Connection, mut send: SendStream, mut rx: UnboundedReceiver<Msg>) {
    while let Some(msg) = rx.recv().await {
        let bytes = msg.encode();
        let sent = match msg.route() {
            Route::Bulk => {
                send_bulk(conn.clone(), bytes);
                Ok(())
            }
            Route::Datagram => conn.send_datagram(Bytes::from(bytes)).map_err(Into::into),
            Route::Stream => write_frame(&mut send, &bytes).await,
        };
        if let Err(e) = sent {
            debug!("send failed: {e}");
            return;
        }
    }
}

/// One clipboard item, one stream. Dropping the handle after `finish` still
/// sends what is buffered, so this task ends as soon as the bytes are handed
/// to the connection.
fn send_bulk(conn: Connection, bytes: Vec<u8>) {
    tokio::spawn(async move {
        match conn.open_uni().await {
            Ok(mut stream) => {
                if let Err(e) = stream.write_all(&bytes).await {
                    debug!("clipboard send failed: {e}");
                } else {
                    let _ = stream.finish();
                }
            }
            Err(e) => debug!("cannot open a clipboard stream: {e}"),
        }
    });
}

/// Keys, buttons and handover, length-prefixed on the one reliable stream.
pub(super) async fn frames(
    mut recv: RecvStream,
    peer: &str,
    to_session: &UnboundedSender<(String, Msg)>,
) -> Result<()> {
    loop {
        let frame = read_frame(&mut recv).await?;
        if frame.is_empty() {
            continue; // the stream-opening frame
        }
        match Msg::decode(&frame) {
            Ok(msg) => {
                let _ = to_session.send((peer.to_string(), msg));
            }
            Err(e) => debug!(%peer, "bad frame: {e}"),
        }
    }
}

/// A clipboard item per stream: the stream ending is the length, so nothing
/// has to be framed and nothing else waits on it.
pub(super) async fn bulk(
    conn: &Connection,
    peer: &str,
    to_session: &UnboundedSender<(String, Msg)>,
) -> Result<()> {
    loop {
        let mut recv = conn.accept_uni().await?;
        let (peer, to_session) = (peer.to_string(), to_session.clone());
        tokio::spawn(async move {
            match recv.read_to_end(MAX_BULK).await {
                Ok(bytes) => match Msg::decode(&bytes) {
                    Ok(msg) => {
                        let _ = to_session.send((peer, msg));
                    }
                    Err(e) => debug!(%peer, "bad clipboard message: {e}"),
                },
                Err(e) => debug!(%peer, "clipboard stream failed: {e}"),
            }
        });
    }
}

/// Motion and pings, plus the running latency summary.
pub(super) async fn datagrams(
    conn: &Connection,
    peer: &str,
    started: Instant,
    tx: &UnboundedSender<Msg>,
    status: &Status,
    to_session: &UnboundedSender<(String, Msg)>,
) -> Result<()> {
    let mut samples = Samples::new();
    let mut report = tokio::time::interval(STATS_INTERVAL);
    report.tick().await; // the first tick is immediate
    loop {
        tokio::select! {
            datagram = conn.read_datagram() => {
                match Msg::decode(&datagram?) {
                    Ok(Msg::Ping(t)) => { let _ = tx.send(Msg::Pong(t)); }
                    Ok(Msg::Pong(t)) => samples.push(round_trip(t, started)),
                    Ok(msg) => { let _ = to_session.send((peer.to_string(), msg)); }
                    Err(e) => debug!(%peer, "bad datagram: {e}"),
                }
            }
            _ = report.tick() => {
                if let Some(line) = samples.summary() {
                    info!(%peer, "{line}");
                    status.set_peer(peer, |p| p.rtt_ms = samples.p50());
                }
            }
        }
    }
}

/// `[u32 length][bytes]`. View Screen's sound uses it too.
pub(crate) async fn write_frame(send: &mut SendStream, bytes: &[u8]) -> Result<()> {
    let len = u32::try_from(bytes.len()).context("frame too large")?;
    send.write_all(&len.to_be_bytes()).await?;
    send.write_all(bytes).await?;
    Ok(())
}

/// Not cancel-safe: only ever awaited by a loop with nothing else to race it
/// against, [`frames`] or View Screen's sound.
pub(crate) async fn read_frame(recv: &mut RecvStream) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(anyhow!("frame of {len} bytes is too large"));
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(buf)
}

fn round_trip(sent: u64, started: Instant) -> f64 {
    let now = started.elapsed().as_nanos() as u64;
    now.saturating_sub(sent) as f64 / 1_000_000.0
}

pub(super) async fn ping_loop(tx: UnboundedSender<Msg>, started: Instant) {
    let mut ping = tokio::time::interval(PING_INTERVAL);
    loop {
        ping.tick().await;
        if tx.send(Msg::Ping(started.elapsed().as_nanos() as u64)).is_err() {
            return;
        }
    }
}

struct Samples {
    values: Vec<f64>,
}

impl Samples {
    const fn new() -> Self {
        Self { values: Vec::new() }
    }

    fn push(&mut self, ms: f64) {
        if self.values.len() == SAMPLE_WINDOW {
            self.values.remove(0);
        }
        self.values.push(ms);
    }

    /// The typical round trip, for the status file.
    fn p50(&self) -> Option<f64> {
        let mut sorted = self.values.clone();
        sorted.sort_by(f64::total_cmp);
        sorted.get(sorted.len().checked_sub(1)? / 2).copied()
    }

    fn summary(&self) -> Option<String> {
        if self.values.is_empty() {
            return None;
        }
        let mut sorted = self.values.clone();
        sorted.sort_by(f64::total_cmp);
        let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
        Some(format!(
            "rtt p50 {:.2}ms  p99 {:.2}ms  min {:.2}ms  max {:.2}ms  n={}",
            at(0.5),
            at(0.99),
            sorted[0],
            sorted[sorted.len() - 1],
            sorted.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn round_trip_counts_from_the_send_time() {
        let started = Instant::now();
        std::thread::sleep(Duration::from_millis(5));
        assert!(round_trip(0, started) >= 5.0);
    }

    #[test]
    fn percentiles_and_window() {
        let mut s = Samples::new();
        assert!(s.summary().is_none());
        for i in 0..SAMPLE_WINDOW + 50 {
            s.push(i as f64);
        }
        assert_eq!(s.values.len(), SAMPLE_WINDOW);
        assert_eq!(s.values[0], 50.0);
        assert!(s.summary().unwrap().contains("p50"));
    }
}
