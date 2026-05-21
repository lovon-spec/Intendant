use super::EncodedFrame;

#[cfg(target_os = "linux")]
pub mod h264_linux;
#[cfg(target_os = "macos")]
pub mod h264_macos;
#[cfg(target_os = "windows")]
pub mod h264_windows;
pub mod pool;
pub mod vp8;

pub use vp8::Vp8Encoder;

pub const MIME_TYPE_VP8: &str = "video/VP8";
pub const MIME_TYPE_H264: &str = "video/H264";

// ---------------------------------------------------------------------------
// PayloadSpec — codec identity + fmtp-equivalent parameters
// ---------------------------------------------------------------------------

/// Codec + format-params identity carried on every encoded frame, used by
/// the WebRTC driver to verify the frame matches the peer-negotiated RTP
/// codec before packetizing it.
///
/// Distinct from [`pool::CodecKind`] because two frames from the same
/// codec can disagree on fmtp (e.g. H.264 Baseline vs Main with different
/// profile-level-id, packetization-mode 0 vs 1) and browsers negotiate those
/// independently per peer. Keying driver state on `PayloadSpec` (not
/// `CodecKind`) stays correct under mixed-profile fleets.
///
/// `PartialEq + Eq + Hash` are derived so the driver can use it as a
/// cache key. Fields not relevant to the codec are `None` by convention
/// (e.g. H.264 fields are `None` for VP8 frames).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PayloadSpec {
    /// MIME string for the codec (e.g. `"video/VP8"`, `"video/H264"`).
    /// Using the static str interned by the encoder crate rather than an
    /// enum so new codecs (VP9, AV1) don't require changes here.
    pub codec_mime: &'static str,

    /// Sample clock rate in Hz. 90000 for all video codecs RFC 7741 /
    /// RFC 6184 / etc. Kept as a field so audio codecs can reuse the
    /// type if needed later.
    pub clock_rate: u32,

    /// H.264 profile-level-id from SDP fmtp (6 hex digits lowercase, e.g.
    /// `"42e01f"`). `None` for non-H.264 codecs. Matters because browsers
    /// discriminate H.264 parameter sets by profile.
    pub h264_profile_level_id: Option<String>,

    /// H.264 packetization-mode (0 or 1). `None` for non-H.264 codecs.
    /// The WebRTC driver checks this; mismatches fail even if
    /// the codec name matches.
    pub h264_packetization_mode: Option<u32>,
}

impl PayloadSpec {
    /// VP8 has no fmtp parameters to disambiguate on; all VP8 senders /
    /// receivers agree on the single parameter set.
    pub fn vp8() -> Self {
        Self {
            codec_mime: MIME_TYPE_VP8,
            clock_rate: 90_000,
            h264_profile_level_id: None,
            h264_packetization_mode: None,
        }
    }

    /// H.264 Constrained Baseline, packetization-mode 1 — what both of
    /// our H.264 encoder backends (VideoToolbox on macOS, libx264/VAAPI
    /// on Linux) produce. Matches `profile_idc=0x42`, `constraint_set1=1`,
    /// Level 3.1.
    pub fn h264_constrained_baseline() -> Self {
        Self {
            codec_mime: MIME_TYPE_H264,
            clock_rate: 90_000,
            h264_profile_level_id: Some("42e01f".to_string()),
            h264_packetization_mode: Some(1),
        }
    }
}

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
    ///
    /// `force_keyframe` asks the encoder to emit this frame as a keyframe.
    /// Implementations that cannot force mid-stream (e.g. H.264 over an
    /// already-running ffmpeg stdin pipe) may ignore the flag and rely on
    /// their configured GOP cadence instead.
    ///
    /// Packets emitted must carry this encoder's [`PayloadSpec`] so the
    /// WebRTC driver can verify the packet matches the negotiated sender
    /// codec. Encoders should `.clone()` their cached `PayloadSpec` onto every
    /// emitted packet (cheap — small struct with one `Option<String>`).
    fn encode(
        &mut self,
        i420: &[u8],
        duration_ms: u64,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedPacket>, String>;

    /// The MIME type of the encoded output (e.g. `"video/VP8"`, `"video/H264"`).
    fn codec_mime(&self) -> &'static str;

    /// Canonical [`PayloadSpec`] for this encoder. Attached to every
    /// emitted packet; also returned here for code that needs the spec
    /// without going through encode (e.g. the pool constructing encoder
    /// handles before the first frame is available).
    fn payload_spec(&self) -> &PayloadSpec;
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
            let (profile_level_id, packetization_mode) =
                fmtp_map.remove(&pt).unwrap_or_else(|| (String::new(), 0));
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
///
/// **Legacy helper, broader than the pool path needs.** The encoder-pool path
/// uses [`offer_has_poolable_h264_variant`] instead, which mirrors the exact
/// profile / packetization / level that our H.264 encoder can produce.
pub fn has_compatible_h264_offer(sdp: &str) -> bool {
    let profiles = parse_h264_fmtp(sdp);
    if profiles.is_empty() {
        return false;
    }
    profiles.iter().any(is_compatible_h264_profile)
}

// ---------------------------------------------------------------------------
// Encoder-pool-strict H.264 offer check.
// ---------------------------------------------------------------------------
//
// The pool path must reject H.264 variants the encoder cannot produce before
// constructing a peer. If an offer passes `codec_preferences_from_offer` →
// pool subscribes → frames flow → the driver rejects the frame's PayloadSpec
// at write time, the result is a silent black screen. So the offer-level
// filter pre-excludes variants that would fail.
//
// The encoder produces exactly `PayloadSpec::h264_constrained_baseline`:
// profile-level-id `42e01f`, packetization-mode 1, i.e. Constrained
// Baseline at Level 3.1. We accept an offer variant iff:
//
//   1. Its packetization-mode (default 0 if the fmtp line doesn't
//      mention it, or no fmtp line at all) equals 1.
//   2. Its profile-level-id resolves to Constrained Baseline. For
//      `profile_idc = 0x42` that means `constraint_set1_flag == 1` AND
//      reserved bits 4-7 == 0.
//   3. Its level_idc does not exceed our encoder's (`0x1F` = Level 3.1).
//   4. The profile-level-id must be present (fmtp missing or empty
//      defaults to Baseline+Level 1, which is the wrong family for our
//      Constrained Baseline encoder).
//
// Other paths to ConstrainedBaseline exist in H.264 profile tables (idc 0x4D
// Main + cs0 flag, idc 0x58 Extended + cs0 cs1 flags). In practice WebRTC
// browsers advertise the idc 0x42 family, so the simpler check is enough; the
// fallback is our encoder's broader VP8 always-on which handles the long tail.

/// Packetization-mode our encoder produces. Matches both `h264_linux.rs`
/// (ffmpeg's default) and `h264_macos.rs` (VideoToolbox Baseline).
const ENCODER_H264_PACKETIZATION_MODE: u32 = 1;

/// profile_idc that `PayloadSpec::h264_constrained_baseline` encodes
/// — Baseline family byte, ConstrainedBaseline distinguished by
/// constraint_set1_flag.
const ENCODER_H264_PROFILE_IDC: u8 = 0x42;

/// Maximum level_idc our encoder is configured for (Level 3.1).
const ENCODER_H264_MAX_LEVEL_IDC: u8 = 0x1F;

/// Whether one offered H.264 fmtp variant would match the encoder's
/// exact [`PayloadSpec::h264_constrained_baseline`]. See the comment above for
/// the full set of requirements this encodes.
pub fn offer_variant_matches_encoder_payload_spec(profile: &H264FmtpProfile) -> bool {
    // Requirement 1: packetization-mode must equal our encoder's.
    // `parse_h264_fmtp` already defaults missing fmtp / missing
    // packetization-mode key to `0`, so a missing / empty fmtp line
    // gets a value of 0 here and correctly fails this check.
    if profile.packetization_mode != ENCODER_H264_PACKETIZATION_MODE {
        return false;
    }

    // Requirement 4: profile-level-id must be present. An empty
    // Missing profile_level_id maps to Baseline at Level 1, which is the wrong
    // profile family for our Constrained Baseline encoder.
    if profile.profile_level_id.len() != 6 {
        return false;
    }

    // Parse the three bytes of profile-level-id.
    let bytes = profile.profile_level_id.as_bytes();
    let Ok(profile_idc) = u8::from_str_radix(std::str::from_utf8(&bytes[0..2]).unwrap_or(""), 16)
    else {
        return false;
    };
    let Ok(profile_iop) = u8::from_str_radix(std::str::from_utf8(&bytes[2..4]).unwrap_or(""), 16)
    else {
        return false;
    };
    let Ok(level_idc) = u8::from_str_radix(std::str::from_utf8(&bytes[4..6]).unwrap_or(""), 16)
    else {
        return false;
    };

    // Requirement 2: profile_idc must be 0x42 (Baseline family byte) AND
    // profile-iop must match ConstrainedBaseline at that idc: constraint_set1
    // flag (bit 1 from the left of the iop byte, mask 0x40) must be 1, and the
    // low four bits (reserved) must be 0.
    if profile_idc != ENCODER_H264_PROFILE_IDC {
        return false;
    }
    if profile_iop & 0x40 == 0 {
        return false;
    }
    if profile_iop & 0x0F != 0 {
        return false;
    }

    // Requirement 3: offer's level must be <= our encoder's capability.
    // Level_idc ordering is numeric across all common WebRTC levels (1.0
    // through 5.2), so a simple inequality suffices; the lone exception is
    // Level 1b which encodes as 0x09 + constraint_set3_flag, below 3.1 by any
    // measure.
    if level_idc > ENCODER_H264_MAX_LEVEL_IDC {
        return false;
    }

    true
}

/// Whether any H.264 variant in the SDP offer would match the encoder's
/// exact PayloadSpec. Use this gate — not the older
/// [`has_compatible_h264_offer`] — on the encoder-pool path; see the detailed
/// comment above [`offer_variant_matches_encoder_payload_spec`] for why.
pub fn offer_has_poolable_h264_variant(sdp: &str) -> bool {
    parse_h264_fmtp(sdp)
        .iter()
        .any(offer_variant_matches_encoder_payload_spec)
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

    let enc =
        Vp8Encoder::new(width, height, bitrate_kbps).expect("VP8 encoder creation must not fail");
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
    // The specific backend (h264_vaapi vs libx264) is logged by
    // FfmpegH264Encoder::new itself — don't lie about it here.
    eprintln!(
        "[display/encoder] Using H264 (ffmpeg) for {}x{}",
        width, height,
    );
    Ok((Box::new(enc), CodecChoice::H264))
}

/// Windows H264 encoder via Media Foundation (the in-box software H.264
/// encoder MFT, Constrained Baseline). This is the always-on baseline codec on
/// Windows since VP8/libvpx is gated off there — see [`pool`].
#[cfg(target_os = "windows")]
fn create_h264_encoder(
    width: u32,
    height: u32,
    bitrate_kbps: u32,
) -> Result<(Box<dyn Encoder>, CodecChoice), String> {
    let enc = h264_windows::MediaFoundationEncoder::new(width, height, bitrate_kbps)?;
    eprintln!(
        "[display/encoder] Using H264 (Media Foundation) for {}x{}",
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

    // Compute luma in row-major order. This is the largest plane and the
    // most cache-sensitive loop; fixed-point math avoids the old per-pixel
    // floating-point conversions while preserving the BT.601 coefficients.
    for row in 0..h {
        let row_start = row * s;
        let y_row_start = row * w;
        for col in 0..w {
            let px = row_start + col * 4;
            y_plane[y_row_start + col] =
                rgb_to_y(bgra[px + 2] as i32, bgra[px + 1] as i32, bgra[px] as i32);
        }
    }

    // Compute U, V by averaging 2x2 blocks.
    for uv_row in 0..uv_h {
        for uv_col in 0..uv_w {
            let mut sum_r: i32 = 0;
            let mut sum_g: i32 = 0;
            let mut sum_b: i32 = 0;
            let mut count: i32 = 0;

            for dy in 0..2usize {
                let row = uv_row * 2 + dy;
                if row >= h {
                    continue;
                }
                let row_start = row * s;
                for dx in 0..2usize {
                    let col = uv_col * 2 + dx;
                    if col >= w {
                        continue;
                    }
                    let px = row_start + col * 4;
                    let b = bgra[px] as i32;
                    let g = bgra[px + 1] as i32;
                    let r = bgra[px + 2] as i32;
                    sum_b += b;
                    sum_g += g;
                    sum_r += r;
                    count += 1;
                }
            }

            let idx = uv_row * uv_w + uv_col;
            u_plane[idx] = rgb_sum_to_u(sum_r, sum_g, sum_b, count);
            v_plane[idx] = rgb_sum_to_v(sum_r, sum_g, sum_b, count);
        }
    }

    out
}

#[inline]
fn rgb_to_y(r: i32, g: i32, b: i32) -> u8 {
    // 0.299, 0.587, 0.114 in 16.16 fixed point. Coefficients sum to
    // exactly 65536, so white maps to 255 without an explicit clamp.
    (((19_595 * r + 38_470 * g + 7_471 * b + 32_768) >> 16).clamp(0, 255)) as u8
}

#[inline]
fn rgb_sum_to_u(sum_r: i32, sum_g: i32, sum_b: i32, count: i32) -> u8 {
    // -0.169, -0.331, 0.500 + 128 in 16.16 fixed point.
    let n = -11_076 * sum_r - 21_692 * sum_g + 32_768 * sum_b + 8_388_608 * count;
    rounded_fixed_avg_clamped_u8(n, count)
}

#[inline]
fn rgb_sum_to_v(sum_r: i32, sum_g: i32, sum_b: i32, count: i32) -> u8 {
    // 0.500, -0.419, -0.081 + 128 in 16.16 fixed point.
    let n = 32_768 * sum_r - 27_460 * sum_g - 5_308 * sum_b + 8_388_608 * count;
    rounded_fixed_avg_clamped_u8(n, count)
}

#[inline]
fn rounded_fixed_avg_clamped_u8(n: i32, count: i32) -> u8 {
    let denom = count << 16;
    ((n + denom / 2) / denom).clamp(0, 255) as u8
}

/// Bilinear downscale an I420 frame from `(src_w, src_h)` to `(dst_w, dst_h)`.
///
/// All four dimensions must be even — I420 subsamples chroma 2×2 and VP8
/// requires even output dims, so this is the only useful shape anyway.
/// Source buffer length must be exactly `src_w * src_h * 3 / 2`.
///
/// **Implementation: pure-Rust bilinear, three-plane independent downscale.**
/// Y plane downscales to `dst_w × dst_h`; U and V planes each downscale
/// to `(dst_w/2) × (dst_h/2)`. Output layout matches I420 (Y followed by
/// U followed by V).
///
/// **Why bilinear and not libyuv-sys.** Pure Rust costs ~3 ms per 1080p→
/// 540p downscale on modern x86 / Apple Silicon (one core). At 30 fps that's
/// ~10% of one core for a single layer — fine for our 1-3-layer simulcast
/// at typical workloads. libyuv-sys would be 2-3× faster but adds a `-sys`
/// crate plus setup-script churn (per CLAUDE.md the rule is updating both
/// `setup-linux.sh` and `setup-macos.sh` for any new sys dep). When/if the
/// CPU budget actually binds, swapping the per-plane loop to a libyuv call
/// is local — the function signature and output shape don't change.
pub fn downscale_i420(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    debug_assert!(
        src_w % 2 == 0 && src_h % 2 == 0,
        "downscale_i420: src dims must be even, got {src_w}x{src_h}"
    );
    debug_assert!(
        dst_w % 2 == 0 && dst_h % 2 == 0,
        "downscale_i420: dst dims must be even, got {dst_w}x{dst_h}"
    );
    let src_w = src_w as usize;
    let src_h = src_h as usize;
    let dst_w = dst_w as usize;
    let dst_h = dst_h as usize;

    let src_y_size = src_w * src_h;
    let src_uv_size = (src_w / 2) * (src_h / 2);
    debug_assert_eq!(
        src.len(),
        src_y_size + 2 * src_uv_size,
        "downscale_i420: src len {} doesn't match {src_w}x{src_h} I420 \
         (expected {})",
        src.len(),
        src_y_size + 2 * src_uv_size,
    );

    let dst_y_size = dst_w * dst_h;
    let dst_uv_size = (dst_w / 2) * (dst_h / 2);
    let mut out = vec![0u8; dst_y_size + 2 * dst_uv_size];

    let (src_y, src_uv) = src.split_at(src_y_size);
    let (src_u, src_v) = src_uv.split_at(src_uv_size);
    let (dst_y_plane, dst_uv) = out.split_at_mut(dst_y_size);
    let (dst_u_plane, dst_v_plane) = dst_uv.split_at_mut(dst_uv_size);

    downscale_plane_bilinear(src_y, src_w, src_h, dst_y_plane, dst_w, dst_h);
    downscale_plane_bilinear(
        src_u,
        src_w / 2,
        src_h / 2,
        dst_u_plane,
        dst_w / 2,
        dst_h / 2,
    );
    downscale_plane_bilinear(
        src_v,
        src_w / 2,
        src_h / 2,
        dst_v_plane,
        dst_w / 2,
        dst_h / 2,
    );

    out
}

/// Bilinear single-plane downscale. The `dst` slice is written in
/// row-major order. `src` and `dst` are non-overlapping (caller's
/// `split_at_mut` enforces that for [`downscale_i420`]).
///
/// Sampling convention: pixel centers at `+0.5`, so the destination
/// pixel's source position is `(dx + 0.5) * x_ratio - 0.5`. Without
/// the `+0.5/-0.5`, a 2× downscale would sample only the top-left of
/// each 2×2 source block — biased instead of averaged.
fn downscale_plane_bilinear(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    dst: &mut [u8],
    dst_w: usize,
    dst_h: usize,
) {
    let x_ratio = src_w as f32 / dst_w as f32;
    let y_ratio = src_h as f32 / dst_h as f32;
    let max_sx = src_w - 1;
    let max_sy = src_h - 1;

    for dy in 0..dst_h {
        let sy_f = (dy as f32 + 0.5) * y_ratio - 0.5;
        let sy0 = sy_f.floor().max(0.0) as usize;
        let sy1 = (sy0 + 1).min(max_sy);
        let fy = (sy_f - sy0 as f32).clamp(0.0, 1.0);

        for dx in 0..dst_w {
            let sx_f = (dx as f32 + 0.5) * x_ratio - 0.5;
            let sx0 = sx_f.floor().max(0.0) as usize;
            let sx1 = (sx0 + 1).min(max_sx);
            let fx = (sx_f - sx0 as f32).clamp(0.0, 1.0);

            let p00 = src[sy0 * src_w + sx0] as f32;
            let p01 = src[sy0 * src_w + sx1] as f32;
            let p10 = src[sy1 * src_w + sx0] as f32;
            let p11 = src[sy1 * src_w + sx1] as f32;
            let p = p00 * (1.0 - fx) * (1.0 - fy)
                + p01 * fx * (1.0 - fy)
                + p10 * (1.0 - fx) * fy
                + p11 * fx * fy;
            dst[dy * dst_w + dx] = p.round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// A single packet produced by the VP8 encoder.
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub pts_ms: u64,
    pub duration_ms: u64,
    pub is_keyframe: bool,
    /// Codec + fmtp identity for this packet's payload. Propagated to
    /// [`EncodedFrame`] on conversion and consumed by the WebRTC driver. Every
    /// encoder attaches its canonical [`PayloadSpec`] here.
    pub payload_spec: PayloadSpec,
}

impl EncodedPacket {
    /// Convert to the shared `EncodedFrame` type used by the display session.
    pub fn into_encoded_frame(self) -> EncodedFrame {
        EncodedFrame {
            data: self.data,
            pts_ms: self.pts_ms,
            duration_ms: self.duration_ms,
            is_keyframe: self.is_keyframe,
            payload_spec: self.payload_spec,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

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

    fn make_pattern_bgra(width: u32, height: u32) -> Vec<u8> {
        let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                bgra.push(((x * 3 + y * 5) & 0xff) as u8);
                bgra.push(((x * 7 + y * 11) & 0xff) as u8);
                bgra.push(((x * 13 + y * 17) & 0xff) as u8);
                bgra.push(255);
            }
        }
        bgra
    }

    fn bgra_to_i420_reference(bgra: &[u8], width: u32, height: u32, stride: u32) -> Vec<u8> {
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
    fn bgra_to_i420_matches_reference_pattern() {
        let width = 17;
        let height = 13;
        let stride = 80;
        let mut bgra = vec![0u8; height as usize * stride as usize];
        let compact = make_pattern_bgra(width, height);
        for row in 0..height as usize {
            let src = row * width as usize * 4;
            let dst = row * stride as usize;
            bgra[dst..dst + width as usize * 4]
                .copy_from_slice(&compact[src..src + width as usize * 4]);
        }

        assert_eq!(
            bgra_to_i420(&bgra, width, height, stride),
            bgra_to_i420_reference(&bgra, width, height, stride)
        );
    }

    #[test]
    #[ignore = "micro-benchmark for local encoder hot-path tuning"]
    fn bgra_to_i420_perf_1080p() {
        let width = 1920;
        let height = 1080;
        let stride = width * 4;
        let bgra = make_pattern_bgra(width, height);
        let iterations = 30;

        let mut reference_checksum = 0u64;
        let start = Instant::now();
        for _ in 0..iterations {
            let i420 = bgra_to_i420_reference(&bgra, width, height, stride);
            reference_checksum = reference_checksum.wrapping_add(i420[0] as u64);
            reference_checksum = reference_checksum.wrapping_add(i420[i420.len() / 2] as u64);
        }
        let reference_elapsed = start.elapsed();

        let mut checksum = 0u64;
        let start = Instant::now();
        for _ in 0..iterations {
            let i420 = bgra_to_i420(&bgra, width, height, stride);
            checksum = checksum.wrapping_add(i420[0] as u64);
            checksum = checksum.wrapping_add(i420[i420.len() / 2] as u64);
        }
        let elapsed = start.elapsed();
        eprintln!(
            "bgra_to_i420 reference 1080p: {:.3} ms/frame (checksum={reference_checksum})",
            reference_elapsed.as_secs_f64() * 1000.0 / iterations as f64,
        );
        eprintln!(
            "bgra_to_i420 optimized 1080p: {:.3} ms/frame (checksum={checksum})",
            elapsed.as_secs_f64() * 1000.0 / iterations as f64,
        );
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

        let packets = enc.encode(&i420, 33, false).expect("encode");
        // VP8 typically emits at least one packet for the first frame (keyframe).
        assert!(!packets.is_empty(), "expected at least one packet");
        assert!(packets[0].is_keyframe, "first frame should be keyframe");
        assert!(
            !packets[0].data.is_empty(),
            "packet data should not be empty"
        );
    }

    #[test]
    fn vp8_encoder_force_keyframe_on_demand() {
        let mut enc = Vp8Encoder::new(320, 240, 500).expect("encoder creation");
        let y_size = 320 * 240;
        let uv_size = 160 * 120;
        let mut i420 = vec![128u8; y_size + 2 * uv_size];

        // Prime with a keyframe (the first frame is always a keyframe in VP8)
        // then feed identical frames without forcing; expect P-frames.
        let _ = enc.encode(&i420, 33, false).expect("encode 1");
        let packets = enc.encode(&i420, 33, false).expect("encode 2");
        assert!(
            packets.iter().all(|p| !p.is_keyframe),
            "identical frame without force should be a P-frame"
        );

        // Now force. Tweak one byte so libvpx produces some output; the
        // flag should make whatever it produces a keyframe.
        i420[0] = 200;
        let forced = enc.encode(&i420, 33, true).expect("encode 3");
        assert!(!forced.is_empty(), "forced encode should produce output");
        assert!(
            forced.iter().any(|p| p.is_keyframe),
            "force_keyframe=true must produce a keyframe"
        );
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

    // -----------------------------------------------------------------------
    // downscale_i420 — Phase 4a
    // -----------------------------------------------------------------------

    /// Build a constant-Y I420 buffer of the given dims for tests where
    /// only the dimensions / output length matter.
    fn make_constant_i420(w: u32, h: u32, y: u8, u: u8, v: u8) -> Vec<u8> {
        let w = w as usize;
        let h = h as usize;
        let y_size = w * h;
        let uv_size = (w / 2) * (h / 2);
        let mut out = vec![0u8; y_size + 2 * uv_size];
        out[..y_size].fill(y);
        out[y_size..y_size + uv_size].fill(u);
        out[y_size + uv_size..].fill(v);
        out
    }

    /// Output buffer length must match the destination dims exactly:
    /// `dst_w * dst_h * 3 / 2`. Off-by-one here means the encoder reads
    /// past its expected buffer and decodes garbage.
    #[test]
    fn downscale_i420_output_length_matches_dst_dims() {
        let src = make_constant_i420(64, 64, 100, 50, 200);
        let out = downscale_i420(&src, 64, 64, 32, 32);
        // 32*32 (Y) + 2*16*16 (UV) = 1024 + 512 = 1536
        assert_eq!(
            out.len(),
            1536,
            "downscale 64x64 → 32x32 must yield I420 of len 1536"
        );

        let out = downscale_i420(&src, 64, 64, 16, 16);
        // 16*16 (Y) + 2*8*8 (UV) = 256 + 128 = 384
        assert_eq!(
            out.len(),
            384,
            "downscale 64x64 → 16x16 must yield I420 of len 384"
        );
    }

    /// A constant-color I420 must downscale to the same constant color.
    /// This is the trivial-case correctness check — if any pixel ends up
    /// non-Y/non-U/non-V, the bilinear interp is broken.
    #[test]
    fn downscale_i420_preserves_constant_color() {
        let src = make_constant_i420(64, 64, 137, 64, 192);
        let out = downscale_i420(&src, 64, 64, 32, 32);

        let y_size = 32 * 32;
        let uv_size = 16 * 16;
        let (y, uv) = out.split_at(y_size);
        let (u, v) = uv.split_at(uv_size);

        assert!(
            y.iter().all(|&p| p == 137),
            "Y plane must stay constant 137"
        );
        assert!(u.iter().all(|&p| p == 64), "U plane must stay constant 64");
        assert!(
            v.iter().all(|&p| p == 192),
            "V plane must stay constant 192"
        );
    }

    /// A 1:1 "downscale" (dst_dims == src_dims) must round-trip the data
    /// (within bilinear's rounding tolerance — for an exact match the
    /// sampling positions land directly on source pixel centers).
    #[test]
    fn downscale_i420_identity_dims_round_trips_constant_color() {
        let src = make_constant_i420(8, 8, 200, 100, 50);
        let out = downscale_i420(&src, 8, 8, 8, 8);
        assert_eq!(
            out.len(),
            src.len(),
            "identity downscale must produce same-length output"
        );
        // Constant color must survive an identity downscale exactly —
        // every bilinear weight is on a pixel of the same value.
        for (i, (s, o)) in src.iter().zip(out.iter()).enumerate() {
            assert_eq!(s, o, "byte {i} differs: src={s} out={o}");
        }
    }

    /// A 2× downscale of a horizontal Y gradient must produce values
    /// roughly halfway between adjacent source columns. Pin one value
    /// at a known position so a regression in the sampling math fires.
    #[test]
    fn downscale_i420_horizontal_gradient_averages() {
        // 8x2 source (must be even dims). Y plane: column index * 32.
        // So columns are 0, 32, 64, 96, 128, 160, 192, 224.
        let mut src = vec![0u8; 8 * 2 + 2 * (4 * 1)];
        for row in 0..2 {
            for col in 0..8 {
                src[row * 8 + col] = (col * 32) as u8;
            }
        }
        // U + V planes constant 128 (size 4x1 each).
        for byte in &mut src[16..] {
            *byte = 128;
        }

        let out = downscale_i420(&src, 8, 2, 4, 2);
        let y = &out[..4 * 2];

        // 4 dst pixels sampling from 8 src columns at positions 0,1,2,3.
        // bilinear sample positions: dx → (dx+0.5)*2 - 0.5 = 2*dx + 0.5
        //   dx=0 → sx=0.5: avg of cols 0,1 = (0+32)/2 = 16
        //   dx=1 → sx=2.5: avg of cols 2,3 = (64+96)/2 = 80
        //   dx=2 → sx=4.5: avg of cols 4,5 = (128+160)/2 = 144
        //   dx=3 → sx=6.5: avg of cols 6,7 = (192+224)/2 = 208
        // Tolerance ±1 for rounding.
        for (dx, expected) in [(0, 16), (1, 80), (2, 144), (3, 208)].iter() {
            let actual = y[*dx] as i32;
            assert!(
                (actual - expected).abs() <= 1,
                "dst col {dx}: expected ~{expected}, got {actual}",
            );
        }
    }
}
