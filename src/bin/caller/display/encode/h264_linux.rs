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

/// ffmpeg-based H264 encoder implementing the `Encoder` trait.
///
/// Spawns an ffmpeg subprocess that reads raw I420 from stdin and writes
/// Annex-B H264 to stdout.  The subprocess is killed on drop.
pub struct FfmpegH264Encoder {
    child: Child,
    width: u32,
    height: u32,
    frame_count: u64,
    pts_offset: u64,
    frame_size: usize,
    /// Leftover bytes from the previous stdout read that didn't form a
    /// complete NAL unit boundary.
    read_buf: Vec<u8>,
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

        Ok(Self {
            child,
            width,
            height,
            frame_count: 0,
            pts_offset: 0,
            frame_size,
            read_buf: Vec::with_capacity(frame_size),
        })
    }

    /// Read encoded output from ffmpeg stdout using `poll()`.
    ///
    /// After writing exactly one I420 frame, ffmpeg processes it and writes
    /// the resulting H264 access unit(s) before accepting more input (pipe
    /// backpressure).  We read in blocking mode using `poll()` with a short
    /// timeout: keep reading while data arrives, declare the frame complete
    /// once no new data appears within `DRAIN_TIMEOUT_MS`.
    fn drain_output(&mut self) -> Result<Vec<(Vec<u8>, bool)>, String> {
        use std::os::unix::io::AsRawFd;

        let stdout = self
            .child
            .stdout
            .as_mut()
            .ok_or("ffmpeg stdout closed")?;

        let fd = stdout.as_raw_fd();
        let mut tmp = [0u8; 65536];

        /// How long to wait for more data after the last successful read
        /// before declaring the frame complete.
        const DRAIN_TIMEOUT_MS: i32 = 10;

        loop {
            // Poll the fd for readability.
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let poll_ret = unsafe { libc::poll(&mut pfd, 1, DRAIN_TIMEOUT_MS) };

            if poll_ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue; // EINTR — retry
                }
                return Err(format!("poll ffmpeg stdout: {err}"));
            }
            if poll_ret == 0 {
                // Timeout — no more data for this frame.
                break;
            }

            // Data is available — read it (blocking, but we know data is ready).
            match stdout.read(&mut tmp) {
                Ok(0) => break, // EOF
                Ok(n) => self.read_buf.extend_from_slice(&tmp[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("read ffmpeg stdout: {e}")),
            }
        }

        // Split read_buf into complete NAL units at Annex-B start codes.
        Ok(split_annexb_nals(&mut self.read_buf))
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

        // Give ffmpeg a moment to process on the first frame (SPS/PPS generation).
        if self.frame_count == 1 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // Read encoded output.
        let nals = self.drain_output()?;

        if nals.is_empty() {
            // ffmpeg may buffer a few frames before producing output.
            return Ok(Vec::new());
        }

        // Bundle all NAL units into a single packet with Annex-B framing.
        let mut data = Vec::new();
        let mut is_keyframe = false;
        for (nal_data, is_idr) in &nals {
            data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            data.extend_from_slice(nal_data);
            if *is_idr {
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
    }
}

/// Split a buffer of Annex-B H264 data into individual NAL units.
///
/// Looks for `00 00 00 01` or `00 00 01` start codes.  All NAL units are
/// returned, including the last one.  This is correct because ffmpeg
/// processes frames synchronously: we write exactly one I420 frame, then
/// read all encoded output for that frame.  The output is always a
/// complete access unit, so there is no risk of a truncated trailing NAL.
///
/// The buffer is cleared after extraction.
///
/// Returns `(nal_data_without_start_code, is_idr)` for each NAL.
fn split_annexb_nals(buf: &mut Vec<u8>) -> Vec<(Vec<u8>, bool)> {
    let mut nals = Vec::new();

    // Find all start code positions.
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 2 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if i + 3 < buf.len() && buf[i + 2] == 0 && buf[i + 3] == 1 {
                starts.push((i, 4)); // 4-byte start code
                i += 4;
                continue;
            }
            if buf[i + 2] == 1 {
                starts.push((i, 3)); // 3-byte start code
                i += 3;
                continue;
            }
        }
        i += 1;
    }

    if starts.is_empty() {
        return nals;
    }

    // Extract all NAL units, including the last one.
    for idx in 0..starts.len() {
        let (start, sc_len) = starts[idx];
        let nal_start = start + sc_len;
        let nal_end = if idx + 1 < starts.len() {
            starts[idx + 1].0
        } else {
            buf.len()
        };
        let nal_data = buf[nal_start..nal_end].to_vec();

        if !nal_data.is_empty() {
            let nal_type = nal_data[0] & 0x1F;
            let is_idr = nal_type == 5; // IDR slice
            nals.push((nal_data, is_idr));
        }
    }

    // All NALs consumed -- clear the buffer.
    buf.clear();

    nals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_annexb_all_nals_emitted() {
        // Two NAL units separated by 4-byte start codes.
        let mut buf = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x0A, // SPS
            0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x38, 0x80, // PPS
        ];

        let nals = split_annexb_nals(&mut buf);
        assert_eq!(nals.len(), 2); // Both NALs emitted
        assert_eq!(nals[0].0, vec![0x67, 0x42, 0x00, 0x0A]); // SPS
        assert!(!nals[0].1); // SPS is not IDR
        assert_eq!(nals[1].0, vec![0x68, 0xCE, 0x38, 0x80]); // PPS
        assert!(!nals[1].1); // PPS is not IDR

        // Buffer should be cleared.
        assert!(buf.is_empty());
    }

    #[test]
    fn split_annexb_idr_detection() {
        // NAL type 5 = IDR
        let mut buf = vec![
            0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, // IDR
            0x00, 0x00, 0x00, 0x01, 0x41, 0xCC, // non-IDR
        ];

        let nals = split_annexb_nals(&mut buf);
        assert_eq!(nals.len(), 2);
        assert!(nals[0].1); // First NAL is IDR
        assert!(!nals[1].1); // Second NAL is non-IDR
        assert!(buf.is_empty());
    }

    #[test]
    fn split_annexb_empty() {
        let mut buf = vec![0x00, 0x01, 0x02];
        let nals = split_annexb_nals(&mut buf);
        assert!(nals.is_empty());
    }
}
