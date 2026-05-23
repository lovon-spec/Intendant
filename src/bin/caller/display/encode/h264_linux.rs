//! H264 hardware encoder using ffmpeg as a subprocess with GPU acceleration.
//!
//! Pipes raw I420 frames into ffmpeg's stdin and reads Annex-B H264 NAL units
//! from stdout.  Selection order is `h264_nvenc` (NVIDIA) → `h264_vaapi`
//! (VA-API) → software `libx264`, each arm falling through on failure.
//!
//! This approach avoids complex FFI bindings to NVENC/libva while still
//! getting hardware acceleration when available.
//!
//! **Loss-resilience.** All three encoders use a *finite, bounded GOP*
//! ([`GOP_PERIOD_FRAMES`]) so a decoder that has desynced — fresh peer
//! join, or a transport gap that outran NACK retransmission — gets a real
//! IDR within ~1-2 s rather than waiting indefinitely. The complementary
//! half of the strategy lives elsewhere: the federated H.264 layer is
//! encoded at quarter resolution + a capped bitrate (see
//! `LayerSpec::single` / `single_federated` in `encode/pool.rs`), so even
//! the periodic IDR is only a handful of RTP packets and survives the
//! lossy `browser → TURN → remote peer` relay; and `slice-max-size=1200`
//! (libx264) keeps each NAL inside a single ~MTU RTP payload so a lost
//! packet costs one slice, not a frame. An earlier iteration replaced the
//! periodic IDR with libx264 *intra refresh* / an infinite NVENC GOP, but
//! that left a desynced decoder with no clean recovery point (Linux ffmpeg
//! ignores PLI on a long-lived stdin pipe, so there was no on-demand
//! keyframe either) — the finite GOP restores recoverability.

use super::{EncodedPacket, Encoder, PayloadSpec};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

/// Process-wide ban on `h264_vaapi` after the runtime watchdog observes a
/// silent failure (encoder accepts input but produces no output for many
/// frames). On hosts where VA-API "succeeds" at init but then produces
/// nothing — common in VMs where virtio-gpu video acceleration is broken
/// (e.g. UTM on Apple silicon) — the watchdog flips this once and every
/// subsequent `FfmpegH264Encoder::new` call goes straight to libx264.
/// One-way: never cleared, so we don't keep retrying a known-broken path.
static VAAPI_BANNED: AtomicBool = AtomicBool::new(false);

/// Process-wide ban on `h264_nvenc`, mirroring [`VAAPI_BANNED`].
///
/// NVENC's failure mode at *startup* (no NVIDIA GPU / driver, exhausted
/// encoder sessions) is a hard immediate ffmpeg exit, which
/// [`FfmpegH264Encoder::from_child`] already catches and turns into a
/// fall-through to VA-API/libx264. This flag exists for the *silent*
/// failure mode (init succeeds, then no output) so the same watchdog that
/// bans VA-API can ban NVENC and stop re-spawning a known-dead encoder on
/// every peer reconnect. One-way: never cleared.
static NVENC_BANNED: AtomicBool = AtomicBool::new(false);

/// Mark `h264_vaapi` as broken on this host for the rest of the process.
pub fn ban_vaapi() {
    VAAPI_BANNED.store(true, Ordering::SeqCst);
}

/// Whether the watchdog has banned `h264_vaapi` for this process.
pub fn is_vaapi_banned() -> bool {
    VAAPI_BANNED.load(Ordering::Relaxed)
}

/// Mark `h264_nvenc` as broken on this host for the rest of the process.
pub fn ban_nvenc() {
    NVENC_BANNED.store(true, Ordering::SeqCst);
}

/// Whether the watchdog has banned `h264_nvenc` for this process.
pub fn is_nvenc_banned() -> bool {
    NVENC_BANNED.load(Ordering::Relaxed)
}

/// A complete NAL unit extracted by the reader thread.
struct Nal {
    /// Raw NAL bytes (without the Annex-B start code prefix).
    data: Vec<u8>,
    /// H264 NAL unit type (lower 5 bits of the first byte).
    nal_type: u8,
}

/// ffmpeg-based H264 encoder implementing the `Encoder` trait.
///
/// Spawns an ffmpeg subprocess that reads raw I420 from stdin and writes
/// Annex-B H264 to stdout.  A dedicated reader thread splits the stdout
/// stream on Annex-B start codes and sends complete NAL units over a
/// channel.  Frame boundaries are detected deterministically by parsing
/// `first_mb_in_slice` from each slice NAL's Exp-Golomb header — a new
/// frame starts whenever `first_mb_in_slice == 0`.
pub struct FfmpegH264Encoder {
    child: Child,
    width: u32,
    height: u32,
    frame_count: u64,
    pts_offset: u64,
    frame_size: usize,
    /// Channel receiving complete NAL units from the reader thread.
    nal_rx: std::sync::mpsc::Receiver<Nal>,
    /// Handle for the stdout reader thread, joined on drop.
    reader_thread: Option<std::thread::JoinHandle<()>>,
    /// NALs belonging to the current (incomplete) frame, accumulated across
    /// encode() calls. ffmpeg is pipelined — output for frame N may arrive
    /// only after frame N+1 is fed in.
    pending_frame_nals: Vec<Nal>,
    /// Timestamp to assign to the next completed frame.
    pending_frame_pts: u64,
    /// Cached canonical payload spec for this encoder. Attached to every
    /// emitted packet so the WebRTC driver can verify it against the
    /// peer-negotiated sender codec. All ffmpeg H.264 packets produced here
    /// are Constrained Baseline, packetization-mode 1 (matches both the
    /// h264_vaapi config and libx264 `-profile:v baseline` above).
    payload_spec: PayloadSpec,

    /// Most recently observed SPS NAL (type 7) from this encoder's output,
    /// raw bytes without start code. Populated lazily as NALs flow through
    /// `encode()`.
    ///
    /// Used by `build_packet` to guarantee SPS/PPS precede every IDR
    /// access unit on the wire — WebRTC depacketizers (per RFC 6184)
    /// require the parameter sets to repeat alongside each IDR, but
    /// libx264's default behaviour with a long-lived stdin pipe is to
    /// emit them only at stream start. Without this guarantee the
    /// browser receives bare IDR slices that reference an SPS/PPS the
    /// decoder never re-saw after a transport gap and produces no
    /// frames despite RTP flowing healthily.
    ///
    /// Cache lifetime is per encoder instance: when the pool's
    /// `on_resize` regenerates the encoder, the new instance starts
    /// with an empty cache and re-populates it from the first
    /// libx264 IDR (which always carries SPS+PPS at stream start).
    /// So stale parameter sets cannot cross a resolution change.
    cached_sps: Option<Vec<u8>>,

    /// PPS counterpart to [`Self::cached_sps`]. Same lifetime + invariant.
    cached_pps: Option<Vec<u8>>,

    /// True after the encoder has logged a "no cached SPS/PPS at first
    /// IDR" warning, so subsequent occurrences don't spam the log. In
    /// practice this should never fire — libx264's first IDR always
    /// includes parameter sets — but if it does, one diagnostic line
    /// per encoder lifetime is enough to flag the anomaly.
    warned_missing_params: bool,
}

impl FfmpegH264Encoder {
    /// Create a new ffmpeg H264 encoder subprocess.
    ///
    /// Selection order: `h264_nvenc` (NVIDIA) → `h264_vaapi` (VA-API) →
    /// software `libx264`. Each arm logs and falls through on failure;
    /// arms the runtime watchdog has banned for this process (see
    /// [`NVENC_BANNED`] / [`VAAPI_BANNED`]) are skipped outright so we
    /// don't re-spawn a known-broken encoder on every peer reconnect.
    /// Returns `Err` only if ffmpeg is missing or every arm fails.
    pub fn new(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, String> {
        // Check that ffmpeg exists.
        let ffmpeg_check = Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        if ffmpeg_check.is_err() {
            return Err("ffmpeg not found in PATH".to_string());
        }

        // Accumulate per-arm errors for a useful final message if all fail.
        let mut errs: Vec<String> = Vec::new();

        // 1) NVENC (NVIDIA hardware). Skipped if banned or if the ffmpeg
        //    build lacks the encoder (cheap probe avoids a spawn + 100 ms
        //    sleep on the common no-NVIDIA host). A present-but-GPU-less
        //    encoder still exits immediately and is caught by from_child.
        if is_nvenc_banned() {
            eprintln!("[display/h264_linux] skipping h264_nvenc (banned this session)");
        } else if !nvenc_encoder_available() {
            errs.push("nvenc=not built into ffmpeg".to_string());
        } else {
            match Self::spawn_nvenc(width, height, bitrate_kbps) {
                Ok(enc) => {
                    eprintln!("[display/h264_linux] using h264_nvenc encoder");
                    return Ok(enc);
                }
                Err(e) => {
                    eprintln!(
                        "[display/h264_linux] NVENC unavailable ({}), trying VA-API",
                        e,
                    );
                    errs.push(format!("nvenc={e}"));
                }
            }
        }

        // 2) VA-API.
        if is_vaapi_banned() {
            eprintln!("[display/h264_linux] skipping h264_vaapi (banned this session)");
            errs.push("vaapi=banned".to_string());
        } else {
            match Self::spawn_vaapi(width, height, bitrate_kbps) {
                Ok(enc) => {
                    eprintln!("[display/h264_linux] using h264_vaapi encoder");
                    return Ok(enc);
                }
                Err(e) => {
                    eprintln!(
                        "[display/h264_linux] VA-API unavailable ({}), trying libx264",
                        e,
                    );
                    errs.push(format!("vaapi={e}"));
                }
            }
        }

        // 3) Software libx264 (always-available baseline).
        match Self::spawn_x264(width, height, bitrate_kbps) {
            Ok(enc) => {
                eprintln!("[display/h264_linux] using libx264 software encoder");
                Ok(enc)
            }
            Err(x264_err) => {
                errs.push(format!("x264={x264_err}"));
                Err(format!("no H264 encoder: {}", errs.join(", ")))
            }
        }
    }

    /// Spawn ffmpeg with VA-API hardware encoder.
    fn spawn_vaapi(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, String> {
        // VA-API requires uploading frames to GPU via hwupload.
        // Input: raw I420 on stdin.  Output: Annex-B H264 on stdout.
        let child = Command::new("ffmpeg")
            .args(vaapi_ffmpeg_args(width, height, bitrate_kbps))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn ffmpeg: {e}"))?;

        // Wait briefly and check if the process exited immediately (missing
        // VA-API driver, no /dev/dri/renderD128, etc.).
        std::thread::sleep(std::time::Duration::from_millis(100));

        Self::from_child(child, width, height)
    }

    /// Spawn ffmpeg with the NVIDIA NVENC hardware encoder.
    fn spawn_nvenc(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, String> {
        let child = Command::new("ffmpeg")
            .args(nvenc_ffmpeg_args(width, height, bitrate_kbps))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn ffmpeg: {e}"))?;

        // Wait briefly and check if the process exited immediately (no
        // NVIDIA GPU/driver, exhausted encoder sessions, etc.).
        std::thread::sleep(std::time::Duration::from_millis(100));

        Self::from_child(child, width, height)
    }

    /// Spawn ffmpeg with software x264 encoder.
    fn spawn_x264(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, String> {
        let child = Command::new("ffmpeg")
            .args(x264_ffmpeg_args(width, height, bitrate_kbps))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn ffmpeg: {e}"))?;

        std::thread::sleep(std::time::Duration::from_millis(100));

        Self::from_child(child, width, height)
    }

    fn from_child(mut child: Child, width: u32, height: u32) -> Result<Self, String> {
        // Check if ffmpeg died during startup.
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stderr_out = String::new();
                if let Some(ref mut stderr) = child.stderr {
                    let _ = stderr.read_to_string(&mut stderr_out);
                }
                return Err(format!(
                    "ffmpeg exited immediately ({}): {}",
                    status,
                    stderr_out.trim(),
                ));
            }
            Ok(None) => {} // still running -- good
            Err(e) => return Err(format!("ffmpeg wait: {e}")),
        }

        let uv_w = ((width + 1) / 2) as usize;
        let uv_h = ((height + 1) / 2) as usize;
        let frame_size = (width as usize * height as usize) + 2 * (uv_w * uv_h);

        // Spawn a dedicated reader thread that reads ffmpeg stdout in
        // blocking mode, splits on Annex-B start codes, and sends
        // complete NAL units over the channel.
        let (nal_tx, nal_rx) = std::sync::mpsc::sync_channel::<Nal>(64);
        let stdout = child.stdout.take().ok_or("ffmpeg stdout not available")?;
        let reader_thread = std::thread::spawn(move || {
            nal_reader_thread(stdout, nal_tx);
        });

        Ok(Self {
            child,
            width,
            height,
            frame_count: 0,
            pts_offset: 0,
            frame_size,
            nal_rx,
            reader_thread: Some(reader_thread),
            pending_frame_nals: Vec::new(),
            pending_frame_pts: 0,
            payload_spec: PayloadSpec::h264_constrained_baseline(),
            cached_sps: None,
            cached_pps: None,
            warned_missing_params: false,
        })
    }

    /// Flush the currently-accumulated NALs (`pending_frame_nals`) into an
    /// `EncodedPacket` and append it to `out`. Wraps `build_packet` with
    /// the cached SPS/PPS values + the first-IDR-without-cache warning,
    /// so each call site has one short helper instead of duplicating the
    /// glue.
    fn flush_pending_packet(&mut self, duration_ms: u64, out: &mut Vec<EncodedPacket>) {
        if self.pending_frame_nals.is_empty() {
            return;
        }
        let has_idr = self.pending_frame_nals.iter().any(|n| n.nal_type == 5);
        let has_sps_in_au = self.pending_frame_nals.iter().any(|n| n.nal_type == 7);
        let has_pps_in_au = self.pending_frame_nals.iter().any(|n| n.nal_type == 8);
        // Warn at most once per encoder if we ever see a bare IDR before
        // libx264 has shipped its first complete IDR. In practice this
        // never fires on a well-behaved libx264 stdin pipe, but if a
        // future encoder swap or codec misconfig produces this shape, the
        // resulting black-video would otherwise be invisible at this
        // layer. See `warned_missing_params` field doc.
        if has_idr
            && (!has_sps_in_au && self.cached_sps.is_none()
                || !has_pps_in_au && self.cached_pps.is_none())
            && !self.warned_missing_params
        {
            eprintln!(
                "[display/h264_linux] WARN: IDR access unit missing SPS or \
                 PPS and no cached parameter sets available — receiver \
                 cannot decode. Should self-correct on next complete IDR.",
            );
            self.warned_missing_params = true;
        }
        if let Some(pkt) = Self::build_packet(
            &self.pending_frame_nals,
            self.cached_sps.as_deref(),
            self.cached_pps.as_deref(),
            self.pending_frame_pts,
            duration_ms,
            &self.payload_spec,
        ) {
            out.push(pkt);
        }
    }

    /// Check whether a slice NAL starts a new access unit (frame).
    ///
    /// In H264, the first field of every slice header is `first_mb_in_slice`,
    /// encoded as an Exp-Golomb unsigned integer.  The first slice of a new
    /// frame always has `first_mb_in_slice == 0`; continuation slices of the
    /// same frame have `first_mb_in_slice > 0`.
    fn is_first_slice(nal: &Nal) -> bool {
        parse_first_mb_in_slice(&nal.data) == 0
    }
}

impl FfmpegH264Encoder {
    /// Build an EncodedPacket from a set of NAL units forming a complete
    /// access unit (one video frame). Wraps each NAL with an Annex-B
    /// start code. `payload_spec` is cloned in so the function stays a
    /// free-ish associated helper (no `&self`) while each emitted packet
    /// still carries the encoder's fmtp identity.
    ///
    /// **SPS/PPS guarantee for IDR access units.** WebRTC depacketizers
    /// (RFC 6184) require parameter sets to repeat alongside every IDR
    /// — without them the decoder cannot reinitialize after a transport
    /// gap and no frames decode. libx264's long-lived stdin pipe emits
    /// SPS/PPS only at stream start by default, so most natural-cadence
    /// (`-g N`) IDRs arrive on the wire as bare slices that reference
    /// parameter sets the receiver may never have seen.
    ///
    /// This function compensates: if `nals` contains an IDR (type 5)
    /// but is missing SPS (type 7) or PPS (type 8), and the caller
    /// supplied cached values from a prior access unit, the cached
    /// parameter sets are inserted before the first non-AUD NAL.
    /// AUD (type 9), if present, is preserved at the front so the
    /// final order stays canonical (AUD → SPS → PPS → IDR).
    ///
    /// No fabrication: if the cache is empty AND the access unit lacks
    /// parameter sets, the IDR is emitted unchanged. The caller is
    /// expected to log that anomaly once per encoder; the function
    /// itself stays pure and side-effect-free.
    ///
    /// Pure / idempotent on already-complete access units: an input
    /// containing SPS+PPS+IDR (libx264's stream-start shape) emits
    /// exactly the same bytes whether or not the cache is populated —
    /// no duplicate parameter sets.
    fn build_packet(
        nals: &[Nal],
        cached_sps: Option<&[u8]>,
        cached_pps: Option<&[u8]>,
        pts_ms: u64,
        duration_ms: u64,
        payload_spec: &PayloadSpec,
    ) -> Option<EncodedPacket> {
        if nals.is_empty() {
            return None;
        }

        let has_idr = nals.iter().any(|n| n.nal_type == 5);
        let has_sps = nals.iter().any(|n| n.nal_type == 7);
        let has_pps = nals.iter().any(|n| n.nal_type == 8);
        let prepend_sps = has_idr && !has_sps && cached_sps.is_some();
        let prepend_pps = has_idr && !has_pps && cached_pps.is_some();

        let mut data = Vec::new();
        let mut is_keyframe = false;
        // Emit cached parameter sets exactly once, immediately before the
        // FIRST IDR slice in this access unit. Iterating in input order
        // means anything preceding the IDR (AUD, an existing SPS, an
        // existing PPS, SEI) is emitted before the prepend, giving the
        // canonical AUD → SPS → PPS → IDR ordering whether the parameter
        // sets came from input or cache. The "missing" branch only
        // inserts the side that the AU lacks, so existing parameter
        // sets are not duplicated.
        let mut params_inserted = false;
        for nal in nals {
            if !params_inserted && nal.nal_type == 5 && (prepend_sps || prepend_pps) {
                if prepend_sps {
                    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                    data.extend_from_slice(cached_sps.unwrap());
                }
                if prepend_pps {
                    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                    data.extend_from_slice(cached_pps.unwrap());
                }
                params_inserted = true;
            }
            data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            data.extend_from_slice(&nal.data);
            if nal.nal_type == 5 {
                is_keyframe = true;
            }
        }
        Some(EncodedPacket {
            data,
            pts_ms,
            duration_ms,
            is_keyframe,
            payload_spec: payload_spec.clone(),
        })
    }
}

impl Encoder for FfmpegH264Encoder {
    fn encode(
        &mut self,
        i420: &[u8],
        duration_ms: u64,
        _force_keyframe: bool,
    ) -> Result<Vec<EncodedPacket>, String> {
        // NOTE: `force_keyframe` cannot be honored on a long-running ffmpeg
        // stdin pipe -- there is no way to inject per-frame "emit IDR now"
        // metadata through rawvideo.  We rely on the finite GOP
        // (`-g GOP_PERIOD_FRAMES`) configured at spawn time to bound how
        // long a fresh or desynced decoder waits for a clean IDR.
        if i420.len() < self.frame_size {
            return Err(format!(
                "I420 buffer too small: {} < {}",
                i420.len(),
                self.frame_size,
            ));
        }

        // Write the I420 frame to ffmpeg stdin.
        {
            let stdin = self.child.stdin.as_mut().ok_or("ffmpeg stdin closed")?;
            stdin
                .write_all(&i420[..self.frame_size])
                .map_err(|e| format!("write to ffmpeg: {e}"))?;
            stdin.flush().map_err(|e| format!("flush ffmpeg: {e}"))?;
        }

        // Assign pts to THIS input frame. The NALs that come out may
        // correspond to an earlier input — but with zero-latency tuning
        // and no B-frames, output should be in display order.
        let input_pts = self.pts_offset;
        self.pts_offset += duration_ms;
        self.frame_count += 1;

        // Drain whatever NALs are available right now. ffmpeg is pipelined:
        // it may buffer 1-2 frames before producing output. We accumulate
        // NALs into `pending_frame_nals` across calls and flush completed
        // frames when we detect the next access unit boundary (first slice
        // NAL with first_mb_in_slice == 0).
        let mut out_packets = Vec::new();
        let mut have_slice = self
            .pending_frame_nals
            .iter()
            .any(|n| n.nal_type == 1 || n.nal_type == 5);

        loop {
            let nal = match self.nal_rx.try_recv() {
                Ok(n) => n,
                Err(_) => break, // no more NALs right now
            };
            // Cache parameter sets as they flow through. libx264 emits
            // SPS+PPS at stream start (and on encoder resets), so the
            // cache populates on the first complete IDR access unit and
            // stays fresh thereafter. See `cached_sps` field doc for the
            // RFC 6184 rationale.
            match nal.nal_type {
                7 => self.cached_sps = Some(nal.data.clone()),
                8 => self.cached_pps = Some(nal.data.clone()),
                _ => {}
            }
            let is_slice = nal.nal_type == 1 || nal.nal_type == 5;
            if is_slice && have_slice && Self::is_first_slice(&nal) {
                // New frame starts — flush what we have.
                self.flush_pending_packet(duration_ms, &mut out_packets);
                self.pending_frame_nals.clear();
                self.pending_frame_nals.push(nal);
                self.pending_frame_pts = input_pts;
                have_slice = true;
            } else {
                if is_slice && !have_slice {
                    // First slice we've seen in the buffer — stamp pts.
                    self.pending_frame_pts = input_pts;
                    have_slice = true;
                }
                self.pending_frame_nals.push(nal);
            }
        }

        Ok(out_packets)
    }

    fn codec_mime(&self) -> &'static str {
        "video/H264"
    }

    fn payload_spec(&self) -> &PayloadSpec {
        &self.payload_spec
    }
}

impl Drop for FfmpegH264Encoder {
    fn drop(&mut self) {
        // Close stdin to signal EOF, then kill if it doesn't exit.
        drop(self.child.stdin.take());
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            _ => {
                let pid = self.child.id();
                let _ = self.child.kill();
                let _ = self.child.wait();
                eprintln!("[display/h264_linux] killed ffmpeg encoder (pid {})", pid);
            }
        }
        // Join the reader thread.  With stdin closed and the process
        // killed, stdout will EOF and the thread will exit.
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}

/// GOP length in frames (the `-g` interval) for all three H.264 arms.
///
/// A finite, bounded GOP guarantees a desynced decoder a real IDR within
/// `GOP_PERIOD_FRAMES` frames. At the 30 fps feed rate, 60 frames is a
/// keyframe every ~2 s — short enough that a fresh peer or a
/// post-loss-burst decoder recovers promptly, long enough that periodic
/// IDRs don't dominate the bitrate. Combined with the quarter-resolution
/// + capped-bitrate federated layer (`encode/pool.rs`), each such IDR is
/// only a handful of RTP packets, so it survives the lossy relay it has
/// to cross. Replaces the earlier intra-refresh experiment, which left a
/// desynced decoder with no clean recovery point (Linux ffmpeg ignores
/// PLI on a long-lived stdin pipe — see the module docs).
const GOP_PERIOD_FRAMES: &str = "60";

/// Probe whether this ffmpeg build includes the `h264_nvenc` encoder.
///
/// Cheap (`ffmpeg -encoders`, no GPU touched) and run before the first
/// `spawn_nvenc` so the common no-NVIDIA host skips a subprocess spawn +
/// 100 ms detection sleep. A *present* encoder on a GPU-less host still
/// passes this probe but then exits immediately on spawn, which
/// [`FfmpegH264Encoder::from_child`] catches — so this is an optimization,
/// not the correctness gate.
fn nvenc_encoder_available() -> bool {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) => {
            let listing = String::from_utf8_lossy(&o.stdout);
            listing.contains("h264_nvenc")
        }
        Err(_) => false,
    }
}

/// ffmpeg argument vector for the software libx264 encoder.
///
/// **Loss-resilience.** `-g GOP_PERIOD_FRAMES` gives a finite, bounded GOP
/// so a desynced decoder gets a real IDR within ~2 s (the federated layer
/// is quarter-res + bitrate-capped upstream, so that IDR is only a handful
/// of RTP packets and survives the lossy relay). `slice-max-size=1200`
/// keeps each slice small enough to fit a single ~1200-byte RTP payload,
/// so a lost packet costs one slice, not a frame, and composes with the
/// reduced resolution. `ref=1` + `bframes=0` pin the closed-GOP, single-
/// reference shape (`ultrafast`/`zerolatency` already imply no B-frames;
/// the explicit values keep the GOP closed so each IDR is a clean,
/// self-contained recovery point).
///
/// `repeat-headers=1` keeps SPS/PPS in-band (defense-in-depth alongside the
/// `build_packet` daemon-side prepend, which guarantees SPS/PPS precede
/// every periodic IDR even if libx264 omits them on the long-lived pipe).
/// Input stays `rawvideo`/`yuv420p`; output is Constrained-Baseline Annex-B.
fn x264_ffmpeg_args(width: u32, height: u32, bitrate_kbps: u32) -> Vec<String> {
    [
        "-hide_banner",
        "-loglevel",
        "error",
        // Input: raw I420 from stdin
        "-f",
        "rawvideo",
        "-pixel_format",
        "yuv420p",
        "-video_size",
        &format!("{width}x{height}"),
        "-framerate",
        "30",
        "-i",
        "pipe:0",
        // Software encode
        "-c:v",
        "libx264",
        "-b:v",
        &format!("{bitrate_kbps}k"),
        "-profile:v",
        "baseline",
        "-preset",
        "ultrafast",
        "-tune",
        "zerolatency",
        "-g",
        GOP_PERIOD_FRAMES,
        "-bf",
        "0",
        // Finite GOP (above) + small slices for loss-resilience, plus
        // in-band parameter sets. `slice-max-size=1200` caps each NAL to a
        // single ~MTU RTP payload; `ref=1` + `bframes=0` pin a closed,
        // single-reference GOP so each periodic IDR is a clean recovery
        // point. See fn doc.
        "-x264-params",
        "slice-max-size=1200:ref=1:bframes=0:repeat-headers=1",
        // Output: raw H264 Annex-B to stdout
        "-f",
        "h264",
        "pipe:1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// ffmpeg argument vector for the NVIDIA `h264_nvenc` hardware encoder.
///
/// Mirrors [`vaapi_ffmpeg_args`] / [`x264_ffmpeg_args`] but for NVENC.
/// Critically, the input stays `rawvideo`/`yuv420p` with **no**
/// `format=nv12,hwupload` filter: NVENC ingests host I420 directly (the
/// `hwupload` dance is VA-API-only — applying it here would fail or force
/// an unnecessary CUDA frames context). `-preset p1 -tune ll -zerolatency
/// 1` is the low-latency NVENC config; `-rc cbr` with matched `b:v`/
/// `maxrate` and a half-`bufsize` gives a steady stream. `-profile:v
/// baseline` produces a Constrained-Baseline Annex-B stream that matches
/// the existing SPS/PPS-prepend + NAL-reader expectations exactly.
///
/// **Loss-resilience:** `-g GOP_PERIOD_FRAMES` gives a finite, bounded GOP
/// so NVENC emits a periodic IDR every ~2 s — a real recovery point for a
/// desynced decoder. Loss-survivability comes from the upstream
/// quarter-resolution + capped-bitrate federated layer (`encode/pool.rs`),
/// which keeps that IDR small enough to reassemble on the lossy relay. An
/// earlier iteration pinned an effectively-infinite GOP + `-intra-refresh
/// 1` (plus `-no-scenecut 1` / `-forced-idr 0`) to suppress all post-seed
/// IDRs, but that left a desynced decoder with no clean recovery point
/// (NVENC ignores PLI on this long-lived stdin pipe just as libx264 does),
/// so the finite GOP is restored here.
fn nvenc_ffmpeg_args(width: u32, height: u32, bitrate_kbps: u32) -> Vec<String> {
    [
        "-hide_banner",
        "-loglevel",
        "error",
        // Input: raw I420 from stdin (NVENC ingests I420 directly — no
        // nv12/hwupload, that's the VA-API path).
        "-f",
        "rawvideo",
        "-pixel_format",
        "yuv420p",
        "-video_size",
        &format!("{width}x{height}"),
        "-framerate",
        "30",
        "-i",
        "pipe:0",
        // NVENC hardware encode, low-latency config.
        "-c:v",
        "h264_nvenc",
        "-preset",
        "p1",
        "-tune",
        "ll",
        "-zerolatency",
        "1",
        "-rc",
        "cbr",
        "-b:v",
        &format!("{bitrate_kbps}k"),
        "-maxrate",
        &format!("{bitrate_kbps}k"),
        "-bufsize",
        &format!("{}k", bitrate_kbps / 2),
        "-profile:v",
        "baseline",
        // Finite, bounded GOP: a periodic IDR every GOP_PERIOD_FRAMES so a
        // desynced decoder gets a real recovery point (the federated layer
        // is quarter-res + bitrate-capped upstream, keeping that IDR small
        // on the wire). See fn doc + GOP_PERIOD_FRAMES.
        "-g",
        GOP_PERIOD_FRAMES,
        "-bf",
        "0",
        // Output: raw H264 Annex-B to stdout
        "-f",
        "h264",
        "pipe:1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// ffmpeg argument vector for the VA-API `h264_vaapi` hardware encoder.
///
/// Unchanged from the original inline args (periodic-IDR `-g`), extracted to
/// a pure builder for symmetry + testability. VA-API uniquely needs
/// `-vaapi_device` + the `format=nv12,hwupload` filter to move frames onto
/// the GPU; that filter is intentionally absent from the NVENC/x264 paths.
fn vaapi_ffmpeg_args(width: u32, height: u32, bitrate_kbps: u32) -> Vec<String> {
    [
        "-hide_banner",
        "-loglevel",
        "error",
        // Input: raw I420 from stdin
        "-f",
        "rawvideo",
        "-pixel_format",
        "yuv420p",
        "-video_size",
        &format!("{width}x{height}"),
        "-framerate",
        "30",
        "-i",
        "pipe:0",
        // VA-API device
        "-vaapi_device",
        "/dev/dri/renderD128",
        // Upload to GPU + encode
        "-vf",
        "format=nv12,hwupload",
        "-c:v",
        "h264_vaapi",
        "-b:v",
        &format!("{bitrate_kbps}k"),
        "-profile:v",
        "constrained_baseline",
        "-g",
        "30", // keyframe every 1s at 30fps feed rate
        "-bf",
        "0", // no B-frames
        // Output: raw H264 Annex-B to stdout
        "-f",
        "h264",
        "pipe:1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Dedicated reader thread for ffmpeg stdout.
///
/// Reads the Annex-B H264 byte stream in blocking mode, splits on start
/// codes (`00 00 01` or `00 00 00 01`), and sends each complete NAL unit
/// over the channel.  This is deterministic -- frame boundaries are
/// detected by NAL type in the `encode()` caller, not by timeouts.
///
/// The thread exits when stdout reaches EOF (ffmpeg closed or killed) or
/// the channel receiver is dropped.
fn nal_reader_thread(
    mut stdout: std::process::ChildStdout,
    nal_tx: std::sync::mpsc::SyncSender<Nal>,
) {
    let mut buf = Vec::with_capacity(65536);
    let mut tmp = [0u8; 65536];

    loop {
        let n = match stdout.read(&mut tmp) {
            Ok(0) => break, // EOF
            Ok(n) => n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        buf.extend_from_slice(&tmp[..n]);

        // Extract and send all complete NAL units from buf.
        // A NAL is complete when we find the *next* start code after it.
        // Any data before the first start code is discarded (shouldn't
        // happen in practice with ffmpeg, but defensive).
        loop {
            // Find the first start code in buf.
            let first = match find_start_code(&buf, 0) {
                Some(pos) => pos,
                None => {
                    // No start code at all -- keep buffering.
                    break;
                }
            };

            // Discard any bytes before the first start code.
            if first.offset > 0 {
                buf.drain(..first.offset);
                // Recalculate -- first is now at offset 0.
                continue;
            }

            // Look for the next start code after the first one.
            let nal_body_start = first.sc_len;
            let second = find_start_code(&buf, nal_body_start);

            match second {
                Some(next) => {
                    // We have a complete NAL: from nal_body_start..next.offset.
                    let nal_data: Vec<u8> = buf[nal_body_start..next.offset].to_vec();
                    buf.drain(..next.offset);

                    if !nal_data.is_empty() {
                        let nal_type = nal_data[0] & 0x1F;
                        if nal_tx
                            .send(Nal {
                                data: nal_data,
                                nal_type,
                            })
                            .is_err()
                        {
                            return; // receiver dropped
                        }
                    }
                }
                None => {
                    // Only one start code found -- NAL is still
                    // incomplete, wait for more data.
                    break;
                }
            }
        }
    }

    // Flush the last NAL (no trailing start code at EOF).
    if let Some(first) = find_start_code(&buf, 0) {
        let nal_data: Vec<u8> = buf[first.sc_len..].to_vec();
        if !nal_data.is_empty() {
            let nal_type = nal_data[0] & 0x1F;
            let _ = nal_tx.send(Nal {
                data: nal_data,
                nal_type,
            });
        }
    }
}

/// Position and length of an Annex-B start code found in a buffer.
struct StartCode {
    /// Byte offset of the start code in the buffer.
    offset: usize,
    /// Length of the start code (3 for `00 00 01`, 4 for `00 00 00 01`).
    sc_len: usize,
}

/// Find the next Annex-B start code in `buf` starting at `from`.
///
/// Recognises both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) forms.
fn find_start_code(buf: &[u8], from: usize) -> Option<StartCode> {
    let mut i = from;
    while i + 2 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if i + 3 < buf.len() && buf[i + 2] == 0 && buf[i + 3] == 1 {
                return Some(StartCode {
                    offset: i,
                    sc_len: 4,
                });
            }
            if buf[i + 2] == 1 {
                return Some(StartCode {
                    offset: i,
                    sc_len: 3,
                });
            }
        }
        i += 1;
    }
    None
}

/// Parse `first_mb_in_slice` from a slice NAL's raw data.
///
/// The first byte is the NAL header (forbidden_bit + nal_ref_idc + nal_type).
/// Immediately after that, the slice header begins with `first_mb_in_slice`
/// encoded as an Exp-Golomb unsigned integer.
///
/// Exp-Golomb decoding: count N leading zero bits, then read (N+1) bits as a
/// binary number, subtract 1.  For example:
/// - `1`       → 0 leading zeros → value = 1 - 1 = 0
/// - `010`     → 1 leading zero  → value = 2 - 1 = 1
/// - `00110`   → 2 leading zeros → value = 6 - 1 = 5
fn parse_first_mb_in_slice(nal_data: &[u8]) -> u32 {
    if nal_data.len() < 2 {
        return 0;
    }
    // Skip NAL header byte — slice header starts at bit 8.
    let mut bit_offset: usize = 8;
    // Count leading zero bits.
    let mut leading_zeros = 0u32;
    while bit_offset / 8 < nal_data.len() {
        let byte_idx = bit_offset / 8;
        let bit_idx = 7 - (bit_offset % 8);
        if (nal_data[byte_idx] >> bit_idx) & 1 == 0 {
            leading_zeros += 1;
            bit_offset += 1;
        } else {
            break;
        }
    }
    if leading_zeros == 0 {
        // Code word is `1` → value = 0.
        return 0;
    }
    // Skip the `1` bit that terminated the leading zeros.
    bit_offset += 1;
    // Read the next `leading_zeros` bits to form the suffix.
    let mut value = 1u32;
    for _ in 0..leading_zeros {
        let byte_idx = bit_offset / 8;
        if byte_idx >= nal_data.len() {
            return 0;
        }
        let bit_idx = 7 - (bit_offset % 8);
        value = (value << 1) | ((nal_data[byte_idx] >> bit_idx) & 1) as u32;
        bit_offset += 1;
    }
    value - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_start_code_4byte() {
        let buf = [0x00, 0x00, 0x00, 0x01, 0x67];
        let sc = find_start_code(&buf, 0).unwrap();
        assert_eq!(sc.offset, 0);
        assert_eq!(sc.sc_len, 4);
    }

    #[test]
    fn find_start_code_3byte() {
        let buf = [0x00, 0x00, 0x01, 0x67];
        let sc = find_start_code(&buf, 0).unwrap();
        assert_eq!(sc.offset, 0);
        assert_eq!(sc.sc_len, 3);
    }

    #[test]
    fn find_start_code_with_offset() {
        let buf = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x01, 0x68, 0xCE,
        ];
        // Skip past the first start code body.
        let sc = find_start_code(&buf, 4).unwrap();
        assert_eq!(sc.offset, 8);
        assert_eq!(sc.sc_len, 4);
    }

    #[test]
    fn find_start_code_none() {
        let buf = [0x00, 0x01, 0x02];
        assert!(find_start_code(&buf, 0).is_none());
    }

    #[test]
    fn nal_reader_thread_emits_all_nals() {
        // Simulate ffmpeg stdout with SPS + PPS + IDR.
        let stream = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x0A, // SPS (type 7)
            0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x38, 0x80, // PPS (type 8)
            0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, // IDR (type 5)
        ];

        let (nal_tx, nal_rx) = std::sync::mpsc::sync_channel::<Nal>(64);
        let handle = std::thread::spawn(move || {
            let cursor = std::io::Cursor::new(stream);
            // nal_reader_thread expects ChildStdout, but we can test the
            // parsing logic by calling the helper functions directly.
            // Instead, pipe through a real fd pair.
            drop((cursor, nal_tx));
        });

        // Since we can't easily fake a ChildStdout, test the parsing
        // indirectly through find_start_code + the extraction logic.
        let buf = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x01, 0x68, 0xCE,
            0x38, 0x80, 0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB,
        ];

        // Extract NALs the same way the reader thread does.
        let mut nals = Vec::new();
        let mut offset = 0;
        while let Some(first) = find_start_code(&buf, offset) {
            let body_start = first.offset + first.sc_len;
            if let Some(next) = find_start_code(&buf, body_start) {
                let data = buf[body_start..next.offset].to_vec();
                let nal_type = data[0] & 0x1F;
                nals.push(Nal { data, nal_type });
                offset = next.offset;
            } else {
                // Last NAL.
                let data = buf[body_start..].to_vec();
                let nal_type = data[0] & 0x1F;
                nals.push(Nal { data, nal_type });
                break;
            }
        }

        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0].nal_type, 7); // SPS
        assert_eq!(nals[0].data, vec![0x67, 0x42, 0x00, 0x0A]);
        assert_eq!(nals[1].nal_type, 8); // PPS
        assert_eq!(nals[1].data, vec![0x68, 0xCE, 0x38, 0x80]);
        assert_eq!(nals[2].nal_type, 5); // IDR
        assert_eq!(nals[2].data, vec![0x65, 0xAA, 0xBB]);

        let _ = handle.join();
        drop(nal_rx);
    }

    #[test]
    fn nal_type_classification() {
        // Verify NAL type extraction from first byte.
        assert_eq!(0x67 & 0x1F, 7); // SPS
        assert_eq!(0x68 & 0x1F, 8); // PPS
        assert_eq!(0x06 & 0x1F, 6); // SEI
        assert_eq!(0x65 & 0x1F, 5); // IDR slice
        assert_eq!(0x41 & 0x1F, 1); // non-IDR slice
        assert_eq!(0x61 & 0x1F, 1); // non-IDR slice (different NRI)
    }

    #[test]
    fn parse_first_mb_zero() {
        // first_mb_in_slice = 0 → Exp-Golomb code word: `1`
        // NAL header byte 0x65 (IDR slice), then 0b1xxx_xxxx = first bit is 1.
        let nal = [0x65, 0x80]; // 0x80 = 0b1000_0000
        assert_eq!(parse_first_mb_in_slice(&nal), 0);
    }

    #[test]
    fn parse_first_mb_one() {
        // first_mb_in_slice = 1 → Exp-Golomb: `010`
        // NAL header 0x41 (non-IDR), then bits: 0 1 0 ...
        // 0b010x_xxxx = 0x40
        let nal = [0x41, 0x40]; // 0x40 = 0b0100_0000
        assert_eq!(parse_first_mb_in_slice(&nal), 1);
    }

    #[test]
    fn parse_first_mb_five() {
        // first_mb_in_slice = 5 → Exp-Golomb: value = 5, code = 5+1 = 6
        // 6 in binary = 110, needs 3 bits, so 2 leading zeros: `00110`
        // NAL header 0x41, then bits: 0 0 1 1 0 ...
        // 0b00110_xxx = 0x30
        let nal = [0x41, 0x30]; // 0x30 = 0b0011_0000
        assert_eq!(parse_first_mb_in_slice(&nal), 5);
    }

    #[test]
    fn parse_first_mb_short_nal() {
        // Single byte NAL — not enough data, should return 0.
        let nal = [0x65];
        assert_eq!(parse_first_mb_in_slice(&nal), 0);
    }

    #[test]
    fn parse_first_mb_large_value() {
        // first_mb_in_slice = 14 → code_num = 14, code_num+1 = 15
        // 15 in binary = 1111, 4 bits, so 3 leading zeros: `0001111`
        // NAL header 0x41, then bits: 0 0 0 1 1 1 1 ...
        // After NAL header byte: 0b0001_1110 = 0x1E
        let nal = [0x41, 0x1E];
        assert_eq!(parse_first_mb_in_slice(&nal), 14);
    }

    #[test]
    fn is_first_slice_detection() {
        // first_mb = 0 → first slice of frame
        let first = Nal {
            data: vec![0x65, 0x80],
            nal_type: 5,
        };
        assert!(FfmpegH264Encoder::is_first_slice(&first));

        // first_mb = 1 → continuation slice
        let cont = Nal {
            data: vec![0x41, 0x40],
            nal_type: 1,
        };
        assert!(!FfmpegH264Encoder::is_first_slice(&cont));
    }

    /// Helper: Annex-B start code prepended to a NAL body, matching what
    /// `build_packet` writes for each NAL it emits.
    fn annexb(body: &[u8]) -> Vec<u8> {
        let mut out = vec![0x00, 0x00, 0x00, 0x01];
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn build_packet_passes_through_complete_idr_unchanged() {
        // libx264's stream-start shape: a single access unit containing
        // SPS + PPS + IDR. Cache may or may not be populated; either way
        // the output must be the canonical [SPS][PPS][IDR] byte sequence
        // with no duplicate parameter sets.
        let sps = vec![0x67, 0x42, 0x00, 0x0A];
        let pps = vec![0x68, 0xCE, 0x38, 0x80];
        let idr = vec![0x65, 0xAA, 0xBB];
        let nals = vec![
            Nal {
                data: sps.clone(),
                nal_type: 7,
            },
            Nal {
                data: pps.clone(),
                nal_type: 8,
            },
            Nal {
                data: idr.clone(),
                nal_type: 5,
            },
        ];

        // Cache populated case (the realistic path: cache was filled by
        // an earlier IDR). Output must still be exactly SPS+PPS+IDR.
        let pkt = FfmpegH264Encoder::build_packet(
            &nals,
            Some(&sps),
            Some(&pps),
            0,
            33,
            &PayloadSpec::h264_constrained_baseline(),
        )
        .unwrap();
        assert!(pkt.is_keyframe);
        let mut want = Vec::new();
        want.extend_from_slice(&annexb(&sps));
        want.extend_from_slice(&annexb(&pps));
        want.extend_from_slice(&annexb(&idr));
        assert_eq!(pkt.data, want, "complete IDR should not be modified");

        // Empty cache case (first IDR, before cache is populated). Same
        // expected output — no fabrication, but also no insertion since
        // the access unit is already complete.
        let pkt = FfmpegH264Encoder::build_packet(
            &nals,
            None,
            None,
            0,
            33,
            &PayloadSpec::h264_constrained_baseline(),
        )
        .unwrap();
        assert_eq!(pkt.data, want, "empty cache must not change complete IDR");
    }

    #[test]
    fn build_packet_prepends_cached_sps_pps_before_bare_idr() {
        // Bare IDR access unit (the libx264 -g N cadence shape after the
        // first keyframe): only an IDR slice, no SPS/PPS. Cached values
        // from a prior keyframe should be inserted before the IDR.
        let cached_sps = vec![0x67, 0x42, 0x00, 0x0A];
        let cached_pps = vec![0x68, 0xCE, 0x38, 0x80];
        let idr = vec![0x65, 0xAA, 0xBB];
        let nals = vec![Nal {
            data: idr.clone(),
            nal_type: 5,
        }];

        let pkt = FfmpegH264Encoder::build_packet(
            &nals,
            Some(&cached_sps),
            Some(&cached_pps),
            33,
            33,
            &PayloadSpec::h264_constrained_baseline(),
        )
        .unwrap();
        assert!(pkt.is_keyframe);
        let mut want = Vec::new();
        want.extend_from_slice(&annexb(&cached_sps));
        want.extend_from_slice(&annexb(&cached_pps));
        want.extend_from_slice(&annexb(&idr));
        assert_eq!(
            pkt.data, want,
            "bare IDR with cached SPS/PPS must come out as SPS+PPS+IDR"
        );
    }

    #[test]
    fn build_packet_prepends_only_missing_param() {
        // Partial case: access unit has SPS but is missing PPS. The
        // function must insert ONLY the cached PPS — not duplicate SPS.
        let sps = vec![0x67, 0x42, 0x00, 0x0A];
        let cached_pps = vec![0x68, 0xCE, 0x38, 0x80];
        let idr = vec![0x65, 0xAA, 0xBB];
        let nals = vec![
            Nal {
                data: sps.clone(),
                nal_type: 7,
            },
            Nal {
                data: idr.clone(),
                nal_type: 5,
            },
        ];

        let pkt = FfmpegH264Encoder::build_packet(
            &nals,
            Some(&sps), // cache also has SPS, but it's already in AU
            Some(&cached_pps),
            33,
            33,
            &PayloadSpec::h264_constrained_baseline(),
        )
        .unwrap();
        assert!(pkt.is_keyframe);
        // Expected: input order preserved (SPS first since it was in the
        // AU), cached PPS inserted before the IDR (the only NAL that
        // would otherwise be missing it), then IDR.
        let mut want = Vec::new();
        want.extend_from_slice(&annexb(&sps));
        want.extend_from_slice(&annexb(&cached_pps));
        want.extend_from_slice(&annexb(&idr));
        assert_eq!(
            pkt.data, want,
            "AU with SPS but no PPS must get only PPS prepended"
        );
        // Sanity: SPS appears exactly once in the output bytes.
        let sps_count = pkt
            .data
            .windows(sps.len())
            .filter(|w| *w == sps.as_slice())
            .count();
        assert_eq!(sps_count, 1, "SPS must not be duplicated");
    }

    #[test]
    fn build_packet_preserves_aud_before_sps_pps() {
        // If an AUD (NAL type 9) is present at the start of the access
        // unit, the canonical RFC 6184 / H.264 ordering is
        // AUD → SPS → PPS → IDR. The cache prepend must land between
        // AUD and IDR, not before AUD.
        let aud = vec![0x09, 0xF0];
        let cached_sps = vec![0x67, 0x42, 0x00, 0x0A];
        let cached_pps = vec![0x68, 0xCE, 0x38, 0x80];
        let idr = vec![0x65, 0xAA, 0xBB];
        let nals = vec![
            Nal {
                data: aud.clone(),
                nal_type: 9,
            },
            Nal {
                data: idr.clone(),
                nal_type: 5,
            },
        ];

        let pkt = FfmpegH264Encoder::build_packet(
            &nals,
            Some(&cached_sps),
            Some(&cached_pps),
            33,
            33,
            &PayloadSpec::h264_constrained_baseline(),
        )
        .unwrap();
        assert!(pkt.is_keyframe);
        let mut want = Vec::new();
        want.extend_from_slice(&annexb(&aud));
        want.extend_from_slice(&annexb(&cached_sps));
        want.extend_from_slice(&annexb(&cached_pps));
        want.extend_from_slice(&annexb(&idr));
        assert_eq!(
            pkt.data, want,
            "AUD must precede the cached SPS/PPS prepend"
        );
    }

    #[test]
    fn build_packet_no_prepend_for_p_frame() {
        // P-frames (non-IDR slice, NAL type 1) do not require parameter
        // sets — the decoder still has them from the prior IDR. Cache
        // population MUST NOT cause prepending on non-IDR access units.
        let cached_sps = vec![0x67, 0x42, 0x00, 0x0A];
        let cached_pps = vec![0x68, 0xCE, 0x38, 0x80];
        let nidr = vec![0x41, 0x9A];
        let nals = vec![Nal {
            data: nidr.clone(),
            nal_type: 1,
        }];

        let pkt = FfmpegH264Encoder::build_packet(
            &nals,
            Some(&cached_sps),
            Some(&cached_pps),
            66,
            33,
            &PayloadSpec::h264_constrained_baseline(),
        )
        .unwrap();
        assert!(!pkt.is_keyframe);
        assert_eq!(
            pkt.data,
            annexb(&nidr),
            "P-frame must not have parameter sets prepended"
        );
    }

    /// True if `flag` appears in `args` immediately followed by `value`.
    /// Models ffmpeg's `-flag value` arg pairing so a test can assert a
    /// specific option value, not just the flag's presence.
    fn has_flag_value(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    fn has_arg(args: &[String], arg: &str) -> bool {
        args.iter().any(|a| a == arg)
    }

    #[test]
    fn x264_args_use_finite_gop_and_small_slices() {
        let args = x264_ffmpeg_args(1280, 720, 2000);
        // Codec + baseline profile + zerolatency tune preserved.
        assert!(has_flag_value(&args, "-c:v", "libx264"));
        assert!(has_flag_value(&args, "-profile:v", "baseline"));
        assert!(has_flag_value(&args, "-tune", "zerolatency"));
        // B-frames off.
        assert!(has_flag_value(&args, "-bf", "0"));
        // -g is a finite, bounded GOP (periodic IDR for recoverability),
        // not an intra-refresh / infinite period.
        assert!(has_flag_value(&args, "-g", GOP_PERIOD_FRAMES));
        // The x264-params string keeps small slices + closed-GOP pin +
        // in-band headers, but NO intra-refresh (periodic IDRs restored).
        let params_idx = args
            .iter()
            .position(|a| a == "-x264-params")
            .expect("-x264-params present");
        let params = &args[params_idx + 1];
        assert!(
            !params.contains("intra-refresh"),
            "intra-refresh must be gone (periodic IDR restored): {params}"
        );
        assert!(params.contains("slice-max-size=1200"), "params: {params}");
        assert!(params.contains("ref=1"), "params: {params}");
        assert!(params.contains("bframes=0"), "params: {params}");
        assert!(params.contains("repeat-headers=1"), "params: {params}");
        // Input stays rawvideo/yuv420p; no nv12/hwupload on the x264 path.
        assert!(has_flag_value(&args, "-pixel_format", "yuv420p"));
        assert!(
            !args.iter().any(|a| a.contains("hwupload")),
            "x264 path must not hwupload"
        );
    }

    #[test]
    fn nvenc_args_use_finite_gop_and_baseline() {
        let args = nvenc_ffmpeg_args(1920, 1080, 4000);
        // NVENC codec + low-latency preset/tune.
        assert!(has_flag_value(&args, "-c:v", "h264_nvenc"));
        assert!(has_flag_value(&args, "-preset", "p1"));
        assert!(has_flag_value(&args, "-tune", "ll"));
        assert!(has_flag_value(&args, "-zerolatency", "1"));
        // CBR rate control with matched bitrate/maxrate and half bufsize.
        assert!(has_flag_value(&args, "-rc", "cbr"));
        assert!(has_flag_value(&args, "-b:v", "4000k"));
        assert!(has_flag_value(&args, "-maxrate", "4000k"));
        assert!(has_flag_value(&args, "-bufsize", "2000k"));
        // Constrained-Baseline Annex-B, no B-frames.
        assert!(has_flag_value(&args, "-profile:v", "baseline"));
        assert!(has_flag_value(&args, "-bf", "0"));
        assert!(has_flag_value(&args, "-f", "h264"));
        // Finite, bounded GOP (periodic IDR for recoverability) — the
        // intra-refresh + infinite-GOP + no-scenecut/forced-idr config is
        // gone.
        assert!(has_flag_value(&args, "-g", GOP_PERIOD_FRAMES));
        assert!(
            !has_arg(&args, "-intra-refresh"),
            "intra-refresh must be gone (periodic IDR restored): {args:?}"
        );
        assert!(
            !has_arg(&args, "-no-scenecut"),
            "no-scenecut must be gone: {args:?}"
        );
        assert!(
            !has_arg(&args, "-forced-idr"),
            "forced-idr must be gone: {args:?}"
        );
        // NVENC ingests I420 directly: rawvideo/yuv420p input and NO
        // VA-API-only nv12/hwupload filter.
        assert!(has_flag_value(&args, "-f", "rawvideo"));
        assert!(has_flag_value(&args, "-pixel_format", "yuv420p"));
        assert!(
            !args.iter().any(|a| a.contains("hwupload")),
            "NVENC must not hwupload (that's VA-API only): {args:?}"
        );
        assert!(
            !has_arg(&args, "-vaapi_device"),
            "NVENC must not set -vaapi_device"
        );
    }

    #[test]
    fn vaapi_args_keep_hwupload_and_periodic_idr() {
        // Regression guard: the VA-API path is the ONE path that still
        // needs nv12/hwupload + a vaapi device, and it keeps the periodic
        // -g (no intra-refresh flag on the VA-API arm).
        let args = vaapi_ffmpeg_args(1280, 720, 2000);
        assert!(has_flag_value(&args, "-c:v", "h264_vaapi"));
        assert!(has_flag_value(
            &args,
            "-vaapi_device",
            "/dev/dri/renderD128"
        ));
        assert!(has_flag_value(&args, "-vf", "format=nv12,hwupload"));
        assert!(has_flag_value(&args, "-profile:v", "constrained_baseline"));
    }

    /// End-to-end periodic-IDR check against the *real* encoder selected by
    /// `FfmpegH264Encoder::new` (nvenc → vaapi → libx264). Feeds ~150 frames
    /// of synthetic, slowly-changing I420 and asserts the emitted packets
    /// contain the seed IDR at frame 0 PLUS at least one further IDR within
    /// the GOP cadence — i.e. the finite GOP is actually producing the
    /// periodic recovery keyframes a desynced decoder relies on, on
    /// whichever encoder this host uses.
    ///
    /// `#[ignore]` because it spawns ffmpeg and needs a working H.264
    /// encoder; run explicitly on a build box:
    ///   `cargo test --release periodic_idr_real_encoder -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn periodic_idr_real_encoder_emits_recurring_idrs() {
        let (w, h, kbps) = (640u32, 480u32, 2000u32);
        let mut enc = match FfmpegH264Encoder::new(w, h, kbps) {
            Ok(e) => e,
            Err(e) => panic!("no H.264 encoder available on this host: {e}"),
        };
        let uv_w = ((w + 1) / 2) as usize;
        let uv_h = ((h + 1) / 2) as usize;
        let frame_size = (w as usize * h as usize) + 2 * (uv_w * uv_h);

        let mut keyframes = 0usize;
        let mut frames_emitted = 0usize;
        let mut first_keyframe_at: Option<usize> = None;
        let mut last_keyframe_at: Option<usize> = None;
        // Feed more than 2× the GOP so we observe at least a second IDR.
        let total_frames = 150usize;
        for i in 0..total_frames {
            // Slowly varying luma so the encoder has real content (a flat
            // frame can be coded as skip and never exercise the GOP).
            let mut buf = vec![0u8; frame_size];
            let luma = w as usize * h as usize;
            let v = (i * 3) as u8;
            for (j, b) in buf[..luma].iter_mut().enumerate() {
                *b = v.wrapping_add((j % 251) as u8);
            }
            for b in buf[luma..].iter_mut() {
                *b = 128;
            }
            let pkts = enc.encode(&buf, 33, false).expect("encode frame");
            for p in pkts {
                frames_emitted += 1;
                if p.is_keyframe {
                    keyframes += 1;
                    first_keyframe_at.get_or_insert(frames_emitted - 1);
                    last_keyframe_at = Some(frames_emitted - 1);
                }
            }
        }
        // Drop the encoder to flush; one or two trailing frames may remain
        // in ffmpeg's pipeline and are not required for the assertion.
        eprintln!(
            "[periodic_idr_real_encoder] emitted {frames_emitted} frames, \
             {keyframes} keyframe(s), first@{first_keyframe_at:?} last@{last_keyframe_at:?}",
        );
        assert_eq!(
            first_keyframe_at,
            Some(0),
            "the seed keyframe must be the very first emitted frame"
        );
        assert!(
            keyframes >= 2,
            "finite GOP must emit recurring IDRs — got only {keyframes} \
             keyframe(s) over {frames_emitted} frames, which means periodic \
             IDRs are NOT being produced (a desynced decoder would never \
             recover)"
        );
    }
}
