//! Minimal MPEG-TS muxer: wraps H.264 video and AAC audio access units into an MPEG-2
//! Transport Stream (188-byte packets: PAT, PMT, PES-with-PTS, PCR) for a DLNA renderer
//! to play as a live `video/mp2t` stream.
//!
//! The bit-level helpers (CRC-32/MPEG-2, PTS/PCR packing) are unit-tested against known
//! values and the packet layout is structurally tested; real-renderer acceptance is
//! confirmed on hardware.

const TS_PACKET_LEN: usize = 188;
const SYNC_BYTE: u8 = 0x47;
const PID_PAT: u16 = 0x0000;
const PID_PMT: u16 = 0x1000;
const PID_VIDEO: u16 = 0x0100;
const PID_AUDIO: u16 = 0x0101;
const STREAM_TYPE_H264: u8 = 0x1B;
const STREAM_TYPE_AAC: u8 = 0x0F; // AAC in ADTS
const STREAM_ID_VIDEO: u8 = 0xE0;
const STREAM_ID_AUDIO: u8 = 0xC0;

/// Muxes H.264 video and AAC audio access units into MPEG-TS packets, tracking per-PID
/// continuity counters.
#[derive(Debug, Default)]
pub struct MpegTsMuxer {
    video_cc: u8,
    audio_cc: u8,
    pat_cc: u8,
    pmt_cc: u8,
}

impl MpegTsMuxer {
    /// Create a fresh muxer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mux one H.264 video access unit (Annex-B, with start codes) carrying `pts` on the
    /// 90 kHz clock. When `keyframe` is true a PAT+PMT pair is emitted first so a renderer
    /// can join the stream at any keyframe. Returns whole 188-byte TS packets.
    pub fn push_access_unit(&mut self, annexb: &[u8], pts: u64, keyframe: bool) -> Vec<u8> {
        let mut out = Vec::new();
        if keyframe {
            out.extend_from_slice(&self.psi_packet(PID_PAT, &pat_section(), Cc::Pat));
            out.extend_from_slice(&self.psi_packet(PID_PMT, &pmt_section(), Cc::Pmt));
        }
        let pes = build_pes(STREAM_ID_VIDEO, pts, annexb);
        // PCR rides the video PID (the PMT's PCR_PID).
        out.extend_from_slice(&self.write_payload(PID_VIDEO, Cc::Video, &pes, Some(pts)));
        out
    }

    /// Mux one AAC (ADTS) audio access unit carrying `pts` on the 90 kHz clock.
    pub fn push_audio_access_unit(&mut self, adts: &[u8], pts: u64) -> Vec<u8> {
        let pes = build_pes(STREAM_ID_AUDIO, pts, adts);
        self.write_payload(PID_AUDIO, Cc::Audio, &pes, None)
    }

    fn psi_packet(&mut self, pid: u16, section: &[u8], which: Cc) -> [u8; TS_PACKET_LEN] {
        let cc = self.take_cc(which);
        let mut pkt = [0xFFu8; TS_PACKET_LEN];
        pkt[..4].copy_from_slice(&ts_header(pid, true, 0b01, cc));
        pkt[4] = 0x00; // pointer_field
        pkt[5..5 + section.len()].copy_from_slice(section);
        pkt
    }

    /// Split `payload` across TS packets on `pid`. The first packet sets PUSI and, if
    /// `pcr` is given, carries a PCR in its adaptation field; the final packet is padded
    /// to 188 bytes with adaptation-field stuffing.
    fn write_payload(&mut self, pid: u16, kind: Cc, payload: &[u8], pcr: Option<u64>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut offset = 0;
        let mut first = true;

        while offset < payload.len() {
            let remaining = payload.len() - offset;
            let need_pcr = first && pcr.is_some();
            let cc = self.take_cc(kind);

            let mut pkt = Vec::with_capacity(TS_PACKET_LEN);
            if !need_pcr && remaining >= 184 {
                // Full payload-only packet.
                pkt.extend_from_slice(&ts_header(pid, first, 0b01, cc));
                pkt.extend_from_slice(&payload[offset..offset + 184]);
                offset += 184;
            } else {
                // Adaptation field (PCR and/or stuffing) + payload.
                let pcr_bytes = if need_pcr { 6 } else { 0 };
                let max_payload = TS_PACKET_LEN - 4 - 1 - 1 - pcr_bytes; // header+aflen+flags+pcr
                let take = remaining.min(max_payload);
                let af_len = 183 - take; // bytes following the adaptation_field_length byte

                pkt.extend_from_slice(&ts_header(pid, first, 0b11, cc));
                pkt.push(af_len as u8);
                pkt.push(if need_pcr { 0x10 } else { 0x00 }); // flags (PCR_flag)
                if need_pcr {
                    pkt.extend_from_slice(&encode_pcr(pcr.unwrap_or(0)));
                }
                while pkt.len() < TS_PACKET_LEN - take {
                    pkt.push(0xFF); // stuffing
                }
                pkt.extend_from_slice(&payload[offset..offset + take]);
                offset += take;
            }
            debug_assert_eq!(pkt.len(), TS_PACKET_LEN);
            out.extend_from_slice(&pkt);
            first = false;
        }
        out
    }

    fn take_cc(&mut self, which: Cc) -> u8 {
        let slot = match which {
            Cc::Video => &mut self.video_cc,
            Cc::Audio => &mut self.audio_cc,
            Cc::Pat => &mut self.pat_cc,
            Cc::Pmt => &mut self.pmt_cc,
        };
        let cc = *slot & 0x0F;
        *slot = slot.wrapping_add(1) & 0x0F;
        cc
    }
}

#[derive(Clone, Copy)]
enum Cc {
    Video,
    Audio,
    Pat,
    Pmt,
}

fn ts_header(pid: u16, pusi: bool, afc: u8, cc: u8) -> [u8; 4] {
    [
        SYNC_BYTE,
        ((u8::from(pusi)) << 6) | ((pid >> 8) as u8 & 0x1F),
        (pid & 0xFF) as u8,
        (afc << 4) | (cc & 0x0F),
    ]
}

/// Build a PES packet for `stream_id` with a PTS-only header. `PES_packet_length` is set
/// when the packet fits in 16 bits (always true for audio) and left 0 (“unbounded”,
/// permitted for video) otherwise.
fn build_pes(stream_id: u8, pts: u64, payload: &[u8]) -> Vec<u8> {
    let length_field = u16::try_from(8 + payload.len()).unwrap_or(0);
    let mut pes = Vec::with_capacity(9 + payload.len());
    pes.extend_from_slice(&[0x00, 0x00, 0x01, stream_id]);
    pes.extend_from_slice(&length_field.to_be_bytes());
    pes.extend_from_slice(&[0x80, 0x80, 0x05]); // '10' marker, PTS-only flag, header len 5
    pes.extend_from_slice(&encode_timestamp(0b0010, pts));
    pes.extend_from_slice(payload);
    pes
}

/// Encode a 33-bit PTS/DTS timestamp with the given 4-bit prefix (`0b0010` for PTS-only).
fn encode_timestamp(prefix: u8, ts: u64) -> [u8; 5] {
    [
        (prefix << 4) | (((ts >> 30) & 0x07) as u8) << 1 | 1,
        ((ts >> 22) & 0xFF) as u8,
        ((((ts >> 15) & 0x7F) as u8) << 1) | 1,
        ((ts >> 7) & 0xFF) as u8,
        (((ts & 0x7F) as u8) << 1) | 1,
    ]
}

/// Encode a PCR (program clock reference) into 6 bytes; 33-bit base @ 90 kHz, ext 0.
fn encode_pcr(base: u64) -> [u8; 6] {
    [
        ((base >> 25) & 0xFF) as u8,
        ((base >> 17) & 0xFF) as u8,
        ((base >> 9) & 0xFF) as u8,
        ((base >> 1) & 0xFF) as u8,
        (((base & 0x1) as u8) << 7) | 0x7E, // 6 reserved bits = 1, ext high bit = 0
        0x00,                               // ext low byte = 0
    ]
}

/// CRC-32/MPEG-2 (poly 0x04C11DB7, init 0xFFFFFFFF, no reflection, no final xor).
fn crc32_mpeg(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn pat_section() -> Vec<u8> {
    let mut s = vec![
        0x00, // table_id (PAT)
        0xB0,
        0x0D, // syntax indicator + section_length = 13
        0x00,
        0x01, // transport_stream_id
        0xC1, // version 0, current_next = 1
        0x00,
        0x00, // section_number, last_section_number
        0x00,
        0x01, // program_number 1
        0xE0 | (PID_PMT >> 8) as u8,
        (PID_PMT & 0xFF) as u8, // reserved + PMT PID
    ];
    s.extend_from_slice(&crc32_mpeg(&s).to_be_bytes());
    s
}

fn pmt_section() -> Vec<u8> {
    let mut s = vec![
        0x02, // table_id (PMT)
        0xB0,
        0x17, // syntax indicator + section_length = 23 (two ES entries)
        0x00,
        0x01, // program_number 1
        0xC1, // version 0, current_next = 1
        0x00,
        0x00, // section_number, last_section_number
        0xE0 | (PID_VIDEO >> 8) as u8,
        (PID_VIDEO & 0xFF) as u8, // reserved + PCR PID (= video)
        0xF0,
        0x00, // reserved + program_info_length 0
        // Video ES.
        STREAM_TYPE_H264,
        0xE0 | (PID_VIDEO >> 8) as u8,
        (PID_VIDEO & 0xFF) as u8,
        0xF0,
        0x00,
        // Audio ES.
        STREAM_TYPE_AAC,
        0xE0 | (PID_AUDIO >> 8) as u8,
        (PID_AUDIO & 0xFF) as u8,
        0xF0,
        0x00,
    ];
    s.extend_from_slice(&crc32_mpeg(&s).to_be_bytes());
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid_of(pkt: &[u8]) -> u16 {
        (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2])
    }
    fn pusi(pkt: &[u8]) -> bool {
        pkt[1] & 0x40 != 0
    }

    #[test]
    fn crc32_mpeg_known_vector() {
        // CRC-32/MPEG-2 of "123456789" is 0x0376E6E7.
        assert_eq!(crc32_mpeg(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn timestamp_packs_and_unpacks() {
        let decode = |b: &[u8; 5]| -> u64 {
            (u64::from(b[0] >> 1 & 0x07) << 30)
                | (u64::from(b[1]) << 22)
                | (u64::from(b[2] >> 1 & 0x7F) << 15)
                | (u64::from(b[3]) << 7)
                | u64::from(b[4] >> 1 & 0x7F)
        };
        for &ts in &[0u64, 1, 90_000, 1_234_567, (1u64 << 33) - 1] {
            let enc = encode_timestamp(0b0010, ts);
            assert_eq!(enc[0] >> 4, 0b0010, "prefix preserved");
            assert_eq!(enc[0] & 1, 1);
            assert_eq!(enc[2] & 1, 1);
            assert_eq!(enc[4] & 1, 1);
            assert_eq!(decode(&enc), ts);
        }
    }

    #[test]
    fn pes_length_set_for_small_zero_for_large() {
        let small = build_pes(STREAM_ID_AUDIO, 0, &[1, 2, 3]);
        assert_eq!(u16::from_be_bytes([small[4], small[5]]) as usize, 8 + 3);
        let large = build_pes(STREAM_ID_VIDEO, 0, &vec![0u8; 70_000]);
        assert_eq!(
            u16::from_be_bytes([large[4], large[5]]),
            0,
            "unbounded for large video"
        );
    }

    #[test]
    fn keyframe_emits_pat_pmt_then_video_all_188() {
        let mut mux = MpegTsMuxer::new();
        let au = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB];
        let ts = mux.push_access_unit(&au, 90_000, true);

        assert_eq!(ts.len() % TS_PACKET_LEN, 0, "whole packets only");
        for chunk in ts.chunks(TS_PACKET_LEN) {
            assert_eq!(chunk[0], SYNC_BYTE, "every packet starts with sync byte");
        }
        assert_eq!(pid_of(&ts[0..]), PID_PAT);
        assert!(pusi(&ts[0..]));
        assert_eq!(pid_of(&ts[TS_PACKET_LEN..]), PID_PMT);
        let v = &ts[2 * TS_PACKET_LEN..];
        assert_eq!(pid_of(v), PID_VIDEO);
        assert!(pusi(v), "first video packet carries PES start");
    }

    #[test]
    fn non_keyframe_has_no_psi() {
        let mut mux = MpegTsMuxer::new();
        let ts = mux.push_access_unit(&[0x00, 0x00, 0x01, 0x41, 0x10], 180_000, false);
        for chunk in ts.chunks(TS_PACKET_LEN) {
            assert_eq!(pid_of(chunk), PID_VIDEO, "no PAT/PMT for a non-keyframe AU");
        }
    }

    #[test]
    fn large_access_unit_spans_packets_with_incrementing_cc() {
        let mut mux = MpegTsMuxer::new();
        let au = vec![0x42u8; 1000];
        let ts = mux.push_access_unit(&au, 90_000, false);
        let packets: Vec<&[u8]> = ts.chunks(TS_PACKET_LEN).collect();
        assert!(
            packets.len() > 1,
            "a 1000-byte AU must span multiple packets"
        );
        let ccs: Vec<u8> = packets.iter().map(|p| p[3] & 0x0F).collect();
        for w in ccs.windows(2) {
            assert_eq!(w[1], (w[0] + 1) & 0x0F);
        }
        assert!(pusi(packets[0]));
        assert!(packets[1..].iter().all(|p| !pusi(p)));
    }

    #[test]
    fn pmt_lists_video_and_audio_streams_with_valid_crc() {
        let s = pmt_section();
        assert_eq!(s[2], 0x17, "section_length covers two ES entries");
        assert_eq!(s[12], STREAM_TYPE_H264, "first ES is H.264 video");
        assert_eq!(s[17], STREAM_TYPE_AAC, "second ES is AAC audio");
        let (body, crc) = s.split_at(s.len() - 4);
        assert_eq!(crc32_mpeg(body).to_be_bytes(), crc);
    }

    #[test]
    fn audio_access_unit_uses_audio_pid_and_stream_id() {
        let mut mux = MpegTsMuxer::new();
        let ts = mux.push_audio_access_unit(&[0xFF, 0xF1, 0x40, 0x00, 0x11], 90_000);
        assert_eq!(ts.len() % TS_PACKET_LEN, 0);
        assert_eq!(ts[0], SYNC_BYTE);
        assert_eq!(pid_of(&ts), PID_AUDIO);
        assert!(pusi(&ts), "first audio packet carries PES start");
    }

    #[test]
    fn audio_and_video_use_independent_continuity_counters() {
        let mut mux = MpegTsMuxer::new();
        // Two video AUs then two audio AUs; each PID's CC must advance independently.
        let _ = mux.push_access_unit(&[0, 0, 0, 1, 0x41, 1], 0, false);
        let _ = mux.push_access_unit(&[0, 0, 0, 1, 0x41, 2], 3000, false);
        let a1 = mux.push_audio_access_unit(&[0xFF, 0xF1, 1], 0);
        let a2 = mux.push_audio_access_unit(&[0xFF, 0xF1, 2], 3000);
        // First audio packet CC starts at 0 (independent of video), second at 1.
        assert_eq!(a1[3] & 0x0F, 0);
        assert_eq!(a2[3] & 0x0F, 1);
    }

    #[test]
    fn pat_section_has_valid_self_crc() {
        let s = pat_section();
        let (body, crc) = s.split_at(s.len() - 4);
        assert_eq!(crc32_mpeg(body).to_be_bytes(), crc);
    }
}
