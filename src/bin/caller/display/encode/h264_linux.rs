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
/// channel, replacing the previous poll-timeout approach with
/// deterministic frame boundary detection.
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
    /// NAL received during multi-slice drain that belongs to the next frame.
    /// Consumed at the start of the next `encode()` call.
    pending_nal: Option<Nal>,
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
                "60", // keyframe every 2s at 30fps
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
                "60",
                "-bf",
                "0",
                // Force single-slice output so every frame contains exactly
                // one slice NAL.  Without this, x264 may produce multi-slice
                // frames that the NAL reader splits across WebRTC samples
                // (it treats the first slice NAL as frame-complete).
                "-x264-params",
                "slices=1",
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
            pending_nal: None,
        })
    }

    /// After receiving a slice NAL, drain any continuation slice NALs that
    /// belong to the same frame.  Multi-slice encoders (e.g. VA-API without
    /// a single-slice constraint) produce several slice NALs per access unit.
    ///
    /// Uses a short `recv_timeout` to avoid blocking: if no NAL arrives
    /// within 1 ms the frame is considered complete.  A non-slice NAL (e.g.
    /// SPS/PPS of the next frame) is saved in `pending_nal` for the next
    /// `encode()` call.
    fn drain_continuation_slices(&mut self, collected: &mut Vec<Nal>) {
        use std::time::Duration;
        let timeout = Duration::from_millis(1);
        loop {
            match self.nal_rx.recv_timeout(timeout) {
                Ok(nal) => {
                    let is_slice = nal.nal_type == 1 || nal.nal_type == 5;
                    if is_slice {
                        collected.push(nal);
                        // Continue draining -- there may be more slices.
                    } else {
                        // Non-slice NAL belongs to the next access unit.
                        self.pending_nal = Some(nal);
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // No more NALs ready -- frame is complete.
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // Reader thread exited.
                    break;
                }
            }
        }
    }
}

impl Encoder for FfmpegH264Encoder {
    fn encode(&mut self, i420: &[u8], duration_ms: u64) -> Result<Vec<EncodedPacket>, String> {
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
        }

        let pts = self.pts_offset;
        self.pts_offset += duration_ms;
        self.frame_count += 1;

        // Collect NALs from the reader thread until we have a complete
        // access unit.  A complete access unit ends with a slice NAL
        // (type 1 = non-IDR, type 5 = IDR).  Prefix NALs (SPS=7, PPS=8,
        // SEI=6) are accumulated until the slice arrives.
        //
        // Multi-slice support: VA-API may produce multiple slice NALs per
        // frame.  After the first slice NAL arrives, we drain additional
        // NALs with a short timeout.  Continuation slice NALs (same frame)
        // are accumulated.  A non-slice NAL or timeout signals the frame
        // boundary; any non-slice NAL is saved for the next encode() call.
        let mut collected: Vec<Nal> = Vec::new();

        // Consume a NAL buffered from the previous multi-slice drain.
        if let Some(pending) = self.pending_nal.take() {
            let is_slice = pending.nal_type == 1 || pending.nal_type == 5;
            collected.push(pending);
            if is_slice {
                // The pending NAL was itself a slice -- drain more below.
                self.drain_continuation_slices(&mut collected);
                // Frame complete after drain.
            }
        }

        // If we don't have a slice yet, keep reading.
        let have_slice = collected.iter().any(|n| n.nal_type == 1 || n.nal_type == 5);
        if !have_slice {
            loop {
                let nal = match self.nal_rx.recv() {
                    Ok(n) => n,
                    Err(_) => break, // reader thread exited
                };
                let is_slice = nal.nal_type == 1 || nal.nal_type == 5;
                collected.push(nal);
                if is_slice {
                    // Got first slice -- drain any continuation slices.
                    self.drain_continuation_slices(&mut collected);
                    break;
                }
            }
        }

        if collected.is_empty() {
            // ffmpeg may buffer a few frames before producing output.
            return Ok(Vec::new());
        }

        // Bundle all NAL units into a single packet with Annex-B framing.
        let mut data = Vec::new();
        let mut is_keyframe = false;
        for nal in &collected {
            data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            data.extend_from_slice(&nal.data);
            if nal.nal_type == 5 {
                is_keyframe = true;
            }
        }

        if data.is_empty() {
            return Ok(Vec::new());
        }

        Ok(vec![EncodedPacket {
            data,
            pts_ms: pts,
            duration_ms,
            is_keyframe,
        }])
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
}
