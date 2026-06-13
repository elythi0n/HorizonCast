//! Windows H.264 encoding via Media Foundation.
//!
//! Drives the system H.264 encoder MFT (`CLSID_CMSH264EncoderMFT`, a synchronous MFT)
//! with NV12 input samples and reads back Annex-B H.264 access units for the MPEG-TS muxer.
//! The MS H.264 encoder emits a byte-stream (start codes) with SPS/PPS in-band before each
//! IDR, which is exactly what `LiveMirrorSession::push_access_unit` expects.
//!
//! NOTE: `cfg(target_os = "windows")` — built and validated on Windows, not in the Linux
//! dev env. COM/MF objects are created and used on a single thread (the capture→encode
//! blocking thread), so the type is intentionally not `Send`.
//!
//! VERIFY markers flag spots that should be confirmed against a real Windows build/run.

use hc_capture::CapturedFrame;
use hc_core::{Error, Result};

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::core::{GUID, Interface};

use crate::{EncodedFrame, EncoderConfig};

/// CLSID of the Microsoft H.264 Video Encoder MFT. (Not exported as a named constant by
/// the `windows` crate, so we spell out the GUID: {6CA50344-051A-4DED-9779-A43305165E35}.)
const CLSID_CMSH264_ENCODER_MFT: GUID = GUID::from_u128(0x6ca50344_051a_4ded_9779_a43305165e35);

/// Map a Windows error into our error type.
fn mf_err(context: &str, e: windows::core::Error) -> Error {
    Error::Encode(format!("{context}: {e}"))
}

/// Pack two u32s into the u64 form MF uses for size/ratio attributes (high | low).
fn pack_u64(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

pub(crate) struct MfEncoder {
    transform: IMFTransform,
    /// 90 kHz frame duration, used for sample timing.
    frame_duration_100ns: i64,
    width: u32,
    height: u32,
    /// Whether ProcessOutput must allocate the output sample (vs the MFT providing it).
    output_provides_samples: bool,
    output_buffer_size: u32,
}

// The MFT and its buffers never leave the capture/encode thread.
impl MfEncoder {
    pub(crate) fn new(config: EncoderConfig) -> Result<Self> {
        unsafe {
            // Per-thread COM + MF init. (CoInitializeEx returns S_FALSE if already inited
            // on this thread — both are success.)
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| mf_err("MFStartup", e))?;

            let transform: IMFTransform =
                CoCreateInstance(&CLSID_CMSH264_ENCODER_MFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| mf_err("create H.264 encoder MFT", e))?;

            // Output type (H.264) MUST be set before the input type.
            let out_type = MFCreateMediaType().map_err(|e| mf_err("MFCreateMediaType(out)", e))?;
            out_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| mf_err("out major type", e))?;
            out_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(|e| mf_err("out subtype", e))?;
            out_type
                .SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_kbps.saturating_mul(1000))
                .map_err(|e| mf_err("out bitrate", e))?;
            out_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| mf_err("out interlace", e))?;
            out_type
                .SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(config.width, config.height))
                .map_err(|e| mf_err("out frame size", e))?;
            out_type
                .SetUINT64(&MF_MT_FRAME_RATE, pack_u64(config.fps, 1))
                .map_err(|e| mf_err("out frame rate", e))?;
            out_type
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))
                .map_err(|e| mf_err("out par", e))?;
            transform
                .SetOutputType(0, &out_type, 0)
                .map_err(|e| mf_err("SetOutputType", e))?;

            // Input type (NV12).
            let in_type = MFCreateMediaType().map_err(|e| mf_err("MFCreateMediaType(in)", e))?;
            in_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| mf_err("in major type", e))?;
            in_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(|e| mf_err("in subtype", e))?;
            in_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| mf_err("in interlace", e))?;
            in_type
                .SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(config.width, config.height))
                .map_err(|e| mf_err("in frame size", e))?;
            in_type
                .SetUINT64(&MF_MT_FRAME_RATE, pack_u64(config.fps, 1))
                .map_err(|e| mf_err("in frame rate", e))?;
            in_type
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))
                .map_err(|e| mf_err("in par", e))?;
            transform
                .SetInputType(0, &in_type, 0)
                .map_err(|e| mf_err("SetInputType", e))?;

            // Low-latency tuning + frequent keyframes (best-effort via ICodecAPI).
            tune_low_latency(&transform, config.keyframe_interval);

            // How does the MFT want output samples?
            let out_info = transform
                .GetOutputStreamInfo(0)
                .map_err(|e| mf_err("GetOutputStreamInfo", e))?;
            let output_provides_samples = (out_info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32))
                != 0;

            // Start streaming.
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| mf_err("BEGIN_STREAMING", e))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| mf_err("START_OF_STREAM", e))?;

            Ok(Self {
                transform,
                frame_duration_100ns: 10_000_000 / i64::from(config.fps.max(1)),
                width: config.width,
                height: config.height,
                output_provides_samples,
                output_buffer_size: out_info.cbSize.max(config.width * config.height),
            })
        }
    }

    pub(crate) fn encode(&mut self, frame: &CapturedFrame, pts: u64) -> Result<Vec<EncodedFrame>> {
        unsafe {
            let sample = self.make_input_sample(frame, pts)?;
            // Synchronous MFT: feed one input, then drain all available output.
            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(|e| mf_err("ProcessInput", e))?;
            self.drain()
        }
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<EncodedFrame>> {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
            self.drain()
        }
    }

    /// Build an NV12 input sample (contiguous Y then UV) stamped with `pts` (90 kHz).
    unsafe fn make_input_sample(&self, frame: &CapturedFrame, pts: u64) -> Result<IMFSample> {
        unsafe {
            let y_rows = self.height as usize;
            let row = self.width as usize;
            let total = row * y_rows + row * (y_rows / 2);

            let buffer = MFCreateMemoryBuffer(total as u32)
                .map_err(|e| mf_err("MFCreateMemoryBuffer", e))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer
                .Lock(&mut ptr, None, None)
                .map_err(|e| mf_err("buffer Lock", e))?;
            // Copy Y plane then UV plane, tightening any stride padding to `width`.
            {
                let p = &frame.planes;
                let dst = std::slice::from_raw_parts_mut(ptr, total);
                let mut o = 0usize;
                for r in 0..y_rows {
                    let s = r * p.y_stride as usize;
                    dst[o..o + row].copy_from_slice(&p.y[s..s + row]);
                    o += row;
                }
                for r in 0..(y_rows / 2) {
                    let s = r * p.uv_stride as usize;
                    dst[o..o + row].copy_from_slice(&p.uv[s..s + row]);
                    o += row;
                }
            }
            buffer
                .SetCurrentLength(total as u32)
                .map_err(|e| mf_err("SetCurrentLength", e))?;
            buffer.Unlock().map_err(|e| mf_err("buffer Unlock", e))?;

            let sample = MFCreateSample().map_err(|e| mf_err("MFCreateSample", e))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| mf_err("AddBuffer", e))?;
            // 90 kHz → 100 ns units: t100 = pts * 1e7 / 90000 = pts * 1000 / 9.
            let time_100ns = (pts as i64).saturating_mul(1000) / 9;
            sample
                .SetSampleTime(time_100ns)
                .map_err(|e| mf_err("SetSampleTime", e))?;
            sample
                .SetSampleDuration(self.frame_duration_100ns)
                .map_err(|e| mf_err("SetSampleDuration", e))?;
            Ok(sample)
        }
    }

    /// Pull all currently-available output access units from the MFT.
    unsafe fn drain(&mut self) -> Result<Vec<EncodedFrame>> {
        unsafe {
            let mut out = Vec::new();
            loop {
                // Provide an output sample unless the MFT allocates its own.
                let sample = if self.output_provides_samples {
                    None
                } else {
                    let buf = MFCreateMemoryBuffer(self.output_buffer_size)
                        .map_err(|e| mf_err("out MFCreateMemoryBuffer", e))?;
                    let s = MFCreateSample().map_err(|e| mf_err("out MFCreateSample", e))?;
                    s.AddBuffer(&buf).map_err(|e| mf_err("out AddBuffer", e))?;
                    Some(s)
                };

                let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(sample),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let hr = self.transform.ProcessOutput(0, &mut buffers, &mut status);

                match hr {
                    Ok(()) => {
                        // ManuallyDrop — take the sample back out.
                        let produced = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                        if let Some(s) = produced {
                            out.push(read_access_unit(&s)?);
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        // Output format changed; re-fetch and continue (rare for an encoder).
                        let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                        continue;
                    }
                    Err(e) => return Err(mf_err("ProcessOutput", e)),
                }
            }
            Ok(out)
        }
    }
}

/// Best-effort low-latency + GOP tuning via the MFT's `ICodecAPI`. All optional; failures
/// are ignored so encoding still works on encoders that don't expose these knobs.
unsafe fn tune_low_latency(transform: &IMFTransform, keyframe_interval: u32) {
    use windows::Win32::System::Variant::VARIANT;
    let Ok(codec) = transform.cast::<ICodecAPI>() else {
        return;
    };
    unsafe {
        // Low-latency mode (no B-frames, minimal buffering).
        let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
        // GOP size = keyframe interval (frames between IDRs).
        let _ = codec.SetValue(
            &CODECAPI_AVEncMPVGOPSize,
            &VARIANT::from(keyframe_interval as i32),
        );
    }
}

/// Read an output `IMFSample` into an [`EncodedFrame`] (Annex-B bytes, 90 kHz PTS, keyframe).
unsafe fn read_access_unit(sample: &IMFSample) -> Result<EncodedFrame> {
    unsafe {
        // Keyframe? The encoder marks IDR samples with the CleanPoint attribute.
        let keyframe = sample
            .GetUINT32(&MFSampleExtension_CleanPoint)
            .map(|v| v != 0)
            .unwrap_or(false);

        // 100 ns sample time → 90 kHz PTS: pts = t100 * 90000 / 1e7 = t100 * 9 / 1000.
        let pts = sample
            .GetSampleTime()
            .map(|t| (t.max(0) as u64).saturating_mul(9) / 1000)
            .unwrap_or(0);

        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| mf_err("ConvertToContiguousBuffer", e))?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        buffer
            .Lock(&mut ptr, None, Some(&mut len))
            .map_err(|e| mf_err("out buffer Lock", e))?;
        let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        let _ = buffer.Unlock();

        Ok(EncodedFrame {
            data,
            pts,
            keyframe,
        })
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}
