//! View Screen's sound on the Mac: 16-bit stereo PCM at 48 kHz, played by an
//! AudioQueue as it arrives.
//!
//! Latency is bounded by counting buffers, not by timing: a chunk that finds
//! every buffer still queued is dropped. A click when the network hiccups is
//! better than a delay that never goes away.

use std::ffi::c_void;
use std::ptr::null;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};

const RATE: f64 = 48_000.0;
/// At 10–20 ms a chunk, the most sound that can ever be waiting to play.
const BUFFERS: usize = 6;
/// Bigger than any chunk the peer sends (20 ms is 3840 bytes).
const CAPACITY: u32 = 16 << 10;
/// Chunks queued before playback starts: a little slack for jitter.
const PREROLL: usize = 2;

/// kAudioFormatLinearPCM, 'lpcm'.
const LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
/// kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked; little-endian
/// is what no flag means.
const SIGNED_PACKED: u32 = (1 << 2) | (1 << 3);

#[repr(C)]
struct StreamFormat {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

/// AudioQueueBuffer, of which only the data pointer and sizes are used.
#[repr(C)]
struct QueueBuffer {
    capacity: u32,
    data: *mut c_void,
    size: u32,
    user_data: *mut c_void,
    packet_capacity: u32,
    packets: *mut c_void,
    packet_count: u32,
}

type Queue = *mut c_void;
type Buffer = *mut QueueBuffer;
type Callback = extern "C" fn(*mut c_void, Queue, Buffer);

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewOutput(
        format: *const StreamFormat,
        callback: Callback,
        user_data: *mut c_void,
        run_loop: *const c_void,
        run_loop_mode: *const c_void,
        flags: u32,
        queue: *mut Queue,
    ) -> i32;
    fn AudioQueueAllocateBuffer(queue: Queue, capacity: u32, buffer: *mut Buffer) -> i32;
    fn AudioQueueEnqueueBuffer(
        queue: Queue,
        buffer: Buffer,
        packets: u32,
        descriptions: *const c_void,
    ) -> i32;
    fn AudioQueueStart(queue: Queue, at: *const c_void) -> i32;
    fn AudioQueueDispose(queue: Queue, immediate: u8) -> i32;
}

/// Buffers the queue has played and handed back.
struct Free(Vec<Buffer>);
// SAFETY: the buffers belong to the queue; this list only says which ones are
// idle, and is only touched under its mutex.
unsafe impl Send for Free {}

pub struct Speaker {
    queue: Queue,
    /// Read by the callback through a raw pointer, so it must outlive the
    /// queue: `Drop` disposes the queue before this goes.
    free: Arc<Mutex<Free>>,
    started: bool,
}

// SAFETY: AudioQueue calls are thread-safe; the queue is only ever used by
// whoever owns the speaker.
unsafe impl Send for Speaker {}

impl Speaker {
    pub fn open() -> Result<Self> {
        let format = StreamFormat {
            sample_rate: RATE,
            format_id: LINEAR_PCM,
            format_flags: SIGNED_PACKED,
            bytes_per_packet: 4,
            frames_per_packet: 1,
            bytes_per_frame: 4,
            channels_per_frame: 2,
            bits_per_channel: 16,
            reserved: 0,
        };
        let free = Arc::new(Mutex::new(Free(Vec::with_capacity(BUFFERS))));
        let mut queue: Queue = std::ptr::null_mut();
        let context = Arc::as_ptr(&free) as *mut c_void;
        let status = unsafe {
            AudioQueueNewOutput(&format, returned, context, null(), null(), 0, &mut queue)
        };
        if status != 0 {
            bail!("cannot open the sound output (status {status})");
        }
        // Built now, so `Drop` disposes the queue if a buffer cannot be had.
        let speaker = Self {
            queue,
            free,
            started: false,
        };
        for _ in 0..BUFFERS {
            let mut buffer: Buffer = std::ptr::null_mut();
            let status = unsafe { AudioQueueAllocateBuffer(queue, CAPACITY, &mut buffer) };
            if status != 0 {
                bail!("cannot allocate a sound buffer (status {status})");
            }
            speaker.free.lock().unwrap().0.push(buffer);
        }
        Ok(speaker)
    }

    /// Queue one chunk of PCM, or drop it if the queue is full.
    pub fn play(&mut self, pcm: &[u8]) {
        let Some(buffer) = self.free.lock().unwrap().0.pop() else {
            tracing::debug!("sound is behind, dropped a chunk");
            return;
        };
        // SAFETY: an idle buffer of this queue, with `capacity` bytes behind `data`.
        let status = unsafe {
            let len = pcm.len().min((*buffer).capacity as usize);
            std::ptr::copy_nonoverlapping(pcm.as_ptr(), (*buffer).data.cast::<u8>(), len);
            (*buffer).size = len as u32;
            AudioQueueEnqueueBuffer(self.queue, buffer, 0, null())
        };
        if status != 0 {
            self.free.lock().unwrap().0.push(buffer);
            return tracing::debug!(status, "the sound output refused a chunk");
        }
        let queued = BUFFERS - self.free.lock().unwrap().0.len();
        if !self.started && queued >= PREROLL {
            self.started = unsafe { AudioQueueStart(self.queue, null()) } == 0;
        }
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        // Immediate: no callback runs after this returns, so `free` may go.
        unsafe { AudioQueueDispose(self.queue, 1) };
    }
}

/// The queue finished playing a buffer: it is free for the next chunk.
extern "C" fn returned(context: *mut c_void, _queue: Queue, buffer: Buffer) {
    // SAFETY: `context` is the speaker's `free`, alive until the queue is disposed.
    let free = unsafe { &*(context as *const Mutex<Free>) };
    if let Ok(mut free) = free.lock() {
        free.0.push(buffer);
    }
}
