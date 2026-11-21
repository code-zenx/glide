//! Just enough H.264 for the Mac to show what a Linux machine encodes.
//!
//! GStreamer sends a byte stream (Annex B): NAL units separated by start
//! codes, with the parameter sets in front of every keyframe. CoreMedia wants
//! the parameter sets apart, to build a format description from, and every
//! other NAL unit behind a 4-byte big-endian length instead (AVCC).

/// Sequence and picture parameter sets, and the access unit delimiter, which
/// AVCC has no use for.
const SPS: u8 = 7;
const PPS: u8 = 8;
const AUD: u8 = 9;

/// One encoded picture, split the way CoreMedia wants it.
#[derive(Debug, Default, PartialEq)]
pub struct AccessUnit<'a> {
    pub sps: Option<&'a [u8]>,
    pub pps: Option<&'a [u8]>,
    /// Every other NAL unit, each behind its length.
    pub avcc: Vec<u8>,
}

pub fn parse(stream: &[u8]) -> AccessUnit<'_> {
    let mut unit = AccessUnit::default();
    for nal in split(stream) {
        match nal[0] & 0x1f {
            SPS => unit.sps = Some(nal),
            PPS => unit.pps = Some(nal),
            AUD => {}
            _ => {
                unit.avcc
                    .extend_from_slice(&(nal.len() as u32).to_be_bytes());
                unit.avcc.extend_from_slice(nal);
            }
        }
    }
    unit
}

/// The NAL units between start codes, without them. A 4-byte start code is a
/// 3-byte one after a zero, and a NAL unit never ends in a zero byte, so
/// trailing zeros belong to the next start code (or are padding) and go.
fn split(stream: &[u8]) -> Vec<&[u8]> {
    let starts: Vec<usize> = stream
        .windows(3)
        .enumerate()
        .filter(|(_, w)| *w == [0, 0, 1])
        .map(|(i, _)| i + 3)
        .collect();
    let ends = starts
        .iter()
        .skip(1)
        .map(|next| next - 3)
        .chain([stream.len()]);
    starts
        .iter()
        .zip(ends)
        .filter(|&(&start, end)| start < end)
        .map(|(&start, end)| trim_zeros(&stream[start..end]))
        .filter(|nal| !nal.is_empty())
        .collect()
}

fn trim_zeros(nal: &[u8]) -> &[u8] {
    let end = nal.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &nal[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS_NAL: [u8; 3] = [0x67, 0x42, 0x1f];
    const PPS_NAL: [u8; 2] = [0x68, 0xce];
    const IDR_NAL: [u8; 3] = [0x65, 0x88, 0x84];

    fn keyframe() -> Vec<u8> {
        [
            &[0, 0, 0, 1, 0x09, 0xf0][..],
            &[0, 0, 0, 1],
            &SPS_NAL,
            &[0, 0, 1],
            &PPS_NAL,
            &[0, 0, 0, 1],
            &IDR_NAL,
        ]
        .concat()
    }

    #[test]
    fn start_codes_of_both_lengths_split_the_stream() {
        assert_eq!(
            split(&keyframe()),
            vec![&[0x09, 0xf0][..], &SPS_NAL, &PPS_NAL, &IDR_NAL]
        );
    }

    #[test]
    fn parameter_sets_go_aside_and_the_rest_gets_lengths() {
        let frame = keyframe();
        let unit = parse(&frame);
        assert_eq!(unit.sps, Some(&SPS_NAL[..]));
        assert_eq!(unit.pps, Some(&PPS_NAL[..]));
        assert_eq!(
            unit.avcc,
            [&[0, 0, 0, 3][..], &IDR_NAL].concat(),
            "the delimiter is dropped, the slice kept"
        );
    }

    #[test]
    fn a_frame_between_keyframes_has_no_parameter_sets() {
        let slice = [0, 0, 0, 1, 0x41, 0x9a, 0, 0, 0, 1, 0x41, 0x9b];
        let unit = parse(&slice);
        assert_eq!((unit.sps, unit.pps), (None, None));
        assert_eq!(unit.avcc, [0, 0, 0, 2, 0x41, 0x9a, 0, 0, 0, 2, 0x41, 0x9b]);
    }

    #[test]
    fn padding_and_garbage_come_to_nothing() {
        assert_eq!(
            split(&[0, 0, 1, 0x65, 0x10, 0, 0, 0, 0]),
            vec![&[0x65, 0x10][..]],
            "trailing zeros are trimmed"
        );
        assert_eq!(parse(&[]), AccessUnit::default());
        assert_eq!(
            parse(&[1, 2, 3, 4]),
            AccessUnit::default(),
            "no start code, no NAL unit"
        );
        assert_eq!(
            parse(&[0, 0, 1, 0, 0, 1]),
            AccessUnit::default(),
            "empty units are skipped"
        );
    }
}
