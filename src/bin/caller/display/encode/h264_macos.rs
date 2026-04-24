//! H264 hardware encoder using Apple VideoToolbox via `shiguredo_video_toolbox`.
//!
//! VideoToolbox outputs AVCC-format NAL units (length-prefixed).  WebRTC
//! expects Annex-B (start-code-prefixed).  This module converts on the fly.
//!
//! The encoder is configured for real-time, low-latency encoding with the
//! Baseline profile (widest browser compatibility) and CAVLC entropy coding.

use super::{EncodedPacket, Encoder, PayloadSpec};
use shiguredo_video_toolbox::{
    CodecConfig, EncodeOptions, EncoderConfig, FrameData, H264EncoderConfig, H264EntropyMode,
    H264Profile, PixelFormat,
};
use std::num::NonZeroU32;
use std::time::Duration;

/// Annex-B start code prefix for NAL units.
const ANNEXB_START_CODE: &[u8] = &[0x00, 0x00, 0x00, 0x01];

/// VideoToolbox H264 encoder implementing the `Encoder` trait.
///
/// Wraps `shiguredo_video_toolbox::Encoder` and converts its AVCC output
/// to Annex-B format suitable for WebRTC.
pub struct VideoToolboxEncoder {
    inner: shiguredo_video_toolbox::Encoder,
    width: u32,
    height: u32,
    frame_count: u64,
    pts_offset: u64,
    /// Cached canonical payload spec. VideoToolbox is configured above
    /// for H.264 Baseline, so every emitted packet matches
    /// [`PayloadSpec::h264_constrained_baseline`]: profile-level-id
    /// `42e01f`, packetization-mode 1.
    payload_spec: PayloadSpec,
}

// shiguredo_video_toolbox::Encoder contains VideoToolbox session pointers
// that are safe to move across threads (the VT session is thread-safe).
unsafe impl Send for VideoToolboxEncoder {}

impl VideoToolboxEncoder {
    /// Create a new VideoToolbox H264 encoder.
    ///
    /// Returns `Err` if VideoToolbox is unavailable or rejects the configuration
    /// (e.g. unsupported resolution on this hardware).
    pub fn new(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, String> {
        let config = EncoderConfig {
            width,
            height,
            pixel_format: PixelFormat::I420,
            codec: CodecConfig::H264(H264EncoderConfig {
                profile: H264Profile::Baseline,
                entropy_mode: H264EntropyMode::Cavlc,
            }),
            average_bitrate: Some(bitrate_kbps as u64 * 1000),
            fps_numerator: 30,
            fps_denominator: 1,
            real_time: true,
            prioritize_encoding_speed_over_quality: true,
            maximize_power_efficiency: false,
            allow_frame_reordering: false,
            allow_temporal_compression: true,
            max_key_frame_interval: Some(NonZeroU32::new(30).unwrap()), // 1s at 30fps
            max_key_frame_interval_duration: Some(Duration::from_secs(1)),
            max_frame_delay_count: Some(NonZeroU32::new(1).unwrap()),
        };

        let inner = shiguredo_video_toolbox::Encoder::new(config)
            .map_err(|e| format!("VideoToolbox init: {e}"))?;

        Ok(Self {
            inner,
            width,
            height,
            frame_count: 0,
            pts_offset: 0,
            payload_spec: PayloadSpec::h264_constrained_baseline(),
        })
    }
}

impl Encoder for VideoToolboxEncoder {
    fn encode(
        &mut self,
        i420: &[u8],
        duration_ms: u64,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedPacket>, String> {
        let w = self.width as usize;
        let h = self.height as usize;
        let uv_w = (w + 1) / 2;
        let uv_h = (h + 1) / 2;
        let y_size = w * h;
        let uv_size = uv_w * uv_h;

        if i420.len() < y_size + 2 * uv_size {
            return Err(format!(
                "I420 buffer too small: {} < {}",
                i420.len(),
                y_size + 2 * uv_size,
            ));
        }

        let y = &i420[..y_size];
        let u = &i420[y_size..y_size + uv_size];
        let v = &i420[y_size + uv_size..y_size + 2 * uv_size];

        let frame_data = FrameData::I420 { y, u, v };

        let options = EncodeOptions {
            force_key_frame: force_keyframe || self.frame_count == 0,
            ..Default::default()
        };

        self.inner
            .encode(&frame_data, &options)
            .map_err(|e| format!("VT encode: {e}"))?;

        let pts = self.pts_offset;
        self.pts_offset += duration_ms;
        self.frame_count += 1;

        // Drain all completed frames from the encoder.
        let mut out = Vec::new();
        loop {
            match self.inner.next_frame() {
                Ok(Some(frame)) => {
                    let annexb = avcc_to_annexb(
                        &frame.data,
                        &frame.sps_list,
                        &frame.pps_list,
                        frame.keyframe,
                    );
                    out.push(EncodedPacket {
                        data: annexb,
                        pts_ms: pts,
                        duration_ms,
                        is_keyframe: frame.keyframe,
                        payload_spec: self.payload_spec.clone(),
                    });
                }
                Ok(None) => break,
                Err(e) => return Err(format!("VT next_frame: {e}")),
            }
        }

        Ok(out)
    }

    fn codec_mime(&self) -> &'static str {
        "video/H264"
    }

    fn payload_spec(&self) -> &PayloadSpec {
        &self.payload_spec
    }
}

/// Convert AVCC-format H264 data to Annex-B format.
///
/// AVCC: each NAL unit is prefixed with a 4-byte big-endian length.
/// Annex-B: each NAL unit is prefixed with `00 00 00 01`.
///
/// For keyframes, SPS and PPS are prepended before the IDR slice.
fn avcc_to_annexb(
    avcc_data: &[u8],
    sps_list: &[Vec<u8>],
    pps_list: &[Vec<u8>],
    is_keyframe: bool,
) -> Vec<u8> {
    // Estimate output size: same as input + start codes + parameter sets.
    let param_size = if is_keyframe {
        sps_list.iter().map(|s| 4 + s.len()).sum::<usize>()
            + pps_list.iter().map(|p| 4 + p.len()).sum::<usize>()
    } else {
        0
    };
    let mut out = Vec::with_capacity(avcc_data.len() + param_size + 32);

    // Prepend SPS/PPS for keyframes.
    if is_keyframe {
        for sps in sps_list {
            out.extend_from_slice(ANNEXB_START_CODE);
            out.extend_from_slice(sps);
        }
        for pps in pps_list {
            out.extend_from_slice(ANNEXB_START_CODE);
            out.extend_from_slice(pps);
        }
    }

    // Convert length-prefixed NAL units to start-code-prefixed.
    let mut pos = 0;
    while pos + 4 <= avcc_data.len() {
        let nal_len = u32::from_be_bytes([
            avcc_data[pos],
            avcc_data[pos + 1],
            avcc_data[pos + 2],
            avcc_data[pos + 3],
        ]) as usize;
        pos += 4;

        if pos + nal_len > avcc_data.len() {
            // Truncated NAL -- stop parsing.
            break;
        }

        out.extend_from_slice(ANNEXB_START_CODE);
        out.extend_from_slice(&avcc_data[pos..pos + nal_len]);
        pos += nal_len;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_to_annexb_keyframe() {
        let sps = vec![0x67, 0x42, 0x00, 0x0A]; // fake SPS
        let pps = vec![0x68, 0xCE, 0x38, 0x80]; // fake PPS

        // A single 3-byte NAL unit in AVCC format.
        let nal = vec![0x65, 0xAA, 0xBB]; // IDR slice
        let mut avcc = Vec::new();
        avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        avcc.extend_from_slice(&nal);

        let result = avcc_to_annexb(&avcc, &[sps.clone()], &[pps.clone()], true);

        // Expected: start_code + SPS + start_code + PPS + start_code + NAL
        let mut expected = Vec::new();
        expected.extend_from_slice(ANNEXB_START_CODE);
        expected.extend_from_slice(&sps);
        expected.extend_from_slice(ANNEXB_START_CODE);
        expected.extend_from_slice(&pps);
        expected.extend_from_slice(ANNEXB_START_CODE);
        expected.extend_from_slice(&nal);

        assert_eq!(result, expected);
    }

    #[test]
    fn avcc_to_annexb_non_keyframe() {
        let nal = vec![0x41, 0xCC, 0xDD]; // non-IDR
        let mut avcc = Vec::new();
        avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        avcc.extend_from_slice(&nal);

        let result = avcc_to_annexb(&avcc, &[], &[], false);

        let mut expected = Vec::new();
        expected.extend_from_slice(ANNEXB_START_CODE);
        expected.extend_from_slice(&nal);

        assert_eq!(result, expected);
    }

    #[test]
    fn avcc_to_annexb_multiple_nals() {
        let nal1 = vec![0x41, 0x01];
        let nal2 = vec![0x41, 0x02, 0x03];
        let mut avcc = Vec::new();
        avcc.extend_from_slice(&(nal1.len() as u32).to_be_bytes());
        avcc.extend_from_slice(&nal1);
        avcc.extend_from_slice(&(nal2.len() as u32).to_be_bytes());
        avcc.extend_from_slice(&nal2);

        let result = avcc_to_annexb(&avcc, &[], &[], false);

        let mut expected = Vec::new();
        expected.extend_from_slice(ANNEXB_START_CODE);
        expected.extend_from_slice(&nal1);
        expected.extend_from_slice(ANNEXB_START_CODE);
        expected.extend_from_slice(&nal2);

        assert_eq!(result, expected);
    }
}
