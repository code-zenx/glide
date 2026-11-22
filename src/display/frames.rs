//! The frame pipe: the one-frame slot between encoder and network, and how
//! frames are written to and read from a video stream.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use quinn::{RecvStream, SendStream};
use tokio::sync::Notify;

/// A frame can be a keyframe of a busy 4K screen; anything past this is a
/// desynchronised stream, not a frame.
const MAX_FRAME: usize = 16 << 20;

/// One encoded frame, as Annex-B NAL units.
#[derive(Debug, PartialEq)]
pub struct Frame {
    pub key: bool,
    pub data: Vec<u8>,
}

/// Where the encoder leaves a frame for the network to pick up. Holds one.
///
/// A second frame arriving before the first went out means the network is
/// behind. Both are dropped — the second refers to the first, so sending it
/// alone would smear the picture — and everything after is dropped too until
/// the keyframe the encoder is asked for arrives. Latency can therefore never
/// grow past about a frame.
#[derive(Default)]
pub struct Slot {
    state: Mutex<SlotState>,
    ready: Notify,
    want_key: AtomicBool,
}

#[derive(Default)]
struct SlotState {
    frame: Option<Frame>,
    waiting_for_key: bool,
}

impl Slot {
    /// Called from the encoder's thread.
    pub fn push(&self, frame: Frame) {
        let mut state = self.state.lock().unwrap();
        if state.frame.is_some() {
            state.frame = None;
            state.waiting_for_key = true;
            self.want_key.store(true, Ordering::Relaxed);
        }
        if state.waiting_for_key && !frame.key {
            return;
        }
        state.waiting_for_key = false;
        state.frame = Some(frame);
        drop(state);
        self.ready.notify_one();
    }

    /// The next frame to send, waiting for one if there is none.
    pub async fn pop(&self) -> Frame {
        loop {
            if let Some(frame) = self.state.lock().unwrap().frame.take() {
                return frame;
            }
            self.ready.notified().await;
        }
    }

    /// A new receiver knows nothing: start it from a keyframe.
    pub fn restart(&self) {
        let mut state = self.state.lock().unwrap();
        state.frame = None;
        state.waiting_for_key = true;
        self.want_key.store(true, Ordering::Relaxed);
    }

    /// Whether the encoder should make its next frame a keyframe. Clears the
    /// request.
    pub fn take_key_request(&self) -> bool {
        self.want_key.swap(false, Ordering::Relaxed)
    }
}

/// Drain the slot into a stream until the stream fails. A new receiver knows
/// nothing, so it starts from a keyframe. Extend sends this way from the Mac,
/// View Screen from the Linux end.
pub async fn send_frames(send: &mut SendStream, slot: &Slot) -> Result<()> {
    slot.restart();
    let mut count = 0u64;
    loop {
        let frame = slot.pop().await;
        count += 1;
        if count == 1 || count.is_multiple_of(600) {
            tracing::info!(
                count,
                key = frame.key,
                bytes = frame.data.len(),
                "video frames sent"
            );
        }
        write_frame(send, &frame).await?;
    }
}

/// Hand every frame on a stream to `show` until the stream ends. Not
/// cancel-safe, for the same reason `read_frame` is not.
pub async fn recv_frames(recv: &mut RecvStream, mut show: impl FnMut(Frame)) -> Result<()> {
    let mut count = 0u64;
    loop {
        let frame = read_frame(recv).await?;
        count += 1;
        if count == 1 || count.is_multiple_of(600) {
            tracing::info!(
                count,
                key = frame.key,
                bytes = frame.data.len(),
                "video frames received"
            );
        }
        show(frame);
    }
}

/// `[u32 length][u8 keyframe][NAL units]`, on the video connection's one stream.
pub(crate) async fn write_frame(send: &mut SendStream, frame: &Frame) -> Result<()> {
    let len = u32::try_from(frame.data.len() + 1)?;
    send.write_all(&len.to_be_bytes()).await?;
    send.write_all(&[frame.key as u8]).await?;
    send.write_all(&frame.data).await?;
    Ok(())
}

/// Not cancel-safe: only awaited by a loop that has nothing to race it with.
async fn read_frame(recv: &mut RecvStream) -> Result<Frame> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(anyhow!("video frame of {len} bytes"));
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    let key = buf[0] != 0;
    buf.remove(0);
    Ok(Frame { key, data: buf })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two real endpoints over loopback: the video connection must be told
    /// apart from an input link, and frames must come out as they went in,
    /// boundaries and keyframe flags included.
    #[tokio::test(flavor = "multi_thread")]
    async fn frames_cross_a_video_connection_intact() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("glide-video-{}", std::process::id()));
        let (receiver, sender, peer) = crate::link::loopback(&dir)?;

        let frames: Vec<Frame> = (0..50u8)
            .map(|i| Frame { key: i % 10 == 0, data: vec![i; 1 + i as usize * 4000] })
            .collect();
        let sent = frames.iter().map(|f| Frame { key: f.key, data: f.data.clone() }).collect::<Vec<_>>();
        let (accepted, dialed) = tokio::join!(
            async { receiver.endpoint.accept().await.unwrap().await.unwrap() },
            sender.dial_video(&peer),
        );
        let dialed = dialed?;
        assert!(crate::link::is_video(&accepted), "the accept loop must route this to the display");
        tokio::spawn(async move {
            let mut send = dialed.open_uni().await.unwrap();
            for frame in &sent {
                write_frame(&mut send, frame).await.unwrap();
            }
            send.finish().unwrap();
            dialed.closed().await;
        });
        let mut recv = accepted.accept_uni().await?;
        for expected in &frames {
            assert_eq!(&read_frame(&mut recv).await?, expected);
        }
        accepted.close(0u32.into(), b"done");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    fn frame(key: bool, n: u8) -> Frame {
        Frame { key, data: vec![n] }
    }

    #[tokio::test]
    async fn a_prompt_sender_gets_every_frame() {
        let slot = Slot::default();
        slot.push(frame(true, 1));
        assert_eq!(slot.pop().await, frame(true, 1));
        slot.push(frame(false, 2));
        assert_eq!(slot.pop().await, frame(false, 2));
        assert!(!slot.take_key_request());
    }

    /// The rule that keeps latency flat: a sender a frame behind loses both
    /// frames and everything up to the next keyframe, and asks for one.
    #[tokio::test]
    async fn a_slow_sender_drops_to_the_next_keyframe() {
        let slot = Slot::default();
        slot.push(frame(true, 1));
        slot.push(frame(false, 2)); // 1 was never picked up
        assert!(slot.take_key_request(), "the encoder must be asked for a keyframe");
        slot.push(frame(false, 3));
        assert!(slot.state.lock().unwrap().frame.is_none(), "a frame that refers to a dropped one must not go out");
        slot.push(frame(true, 4));
        assert_eq!(slot.pop().await, frame(true, 4));
        slot.push(frame(false, 5));
        assert_eq!(slot.pop().await, frame(false, 5), "back to normal after the keyframe");
    }

    #[tokio::test]
    async fn a_keyframe_behind_a_stale_frame_still_goes_out() {
        let slot = Slot::default();
        slot.push(frame(false, 1));
        slot.push(frame(true, 2));
        assert_eq!(slot.pop().await, frame(true, 2));
    }

    #[test]
    fn a_new_receiver_starts_from_a_keyframe() {
        let slot = Slot::default();
        slot.push(frame(true, 1));
        slot.restart();
        assert!(slot.take_key_request());
        slot.push(frame(false, 2));
        assert!(slot.state.lock().unwrap().frame.is_none());
    }
}
