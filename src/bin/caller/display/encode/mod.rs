use super::EncodedFrame;

#[cfg(target_os = "linux")]
pub mod h264_linux;
#[cfg(target_os = "macos")]
pub mod h264_macos;

pub const MIME_TYPE_VP8: &str = "video/VP8";
pub const MIME_TYPE_H264: &str = "video/H264";

// ---------------------------------------------------------------------------
// Encoder trait
// ---------------------------------------------------------------------------

/// Codec-agnostic encoder interface.
///
/// Implementations accept I420 frames and produce encoded packets.  The trait
/// is `Send + 'static` so the encoder can live on a dedicated `std::thread`.
pub trait Encoder: Send + 'static {
    /// Encode one I420 frame.  Returns zero or more encoded packets.
    ///
    /// `duration_ms` is the display duration of this frame (typically 1000/fps).
    fn encode(&mut self, i420: &[u8], duration_ms: u64) -> Result<Vec<EncodedPacket>, String>;

    /// The MIME type of the encoded output (e.g. `"video/VP8"`, `"video/H264"`).
    fn codec_mime(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// Codec selection
// ---------------------------------------------------------------------------

/// Which codec was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecChoice {
    Vp8,
    H264,
}

impl CodecChoice {
    pub fn mime(&self) -> &'static str {
        match self {
            CodecChoice::Vp8 => "video/VP8",
            CodecChoice::H264 => "video/H264",
        }
    }
}

/// Try to create an encoder for the given codec and resolution.
///
/// Returns `(encoder, codec)` on success.
pub fn select_codec_for_mime(
    mime: &str,
    width: u32,
    height: u32,
    bitrate_kbps: u32,
) -> Result<(Box<dyn Encoder>, CodecChoice), String> {
    match mime {
        "video/H264" => create_h264_encoder(width, height, bitrate_kbps),
        _ => {
            let enc = Vp8Encoder::new(width, height, bitrate_kbps)?;
            Ok((Box::new(enc), CodecChoice::Vp8))
        }
    }
}

// ---------------------------------------------------------------------------
// H264 fmtp parsing
// ---------------------------------------------------------------------------

/// Parsed H264 profile parameters from SDP `a=fmtp:` lines.
///
/// The `profile_level_id` is the 3-byte hex string from `a=fmtp:N
/// profile-level-id=XXYYZZ` where XX is `profile_idc`, YY is constraint
/// flags, and ZZ is the level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264FmtpProfile {
    /// RTP payload type (matches the number in `a=rtpmap:N H264/90000`).
    pub payload_type: u32,
    /// 6-hex-digit profile-level-id (e.g. `"42e01f"`), lowercased.
    /// Empty string if the fmtp line did not contain one.
    pub profile_level_id: String,
    /// packetization-mode value (0 or 1 in practice).
    pub packetization_mode: u32,
}

/// Collect H264 payload types from `a=rtpmap:` lines, then parse their
/// corresponding `a=fmtp:` lines for `profile-level-id` and
/// `packetization-mode`.
///
/// H264 payload types that have no `a=fmtp:` line are returned with an
/// empty `profile_level_id` and `packetization_mode = 0` (the RFC 6184
/// defaults, which map to Constrained Baseline).
pub fn parse_h264_fmtp(sdp: &str) -> Vec<H264FmtpProfile> {
    // Step 1: collect payload types that map to H264.
    let mut h264_pts = Vec::new();
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            let mut parts = rest.split_whitespace();
            if let (Some(pt_str), Some(codec_clock)) = (parts.next(), parts.next()) {
                if let Some(codec_name) = codec_clock.split('/').next() {
                    if codec_name.eq_ignore_ascii_case("H264") {
                        if let Ok(pt) = pt_str.parse::<u32>() {
                            h264_pts.push(pt);
                        }
                    }
                }
            }
        }
    }

    // Step 2: parse fmtp lines for those payload types.
    let mut fmtp_map: std::collections::HashMap<u32, (String, u32)> =
        std::collections::HashMap::new();
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("a=fmtp:") {
            // Format: a=fmtp:<pt> <param=value>[;<param=value>]*
            let mut parts = rest.splitn(2, ' ');
            let pt_str = match parts.next() {
                Some(s) => s,
                None => continue,
            };
            let pt: u32 = match pt_str.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            if !h264_pts.contains(&pt) {
                continue;
            }
            let params_str = parts.next().unwrap_or("");
            let mut profile_level_id = String::new();
            let mut packetization_mode = 0u32;
            for kv in params_str.split(';') {
                let kv = kv.trim();
                if let Some((key, val)) = kv.split_once('=') {
                    let key = key.trim().to_lowercase();
                    let val = val.trim();
                    if key == "profile-level-id" {
                        profile_level_id = val.to_lowercase();
                    } else if key == "packetization-mode" {
                        packetization_mode = val.parse().unwrap_or(0);
                    }
                }
            }
            fmtp_map.insert(pt, (profile_level_id, packetization_mode));
        }
    }

    // Step 3: build results, using defaults for PTs without fmtp.
    h264_pts
        .into_iter()
        .map(|pt| {
            let (profile_level_id, packetization_mode) = fmtp_map
                .remove(&pt)
                .unwrap_or_else(|| (String::new(), 0));
            H264FmtpProfile {
                payload_type: pt,
                profile_level_id,
                packetization_mode,
            }
        })
        .collect()
}

/// Check whether a parsed H264 fmtp profile is compatible with our encoder.
///
/// Our encoders produce Baseline (profile_idc 66 / 0x42).  We accept:
/// - `profile_idc` 66 (0x42): Baseline or Constrained Baseline (depending
///   on constraint flags).
/// - Empty `profile_level_id`: treated as the RFC 6184 default (Constrained
///   Baseline, Level 1), which is compatible.
/// - `packetization_mode` 0 or 1: mode 1 is what we produce (non-interleaved
///   NAL units), mode 0 (single NAL unit) is a strict subset.
///
/// We do *not* accept High (100), Main (77), or other profiles because our
/// encoder does not produce those.  A decoder that only accepts High will
/// fail on Baseline input.
pub fn is_compatible_h264_profile(profile: &H264FmtpProfile) -> bool {
    // packetization-mode must be 0 or 1.
    if profile.packetization_mode > 1 {
        return false;
    }

    // No profile-level-id → RFC 6184 default (compatible).
    if profile.profile_level_id.is_empty() {
        return true;
    }

    // profile_level_id must be at least 2 hex digits for profile_idc.
    if profile.profile_level_id.len() < 2 {
        return false;
    }

    let profile_idc = match u8::from_str_radix(&profile.profile_level_id[..2], 16) {
        Ok(v) => v,
        Err(_) => return false,
    };

    // 0x42 = 66 = Baseline family (Baseline or Constrained Baseline
    // depending on constraint_set1_flag).  Both are compatible with our
    // encoder output.
    profile_idc == 0x42
}

/// Returns `true` if the SDP offer contains at least one H264 payload type
/// with a compatible fmtp profile (or no fmtp at all, which implies
/// compatible defaults).
///
/// Returns `false` if H264 is not offered at all, or if every offered H264
/// variant has an incompatible profile.
pub fn has_compatible_h264_offer(sdp: &str) -> bool {
    let profiles = parse_h264_fmtp(sdp);
    if profiles.is_empty() {
        return false;
    }
    profiles.iter().any(is_compatible_h264_profile)
}

/// Pick the best available codec that the browser also supports.
///
/// Parses the browser's SDP offer to determine which codecs it advertises,
/// then intersects with locally available encoders.  Tries H264 first (if
/// the browser offered it with a compatible profile *and* a local encoder
/// is available), falls back to VP8 (universally supported by all WebRTC
/// browsers).
///
/// H264 compatibility is determined by parsing `a=fmtp:` lines for
/// `profile-level-id` and `packetization-mode`, then checking that at least
/// one offered H264 variant matches our encoder's Baseline profile
/// (profile_idc 0x42) with packetization-mode 0 or 1.  If no `a=fmtp:`
/// line exists for an H264 payload type, it is accepted per RFC 6184
/// defaults.
pub fn select_codec(
    offer_sdp: &str,
    width: u32,
    height: u32,
    bitrate_kbps: u32,
) -> (Box<dyn Encoder>, CodecChoice) {
    if has_compatible_h264_offer(offer_sdp) {
        match create_h264_encoder(width, height, bitrate_kbps) {
            Ok(pair) => return pair,
            Err(reason) => {
                eprintln!(
                    "[display/encoder] H264 hardware encoder unavailable ({}), using VP8 software fallback",
                    reason,
                );
            }
        }
    } else {
        let browser_codecs = parse_offered_codecs(offer_sdp);
        eprintln!(
            "[display/encoder] browser SDP does not offer compatible H264 (offered: {:?}), using VP8",
            browser_codecs,
        );
    }

    let enc = Vp8Encoder::new(width, height, bitrate_kbps)
        .expect("VP8 encoder creation must not fail");
    (Box::new(enc), CodecChoice::Vp8)
}

/// Parse `a=rtpmap:` lines from an SDP offer to extract codec names.
///
/// Returns codec names such as `"VP8"`, `"VP9"`, `"H264"`, `"AV1"`.
/// For H264-specific fmtp parameter parsing (profile-level-id,
/// packetization-mode), see [`parse_h264_fmtp()`].
pub fn parse_offered_codecs(sdp: &str) -> Vec<String> {
    let mut codecs = Vec::new();
    for line in sdp.lines() {
        // Format: a=rtpmap:<payload> <codec>/<clock> [/<params>]
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            // Skip the payload-type number.
            if let Some(after_pt) = rest.split_whitespace().nth(1) {
                if let Some(codec_name) = after_pt.split('/').next() {
                    let name = codec_name.to_uppercase();
                    if !codecs.contains(&name) {
                        codecs.push(name);
                    }
                }
            }
        }
    }
    codecs
}

/// Platform-specific H264 encoder creation.
#[cfg(target_os = "macos")]
fn create_h264_encoder(
    width: u32,
    height: u32,
    bitrate_kbps: u32,
) -> Result<(Box<dyn Encoder>, CodecChoice), String> {
    let enc = h264_macos::VideoToolboxEncoder::new(width, height, bitrate_kbps)?;
    eprintln!(
        "[display/encoder] Using H264 (VideoToolbox) for {}x{}",
        width, height,
    );
    Ok((Box::new(enc), CodecChoice::H264))
}

#[cfg(target_os = "linux")]
fn create_h264_encoder(
    width: u32,
    height: u32,
    bitrate_kbps: u32,
) -> Result<(Box<dyn Encoder>, CodecChoice), String> {
    let enc = h264_linux::FfmpegH264Encoder::new(width, height, bitrate_kbps)?;
    eprintln!(
        "[display/encoder] Using H264 (ffmpeg VA-API) for {}x{}",
        width, height,
    );
    Ok((Box::new(enc), CodecChoice::H264))
}

// ---------------------------------------------------------------------------
// Color space conversion
// ---------------------------------------------------------------------------

/// Convert a BGRA image buffer to I420 (YCbCr 4:2:0) planar format.
///
/// The output layout is: Y plane (width*height) followed by U plane
/// (width/2 * height/2) followed by V plane (width/2 * height/2).
/// U and V are subsampled 2x2 by averaging the four contributing pixels.
pub fn bgra_to_i420(bgra: &[u8], width: u32, height: u32, stride: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let s = stride as usize;

    let uv_w = (w + 1) / 2;
    let uv_h = (h + 1) / 2;

    let y_size = w * h;
    let uv_size = uv_w * uv_h;
    let mut out = vec![0u8; y_size + 2 * uv_size];

    let (y_plane, uv_planes) = out.split_at_mut(y_size);
    let (u_plane, v_plane) = uv_planes.split_at_mut(uv_size);

    // Compute Y for every pixel.
    for row in 0..h {
        let row_start = row * s;
        for col in 0..w {
            let px = row_start + col * 4;
            let b = bgra[px] as f32;
            let g = bgra[px + 1] as f32;
            let r = bgra[px + 2] as f32;
            let y = (0.299 * r + 0.587 * g + 0.114 * b).round();
            y_plane[row * w + col] = y.clamp(0.0, 255.0) as u8;
        }
    }

    // Compute U, V by averaging 2x2 blocks.
    for uv_row in 0..uv_h {
        for uv_col in 0..uv_w {
            let mut sum_r: f32 = 0.0;
            let mut sum_g: f32 = 0.0;
            let mut sum_b: f32 = 0.0;
            let mut count: f32 = 0.0;

            for dy in 0..2u32 {
                let row = uv_row * 2 + dy as usize;
                if row >= h {
                    continue;
                }
                for dx in 0..2u32 {
                    let col = uv_col * 2 + dx as usize;
                    if col >= w {
                        continue;
                    }
                    let px = row * s + col * 4;
                    sum_b += bgra[px] as f32;
                    sum_g += bgra[px + 1] as f32;
                    sum_r += bgra[px + 2] as f32;
                    count += 1.0;
                }
            }

            let r = sum_r / count;
            let g = sum_g / count;
            let b = sum_b / count;

            let u = (-0.169 * r - 0.331 * g + 0.500 * b + 128.0).round();
            let v = (0.500 * r - 0.419 * g - 0.081 * b + 128.0).round();

            let idx = uv_row * uv_w + uv_col;
            u_plane[idx] = u.clamp(0.0, 255.0) as u8;
            v_plane[idx] = v.clamp(0.0, 255.0) as u8;
        }
    }

    out
}

/// A single packet produced by the VP8 encoder.
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub pts_ms: u64,
    pub duration_ms: u64,
    pub is_keyframe: bool,
}

/// VP8 encoder wrapping `vpx-encode`.
pub struct Vp8Encoder {
    encoder: vpx_encode::Encoder,
    frame_count: u64,
    pts_offset: u64,
}

// vpx_encode::Encoder contains raw pointers from the C FFI that are not
// marked Send.  The encoder is only ever used from a single dedicated
// thread so sending the owning struct across threads is safe.
unsafe impl Send for Vp8Encoder {}

impl Vp8Encoder {
    /// Create a new VP8 encoder for the given resolution and target bitrate.
    pub fn new(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, String> {
        let config = vpx_encode::Config {
            width: width as _,
            height: height as _,
            timebase: [1, 1000],
            bitrate: bitrate_kbps as _,
            codec: vpx_encode::VideoCodecId::VP8,
        };
        let encoder = vpx_encode::Encoder::new(config).map_err(|e| format!("{e}"))?;
        Ok(Self {
            encoder,
            frame_count: 0,
            pts_offset: 0,
        })
    }
}

impl Encoder for Vp8Encoder {
    fn encode(&mut self, i420: &[u8], duration_ms: u64) -> Result<Vec<EncodedPacket>, String> {
        let pts = self.pts_offset;
        self.pts_offset += duration_ms;
        self.frame_count += 1;

        let packets = self
            .encoder
            .encode(pts as i64, i420)
            .map_err(|e| format!("{e}"))?;

        let mut out = Vec::new();
        for pkt in packets {
            out.push(EncodedPacket {
                data: pkt.data.to_vec(),
                pts_ms: pkt.pts as u64,
                duration_ms,
                is_keyframe: pkt.key,
            });
        }
        Ok(out)
    }

    fn codec_mime(&self) -> &'static str {
        "video/VP8"
    }
}

impl EncodedPacket {
    /// Convert to the shared `EncodedFrame` type used by the display session.
    pub fn into_encoded_frame(self) -> EncodedFrame {
        EncodedFrame {
            data: self.data,
            pts_ms: self.pts_ms,
            duration_ms: self.duration_ms,
            is_keyframe: self.is_keyframe,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 2x2 BGRA image with known pixel values.
    fn make_2x2_bgra() -> Vec<u8> {
        // Pixel layout (BGRA):
        //   (0,0): pure red   -> B=0, G=0, R=255, A=255
        //   (1,0): pure green -> B=0, G=255, R=0, A=255
        //   (0,1): pure blue  -> B=255, G=0, R=0, A=255
        //   (1,1): white      -> B=255, G=255, R=255, A=255
        vec![
            0, 0, 255, 255, // red
            0, 255, 0, 255, // green
            255, 0, 0, 255, // blue
            255, 255, 255, 255, // white
        ]
    }

    #[test]
    fn bgra_to_i420_dimensions() {
        let bgra = make_2x2_bgra();
        let i420 = bgra_to_i420(&bgra, 2, 2, 8);
        // Y: 2*2 = 4, U: 1*1 = 1, V: 1*1 = 1 => total 6
        assert_eq!(i420.len(), 6);
    }

    #[test]
    fn bgra_to_i420_y_values() {
        let bgra = make_2x2_bgra();
        let i420 = bgra_to_i420(&bgra, 2, 2, 8);

        // Expected Y values (rounded):
        // Red:   0.299*255 + 0.587*0   + 0.114*0   = 76.245 -> 76
        // Green: 0.299*0   + 0.587*255 + 0.114*0   = 149.685 -> 150
        // Blue:  0.299*0   + 0.587*0   + 0.114*255 = 29.07  -> 29
        // White: 0.299*255 + 0.587*255 + 0.114*255 = 255    -> 255
        assert_eq!(i420[0], 76);
        assert_eq!(i420[1], 150);
        assert_eq!(i420[2], 29);
        assert_eq!(i420[3], 255);
    }

    #[test]
    fn bgra_to_i420_stride_padding() {
        // 2x2 image with stride=12 (4 bytes padding per row)
        let mut bgra = vec![0u8; 24]; // 2 rows * 12 bytes
        // Row 0: white, white
        bgra[0..4].copy_from_slice(&[255, 255, 255, 255]);
        bgra[4..8].copy_from_slice(&[255, 255, 255, 255]);
        // Row 1: black, black
        bgra[12..16].copy_from_slice(&[0, 0, 0, 255]);
        bgra[16..20].copy_from_slice(&[0, 0, 0, 255]);

        let i420 = bgra_to_i420(&bgra, 2, 2, 12);
        assert_eq!(i420.len(), 6);
        // White Y = 255, Black Y = 0
        assert_eq!(i420[0], 255);
        assert_eq!(i420[1], 255);
        assert_eq!(i420[2], 0);
        assert_eq!(i420[3], 0);
    }

    #[test]
    fn parse_offered_codecs_extracts_names() {
        let sdp = "\
v=0\r\n\
o=- 123 456 IN IP4 0.0.0.0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97 98\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtpmap:97 H264/90000\r\n\
a=rtpmap:98 VP9/90000\r\n\
a=fmtp:97 level-asymmetry-allowed=1\r\n";

        let codecs = parse_offered_codecs(sdp);
        assert_eq!(codecs, vec!["VP8", "H264", "VP9"]);
    }

    #[test]
    fn parse_offered_codecs_deduplicates() {
        let sdp = "\
a=rtpmap:96 VP8/90000\r\n\
a=rtpmap:97 VP8/90000\r\n";

        let codecs = parse_offered_codecs(sdp);
        assert_eq!(codecs, vec!["VP8"]);
    }

    #[test]
    fn parse_offered_codecs_empty_sdp() {
        let codecs = parse_offered_codecs("");
        assert!(codecs.is_empty());
    }

    #[test]
    fn vp8_encoder_produces_output() {
        let mut enc = Vp8Encoder::new(320, 240, 500).expect("encoder creation");
        // Solid gray I420 frame
        let y_size = 320 * 240;
        let uv_size = 160 * 120;
        let mut i420 = vec![128u8; y_size + 2 * uv_size];
        // Set U and V to 128 (neutral chroma)
        for b in &mut i420[y_size..] {
            *b = 128;
        }

        let packets = enc.encode(&i420, 33).expect("encode");
        // VP8 typically emits at least one packet for the first frame (keyframe).
        assert!(!packets.is_empty(), "expected at least one packet");
        assert!(packets[0].is_keyframe, "first frame should be keyframe");
        assert!(!packets[0].data.is_empty(), "packet data should not be empty");
    }

    // -----------------------------------------------------------------------
    // H264 fmtp parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_h264_fmtp_chrome_offer() {
        // Realistic Chrome 125 SDP snippet with multiple H264 payload types.
        let sdp = "\
v=0\r\n\
o=- 5765891208566location 2 IN IP4 127.0.0.1\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97 98 99 100 101 102\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtpmap:97 H264/90000\r\n\
a=rtpmap:98 H264/90000\r\n\
a=rtpmap:99 H264/90000\r\n\
a=rtpmap:100 VP9/90000\r\n\
a=rtpmap:101 H264/90000\r\n\
a=rtpmap:102 AV1/90000\r\n\
a=fmtp:97 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r\n\
a=fmtp:98 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f\r\n\
a=fmtp:99 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
a=fmtp:101 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=4d001f\r\n";

        let profiles = parse_h264_fmtp(sdp);
        assert_eq!(profiles.len(), 4);

        // PT 97: Baseline, packetization-mode 1
        assert_eq!(profiles[0].payload_type, 97);
        assert_eq!(profiles[0].profile_level_id, "42001f");
        assert_eq!(profiles[0].packetization_mode, 1);

        // PT 98: Baseline, packetization-mode 0
        assert_eq!(profiles[1].payload_type, 98);
        assert_eq!(profiles[1].profile_level_id, "42001f");
        assert_eq!(profiles[1].packetization_mode, 0);

        // PT 99: Constrained Baseline (42e0), packetization-mode 1
        assert_eq!(profiles[2].payload_type, 99);
        assert_eq!(profiles[2].profile_level_id, "42e01f");
        assert_eq!(profiles[2].packetization_mode, 1);

        // PT 101: Main profile (4d), packetization-mode 1
        assert_eq!(profiles[3].payload_type, 101);
        assert_eq!(profiles[3].profile_level_id, "4d001f");
        assert_eq!(profiles[3].packetization_mode, 1);
    }

    #[test]
    fn parse_h264_fmtp_firefox_offer() {
        // Firefox typically offers fewer H264 variants.
        let sdp = "\
a=rtpmap:126 H264/90000\r\n\
a=rtpmap:97 H264/90000\r\n\
a=fmtp:126 profile-level-id=42e01f;level-asymmetry-allowed=1;packetization-mode=1\r\n\
a=fmtp:97 profile-level-id=42e01f;level-asymmetry-allowed=1;packetization-mode=0\r\n";

        let profiles = parse_h264_fmtp(sdp);
        assert_eq!(profiles.len(), 2);

        assert_eq!(profiles[0].payload_type, 126);
        assert_eq!(profiles[0].profile_level_id, "42e01f");
        assert_eq!(profiles[0].packetization_mode, 1);

        assert_eq!(profiles[1].payload_type, 97);
        assert_eq!(profiles[1].profile_level_id, "42e01f");
        assert_eq!(profiles[1].packetization_mode, 0);
    }

    #[test]
    fn parse_h264_fmtp_no_fmtp_line() {
        // H264 offered by rtpmap but no corresponding fmtp line.
        // Should use RFC 6184 defaults.
        let sdp = "\
a=rtpmap:97 H264/90000\r\n";

        let profiles = parse_h264_fmtp(sdp);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].payload_type, 97);
        assert_eq!(profiles[0].profile_level_id, "");
        assert_eq!(profiles[0].packetization_mode, 0);
    }

    #[test]
    fn parse_h264_fmtp_no_h264() {
        let sdp = "\
a=rtpmap:96 VP8/90000\r\n\
a=rtpmap:100 VP9/90000\r\n";

        let profiles = parse_h264_fmtp(sdp);
        assert!(profiles.is_empty());
    }

    #[test]
    fn is_compatible_baseline() {
        let p = H264FmtpProfile {
            payload_type: 97,
            profile_level_id: "42001f".into(),
            packetization_mode: 1,
        };
        assert!(is_compatible_h264_profile(&p));
    }

    #[test]
    fn is_compatible_constrained_baseline() {
        // 42e0 = profile_idc 0x42 with constraint_set1_flag set
        let p = H264FmtpProfile {
            payload_type: 99,
            profile_level_id: "42e01f".into(),
            packetization_mode: 1,
        };
        assert!(is_compatible_h264_profile(&p));
    }

    #[test]
    fn is_compatible_packetization_mode_0() {
        let p = H264FmtpProfile {
            payload_type: 98,
            profile_level_id: "42001f".into(),
            packetization_mode: 0,
        };
        assert!(is_compatible_h264_profile(&p));
    }

    #[test]
    fn is_compatible_empty_profile() {
        // Missing fmtp → accept (RFC 6184 defaults are compatible).
        let p = H264FmtpProfile {
            payload_type: 97,
            profile_level_id: "".into(),
            packetization_mode: 0,
        };
        assert!(is_compatible_h264_profile(&p));
    }

    #[test]
    fn is_incompatible_main_profile() {
        // Main profile (0x4d = 77) — our encoder does not produce this.
        let p = H264FmtpProfile {
            payload_type: 101,
            profile_level_id: "4d001f".into(),
            packetization_mode: 1,
        };
        assert!(!is_compatible_h264_profile(&p));
    }

    #[test]
    fn is_incompatible_high_profile() {
        // High profile (0x64 = 100).
        let p = H264FmtpProfile {
            payload_type: 102,
            profile_level_id: "640032".into(),
            packetization_mode: 1,
        };
        assert!(!is_compatible_h264_profile(&p));
    }

    #[test]
    fn is_incompatible_packetization_mode_2() {
        // packetization-mode 2 (interleaved) — not supported.
        let p = H264FmtpProfile {
            payload_type: 97,
            profile_level_id: "42001f".into(),
            packetization_mode: 2,
        };
        assert!(!is_compatible_h264_profile(&p));
    }

    #[test]
    fn has_compatible_h264_offer_mixed_profiles() {
        // Offer has both compatible (Baseline) and incompatible (Main) H264.
        // Should return true because at least one is compatible.
        let sdp = "\
a=rtpmap:97 H264/90000\r\n\
a=rtpmap:98 H264/90000\r\n\
a=fmtp:97 profile-level-id=4d001f;packetization-mode=1\r\n\
a=fmtp:98 profile-level-id=42e01f;packetization-mode=1\r\n";

        assert!(has_compatible_h264_offer(sdp));
    }

    #[test]
    fn has_compatible_h264_offer_only_high() {
        // Only High profile offered — incompatible.
        let sdp = "\
a=rtpmap:97 H264/90000\r\n\
a=fmtp:97 profile-level-id=640032;packetization-mode=1\r\n";

        assert!(!has_compatible_h264_offer(sdp));
    }

    #[test]
    fn has_compatible_h264_offer_no_h264() {
        let sdp = "a=rtpmap:96 VP8/90000\r\n";
        assert!(!has_compatible_h264_offer(sdp));
    }

    #[test]
    fn has_compatible_h264_offer_no_fmtp() {
        // H264 offered without fmtp → accept (defaults are compatible).
        let sdp = "a=rtpmap:97 H264/90000\r\n";
        assert!(has_compatible_h264_offer(sdp));
    }
}
