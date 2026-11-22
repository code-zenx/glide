//! View Screen's Opus sound, decoded by AudioToolbox's own Opus codec into the
//! PCM the speaker plays. No crate: the codec ships with macOS.
//!
//! An AudioConverter pulls its input through a callback. Each call here hands
//! it one packet and then says there is nothing more for now (`EMPTY`), so it
//! gives back that packet's sound and keeps its state for the next one.

use std::ffi::c_void;
use std::ptr::null_mut;

use anyhow::{bail, Result};

use super::{pcm_format, StreamFormat, RATE};

/// kAudioFormatOpus, 'opus'.
const OPUS: u32 = u32::from_be_bytes(*b"opus");
/// 10 ms at 48 kHz: the packets the peer's `opusenc` sends.
const PACKET_FRAMES: u32 = 480;
/// Room for 60 ms, the longest Opus frame, which still fits a speaker buffer.
const MAX_FRAMES: usize = 2880;
/// 16-bit stereo.
const FRAME_BYTES: usize = 4;
/// What the input callback returns when its packet is used up. An error, not
/// "no packets", because no packets means the stream ended, and an ended
/// converter needs a reset before it takes the next packet.
const EMPTY: i32 = i32::from_be_bytes(*b"empt");

type Converter = *mut c_void;

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    size: u32,
    data: *mut c_void,
}

/// AudioBufferList with its one buffer: the sound is interleaved.
#[repr(C)]
struct BufferList {
    count: u32,
    buffers: [AudioBuffer; 1],
}

/// AudioStreamPacketDescription: how big a compressed packet is.
#[repr(C)]
#[derive(Default)]
struct PacketDesc {
    start: i64,
    variable_frames: u32,
    size: u32,
}

type InputProc =
    extern "C" fn(Converter, *mut u32, *mut BufferList, *mut *mut PacketDesc, *mut c_void) -> i32;

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioConverterNew(
        from: *const StreamFormat,
        to: *const StreamFormat,
        converter: *mut Converter,
    ) -> i32;
    fn AudioConverterFillComplexBuffer(
        converter: Converter,
        input: InputProc,
        user_data: *mut c_void,
        packets: *mut u32,
        output: *mut BufferList,
        descriptions: *mut PacketDesc,
    ) -> i32;
    fn AudioConverterReset(converter: Converter) -> i32;
    fn AudioConverterDispose(converter: Converter) -> i32;
}

/// 48 kHz stereo Opus in 10 ms packets: what the peer sends.
fn opus_format() -> StreamFormat {
    StreamFormat {
        sample_rate: RATE,
        format_id: OPUS,
        format_flags: 0,
        bytes_per_packet: 0,
        frames_per_packet: PACKET_FRAMES,
        bytes_per_frame: 0,
        channels_per_frame: 2,
        bits_per_channel: 0,
        reserved: 0,
    }
}

/// One interleaved buffer over `bytes`, for the converter to fill.
fn buffer_list(bytes: &mut [u8]) -> BufferList {
    BufferList {
        count: 1,
        buffers: [AudioBuffer {
            channels: 2,
            size: bytes.len() as u32,
            data: bytes.as_mut_ptr().cast(),
        }],
    }
}

/// What the converter is handed next: `packets` packets of `stride` bytes.
/// A compressed feed is a single packet, whose description lives here so it
/// outlives the callback that hands it over.
struct Feed {
    data: *const u8,
    packets: u32,
    stride: u32,
    desc: PacketDesc,
}

/// The converter asks for input: as much of the feed as it wants, then `EMPTY`.
extern "C" fn pull(
    _converter: Converter,
    packets: *mut u32,
    io: *mut BufferList,
    desc: *mut *mut PacketDesc,
    context: *mut c_void,
) -> i32 {
    // SAFETY: `context` is the `Feed` lent to AudioConverterFillComplexBuffer
    // by its caller, and the converter's pointers are valid for this call.
    unsafe {
        let feed = &mut *context.cast::<Feed>();
        let n = (*packets).min(feed.packets);
        *packets = n;
        if n == 0 {
            return EMPTY;
        }
        let size = n * feed.stride;
        (*io).buffers[0] = AudioBuffer {
            channels: 2,
            size,
            data: feed.data.cast_mut().cast(),
        };
        if !desc.is_null() {
            feed.desc = PacketDesc { start: 0, variable_frames: 0, size };
            *desc = &raw mut feed.desc;
        }
        feed.data = feed.data.add(size as usize);
        feed.packets -= n;
    }
    0
}

/// Turns one peer's Opus packets back into PCM, in order.
pub(super) struct Decoder {
    converter: Converter,
    pcm: Vec<u8>,
    /// Whether a bad packet was reported: the first one says it all.
    warned: bool,
}

impl Decoder {
    pub(super) fn open() -> Result<Self> {
        let mut converter = null_mut();
        let status = unsafe { AudioConverterNew(&opus_format(), &pcm_format(), &mut converter) };
        if status != 0 {
            bail!("this Mac has no Opus decoder (status {status})");
        }
        Ok(Self {
            converter,
            pcm: vec![0; MAX_FRAMES * FRAME_BYTES],
            warned: false,
        })
    }

    /// One packet's sound as PCM, or `None` for a packet that would not
    /// decode: that one is dropped. AudioToolbox mostly says so by giving no
    /// sound back rather than by an error.
    pub(super) fn decode(&mut self, packet: &[u8]) -> Option<&[u8]> {
        let mut feed = Feed {
            data: packet.as_ptr(),
            packets: 1,
            stride: packet.len() as u32,
            desc: PacketDesc::default(),
        };
        let mut out = buffer_list(&mut self.pcm);
        let mut frames = MAX_FRAMES as u32;
        // SAFETY: `feed` and `out` outlive the call; `out` has room for `frames`.
        let status = unsafe {
            let context = (&raw mut feed).cast();
            let (packets, output) = (&mut frames, &mut out);
            AudioConverterFillComplexBuffer(self.converter, pull, context, packets, output, null_mut())
        };
        let decoded = status == 0 || status == EMPTY;
        if decoded && frames > 0 {
            return Some(&self.pcm[..frames as usize * FRAME_BYTES]);
        }
        if !decoded {
            // Whatever the bad packet left half-done must not spoil the next.
            unsafe { AudioConverterReset(self.converter) };
        }
        if !std::mem::replace(&mut self.warned, true) {
            tracing::warn!(status, "an Opus packet would not decode; dropping it and any like it");
        }
        None
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.converter) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMPLITUDE: f64 = 16_000.0;

    /// `pcm` as Opus packets, by AudioToolbox's own Opus encoder: the half of
    /// the round trip the peer does with `opusenc`.
    fn encode(pcm: &[u8]) -> Vec<Vec<u8>> {
        let mut converter = null_mut();
        let status = unsafe { AudioConverterNew(&pcm_format(), &opus_format(), &mut converter) };
        assert_eq!(status, 0, "no Opus encoder");
        let mut feed = Feed {
            data: pcm.as_ptr(),
            packets: (pcm.len() / FRAME_BYTES) as u32,
            stride: FRAME_BYTES as u32,
            desc: PacketDesc::default(),
        };
        let mut packets = Vec::new();
        loop {
            let mut packet = vec![0u8; 4000];
            let mut out = buffer_list(&mut packet);
            let (mut count, mut desc) = (1, PacketDesc::default());
            let status = unsafe {
                let context = (&raw mut feed).cast();
                let (packets, output) = (&mut count, &mut out);
                AudioConverterFillComplexBuffer(converter, pull, context, packets, output, &mut desc)
            };
            assert!(status == 0 || status == EMPTY, "encoder status {status}");
            if count == 0 {
                break;
            }
            packet.truncate(desc.size as usize);
            packets.push(packet);
        }
        unsafe { AudioConverterDispose(converter) };
        packets
    }

    /// `ms` of a `hz` tone, the same on both sides.
    fn sine(hz: f64, ms: usize) -> Vec<u8> {
        (0..48 * ms)
            .map(|i| ((i as f64 * hz * std::f64::consts::TAU / RATE).sin() * AMPLITUDE) as i16)
            .flat_map(|s| [s, s])
            .flat_map(i16::to_le_bytes)
            .collect()
    }

    #[test]
    fn opus_decodes_back_to_the_tone_it_was_made_from() {
        let packets = encode(&sine(1000.0, 100));
        assert!(packets.len() >= 8, "100 ms is about ten packets, got {}", packets.len());
        let mut decoder = Decoder::open().unwrap();
        let mut left = Vec::new();
        for (i, packet) in packets.iter().enumerate() {
            let pcm = decoder.decode(packet).expect("a packet from the encoder decodes");
            let full = PACKET_FRAMES as usize * FRAME_BYTES;
            // The first comes back short by the decoder's own start-up delay.
            let short_first = i == 0 && pcm.len() < full;
            assert!(pcm.len() == full || short_first, "packet {i}: {} bytes", pcm.len());
            left.extend(pcm.chunks(FRAME_BYTES).map(|f| i16::from_le_bytes([f[0], f[1]])));
        }
        // The codec's own delay keeps the start quiet: judge the second half.
        let tail = &left[left.len() / 2..];
        let power = tail.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / tail.len() as f64;
        let (rms, want) = (power.sqrt(), AMPLITUDE / 2f64.sqrt());
        assert!((rms - want).abs() < want * 0.2, "RMS {rms}, want about {want}");
        let crossings = tail.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count() as f64;
        let expected = 2.0 * 1000.0 * tail.len() as f64 / RATE;
        let off = (crossings - expected).abs();
        assert!(off <= expected * 0.1, "{crossings} zero crossings, want about {expected}");
    }

    #[test]
    fn a_bad_packet_is_dropped_and_the_next_still_decodes() {
        let packets = encode(&sine(440.0, 30));
        let mut decoder = Decoder::open().unwrap();
        // Code 3 with a frame count of zero: no valid Opus packet starts so.
        assert!(decoder.decode(&[0xFF, 0x00]).is_none());
        assert!(decoder.decode(&packets[0]).is_some());
    }
}
