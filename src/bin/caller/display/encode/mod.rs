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

/// Pick the best available codec that the browser also supports.
///
/// Parses the browser's SDP offer to determine which codecs it advertises,
/// then intersects with locally available encoders.  Tries H264 first (if
/// the browser offered it *and* a local encoder is available), falls back to
/// VP8 (universally supported by all WebRTC browsers).
pub fn select_codec(
    offer_sdp: &str,
    width: u32,
    height: u32,
    bitrate_kbps: u32,
) -> (Box<dyn Encoder>, CodecChoice) {
    let browser_codecs = parse_offered_codecs(offer_sdp);

    if browser_codecs.iter().any(|c| c.eq_ignore_ascii_case("H264")) {
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
        eprintln!(
            "[display/encoder] browser SDP does not offer H264 (offered: {:?}), using VP8",
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
}
