//! The encoder: HEVC on the media engine, and its length-prefixed output
//! rewritten as the start-code byte stream a decoder reads.

use std::ffi::c_void;
use std::ptr::{null_mut, NonNull};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_video::{
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
};
use objc2_core_media::{
    kCMSampleAttachmentKey_NotSync, kCMVideoCodecType_HEVC, CMSampleBuffer,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_YCbCrMatrix,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxFrameDelayCount,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_HEVC_Main_AutoLevel, kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, VTCompressionSession,
    VTEncodeInfoFlags, VTSessionSetProperty,
};

use crate::display::{Frame, Slot};
use crate::proto::ScreenMode;

/// HEVC on the media engine, tuned for latency over everything else: real
/// time, no B-frames, no frames held back, keyframes only when asked for.
pub(super) fn encoder(screen: ScreenMode, bitrate: u32, slot: &Arc<Slot>) -> Result<CFRetained<VTCompressionSession>> {
    let yes: &CFType = CFBoolean::new(true);
    let spec = CFDictionary::<CFString, CFType>::from_slices(
        &[
            unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl },
            unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder },
        ],
        &[yes, yes],
    );
    let mut raw: *mut VTCompressionSession = null_mut();
    let status = unsafe {
        VTCompressionSession::create(
            None,
            screen.w as i32,
            screen.h as i32,
            kCMVideoCodecType_HEVC,
            Some(spec.as_opaque()),
            None,
            None,
            Some(encoded),
            Arc::as_ptr(slot) as *mut c_void,
            NonNull::from(&mut raw),
        )
    };
    let session = NonNull::new(raw)
        .filter(|_| status == 0)
        .map(|raw| unsafe { CFRetained::from_raw(raw) })
        .ok_or_else(|| anyhow!("cannot create the HEVC encoder (status {status})"))?;

    let no: &CFType = CFBoolean::new(false);
    let rate = CFNumber::new_i32(bitrate as i32);
    let fps = CFNumber::new_i32(screen.hz as i32);
    // Never on a timer: a reliable stream only needs one after a drop.
    let key_interval = CFNumber::new_i32(i32::MAX);
    let zero = CFNumber::new_i32(0);
    let profile: &CFType = unsafe { kVTProfileLevel_HEVC_Main_AutoLevel };
    let properties: [(&CFString, &CFType); 11] = unsafe {
        [
            (kVTCompressionPropertyKey_RealTime, yes),
            (kVTCompressionPropertyKey_AllowFrameReordering, no),
            (kVTCompressionPropertyKey_ProfileLevel, profile),
            (kVTCompressionPropertyKey_AverageBitRate, &rate),
            (kVTCompressionPropertyKey_ExpectedFrameRate, &fps),
            (kVTCompressionPropertyKey_MaxKeyFrameInterval, &key_interval),
            (kVTCompressionPropertyKey_MaxFrameDelayCount, &zero),
            (kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, yes),
            (kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2),
            (kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2),
            (kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2),
        ]
    };
    for (key, value) in properties {
        // Some encoders refuse some keys; each one is a preference, not a need.
        let status = unsafe { VTSessionSetProperty(&session, key, Some(value)) };
        if status != 0 {
            tracing::debug!(%key, status, "encoder declined a setting");
        }
    }
    Ok(session)
}

pub(super) fn force_key() -> CFRetained<CFDictionary<CFString, CFType>> {
    let yes: &CFType = CFBoolean::new(true);
    CFDictionary::from_slices(&[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }], &[yes])
}

/// VideoToolbox's output callback, on the encoder's own thread.
unsafe extern "C-unwind" fn encoded(
    slot: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    let (Some(slot), Some(sample)) = (unsafe { (slot as *const Slot).as_ref() }, unsafe { sample.as_ref() }) else {
        return;
    };
    if status != 0 {
        return;
    }
    if let Some(frame) = unsafe { annex_b(sample) } {
        slot.push(frame);
    }
}

/// VideoToolbox writes length-prefixed NAL units with the parameter sets kept
/// aside; a decoder reading a plain byte stream wants start codes, and the
/// parameter sets in front of every keyframe.
unsafe fn annex_b(sample: &CMSampleBuffer) -> Option<Frame> {
    let block = unsafe { sample.data_buffer() }?;
    let len = unsafe { block.data_length() };
    let mut body = vec![0u8; len];
    if unsafe { block.copy_data_bytes(0, len, NonNull::new(body.as_mut_ptr().cast())?) } != 0 {
        return None;
    }
    let key = unsafe { is_keyframe(sample) };
    let mut params: Vec<&[u8]> = Vec::new();
    let mut prefix = 4;
    if key {
        let format = unsafe { sample.format_description() }?;
        let mut count = 0usize;
        let mut index = 0usize;
        loop {
            let (mut ptr, mut size, mut header) = (std::ptr::null(), 0usize, 0i32);
            let got = unsafe {
                CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                    &format, index, &mut ptr, &mut size, &mut count, &mut header,
                )
            };
            if got != 0 || ptr.is_null() {
                break;
            }
            prefix = header as usize;
            // SAFETY: owned by the format description, which lives until this
            // frame has been rewritten below.
            params.push(unsafe { std::slice::from_raw_parts(ptr, size) });
            index += 1;
            if index >= count {
                break;
            }
        }
    }
    Some(Frame { key, data: to_annex_b(&body, prefix, &params)? })
}

/// Keyframes are the samples *not* marked "not sync".
unsafe fn is_keyframe(sample: &CMSampleBuffer) -> bool {
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else { return true };
    // SAFETY: CFArray of CFDictionary is toll-free bridged to NSArray of NSDictionary.
    let attachments: &NSArray<NSDictionary<NSString, AnyObject>> =
        unsafe { &*(CFRetained::as_ptr(&attachments).as_ptr() as *const _) };
    let not_sync: &NSString = unsafe { &*(kCMSampleAttachmentKey_NotSync as *const CFString as *const NSString) };
    attachments
        .firstObject()
        .and_then(|dict| dict.objectForKey(not_sync))
        .and_then(|value| value.downcast::<NSNumber>().ok())
        .is_none_or(|value| !value.boolValue())
}

pub(super) const START: [u8; 4] = [0, 0, 0, 1];

/// Rewrite `prefix`-byte length-prefixed NAL units as start-code delimited
/// ones, parameter sets first. `None` if a length runs past the end.
fn to_annex_b(body: &[u8], prefix: usize, params: &[&[u8]]) -> Option<Vec<u8>> {
    if !(1..=4).contains(&prefix) {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() + params.iter().map(|p| p.len() + 4).sum::<usize>());
    for param in params {
        out.extend_from_slice(&START);
        out.extend_from_slice(param);
    }
    let mut at = 0;
    while at < body.len() {
        let len = body.get(at..at + prefix)?.iter().fold(0usize, |n, &b| n << 8 | b as usize);
        at += prefix;
        out.extend_from_slice(&START);
        out.extend_from_slice(body.get(at..at + len)?);
        at += len;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_prefixes_become_start_codes() {
        let body = [0, 0, 0, 2, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC];
        let out = to_annex_b(&body, 4, &[&[0x40, 1], &[0x42]]).unwrap();
        assert_eq!(
            out,
            [0, 0, 0, 1, 0x40, 1, 0, 0, 0, 1, 0x42, 0, 0, 0, 1, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC]
        );
    }

    #[test]
    fn a_truncated_frame_is_refused() {
        assert_eq!(to_annex_b(&[0, 0, 0, 9, 1], 4, &[]), None);
        assert_eq!(to_annex_b(&[0, 0], 4, &[]), None);
        assert_eq!(to_annex_b(&[1], 0, &[]), None);
    }

    #[test]
    fn short_length_prefixes_work_too() {
        assert_eq!(to_annex_b(&[0, 1, 7], 2, &[]).unwrap(), [0, 0, 0, 1, 7]);
    }
}
