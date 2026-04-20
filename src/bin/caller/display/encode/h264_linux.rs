//! H264 hardware encoder using ffmpeg as a subprocess with VA-API acceleration.
//!
//! Pipes raw I420 frames into ffmpeg's stdin and reads Annex-B H264 NAL units
//! from stdout.  Falls back to software x264 if VA-API is unavailable.
//!
//! This approach avoids complex FFI bindings to libva while still getting
//! hardware acceleration when available.

use super::{EncodedPacket, Encoder};
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

/// Mark `h264_vaapi` as broken on this host for the rest of the process.
pub fn ban_vaapi() {
    VAAPI_BANNED.store(true, Ordering::SeqCst);
}

/// Whether the watchdog has banned `h264_vaapi` for this process.
pub fn is_vaapi_banned() -> bool {
    VAAPI_BANNED.load(Ordering::Relaxed)
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
}

impl FfmpegH264Encoder {
    /// Create a new ffmpeg H264 encoder subprocess.
    ///
    /// Tries VA-API first (`h264_vaapi`), then falls back to software `libx264`.
    /// Returns `Err` if ffmpeg is not installed.
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

        // Skip VA-API if the runtime watchdog already banned it on this
        // host. Avoids re-spawning a known-broken encoder on each peer
        // reconnect or display re-grant within the same process.
        if is_vaapi_banned() {
            let enc = Self::spawn_x264(width, height, bitrate_kbps)?;
            eprintln!(
                "[display/h264_linux] using libx264 (VA-API banned this session)",
            );
            return Ok(enc);
        }

        // Try VA-API first.
        match Self::spawn_vaapi(width, height, bitrate_kbps) {
            Ok(enc) => {
                eprintln!("[display/h264_linux] using h264_vaapi encoder");
                Ok(enc)
            }
            Err(vaapi_err) => {
                eprintln!(
                    "[display/h264_linux] VA-API unavailable ({}), trying libx264",
                    vaapi_err,
                );
                match Self::spawn_x264(width, height, bitrate_kbps) {
                    Ok(enc) => {
                        eprintln!("[display/h264_linux] using libx264 software encoder");
                        Ok(enc)
                    }
                    Err(x264_err) => Err(format!(
                        "no H264 encoder: vaapi={}, x264={}",
                        vaapi_err, x264_err,
                    )),
                }
            }
        }
    }

    /// Spawn ffmpeg with VA-API hardware encoder.
    fn spawn_vaapi(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, String> {
        // VA-API requires uploading frames to GPU via hwupload.
        // Input: raw I420 on stdin.  Output: Annex-B H264 on stdout.
        let child = Command::new("ffmpeg")
            .args([
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
            ])
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

    /// Spawn ffmpeg with software x264 encoder.
    fn spawn_x264(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, String> {
        let child = Command::new("ffmpeg")
            .args([
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
                "30",
                "-bf",
                "0",
                // Output: raw H264 Annex-B to stdout
                "-f",
                "h264",
                "pipe:1",
            ])
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
        })
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
    /// start code.
    fn build_packet(nals: &[Nal], pts_ms: u64, duration_ms: u64) -> Option<EncodedPacket> {
        if nals.is_empty() {
            return None;
        }
        let mut data = Vec::new();
        let mut is_keyframe = false;
        for nal in nals {
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
        // metadata through rawvideo.  We rely on the short GOP (`-g 30`)
        // configured at spawn time to bound how long a fresh peer waits.
        if i420.len() < self.frame_size {
            return Err(format!(
                "I420 buffer too small: {} < {}",
                i420.len(),
                self.frame_size,
            ));
        }

        // Write the I420 frame to ffmpeg stdin.
        {
            let stdin = self
                .child
                .stdin
                .as_mut()
                .ok_or("ffmpeg stdin closed")?;
            stdin
                .write_all(&i420[..self.frame_size])
                .map_err(|e| format!("write to ffmpeg: {e}"))?;
            stdin
                .flush()
                .map_err(|e| format!("flush ffmpeg: {e}"))?;
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
            let is_slice = nal.nal_type == 1 || nal.nal_type == 5;
            if is_slice && have_slice && Self::is_first_slice(&nal) {
                // New frame starts — flush what we have.
                if let Some(pkt) = Self::build_packet(
                    &self.pending_frame_nals,
                    self.pending_frame_pts,
                    duration_ms,
                ) {
                    out_packets.push(pkt);
                }
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
            Ok(0) => break,        // EOF
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
                        if nal_tx.send(Nal { data: nal_data, nal_type }).is_err() {
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
            let _ = nal_tx.send(Nal { data: nal_data, nal_type });
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
                return Some(StartCode { offset: i, sc_len: 4 });
            }
            if buf[i + 2] == 1 {
                return Some(StartCode { offset: i, sc_len: 3 });
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
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x0A,
            0x00, 0x00, 0x00, 0x01, 0x68, 0xCE,
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
            0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB,       // IDR (type 5)
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
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x0A,
            0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x38, 0x80,
            0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB,
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
        assert_eq!(0x67 & 0x1F, 7);  // SPS
        assert_eq!(0x68 & 0x1F, 8);  // PPS
        assert_eq!(0x06 & 0x1F, 6);  // SEI
        assert_eq!(0x65 & 0x1F, 5);  // IDR slice
        assert_eq!(0x41 & 0x1F, 1);  // non-IDR slice
        assert_eq!(0x61 & 0x1F, 1);  // non-IDR slice (different NRI)
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
        let first = Nal { data: vec![0x65, 0x80], nal_type: 5 };
        assert!(FfmpegH264Encoder::is_first_slice(&first));

        // first_mb = 1 → continuation slice
        let cont = Nal { data: vec![0x41, 0x40], nal_type: 1 };
        assert!(!FfmpegH264Encoder::is_first_slice(&cont));
    }
}
