//! macOS H.264 encoder via VideoToolbox (`VTCompressionSession`).
//!
//! ⚠️ **DRAFT — authored without a macOS toolchain to compile against.** The structure and
//! the VideoToolbox/CoreMedia/CoreVideo call *sequence* are written against the inspected
//! objc2-0.3 signatures, but exact details (CF value construction, the output-handler
//! block signature, CMTime field access, the parameter-set loop) need a compile-and-fix
//! pass on macOS — search `VERIFY` below. `cfg(target_os = "macos")`, so the Linux/CI
//! build excludes it (and never type-checks it).
//!
//! Flow: NV12 `CapturedFrame` → `CVPixelBuffer` → `encode_frame_with_output_handler` →
//! the handler receives a `CMSampleBuffer` → we read its AVCC NAL units, convert to
//! Annex-B (start codes), prepend SPS/PPS on IDR frames, and push an [`EncodedFrame`].

use std::ffi::c_char;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2_core_foundation::{CFNumber, CFRetained, kCFBooleanFalse, kCFBooleanTrue};
use objc2_core_media::{
    CMBlockBufferGetDataPointer, CMSampleBuffer, CMSampleBufferGetDataBuffer,
    CMSampleBufferGetFormatDescription, CMSampleBufferGetPresentationTimeStamp, CMTime, CMTimeMake,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMTimeInvalid, kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_RealTime,
};

use hc_capture::CapturedFrame;
use hc_core::{Error, Result};

use crate::{EncodedFrame, EncoderConfig};

const PTS_TIMESCALE: i32 = 90_000;

type OutputQueue = Arc<Mutex<Vec<EncodedFrame>>>;

pub(crate) struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    output: OutputQueue,
}

impl VtEncoder {
    pub(crate) fn new(config: EncoderConfig) -> Result<Self> {
        let mut session_ptr: *mut VTCompressionSession = std::ptr::null_mut();
        let status = unsafe {
            VTCompressionSession::create(
                None,
                config.width as i32,
                config.height as i32,
                kCMVideoCodecType_H264,
                None,
                None,
                None,
                None, // no C callback — we use the per-frame output handler
                std::ptr::null_mut(),
                NonNull::from(&mut session_ptr),
            )
        };
        if status != 0 {
            return Err(Error::Encode(format!(
                "VTCompressionSessionCreate failed: OSStatus {status}"
            )));
        }
        let session = unsafe { CFRetained::from_raw(NonNull::new(session_ptr).unwrap()) };

        unsafe {
            set_bool(&session, kVTCompressionPropertyKey_RealTime, true);
            set_bool(
                &session,
                kVTCompressionPropertyKey_AllowFrameReordering,
                false,
            );
            set_i32(
                &session,
                kVTCompressionPropertyKey_AverageBitRate,
                (config.bitrate_kbps as i32) * 1000,
            );
            set_i32(
                &session,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                config.keyframe_interval as i32,
            );
            let _ = session.prepare_to_encode_frames();
        }

        Ok(Self {
            session,
            output: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub(crate) fn encode(&mut self, frame: &CapturedFrame, pts: u64) -> Result<Vec<EncodedFrame>> {
        let pixel_buffer = nv12_to_pixel_buffer(frame)?;
        let pts_time = CMTimeMake(pts as i64, PTS_TIMESCALE);
        let duration = CMTimeMake(PTS_TIMESCALE as i64 / 60, PTS_TIMESCALE);

        let queue = self.output.clone();
        // VERIFY: exact block arg types (VTEncodeInfoFlags / sample pointer) on macOS.
        let handler = RcBlock::new(
            move |status: i32, _flags: u32, sample: *mut CMSampleBuffer| {
                if status == 0
                    && !sample.is_null()
                    && let Some(encoded) = unsafe { sample_to_encoded(&*sample) }
                {
                    queue.lock().expect("encoder output mutex").push(encoded);
                }
            },
        );

        let mut info_flags: u32 = 0;
        let status = unsafe {
            self.session.encode_frame_with_output_handler(
                &pixel_buffer,
                pts_time,
                duration,
                None,
                NonNull::from(&mut info_flags),
                &handler,
            )
        };
        if status != 0 {
            return Err(Error::Encode(format!(
                "VTCompressionSessionEncodeFrame failed: OSStatus {status}"
            )));
        }
        Ok(self.drain())
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<EncodedFrame>> {
        unsafe {
            let _ = self.session.complete_frames(kCMTimeInvalid);
        }
        Ok(self.drain())
    }

    fn drain(&self) -> Vec<EncodedFrame> {
        std::mem::take(&mut self.output.lock().expect("encoder output mutex"))
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        unsafe { self.session.invalidate() };
    }
}

// --- property setters (VERIFY the CF value argument forms on macOS) ---

unsafe fn set_bool(
    session: &VTCompressionSession,
    key: &objc2_core_foundation::CFString,
    value: bool,
) {
    let v = if value {
        kCFBooleanTrue
    } else {
        kCFBooleanFalse
    };
    unsafe { VTSessionSetProperty(session, key, v.map(|b| b as _)) };
}

unsafe fn set_i32(
    session: &VTCompressionSession,
    key: &objc2_core_foundation::CFString,
    value: i32,
) {
    let number = CFNumber::new_i32(value);
    unsafe { VTSessionSetProperty(session, key, Some(number.as_ref() as _)) };
}

// --- NV12 → CVPixelBuffer ---

fn nv12_to_pixel_buffer(frame: &CapturedFrame) -> Result<CFRetained<CVPixelBuffer>> {
    let mut pb_out: *mut CVPixelBuffer = std::ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            frame.width as usize,
            frame.height as usize,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            None,
            NonNull::from(&mut pb_out),
        )
    };
    if status != 0 {
        return Err(Error::Encode(format!(
            "CVPixelBufferCreate failed: {status}"
        )));
    }
    let pixel_buffer = unsafe { CFRetained::from_raw(NonNull::new(pb_out).unwrap()) };

    unsafe {
        let flags = CVPixelBufferLockFlags::empty(); // 0 = read/write
        CVPixelBufferLockBaseAddress(&pixel_buffer, flags);
        // Plane 0 = Y (width bytes/row, height rows); plane 1 = interleaved CbCr (width
        // bytes/row, height/2 rows).
        copy_plane(
            &pixel_buffer,
            0,
            &frame.planes.y,
            frame.planes.y_stride as usize,
            frame.width as usize,
            frame.height as usize,
        );
        copy_plane(
            &pixel_buffer,
            1,
            &frame.planes.uv,
            frame.planes.uv_stride as usize,
            frame.width as usize,
            (frame.height as usize) / 2,
        );
        CVPixelBufferUnlockBaseAddress(&pixel_buffer, flags);
    }
    Ok(pixel_buffer)
}

unsafe fn copy_plane(
    pb: &CVPixelBuffer,
    plane: usize,
    src: &[u8],
    src_stride: usize,
    bytes_per_row: usize,
    rows: usize,
) {
    let dst = unsafe { CVPixelBufferGetBaseAddressOfPlane(pb, plane) } as *mut u8;
    if dst.is_null() {
        return;
    }
    let dst_stride = unsafe { CVPixelBufferGetBytesPerRowOfPlane(pb, plane) };
    let copy = bytes_per_row.min(src_stride).min(dst_stride);
    for row in 0..rows {
        let s = row * src_stride;
        let d = row * dst_stride;
        if s + copy <= src.len() {
            unsafe { std::ptr::copy_nonoverlapping(src.as_ptr().add(s), dst.add(d), copy) };
        }
    }
}

// --- CMSampleBuffer → Annex-B EncodedFrame ---

unsafe fn sample_to_encoded(sample: &CMSampleBuffer) -> Option<EncodedFrame> {
    let pts = cmtime_to_90k(unsafe { CMSampleBufferGetPresentationTimeStamp(sample) });

    let block = unsafe { CMSampleBufferGetDataBuffer(sample) }?;
    let mut total: usize = 0;
    let mut ptr: *mut c_char = std::ptr::null_mut();
    let st = unsafe {
        CMBlockBufferGetDataPointer(&block, 0, std::ptr::null_mut(), &mut total, &mut ptr)
    };
    if st != 0 || ptr.is_null() {
        return None;
    }
    let avcc = unsafe { std::slice::from_raw_parts(ptr as *const u8, total) };

    // Detect an IDR by NAL type 5 (more robust than poking sample attachments).
    let keyframe = avcc_nal_types(avcc).any(|t| t == 5);

    let mut out = Vec::with_capacity(total + 64);
    if keyframe {
        // Prepend SPS/PPS from the format description so the TV can decode at the join.
        if let Some(fmt) = unsafe { CMSampleBufferGetFormatDescription(sample) } {
            for set in h264_parameter_sets(&fmt) {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(&set);
            }
        }
    }
    // AVCC (4-byte big-endian length prefixes) → Annex-B (00 00 00 01 start codes).
    let mut i = 0;
    while i + 4 <= avcc.len() {
        let nal_len = u32::from_be_bytes([avcc[i], avcc[i + 1], avcc[i + 2], avcc[i + 3]]) as usize;
        i += 4;
        if i + nal_len > avcc.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[i..i + nal_len]);
        i += nal_len;
    }

    Some(EncodedFrame {
        data: out,
        pts,
        keyframe,
    })
}

/// Iterate the NAL types in an AVCC buffer (assumes 4-byte length prefixes).
fn avcc_nal_types(avcc: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 4 <= avcc.len() {
            let len = u32::from_be_bytes([avcc[i], avcc[i + 1], avcc[i + 2], avcc[i + 3]]) as usize;
            let header = i + 4;
            i = header + len;
            if header < avcc.len() {
                return Some(avcc[header] & 0x1F);
            }
        }
        None
    })
}

fn cmtime_to_90k(t: CMTime) -> u64 {
    // VERIFY field names on macOS (objc2 exposes CMTime.value / CMTime.timescale).
    if t.timescale <= 0 {
        return 0;
    }
    let v = (t.value as i128) * i128::from(PTS_TIMESCALE) / i128::from(t.timescale);
    u64::try_from(v.max(0)).unwrap_or(0)
}

/// Pull the SPS/PPS parameter sets out of an H.264 `CMVideoFormatDescription`.
fn h264_parameter_sets(fmt: &objc2_core_media::CMFormatDescription) -> Vec<Vec<u8>> {
    let mut sets = Vec::new();
    let mut index = 0usize;
    loop {
        let mut ptr: *const u8 = std::ptr::null();
        let mut size: usize = 0;
        let mut count: usize = 0;
        let st = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                fmt,
                index,
                &mut ptr,
                &mut size,
                &mut count,
                std::ptr::null_mut(),
            )
        };
        if st != 0 || ptr.is_null() {
            break;
        }
        sets.push(unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec());
        index += 1;
        if index >= count {
            break;
        }
    }
    sets
}
