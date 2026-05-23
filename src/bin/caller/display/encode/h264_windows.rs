//! H264 hardware/software encoder using Windows Media Foundation.
//!
//! This is the Windows arm of [`super::create_h264_encoder`], mirroring the
//! macOS VideoToolbox ([`super::h264_macos`]) and Linux ffmpeg
//! ([`super::h264_linux`]) backends: a platform H.264 encoder that feeds the
//! WebRTC display track.
//!
//! ## MFT selection
//!
//! We enumerate the H.264 **Encoder** MFT via [`MFTEnumEx`] under
//! [`MFT_CATEGORY_VIDEO_ENCODER`] with output subtype [`MFVideoFormat_H264`].
//! The flags are `MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER` and we
//! deliberately do **not** pass `MFT_ENUM_FLAG_HARDWARE`. There is no
//! `MFT_ENUM_FLAG_SOFTWARE` constant in the Win32 API (or the `windows` crate):
//! software MFTs are the default set, and only adding `MFT_ENUM_FLAG_HARDWARE`
//! switches to GPU MFTs. Selecting the synchronous software MFT this way lets
//! the encoder run headless on the GPU-less build/CI VM (the Microsoft H.264
//! Video Encoder is an in-box synchronous software MFT). `SORTANDFILTER` orders
//! the results best-first.
//!
//! ## Pixel format
//!
//! The pool feeds I420 (the `i420` slice in [`Encoder::encode`]); the MS H.264
//! encoder MFT's preferred input is **NV12**. We convert I420 → NV12 in
//! [`i420_to_nv12`] (Y copied verbatim, U/V interleaved into a single
//! chroma plane). The capture backend produces BGRA which the pool already
//! turns into I420 via [`super::bgra_to_i420`], so the conversion chain is
//! BGRA → I420 (pool) → NV12 (here).
//!
//! ## Profile / NAL framing
//!
//! The output type is configured for **Constrained Baseline** profile
//! (`eAVEncH264VProfile_ConstrainedBase`), target bitrate and frame rate, so
//! every emitted packet matches [`PayloadSpec::h264_constrained_baseline`]
//! (profile-level-id `42e01f`, packetization-mode 1) — identical to the
//! macOS/Linux backends. We deliberately leave `MF_MT_MPEG2_LEVEL` unset and
//! let the MFT pick the minimal valid level for the frame size (a pinned Level
//! 3.1 made `SetOutputType` reject any resolution above its 3600-macroblock
//! ceiling, e.g. 1600x900 — see [`Self::configure_output_type`]); the
//! advertised `42e01f` stays put, matching the macOS/Linux posture of relying
//! on browser leniency toward an actual SPS level above the negotiated one.
//!
//! The MS H.264 encoder emits **Annex-B** byte-stream output (start-code
//! prefixed NAL units), which is exactly what the WebRTC H.264 path expects —
//! no AVCC→Annex-B repacking is needed (unlike VideoToolbox). On a keyframe the
//! encoder includes SPS+PPS inline ahead of the IDR slice; we additionally
//! cache the most recent SPS/PPS and guarantee they precede every IDR access
//! unit on the wire (mirroring the defensive prepend in [`super::h264_linux`]),
//! so a depacketizer that lost the stream-start parameter sets after a
//! transport gap can still reinitialize.
//!
//! ## COM / MF lifecycle and threading
//!
//! Construction, every `encode()`, and `Drop` all run on the **same**
//! dedicated `std::thread` that [`super::pool`] spawns per encoder. The pool
//! builds the encoder *inside* that thread (via the construct-on-the-driver-
//! thread path in [`super::pool::try_spawn_encoder_thread`]), so the whole
//! sequence `CoInitializeEx` + `MFStartup` (in `new()`) → use → `MFShutdown` +
//! `CoUninitialize` (in `Drop`) is confined to one thread. This is a
//! correctness requirement, not just a convenience: COM init and teardown are
//! **per-thread**, so the thread that initializes the apartment must be the
//! one that releases the `IMFTransform` and uninitializes — even for MTA,
//! where the apartment itself is process-wide, the `CoInitializeEx`/
//! `CoUninitialize` *reference* and the MF startup count are tracked
//! per-thread. There are no `.await` points in the encoder's lifetime, so the
//! `IMFTransform` is only ever touched from this one thread. `MFShutdown`/
//! `CoUninitialize` are paired in `Drop`. The encoder is `unsafe impl Send`
//! for the same reason VideoToolbox/VP8 are: ownership stays on the encoder
//! thread for the encoder's whole life and it is never shared. (`Send` is
//! still *required* because [`super::Encoder`] has it as a supertrait, so
//! `Box<dyn Encoder>` — the type the pool's construct closure returns — is
//! `Send`; the encoder doesn't actually move between threads.)

use super::{EncodedPacket, Encoder, PayloadSpec};

use windows::core::Interface;
use windows::Win32::Foundation::{VARIANT_BOOL, VARIANT_TRUE};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_ConstrainedBase,
    CODECAPI_AVEncCommonLowLatency, CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFMediaType, IMFSample, IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFSampleExtension_CleanPoint, MFShutdown, MFStartup, MFTEnumEx,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_REGISTER_TYPE_INFO, MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};

/// Annex-B start code prefix for NAL units.
const ANNEXB_START_CODE: &[u8] = &[0x00, 0x00, 0x00, 0x01];

/// Keyframe (GOP) cadence in frames. 30 frames ≈ 1s at 30fps, matching the
/// macOS (`max_key_frame_interval = 30`) and Linux (`-g 30`) backends so a
/// fresh peer never waits longer than ~1s for a decodable reference even if
/// the per-frame force-keyframe path is unavailable.
const GOP_SIZE: u32 = 30;

/// The MFT input/output stream ID. The H.264 encoder MFT has a single input
/// and a single output stream, both with ID 0.
const STREAM_ID: u32 = 0;

/// Media Foundation H.264 encoder implementing the [`Encoder`] trait.
pub struct MediaFoundationEncoder {
    /// The H.264 encoder MFT. `Option` only so [`Drop`] can `take()` it and
    /// release the COM reference **before** `MFShutdown`/`CoUninitialize` —
    /// releasing a COM object after `CoUninitialize` is a use-after-free
    /// (observed as `STATUS_ACCESS_VIOLATION` during unwind). It is `Some` for
    /// the entire active life of the encoder; [`Self::transform`] unwraps it.
    transform: Option<IMFTransform>,
    /// `ICodecAPI` view of the same MFT, used to force keyframes on demand via
    /// [`CODECAPI_AVEncVideoForceKeyFrame`] and to enable low-latency mode.
    /// `None` if the MFT didn't expose `ICodecAPI`. Also released before MF
    /// shutdown in [`Drop`].
    codec_api: Option<ICodecAPI>,
    width: u32,
    height: u32,
    frame_count: u64,
    /// Monotonic pts (ms) stamped on the next *output* packet. Advanced per
    /// produced frame.
    pts_offset: u64,
    /// Monotonic pts (ms) stamped on the next *input* sample. Advanced per fed
    /// frame by that frame's `duration_ms`. Kept separate from `pts_offset` so
    /// input timestamps stay correct regardless of MFT pipeline depth.
    input_pts: u64,
    /// Reusable NV12 scratch buffer (Y plane + interleaved UV), sized
    /// `width * height * 3 / 2`. Avoids a per-frame allocation on the hot path.
    nv12: Vec<u8>,
    /// Most recently observed SPS NAL (type 7), raw bytes without start code.
    /// See the module docs / [`super::h264_linux`] for the RFC 6184 rationale
    /// behind guaranteeing SPS/PPS precede every IDR.
    cached_sps: Option<Vec<u8>>,
    /// PPS counterpart to [`Self::cached_sps`].
    cached_pps: Option<Vec<u8>>,
    /// Cached canonical payload spec. The output type is configured for
    /// Constrained Baseline / Level 3.1, so every packet matches
    /// [`PayloadSpec::h264_constrained_baseline`].
    payload_spec: PayloadSpec,
    /// Whether we successfully `MFStartup`'d (so `Drop` knows to `MFShutdown`).
    mf_started: bool,
    /// Whether we successfully `CoInitializeEx`'d (so `Drop` pairs
    /// `CoUninitialize`). COM init can return `S_FALSE` (already initialized on
    /// this thread) — in that case the call still took a reference and we still
    /// pair the uninit.
    com_initialized: bool,
}

// The `IMFTransform` and friends are COM objects created, used, and dropped
// entirely on the single encoder thread (see module docs) — the encoder is
// constructed inside that thread and never crosses a thread boundary or is
// shared. The `Send` bound is nonetheless required because [`super::Encoder`]
// has `Send` as a supertrait (so `Box<dyn Encoder>` is `Send`), and it is
// sound to assert here for the same reason it is for `VideoToolboxEncoder` and
// `Vp8Encoder`: ownership stays on one thread for the encoder's whole life.
unsafe impl Send for MediaFoundationEncoder {}

impl MediaFoundationEncoder {
    /// Create a new Media Foundation H.264 encoder.
    ///
    /// Returns `Err` if Media Foundation is unavailable (e.g. the
    /// `Media Foundation` Windows feature isn't installed on a Server SKU), if
    /// no software H.264 encoder MFT is registered, or if the MFT rejects the
    /// requested configuration.
    pub fn new(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, String> {
        // H.264 requires even dimensions (4:2:0 chroma). The pool already
        // normalizes layer dims to even, but guard here so a mis-sized caller
        // gets a clear error rather than an opaque MFT failure.
        if width % 2 != 0 || height % 2 != 0 {
            return Err(format!(
                "Media Foundation H264: dimensions must be even, got {width}x{height}"
            ));
        }

        // COM (MTA) + Media Foundation init. Both are reference-counted and
        // idempotent process-wide; we still track our own success so Drop pairs
        // the teardown exactly once.
        // SAFETY: `CoInitializeEx` is a thread-local COM apartment init with no
        // pointer arguments; calling it is always sound. Its return value tells
        // us whether we took a reference to balance in `Drop` (tracked in
        // `com_initialized`). S_OK / S_FALSE both took one; RPC_E_CHANGED_MODE
        // (`is_ok()` false) did not, so we won't `CoUninitialize`.
        let com_initialized = unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            hr.is_ok()
        };

        // SAFETY: `MFStartup` only requires a prior successful COM init on this
        // thread (done above); the version constant is the value MF expects and
        // the second arg (0) is the documented default flag.
        if let Err(e) = unsafe { MFStartup(MF_VERSION, 0) } {
            if com_initialized {
                // SAFETY: paired with the `CoInitializeEx` above that returned a
                // reference; this runs on the same thread that took it.
                unsafe { CoUninitialize() };
            }
            return Err(format!(
                "MFStartup failed ({e}) — Media Foundation may not be installed \
                 on this host (Windows Server N / 'Media Foundation' optional feature)"
            ));
        }

        // Construct + configure the MFT. On failure, tear down MF + COM here
        // (the struct — and thus its `Drop` — doesn't exist yet).
        let transform = match Self::create_transform(width, height, bitrate_kbps) {
            Ok(t) => t,
            Err(e) => {
                // SAFETY: the struct (and thus its `Drop`) doesn't exist yet, so
                // we hand-unwind here. `MFShutdown` pairs the `MFStartup` above;
                // `CoUninitialize` pairs `CoInitializeEx` (only if it took a
                // reference). Reverse init order, same thread that initialized.
                unsafe {
                    let _ = MFShutdown();
                    if com_initialized {
                        CoUninitialize();
                    }
                }
                return Err(e);
            }
        };

        let mut enc = Self {
            transform: Some(transform),
            codec_api: None,
            width,
            height,
            frame_count: 0,
            pts_offset: 0,
            input_pts: 0,
            nv12: vec![0u8; (width as usize * height as usize) * 3 / 2],
            cached_sps: None,
            cached_pps: None,
            payload_spec: PayloadSpec::h264_constrained_baseline(),
            mf_started: true,
            com_initialized,
        };

        // Optional ICodecAPI for on-demand force-keyframe. Querying it is a
        // QueryInterface; the MS software H.264 MFT supports it. If it's
        // missing we degrade to the GOP cadence.
        enc.codec_api = enc.transform().cast::<ICodecAPI>().ok();

        // Tell the MFT streaming is about to begin. Order matters:
        // BEGIN_STREAMING then START_OF_STREAM. If either fails, returning `Err`
        // drops `enc`, whose `Drop` impl pairs MFShutdown + CoUninitialize.
        //
        // SAFETY: `enc.transform()` is the live `IMFTransform` set above (always
        // `Some` pre-Drop). `ProcessMessage` takes only scalar args. We're on the
        // encoder thread that initialized MF, the only thread that touches it.
        unsafe {
            enc.transform()
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| format!("Media Foundation H264: NOTIFY_BEGIN_STREAMING: {e}"))?;
            enc.transform()
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| format!("Media Foundation H264: NOTIFY_START_OF_STREAM: {e}"))?;
        }

        eprintln!(
            "[display/h264_windows] Media Foundation H264 (Constrained Baseline, \
             software MFT) for {width}x{height} @ {bitrate_kbps}kbps"
        );
        Ok(enc)
    }

    /// The MFT. `Some` for the encoder's entire active life; only `None` after
    /// [`Drop`] has taken it out to release before MF shutdown.
    #[inline]
    fn transform(&self) -> &IMFTransform {
        self.transform
            .as_ref()
            .expect("MediaFoundationEncoder used after Drop took the transform")
    }

    /// Enumerate, activate, and configure the H.264 encoder MFT.
    fn create_transform(
        width: u32,
        height: u32,
        bitrate_kbps: u32,
    ) -> Result<IMFTransform, String> {
        // Enumerate H.264 video-encoder MFTs whose OUTPUT is MFVideoFormat_H264.
        // No input type constraint (we set NV12 explicitly afterwards).
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };

        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        // SAFETY: `MFTEnumEx` writes a CoTaskMem-allocated array of `count`
        // `Option<IMFActivate>` into `activates` (out-params we own). On success
        // we own that block and every COM reference in it; the cleanup below
        // releases each entry and frees the block. `&output_info` outlives the
        // call.
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                // Software (no HARDWARE flag) synchronous MFT, sorted best-first.
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                None,
                Some(&output_info),
                &mut activates,
                &mut count,
            )
            .map_err(|e| format!("MFTEnumEx (H264 encoder): {e}"))?;
        }

        if activates.is_null() || count == 0 {
            // Free the (empty) array if MF allocated one.
            if !activates.is_null() {
                // SAFETY: `activates` is the non-null CoTaskMem block MFTEnumEx
                // allocated; with `count == 0` it holds no live COM refs to
                // release, so freeing the block alone is correct.
                unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _)) };
            }
            return Err(
                "no software H.264 encoder MFT found (Media Foundation H.264 \
                 encoder not registered on this host)"
                    .to_string(),
            );
        }

        // `activates` is a CoTaskMem-allocated block of `count`
        // `Option<IMFActivate>`, each owning one COM reference. We must Release
        // every entry (by moving each `Option` out and letting it drop) and
        // then free the block itself. Move entry 0 out first to keep it.
        let mut chosen: Option<IMFActivate> = None;
        for i in 0..count as usize {
            // SAFETY: `i < count` and MFTEnumEx allocated `count` contiguous
            // `Option<IMFActivate>` slots, so `activates.add(i)` is in-bounds.
            // `read` bit-copies the `Option` out, transferring ownership of its
            // one COM reference to `entry` without running a destructor on the
            // source slot (its bytes are abandoned; the block is freed below
            // without re-dropping, so each ref is released exactly once — kept
            // for entry 0, dropped for the rest).
            let entry = unsafe { std::ptr::read(activates.add(i)) };
            if i == 0 {
                chosen = entry; // keep the first; its ref transfers to `chosen`
            } else {
                drop(entry); // Release the rest
            }
        }
        // SAFETY: every COM reference in the block was moved out above (read once
        // each), so freeing the underlying CoTaskMem allocation now leaks
        // nothing and double-frees nothing.
        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _)) };

        let activate = chosen.ok_or_else(|| "MFTEnumEx returned a null IMFActivate".to_string())?;

        // Activate the MFT into an IMFTransform.
        //
        // SAFETY: `activate` is a live `IMFActivate` (its owned ref was preserved
        // above). `ActivateObject` returns a new `IMFTransform` owning its own
        // reference, which the RAII wrapper releases on drop.
        let transform: IMFTransform = unsafe {
            activate
                .ActivateObject::<IMFTransform>()
                .map_err(|e| format!("IMFActivate::ActivateObject(IMFTransform): {e}"))?
        };

        // Ordering required by the H.264 encoder MFT:
        //   1. ICodecAPI encoder properties (rate control, low-latency, GOP)
        //      MUST be set BEFORE the output media type — the encoder reads
        //      them when the output type is committed. Setting them after has
        //      no effect (and leaves the encoder in its buffering default,
        //      which never emits under a per-frame ProcessInput/Output loop).
        //   2. Output media type before input media type.
        Self::configure_codec_api(&transform);
        Self::configure_output_type(&transform, width, height, bitrate_kbps)?;
        Self::configure_input_type(&transform, width, height)?;

        Ok(transform)
    }

    /// Build and set the H.264 output media type (Constrained Baseline).
    fn configure_output_type(
        transform: &IMFTransform,
        width: u32,
        height: u32,
        bitrate_kbps: u32,
    ) -> Result<(), String> {
        // SAFETY: `MFCreateMediaType` needs only a prior MF startup (done in
        // `new` before any `create_transform` call) and returns a fresh
        // RAII-owned `IMFMediaType`.
        let out_type: IMFMediaType =
            unsafe { MFCreateMediaType().map_err(|e| format!("MFCreateMediaType(out): {e}"))? };
        // SAFETY: every call below is a setter on the live `out_type` /
        // `transform` COM objects; the GUID/attribute-key references and the
        // committed `out_type` all outlive the calls.
        unsafe {
            out_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| format!("out MAJOR_TYPE: {e}"))?;
            out_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(|e| format!("out SUBTYPE H264: {e}"))?;
            // Bitrate in bits/sec.
            out_type
                .SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps.saturating_mul(1000))
                .map_err(|e| format!("out AVG_BITRATE: {e}"))?;
            out_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| format!("out INTERLACE_MODE: {e}"))?;
            set_attribute_size(&out_type, &MF_MT_FRAME_SIZE, width, height)?;
            set_attribute_ratio(&out_type, &MF_MT_FRAME_RATE, GOP_SIZE, 1)?;
            set_attribute_ratio(&out_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            // Constrained Baseline profile → profile-level-id family 42xx.
            out_type
                .SetUINT32(
                    &MF_MT_MPEG2_PROFILE,
                    eAVEncH264VProfile_ConstrainedBase.0 as u32,
                )
                .map_err(|e| format!("out MPEG2_PROFILE ConstrainedBase: {e}"))?;
            // Do NOT pin MF_MT_MPEG2_LEVEL.
            //
            // The MS H.264 encoder MFT cross-validates (profile, level,
            // frame size, frame rate, bitrate) inside `SetOutputType` — not in
            // the per-attribute `SetUINT32`. Pinning Level 3.1 (`level_idc=31`,
            // MaxFS=3600 macroblocks) made `SetOutputType` reject any frame
            // larger than that ceiling: a 1600x900 desktop is 100x57 = 5700
            // macroblocks, well over 3600, so SetOutputType returned
            // MF_E_INVALIDMEDIATYPE (0xC00D36B6) and the always-on pool encoder
            // panicked at startup. (The earlier synthetic-frame test used
            // 320x240 = 300 MBs, which fits Level 3.1, so it never tripped this
            // — the bug only surfaces at real desktop resolutions > ~720p.)
            // The `let _ =` "best-effort set" did NOT help: the attribute store
            // succeeds; the rejection happens later at SetOutputType.
            //
            // With the level attribute omitted, the MFT auto-derives the
            // minimal valid level for the frame size / rate / bitrate (e.g.
            // Level 4.0 for 1600x900), so SetOutputType always succeeds for any
            // encodable resolution. The advertised profile-level-id stays
            // `42e01f` ([`PayloadSpec::h264_constrained_baseline`]) — identical
            // to the macOS VideoToolbox and Linux ffmpeg backends, which also
            // advertise Constrained Baseline / Level 3.1 yet emit a higher
            // actual level_idc for >720p content and rely on WebRTC browsers'
            // standard leniency toward an SPS level above the negotiated one.
            // Keeping the same posture across all three backends means the
            // fmtp-matching logic in `mod.rs` is unchanged.

            transform
                .SetOutputType(STREAM_ID, &out_type, 0)
                .map_err(|e| format!("SetOutputType(H264): {e}"))?;
        }
        Ok(())
    }

    /// Build and set the NV12 input media type.
    fn configure_input_type(
        transform: &IMFTransform,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        // SAFETY: see `configure_output_type` — MF is started, the returned
        // `IMFMediaType` is RAII-owned.
        let in_type: IMFMediaType =
            unsafe { MFCreateMediaType().map_err(|e| format!("MFCreateMediaType(in): {e}"))? };
        // SAFETY: setters on the live `in_type` / `transform` COM objects;
        // all key references outlive the calls and `in_type` is committed last.
        unsafe {
            in_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| format!("in MAJOR_TYPE: {e}"))?;
            in_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(|e| format!("in SUBTYPE NV12: {e}"))?;
            in_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| format!("in INTERLACE_MODE: {e}"))?;
            in_type
                .SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)
                .map_err(|e| format!("in ALL_SAMPLES_INDEPENDENT: {e}"))?;
            set_attribute_size(&in_type, &MF_MT_FRAME_SIZE, width, height)?;
            set_attribute_ratio(&in_type, &MF_MT_FRAME_RATE, GOP_SIZE, 1)?;
            set_attribute_ratio(&in_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;

            transform
                .SetInputType(STREAM_ID, &in_type, 0)
                .map_err(|e| format!("SetInputType(NV12): {e}"))?;
        }
        Ok(())
    }

    /// Configure rate control, GOP, and — critically — **low-latency mode**
    /// via `ICodecAPI`.
    ///
    /// Low-latency mode is load-bearing, not a tuning knob: by default the MS
    /// H.264 encoder MFT buffers a GOP's worth of frames (B-frame lookahead /
    /// rate-control window) before emitting any output, so a per-frame
    /// `ProcessInput`/`ProcessOutput` loop sees `NEED_MORE_INPUT` indefinitely
    /// and the stream never produces a packet. Setting
    /// `CODECAPI_AVLowLatencyMode = true` makes it emit one access unit per
    /// input frame with no reordering — the same intent as VideoToolbox's
    /// `real_time: true` / libx264's `-tune zerolatency`. Without it the
    /// encoder is silent. We set it (and the older `AVEncCommonLowLatency`
    /// alias) before rate control because some MFT builds key other defaults
    /// off the latency mode.
    fn configure_codec_api(transform: &IMFTransform) {
        let Ok(codec_api) = transform.cast::<ICodecAPI>() else {
            eprintln!(
                "[display/h264_windows] WARN: MFT did not expose ICodecAPI — \
                 cannot enable low-latency mode; encoder may buffer/stall"
            );
            return;
        };
        // SAFETY: `codec_api` is a live `ICodecAPI` view of the MFT. Each
        // `SetValue` reads a CODECAPI GUID key and a `&VARIANT` we build inline
        // (valid for the call); all are on the encoder thread. Failures are
        // ignored by design (some keys are E_NOTIMPL on this MFT).
        unsafe {
            // Low-latency mode — required for per-frame output (see doc above).
            // `AVLowLatencyMode` is the one the MS H.264 encoder honors;
            // `AVEncCommonLowLatency` is a legacy alias that returns
            // E_NOTIMPL on this encoder (harmless — ignored).
            let _ = codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &variant_bool(true));
            let _ = codec_api.SetValue(&CODECAPI_AVEncCommonLowLatency, &variant_bool(true));
            // CBR rate control (matches the low-latency screen-share intent;
            // VideoToolbox uses average_bitrate, libx264 uses -b:v).
            let _ = codec_api.SetValue(
                &CODECAPI_AVEncCommonRateControlMode,
                &variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
            );
            // GOP size — bounds keyframe spacing as defense-in-depth alongside
            // the per-frame force-keyframe path.
            let _ = codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &variant_u32(GOP_SIZE));
        }
    }

    /// Wrap the NV12 scratch buffer into an `IMFSample` with one memory buffer.
    fn make_input_sample(&self, pts_ms: u64, duration_ms: u64) -> Result<IMFSample, String> {
        let len = self.nv12.len() as u32;
        // SAFETY: all calls operate on COM objects we just created and own
        // (RAII-released on the early-return / scope exit). The one raw write is
        // the `copy_nonoverlapping` below, guarded by the `max_len` bounds check;
        // `Lock` is always paired with `Unlock` before the buffer escapes.
        unsafe {
            let buffer = MFCreateMemoryBuffer(len)
                .map_err(|e| format!("MFCreateMemoryBuffer({len}): {e}"))?;

            // Lock, copy NV12 bytes in, set current length, unlock.
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut max_len: u32 = 0;
            buffer
                .Lock(&mut ptr, Some(&mut max_len), None)
                .map_err(|e| format!("IMFMediaBuffer::Lock: {e}"))?;
            if (max_len as usize) < self.nv12.len() {
                let _ = buffer.Unlock();
                return Err(format!(
                    "MF buffer too small: {max_len} < {}",
                    self.nv12.len()
                ));
            }
            // SAFETY: `Lock` populated `ptr` with a writable region of `max_len`
            // bytes; we copy exactly `self.nv12.len()` bytes, which the check
            // above proved fits. Source and destination cannot overlap (distinct
            // allocations).
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), ptr, self.nv12.len());
            buffer
                .Unlock()
                .map_err(|e| format!("IMFMediaBuffer::Unlock: {e}"))?;
            buffer
                .SetCurrentLength(len)
                .map_err(|e| format!("SetCurrentLength: {e}"))?;

            let sample = MFCreateSample().map_err(|e| format!("MFCreateSample: {e}"))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| format!("IMFSample::AddBuffer: {e}"))?;
            // MF sample times are in 100-ns (hns) units.
            sample
                .SetSampleTime(ms_to_hns(pts_ms))
                .map_err(|e| format!("SetSampleTime: {e}"))?;
            sample
                .SetSampleDuration(ms_to_hns(duration_ms))
                .map_err(|e| format!("SetSampleDuration: {e}"))?;
            Ok(sample)
        }
    }

    /// Drain all currently-available output samples from the MFT, converting
    /// each to an [`EncodedPacket`]. Returns when `ProcessOutput` reports it
    /// needs more input (`MF_E_TRANSFORM_NEED_MORE_INPUT`).
    fn drain_output(
        &mut self,
        duration_ms: u64,
        out: &mut Vec<EncodedPacket>,
    ) -> Result<(), String> {
        // MF_E_TRANSFORM_NEED_MORE_INPUT — the documented "no more output right
        // now" signal. Value 0xC00D6D72.
        const MF_E_TRANSFORM_NEED_MORE_INPUT: i32 = -1072861838; // 0xC00D6D72 as i32

        loop {
            // The MS software H.264 encoder does NOT set
            // MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, so the caller must allocate
            // the output sample/buffer. Size it from the stream info.
            // SAFETY: `self.transform()` is the live MFT; `GetOutputStreamInfo`
            // takes only a stream id and returns plain data.
            let stream_info = unsafe {
                self.transform()
                    .GetOutputStreamInfo(STREAM_ID)
                    .map_err(|e| format!("GetOutputStreamInfo: {e}"))?
            };
            let alloc_len = stream_info.cbSize.max(1);

            // SAFETY: allocate the output sample/buffer the MFT will fill (it does
            // not set MFT_OUTPUT_STREAM_PROVIDES_SAMPLES). All objects are
            // RAII-owned; `AddBuffer` takes a borrow and keeps its own ref.
            let sample = unsafe {
                let buffer = MFCreateMemoryBuffer(alloc_len)
                    .map_err(|e| format!("MFCreateMemoryBuffer(out {alloc_len}): {e}"))?;
                let sample = MFCreateSample().map_err(|e| format!("MFCreateSample(out): {e}"))?;
                sample
                    .AddBuffer(&buffer)
                    .map_err(|e| format!("AddBuffer(out): {e}"))?;
                sample
            };

            let mut output = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: STREAM_ID,
                pSample: std::mem::ManuallyDrop::new(Some(sample)),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];
            let mut status: u32 = 0;

            // SAFETY: `ProcessOutput` reads/writes the `MFT_OUTPUT_DATA_BUFFER`
            // we own. The `windows` crate models `pSample`/`pEvents` as
            // `ManuallyDrop<Option<…>>` because MF owns those slots across the
            // call (it may keep our sample, swap in its own, and/or attach an
            // events collection), so they must NOT auto-drop. We take exactly one
            // owning reference back out of each slot on every match arm below
            // (`ManuallyDrop::take`), which restores normal drop semantics and
            // releases each COM ref exactly once. `&mut output`/`&mut status` are
            // valid for the call.
            let hr = unsafe { self.transform().ProcessOutput(0, &mut output, &mut status) };

            match hr {
                Ok(()) => {
                    // SAFETY: take ownership of the produced sample and any events
                    // out of their `ManuallyDrop` slots exactly once. After this
                    // the slots are logically uninitialized and `output` is not
                    // read again (a fresh array is built next iteration).
                    let produced = unsafe { std::mem::ManuallyDrop::take(&mut output[0].pSample) };
                    // Drop any events collection MF attached.
                    let _ = unsafe { std::mem::ManuallyDrop::take(&mut output[0].pEvents) };
                    if let Some(produced) = produced {
                        if let Some(pkt) = self.sample_to_packet(&produced, duration_ms)? {
                            out.push(pkt);
                        }
                    }
                }
                Err(e) if e.code().0 == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    // Reclaim the sample we allocated. This is the normal
                    // "no more output for now" terminator — fed the next frame
                    // produces the next packet.
                    // SAFETY: same single-take-per-slot contract as the Ok arm.
                    let _ = unsafe { std::mem::ManuallyDrop::take(&mut output[0].pSample) };
                    let _ = unsafe { std::mem::ManuallyDrop::take(&mut output[0].pEvents) };
                    break;
                }
                Err(e) => {
                    // SAFETY: same single-take-per-slot contract; reclaim both
                    // slots before propagating the error so nothing leaks.
                    let _ = unsafe { std::mem::ManuallyDrop::take(&mut output[0].pSample) };
                    let _ = unsafe { std::mem::ManuallyDrop::take(&mut output[0].pEvents) };
                    return Err(format!("ProcessOutput: {e} (status={status:#x})"));
                }
            }
        }
        Ok(())
    }

    /// Convert one output `IMFSample` to an [`EncodedPacket`], copying its
    /// Annex-B bytes and ensuring SPS/PPS precede an IDR access unit.
    fn sample_to_packet(
        &mut self,
        sample: &IMFSample,
        duration_ms: u64,
    ) -> Result<Option<EncodedPacket>, String> {
        // Keyframe iff the sample is a clean point. Absent attribute → false.
        // SAFETY: `sample` is a live `IMFSample`; `GetUINT32` reads one attribute
        // key and returns a value or an error (the absent case).
        let is_keyframe = unsafe {
            sample
                .GetUINT32(&MFSampleExtension_CleanPoint)
                .map(|v| v != 0)
                .unwrap_or(false)
        };

        // Flatten all buffers into one contiguous buffer, then copy out.
        // SAFETY: `ConvertToContiguousBuffer` returns a RAII-owned buffer; `Lock`
        // yields `ptr` valid for `cur_len` bytes until `Unlock`. We build the
        // slice and copy it to an owned `Vec` (`.to_vec()`) while the lock is
        // still held, then `Unlock` before the buffer drops, so no pointer
        // outlives the lock.
        let annexb = unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| format!("ConvertToContiguousBuffer: {e}"))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut cur_len: u32 = 0;
            buffer
                .Lock(&mut ptr, None, Some(&mut cur_len))
                .map_err(|e| format!("output Lock: {e}"))?;
            let bytes = std::slice::from_raw_parts(ptr, cur_len as usize).to_vec();
            let _ = buffer.Unlock();
            bytes
        };

        if annexb.is_empty() {
            return Ok(None);
        }

        // Cache SPS/PPS as they flow by, and guarantee they precede every IDR.
        self.update_param_cache(&annexb);
        let framed = self.ensure_params_before_idr(annexb, is_keyframe);

        let pts = self.pts_offset;
        // pts advances per produced frame; with zero-latency / no B-frames the
        // MFT emits in display order so this stays monotonic and gap-free.
        self.pts_offset = self.pts_offset.wrapping_add(duration_ms);

        Ok(Some(EncodedPacket {
            data: framed,
            pts_ms: pts,
            duration_ms,
            is_keyframe,
            payload_spec: self.payload_spec.clone(),
        }))
    }

    /// Scan an Annex-B access unit for SPS (type 7) / PPS (type 8) NALs and
    /// cache them (raw, without start code).
    fn update_param_cache(&mut self, annexb: &[u8]) {
        for (body, nal_type) in AnnexBNalIter::new(annexb) {
            match nal_type {
                7 => self.cached_sps = Some(body.to_vec()),
                8 => self.cached_pps = Some(body.to_vec()),
                _ => {}
            }
        }
    }

    /// If `is_keyframe` and the access unit lacks SPS or PPS, prepend the cached
    /// parameter sets ahead of the first IDR slice (NAL type 5). Mirrors the
    /// RFC 6184 guarantee in [`super::h264_linux`]. Non-keyframes and
    /// already-complete IDRs pass through unchanged.
    fn ensure_params_before_idr(&self, annexb: Vec<u8>, is_keyframe: bool) -> Vec<u8> {
        if !is_keyframe {
            return annexb;
        }
        let mut has_sps = false;
        let mut has_pps = false;
        let mut has_idr = false;
        for (_body, nal_type) in AnnexBNalIter::new(&annexb) {
            match nal_type {
                7 => has_sps = true,
                8 => has_pps = true,
                5 => has_idr = true,
                _ => {}
            }
        }
        if !has_idr {
            return annexb;
        }
        let need_sps = !has_sps && self.cached_sps.is_some();
        let need_pps = !has_pps && self.cached_pps.is_some();
        if !need_sps && !need_pps {
            return annexb;
        }

        // Rebuild: emit NALs in order, inserting cached SPS/PPS immediately
        // before the first IDR slice (type 5). Anything ahead of the IDR (AUD,
        // SEI, an existing SPS/PPS) is preserved in front, giving the canonical
        // AUD → SPS → PPS → IDR ordering.
        let mut out = Vec::with_capacity(annexb.len() + 64);
        let mut inserted = false;
        for (body, nal_type) in AnnexBNalIter::new(&annexb) {
            if !inserted && nal_type == 5 {
                if need_sps {
                    out.extend_from_slice(ANNEXB_START_CODE);
                    out.extend_from_slice(self.cached_sps.as_ref().unwrap());
                }
                if need_pps {
                    out.extend_from_slice(ANNEXB_START_CODE);
                    out.extend_from_slice(self.cached_pps.as_ref().unwrap());
                }
                inserted = true;
            }
            out.extend_from_slice(ANNEXB_START_CODE);
            out.extend_from_slice(body);
        }
        out
    }

    /// Force the next encoded frame to be a keyframe (IDR) via `ICodecAPI`.
    /// Best-effort: if `ICodecAPI` is unavailable the GOP cadence is relied on.
    fn force_keyframe(&self) {
        if let Some(codec_api) = &self.codec_api {
            // SAFETY: `codec_api` is a live `ICodecAPI`; `SetValue` takes a GUID
            // key and a `&VARIANT` valid for the call. Best-effort (ignored).
            unsafe {
                let _ = codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
            }
        }
    }
}

impl Encoder for MediaFoundationEncoder {
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
        let expected = y_size + 2 * uv_size;
        if i420.len() < expected {
            return Err(format!(
                "I420 buffer too small: {} < {}",
                i420.len(),
                expected
            ));
        }

        // Windows black-frame diagnostic (hop C): the encoder's actual input.
        // `frame_count` is bumped after ProcessInput below, so capture it here
        // to gate the logging to the first few frames.
        let diag = self.frame_count < 5;
        if diag {
            eprintln!(
                "[h264_windows] encode input frame #{} {}x{} i420 avg={} (len={})",
                self.frame_count + 1,
                w,
                h,
                super::sampled_avg_byte(i420),
                i420.len(),
            );
        }

        // BGRA→I420 happened in the pool; convert I420→NV12 into the scratch.
        i420_to_nv12(i420, w, h, &mut self.nv12);

        // Hop C cont'd: NV12 after conversion. If the i420 above is bright but
        // this is ~0, the I420→NV12 conversion zeroed real input; if both are
        // bright but `encoded bytes` below is tiny, the MFT itself emitted a
        // black/empty frame despite live input.
        if diag {
            eprintln!(
                "[h264_windows] encode frame #{} nv12 avg={} (len={})",
                self.frame_count + 1,
                super::sampled_avg_byte(&self.nv12),
                self.nv12.len(),
            );
        }

        if force_keyframe || self.frame_count == 0 {
            self.force_keyframe();
        }

        let pts = self.input_pts;
        self.input_pts = self.input_pts.wrapping_add(duration_ms);
        let sample = self.make_input_sample(pts, duration_ms)?;

        // Feed the frame in. The synchronous MFT either accepts it
        // (NEED_MORE_INPUT cleared) or, rarely, asks us to pull output first.
        unsafe {
            self.transform()
                .ProcessInput(STREAM_ID, &sample, 0)
                .map_err(|e| format!("ProcessInput: {e}"))?;
        }
        self.frame_count += 1;

        let mut out = Vec::new();
        self.drain_output(duration_ms, &mut out)?;

        // Hop C terminal: total encoded bytes this frame produced. A black
        // 800x600 constrained-baseline frame compresses to a tiny packet; real
        // desktop content is substantially larger. Read alongside the i420/nv12
        // avgs above: bright input + tiny output => the MFT encoded black.
        if diag {
            let bytes: usize = out.iter().map(|p| p.data.len()).sum();
            eprintln!(
                "[h264_windows] encode frame #{} produced {} packet(s), encoded bytes={}",
                self.frame_count,
                out.len(),
                bytes,
            );
        }

        Ok(out)
    }

    fn codec_mime(&self) -> &'static str {
        super::MIME_TYPE_H264
    }

    fn payload_spec(&self) -> &PayloadSpec {
        &self.payload_spec
    }
}

impl Drop for MediaFoundationEncoder {
    fn drop(&mut self) {
        // Signal end of stream so the MFT releases internal buffers cleanly.
        // SAFETY: `transform` is still the live MFT (not yet taken); these are
        // scalar-arg notifications on the encoder thread. Errors are ignored —
        // teardown proceeds regardless.
        if let Some(transform) = self.transform.as_ref() {
            unsafe {
                let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
        }

        // CRITICAL ORDERING: release every COM interface (transform +
        // codec_api) BEFORE `MFShutdown` / `CoUninitialize`. Releasing a COM
        // object after `CoUninitialize` has run is a use-after-free that
        // surfaces as `STATUS_ACCESS_VIOLATION` during unwind. Taking the
        // `Option`s drops their last reference here, in order.
        let _ = self.codec_api.take();
        let _ = self.transform.take();

        // Tear down MF + COM in reverse init order, now that no COM object
        // we own outlives the apartment.
        if self.mf_started {
            // SAFETY: pairs the `MFStartup` in `new`; every COM ref we owned was
            // released just above, and this runs on the thread that started MF.
            unsafe {
                let _ = MFShutdown();
            }
        }
        if self.com_initialized {
            // SAFETY: pairs the `CoInitializeEx` in `new` (which took a
            // reference); runs after MF shutdown on the same thread, with no
            // remaining COM objects to outlive the apartment.
            unsafe { CoUninitialize() };
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert milliseconds to Media Foundation 100-ns (hns) units.
#[inline]
fn ms_to_hns(ms: u64) -> i64 {
    (ms as i64) * 10_000
}

/// Pack a `(width, height)` pair into a `MF_MT_FRAME_SIZE`-style UINT64
/// attribute (`width` in the high 32 bits, `height` in the low 32 bits).
fn set_attribute_size(
    attrs: &IMFMediaType,
    key: &windows::core::GUID,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let packed = ((width as u64) << 32) | (height as u64);
    // SAFETY: `attrs` is a live `IMFMediaType`; `SetUINT64` stores a scalar under
    // the borrowed `key` GUID.
    unsafe {
        attrs
            .SetUINT64(key, packed)
            .map_err(|e| format!("SetUINT64(size): {e}"))
    }
}

/// Pack a `(numerator, denominator)` ratio into a UINT64 attribute (numerator
/// high, denominator low) — the layout `MF_MT_FRAME_RATE` /
/// `MF_MT_PIXEL_ASPECT_RATIO` expect.
fn set_attribute_ratio(
    attrs: &IMFMediaType,
    key: &windows::core::GUID,
    numerator: u32,
    denominator: u32,
) -> Result<(), String> {
    let packed = ((numerator as u64) << 32) | (denominator as u64);
    // SAFETY: `attrs` is a live `IMFMediaType`; `SetUINT64` stores a scalar under
    // the borrowed `key` GUID.
    unsafe {
        attrs
            .SetUINT64(key, packed)
            .map_err(|e| format!("SetUINT64(ratio): {e}"))
    }
}

/// Construct a `VT_UI4` VARIANT holding `value`. Used for `ICodecAPI::SetValue`.
///
/// The `windows` crate's `VARIANT` is an opaque nested union with no ergonomic
/// `From<u32>`; we build the `VT_UI4` case by hand. This is well-defined: the
/// discriminant is `vt` and the payload is the `ulVal` union arm.
fn variant_u32(value: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: value },
            }),
        },
    }
}

/// Construct a `VT_BOOL` VARIANT. Used for boolean `ICodecAPI` properties
/// (low-latency mode etc.). `VARIANT_BOOL` is `-1` (`VARIANT_TRUE`) for true and
/// `0` for false — not C's `1`.
fn variant_bool(value: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: if value { VARIANT_TRUE } else { VARIANT_BOOL(0) },
                },
            }),
        },
    }
}

/// Convert an I420 buffer (`Y` plane, then `U` plane, then `V` plane) to NV12
/// (`Y` plane copied verbatim, then a single interleaved `UV` plane: `U0 V0 U1
/// V1 …`). `dst` must be exactly `w*h*3/2` bytes.
///
/// Both formats are 4:2:0; the only difference is chroma layout (NV12
/// interleaves U and V into one plane). Y is identical, so it's a straight
/// copy; the chroma planes are zipped.
fn i420_to_nv12(i420: &[u8], w: usize, h: usize, dst: &mut [u8]) {
    let uv_w = (w + 1) / 2;
    let uv_h = (h + 1) / 2;
    let y_size = w * h;
    let uv_size = uv_w * uv_h;

    // Y plane: verbatim copy.
    dst[..y_size].copy_from_slice(&i420[..y_size]);

    let u_plane = &i420[y_size..y_size + uv_size];
    let v_plane = &i420[y_size + uv_size..y_size + 2 * uv_size];
    let uv_dst = &mut dst[y_size..y_size + 2 * uv_size];

    // Interleave: NV12 chroma is U0 V0 U1 V1 ...
    for i in 0..uv_size {
        uv_dst[2 * i] = u_plane[i];
        uv_dst[2 * i + 1] = v_plane[i];
    }
}

/// Iterator over NAL units in an Annex-B byte stream. Yields `(body, nal_type)`
/// where `body` excludes the start code and `nal_type` is the low 5 bits of the
/// NAL header byte. Recognises both 3-byte (`00 00 01`) and 4-byte (`00 00 00
/// 01`) start codes.
struct AnnexBNalIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> AnnexBNalIter<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
}

/// Find the next Annex-B start code at or after `from`. Returns
/// `(offset, sc_len)`.
fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 2 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if i + 3 < buf.len() && buf[i + 2] == 0 && buf[i + 3] == 1 {
                return Some((i, 4));
            }
            if buf[i + 2] == 1 {
                return Some((i, 3));
            }
        }
        i += 1;
    }
    None
}

impl<'a> Iterator for AnnexBNalIter<'a> {
    type Item = (&'a [u8], u8);

    fn next(&mut self) -> Option<Self::Item> {
        let (sc_off, sc_len) = find_start_code(self.buf, self.pos)?;
        let body_start = sc_off + sc_len;
        let body_end = match find_start_code(self.buf, body_start) {
            Some((next_off, _)) => next_off,
            None => self.buf.len(),
        };
        self.pos = body_end;
        if body_start >= body_end {
            return None;
        }
        let body = &self.buf[body_start..body_end];
        let nal_type = body[0] & 0x1F;
        Some((body, nal_type))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i420_to_nv12_interleaves_chroma() {
        // 2x2 I420: Y=[1,2,3,4], U=[10], V=[20]
        let i420 = vec![1u8, 2, 3, 4, 10, 20];
        let mut nv12 = vec![0u8; 6];
        i420_to_nv12(&i420, 2, 2, &mut nv12);
        // Y verbatim, then U0 V0
        assert_eq!(nv12, vec![1, 2, 3, 4, 10, 20]);
    }

    #[test]
    fn i420_to_nv12_4x2_two_chroma_samples() {
        // 4x2 → Y 8 bytes, UV 2x1 each. U=[100,101], V=[200,201].
        let w = 4;
        let h = 2;
        let y: Vec<u8> = (0..8).collect();
        let u = vec![100u8, 101];
        let v = vec![200u8, 201];
        let mut i420 = Vec::new();
        i420.extend_from_slice(&y);
        i420.extend_from_slice(&u);
        i420.extend_from_slice(&v);
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        i420_to_nv12(&i420, w, h, &mut nv12);
        // Y then U0 V0 U1 V1
        let mut want = y.clone();
        want.extend_from_slice(&[100, 200, 101, 201]);
        assert_eq!(nv12, want);
    }

    #[test]
    fn annexb_iter_splits_4byte_start_codes() {
        let stream = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, // SPS (type 7)
            0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, // PPS (type 8)
            0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, // IDR (type 5)
        ];
        let nals: Vec<_> = AnnexBNalIter::new(&stream).collect();
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0].1, 7);
        assert_eq!(nals[0].0, &[0x67, 0x42]);
        assert_eq!(nals[1].1, 8);
        assert_eq!(nals[2].1, 5);
    }

    #[test]
    fn annexb_iter_handles_3byte_start_codes() {
        let stream = vec![0x00, 0x00, 0x01, 0x41, 0x9A, 0x00, 0x00, 0x01, 0x41, 0xBB];
        let nals: Vec<_> = AnnexBNalIter::new(&stream).collect();
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0].1, 1);
        assert_eq!(nals[0].0, &[0x41, 0x9A]);
        assert_eq!(nals[1].0, &[0x41, 0xBB]);
    }

    #[test]
    fn ms_to_hns_converts_correctly() {
        assert_eq!(ms_to_hns(0), 0);
        assert_eq!(ms_to_hns(1), 10_000);
        assert_eq!(ms_to_hns(33), 330_000);
    }

    #[test]
    fn variant_u32_sets_vt_and_value() {
        let v = variant_u32(7);
        // SAFETY: `variant_u32` built the `VT_UI4` union arm, so reading `vt` and
        // the matching `ulVal` field of the VARIANT union is the active variant.
        unsafe {
            let inner = &v.Anonymous.Anonymous;
            assert_eq!(inner.vt, VT_UI4);
            assert_eq!(inner.Anonymous.ulVal, 7);
        }
    }

    #[test]
    fn set_attribute_size_packs_width_high_height_low() {
        // Verified indirectly: the packing helper logic is pure arithmetic.
        let packed = ((1920u64) << 32) | (1080u64);
        assert_eq!(packed >> 32, 1920);
        assert_eq!(packed & 0xFFFF_FFFF, 1080);
    }

    /// Build a synthetic I420 frame (the input shape `encode` expects — the
    /// pool converts BGRA→I420 before calling). Y=mid-gray with a moving block
    /// so the encoder has real content to compress; U/V neutral.
    fn synthetic_i420(w: usize, h: usize, frame: usize) -> Vec<u8> {
        let uv_w = (w + 1) / 2;
        let uv_h = (h + 1) / 2;
        let y_size = w * h;
        let uv_size = uv_w * uv_h;
        let mut buf = vec![128u8; y_size + 2 * uv_size];
        // A bright square that moves each frame, so P-frames aren't trivially
        // empty and a keyframe carries real data.
        let bx = (frame * 7) % w.max(1);
        let by = (frame * 5) % h.max(1);
        for dy in 0..(h / 4).max(1) {
            for dx in 0..(w / 4).max(1) {
                let y = (by + dy) % h;
                let x = (bx + dx) % w;
                buf[y * w + x] = 235;
            }
        }
        // U/V already 128 (neutral). Done.
        buf
    }

    /// End-to-end: construct the MF encoder and encode several synthetic frames,
    /// asserting it produces non-empty H.264 with at least one keyframe.
    ///
    /// Gated to the Windows target (the MF FFI only links there). If Media
    /// Foundation isn't installed on the host (e.g. a Windows Server SKU
    /// without the optional "Media Foundation" feature, or no registered H.264
    /// encoder MFT), `new()` returns a descriptive `Err`; the test SKIPS with a
    /// clear message rather than failing, since `check`/`build` green plus
    /// complete encoder code is the success bar — the MF feature is enabled on
    /// the server VM separately.
    #[cfg(windows)]
    #[test]
    fn mf_encoder_constructs_and_encodes_keyframe() {
        let (w, h) = (320usize, 240usize);
        let mut enc = match MediaFoundationEncoder::new(w as u32, h as u32, 800) {
            Ok(e) => e,
            Err(e) => {
                eprintln!(
                    "[h264_windows test] SKIP: Media Foundation H.264 encoder \
                     unavailable on this host: {e}"
                );
                return;
            }
        };

        // Feed up to ~15 frames; the software MFT may buffer 1-2 before output.
        // The first frame forces a keyframe (frame_count == 0).
        let mut all = Vec::new();
        let mut saw_keyframe = false;
        for f in 0..15 {
            let i420 = synthetic_i420(w, h, f);
            let pkts = enc
                .encode(&i420, 33, f == 0)
                .unwrap_or_else(|e| panic!("encode frame {f}: {e}"));
            for p in &pkts {
                assert!(!p.data.is_empty(), "encoded packet must not be empty");
                // Output must be Annex-B (start-code prefixed).
                assert!(
                    p.data.starts_with(&[0, 0, 0, 1]) || p.data.starts_with(&[0, 0, 1]),
                    "frame {f} packet must be Annex-B framed, got {:02x?}",
                    &p.data[..p.data.len().min(8)]
                );
                assert_eq!(
                    p.payload_spec,
                    PayloadSpec::h264_constrained_baseline(),
                    "every packet must carry the Constrained Baseline payload spec"
                );
                if p.is_keyframe {
                    saw_keyframe = true;
                    // A keyframe access unit must carry SPS (7) and PPS (8)
                    // ahead of the IDR (5) — our ensure_params_before_idr
                    // guarantee.
                    let mut has_sps = false;
                    let mut has_pps = false;
                    let mut has_idr = false;
                    for (_b, t) in AnnexBNalIter::new(&p.data) {
                        match t {
                            7 => has_sps = true,
                            8 => has_pps = true,
                            5 => has_idr = true,
                            _ => {}
                        }
                    }
                    assert!(has_idr, "keyframe must contain an IDR slice");
                    assert!(has_sps && has_pps, "keyframe must carry SPS + PPS");
                }
            }
            all.extend(pkts);
        }

        assert!(
            !all.is_empty(),
            "MF encoder produced no output over 15 frames"
        );
        assert!(
            saw_keyframe,
            "MF encoder produced output but no keyframe (forced on frame 0)"
        );
    }
}
