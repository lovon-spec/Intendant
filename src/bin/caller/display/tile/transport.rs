//! Binary wire format for dirty-region tile streaming (#82).
//!
//! D-3a keeps this module deliberately narrow: encode/decode the v1
//! frame shapes and provide size-capped packers for snapshot/update
//! records. It does not open WebRTC data channels or talk to the
//! capture pipeline. Those integration points land in later D-3 slices.

/// Conservative target ceiling for one WebRTC data-channel message.
///
/// The design doc calls out 16 KiB as the most conservative universal
/// ceiling and 32 KiB as the target used by this implementation. Raw
/// 64x64 BGRA tiles are 16 KiB each, so 32 KiB still naturally packs
/// one raw tile per message once headers are included while allowing
/// multiple RLE tiles per message.
pub const MAX_DATACHANNEL_MESSAGE_SIZE: usize = 32 * 1024;

pub const WIRE_VERSION: u8 = 0x01;

const HEADER_LEN: usize = 4; // u8 version, u8 type, u16 flags
const RECORD_OVERHEAD: usize = 2 + 2 + 1 + 4; // tile_x, tile_y, encoding, payload_len
const SNAPSHOT_BODY_OVERHEAD: usize = 4 + 4 + 2 + 2 + 2 + 2 + 2 + 4;
const UPDATE_BODY_OVERHEAD: usize = 4 + 4 + 2;
const RESIZE_BODY_LEN: usize = 4 + 2 + 2 + 2;
const EPOCH_BODY_LEN: usize = 4;
const CURSOR_BODY_LEN: usize = 4 + 4 + 4 + 4 + 1;
const SUBSCRIBE_BODY_LEN: usize = 4;
const SNAPSHOT_REQUEST_BODY_LEN: usize = 4 + 1;
const GAP_REPORT_BODY_LEN: usize = 4 + 4 + 4;

const TYPE_SNAPSHOT_CHUNK: u8 = 0x01;
const TYPE_TILE_UPDATE: u8 = 0x02;
const TYPE_RESIZE: u8 = 0x03;
const TYPE_EPOCH_ADVANCE: u8 = 0x04;
const TYPE_FALLBACK_TO_VIDEO: u8 = 0x05;
const TYPE_FALLBACK_TO_TILE: u8 = 0x06;
const TYPE_CURSOR_STATE: u8 = 0x07;
const TYPE_SUBSCRIBE: u8 = 0x10;
const TYPE_SNAPSHOT_REQUEST: u8 = 0x11;
const TYPE_GAP_REPORT: u8 = 0x12;
const TYPE_ERROR: u8 = 0xff;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TileEncoding {
    RawBgra = 0,
    RleBgra = 1,
    WebpLossless = 2,
}

impl TileEncoding {
    pub fn from_wire(v: u8) -> Result<Self, TileWireError> {
        match v {
            0 => Ok(Self::RawBgra),
            1 => Ok(Self::RleBgra),
            2 => Ok(Self::WebpLossless),
            other => Err(TileWireError::UnsupportedEncoding(other)),
        }
    }

    pub fn as_wire(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileRecord {
    pub tile_x: u16,
    pub tile_y: u16,
    pub encoding: TileEncoding,
    pub payload: Vec<u8>,
}

impl TileRecord {
    pub fn new(tile_x: u16, tile_y: u16, encoding: TileEncoding, payload: Vec<u8>) -> Self {
        Self {
            tile_x,
            tile_y,
            encoding,
            payload,
        }
    }

    pub fn wire_len(&self) -> usize {
        RECORD_OVERHEAD + self.payload.len()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TileFrame {
    SnapshotChunk {
        epoch: u32,
        snapshot_id: u32,
        chunk_index: u16,
        chunk_count: u16,
        grid_w_tiles: u16,
        grid_h_tiles: u16,
        tile_size_px: u16,
        records: Vec<TileRecord>,
    },
    TileUpdate {
        epoch: u32,
        seq: u32,
        records: Vec<TileRecord>,
    },
    Resize {
        new_epoch: u32,
        grid_w_tiles: u16,
        grid_h_tiles: u16,
        tile_size_px: u16,
    },
    EpochAdvance {
        new_epoch: u32,
    },
    FallbackToVideo {
        new_epoch: u32,
    },
    FallbackToTile {
        new_epoch: u32,
    },
    CursorState {
        epoch: u32,
        seq: u32,
        x_px: i32,
        y_px: i32,
        visible: bool,
    },
    Subscribe {
        client_id: u32,
    },
    SnapshotRequest {
        epoch: u32,
        reason: SnapshotRequestReason,
    },
    GapReport {
        epoch: u32,
        last_seen_seq: u32,
        expected_seq: u32,
    },
    Error {
        code: u16,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SnapshotRequestReason {
    Startup = 0,
    Resize = 1,
    Gap = 2,
    Manual = 3,
}

impl SnapshotRequestReason {
    pub fn from_wire(v: u8) -> Result<Self, TileWireError> {
        match v {
            0 => Ok(Self::Startup),
            1 => Ok(Self::Resize),
            2 => Ok(Self::Gap),
            3 => Ok(Self::Manual),
            other => Err(TileWireError::InvalidReason(other)),
        }
    }

    pub fn as_wire(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TileWireError {
    MessageTooShort,
    UnsupportedVersion(u8),
    UnsupportedType(u8),
    UnsupportedEncoding(u8),
    InvalidReason(u8),
    TrailingBytes(usize),
    CountTooLarge(usize),
    MessageTooLarge(usize),
    Utf8Error,
}

impl std::fmt::Display for TileWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MessageTooShort => write!(f, "tile wire message too short"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported tile wire version {v}"),
            Self::UnsupportedType(t) => write!(f, "unsupported tile wire frame type {t:#x}"),
            Self::UnsupportedEncoding(e) => write!(f, "unsupported tile encoding {e}"),
            Self::InvalidReason(r) => write!(f, "invalid snapshot request reason {r}"),
            Self::TrailingBytes(n) => write!(f, "tile wire message has {n} trailing bytes"),
            Self::CountTooLarge(n) => write!(f, "tile record count too large: {n}"),
            Self::MessageTooLarge(n) => write!(f, "tile message too large: {n} bytes"),
            Self::Utf8Error => write!(f, "tile error frame message is not utf-8"),
        }
    }
}

impl std::error::Error for TileWireError {}

pub fn encode_frame(frame: &TileFrame) -> Result<Vec<u8>, TileWireError> {
    let mut out = Vec::new();
    match frame {
        TileFrame::SnapshotChunk {
            epoch,
            snapshot_id,
            chunk_index,
            chunk_count,
            grid_w_tiles,
            grid_h_tiles,
            tile_size_px,
            records,
        } => {
            write_header(&mut out, TYPE_SNAPSHOT_CHUNK);
            write_u32(&mut out, *epoch);
            write_u32(&mut out, *snapshot_id);
            write_u16(&mut out, *chunk_index);
            write_u16(&mut out, *chunk_count);
            write_u16(&mut out, *grid_w_tiles);
            write_u16(&mut out, *grid_h_tiles);
            write_u16(&mut out, *tile_size_px);
            write_u32(&mut out, u32_count(records.len())?);
            write_records(&mut out, records)?;
        }
        TileFrame::TileUpdate {
            epoch,
            seq,
            records,
        } => {
            write_header(&mut out, TYPE_TILE_UPDATE);
            write_u32(&mut out, *epoch);
            write_u32(&mut out, *seq);
            write_u16(&mut out, u16_count(records.len())?);
            write_records(&mut out, records)?;
        }
        TileFrame::Resize {
            new_epoch,
            grid_w_tiles,
            grid_h_tiles,
            tile_size_px,
        } => {
            write_header(&mut out, TYPE_RESIZE);
            write_u32(&mut out, *new_epoch);
            write_u16(&mut out, *grid_w_tiles);
            write_u16(&mut out, *grid_h_tiles);
            write_u16(&mut out, *tile_size_px);
        }
        TileFrame::EpochAdvance { new_epoch } => {
            write_header(&mut out, TYPE_EPOCH_ADVANCE);
            write_u32(&mut out, *new_epoch);
        }
        TileFrame::FallbackToVideo { new_epoch } => {
            write_header(&mut out, TYPE_FALLBACK_TO_VIDEO);
            write_u32(&mut out, *new_epoch);
        }
        TileFrame::FallbackToTile { new_epoch } => {
            write_header(&mut out, TYPE_FALLBACK_TO_TILE);
            write_u32(&mut out, *new_epoch);
        }
        TileFrame::CursorState {
            epoch,
            seq,
            x_px,
            y_px,
            visible,
        } => {
            write_header(&mut out, TYPE_CURSOR_STATE);
            write_u32(&mut out, *epoch);
            write_u32(&mut out, *seq);
            write_i32(&mut out, *x_px);
            write_i32(&mut out, *y_px);
            out.push(u8::from(*visible));
        }
        TileFrame::Subscribe { client_id } => {
            write_header(&mut out, TYPE_SUBSCRIBE);
            write_u32(&mut out, *client_id);
        }
        TileFrame::SnapshotRequest { epoch, reason } => {
            write_header(&mut out, TYPE_SNAPSHOT_REQUEST);
            write_u32(&mut out, *epoch);
            out.push(reason.as_wire());
        }
        TileFrame::GapReport {
            epoch,
            last_seen_seq,
            expected_seq,
        } => {
            write_header(&mut out, TYPE_GAP_REPORT);
            write_u32(&mut out, *epoch);
            write_u32(&mut out, *last_seen_seq);
            write_u32(&mut out, *expected_seq);
        }
        TileFrame::Error { code, message } => {
            write_header(&mut out, TYPE_ERROR);
            let bytes = message.as_bytes();
            write_u16(&mut out, *code);
            write_u16(&mut out, u16_count(bytes.len())?);
            out.extend_from_slice(bytes);
        }
    }
    Ok(out)
}

pub fn decode_frame(bytes: &[u8]) -> Result<TileFrame, TileWireError> {
    if bytes.len() < HEADER_LEN {
        return Err(TileWireError::MessageTooShort);
    }
    let mut r = Reader::new(bytes);
    let version = r.u8()?;
    if version != WIRE_VERSION {
        return Err(TileWireError::UnsupportedVersion(version));
    }
    let frame_type = r.u8()?;
    let _flags = r.u16()?;
    let frame = match frame_type {
        TYPE_SNAPSHOT_CHUNK => {
            let epoch = r.u32()?;
            let snapshot_id = r.u32()?;
            let chunk_index = r.u16()?;
            let chunk_count = r.u16()?;
            let grid_w_tiles = r.u16()?;
            let grid_h_tiles = r.u16()?;
            let tile_size_px = r.u16()?;
            let record_count = r.u32()? as usize;
            let records = r.records(record_count)?;
            TileFrame::SnapshotChunk {
                epoch,
                snapshot_id,
                chunk_index,
                chunk_count,
                grid_w_tiles,
                grid_h_tiles,
                tile_size_px,
                records,
            }
        }
        TYPE_TILE_UPDATE => {
            let epoch = r.u32()?;
            let seq = r.u32()?;
            let record_count = r.u16()? as usize;
            let records = r.records(record_count)?;
            TileFrame::TileUpdate {
                epoch,
                seq,
                records,
            }
        }
        TYPE_RESIZE => TileFrame::Resize {
            new_epoch: r.u32()?,
            grid_w_tiles: r.u16()?,
            grid_h_tiles: r.u16()?,
            tile_size_px: r.u16()?,
        },
        TYPE_EPOCH_ADVANCE => TileFrame::EpochAdvance {
            new_epoch: r.u32()?,
        },
        TYPE_FALLBACK_TO_VIDEO => TileFrame::FallbackToVideo {
            new_epoch: r.u32()?,
        },
        TYPE_FALLBACK_TO_TILE => TileFrame::FallbackToTile {
            new_epoch: r.u32()?,
        },
        TYPE_CURSOR_STATE => TileFrame::CursorState {
            epoch: r.u32()?,
            seq: r.u32()?,
            x_px: r.i32()?,
            y_px: r.i32()?,
            visible: r.u8()? != 0,
        },
        TYPE_SUBSCRIBE => TileFrame::Subscribe {
            client_id: r.u32()?,
        },
        TYPE_SNAPSHOT_REQUEST => TileFrame::SnapshotRequest {
            epoch: r.u32()?,
            reason: SnapshotRequestReason::from_wire(r.u8()?)?,
        },
        TYPE_GAP_REPORT => TileFrame::GapReport {
            epoch: r.u32()?,
            last_seen_seq: r.u32()?,
            expected_seq: r.u32()?,
        },
        TYPE_ERROR => {
            let code = r.u16()?;
            let msg_len = r.u16()? as usize;
            let message = std::str::from_utf8(r.take(msg_len)?)
                .map_err(|_| TileWireError::Utf8Error)?
                .to_string();
            TileFrame::Error { code, message }
        }
        other => return Err(TileWireError::UnsupportedType(other)),
    };
    if r.remaining() != 0 {
        return Err(TileWireError::TrailingBytes(r.remaining()));
    }
    Ok(frame)
}

pub fn pack_snapshot_chunks(
    epoch: u32,
    snapshot_id: u32,
    grid_w_tiles: u16,
    grid_h_tiles: u16,
    tile_size_px: u16,
    records: Vec<TileRecord>,
) -> Result<Vec<TileFrame>, TileWireError> {
    let chunks = pack_records(
        records,
        HEADER_LEN + SNAPSHOT_BODY_OVERHEAD,
        u32::MAX as usize,
    )?;
    if chunks.is_empty() {
        return Ok(vec![TileFrame::SnapshotChunk {
            epoch,
            snapshot_id,
            chunk_index: 0,
            chunk_count: 1,
            grid_w_tiles,
            grid_h_tiles,
            tile_size_px,
            records: Vec::new(),
        }]);
    }
    let chunk_count = u16_count(chunks.len())?;
    let mut out = Vec::with_capacity(chunks.len());
    for (idx, records) in chunks.into_iter().enumerate() {
        out.push(TileFrame::SnapshotChunk {
            epoch,
            snapshot_id,
            chunk_index: idx as u16,
            chunk_count,
            grid_w_tiles,
            grid_h_tiles,
            tile_size_px,
            records,
        });
    }
    Ok(out)
}

pub fn pack_tile_updates(
    epoch: u32,
    first_seq: u32,
    records: Vec<TileRecord>,
) -> Result<Vec<TileFrame>, TileWireError> {
    let chunks = pack_records(
        records,
        HEADER_LEN + UPDATE_BODY_OVERHEAD,
        u16::MAX as usize,
    )?;
    let mut out = Vec::with_capacity(chunks.len());
    for (idx, records) in chunks.into_iter().enumerate() {
        out.push(TileFrame::TileUpdate {
            epoch,
            seq: first_seq.wrapping_add(idx as u32),
            records,
        });
    }
    Ok(out)
}

fn pack_records(
    records: Vec<TileRecord>,
    fixed_overhead: usize,
    max_count: usize,
) -> Result<Vec<Vec<TileRecord>>, TileWireError> {
    let mut chunks: Vec<Vec<TileRecord>> = Vec::new();
    let mut current = Vec::new();
    let mut current_len = fixed_overhead;

    for record in records {
        let record_len = record.wire_len();
        if fixed_overhead + record_len > MAX_DATACHANNEL_MESSAGE_SIZE {
            return Err(TileWireError::MessageTooLarge(fixed_overhead + record_len));
        }
        if current_len + record_len > MAX_DATACHANNEL_MESSAGE_SIZE || current.len() >= max_count {
            chunks.push(std::mem::take(&mut current));
            current_len = fixed_overhead;
        }
        current_len += record_len;
        current.push(record);
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    Ok(chunks)
}

fn write_header(out: &mut Vec<u8>, frame_type: u8) {
    out.push(WIRE_VERSION);
    out.push(frame_type);
    write_u16(out, 0);
}

fn write_records(out: &mut Vec<u8>, records: &[TileRecord]) -> Result<(), TileWireError> {
    for r in records {
        write_u16(out, r.tile_x);
        write_u16(out, r.tile_y);
        out.push(r.encoding.as_wire());
        write_u32(out, u32_count(r.payload.len())?);
        out.extend_from_slice(&r.payload);
    }
    Ok(())
}

fn write_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn u16_count(n: usize) -> Result<u16, TileWireError> {
    u16::try_from(n).map_err(|_| TileWireError::CountTooLarge(n))
}

fn u32_count(n: usize) -> Result<u32, TileWireError> {
    u32::try_from(n).map_err(|_| TileWireError::CountTooLarge(n))
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], TileWireError> {
        if self.remaining() < n {
            return Err(TileWireError::MessageTooShort);
        }
        let start = self.pos;
        self.pos += n;
        Ok(&self.bytes[start..self.pos])
    }

    fn u8(&mut self) -> Result<u8, TileWireError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, TileWireError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, TileWireError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i32(&mut self) -> Result<i32, TileWireError> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn records(&mut self, count: usize) -> Result<Vec<TileRecord>, TileWireError> {
        let mut records = Vec::with_capacity(count);
        for _ in 0..count {
            let tile_x = self.u16()?;
            let tile_y = self.u16()?;
            let encoding = TileEncoding::from_wire(self.u8()?)?;
            let payload_len = self.u32()? as usize;
            let payload = self.take(payload_len)?.to_vec();
            records.push(TileRecord {
                tile_x,
                tile_y,
                encoding,
                payload,
            });
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tile_x: u16, tile_y: u16, len: usize) -> TileRecord {
        TileRecord::new(
            tile_x,
            tile_y,
            TileEncoding::RawBgra,
            (0..len).map(|i| (i % 251) as u8).collect(),
        )
    }

    #[test]
    fn snapshot_chunk_round_trip() {
        let frame = TileFrame::SnapshotChunk {
            epoch: 9,
            snapshot_id: 17,
            chunk_index: 1,
            chunk_count: 3,
            grid_w_tiles: 14,
            grid_h_tiles: 10,
            tile_size_px: 64,
            records: vec![
                record(2, 3, 7),
                TileRecord::new(4, 5, TileEncoding::RleBgra, vec![1, 2, 3, 4, 9]),
            ],
        };
        let encoded = encode_frame(&frame).unwrap();
        assert_eq!(encoded[0], WIRE_VERSION);
        assert_eq!(decode_frame(&encoded).unwrap(), frame);
    }

    #[test]
    fn update_round_trip() {
        let frame = TileFrame::TileUpdate {
            epoch: 5,
            seq: 42,
            records: vec![
                record(0, 0, 16),
                TileRecord::new(1, 1, TileEncoding::WebpLossless, vec![82, 73, 70, 70]),
            ],
        };
        assert_eq!(decode_frame(&encode_frame(&frame).unwrap()).unwrap(), frame);
    }

    #[test]
    fn control_frames_round_trip() {
        let frames = [
            TileFrame::Resize {
                new_epoch: 2,
                grid_w_tiles: 3,
                grid_h_tiles: 4,
                tile_size_px: 64,
            },
            TileFrame::EpochAdvance { new_epoch: 3 },
            TileFrame::FallbackToVideo { new_epoch: 4 },
            TileFrame::FallbackToTile { new_epoch: 5 },
            TileFrame::CursorState {
                epoch: 6,
                seq: 7,
                x_px: -8,
                y_px: 9,
                visible: true,
            },
            TileFrame::Subscribe { client_id: 10 },
            TileFrame::SnapshotRequest {
                epoch: 11,
                reason: SnapshotRequestReason::Gap,
            },
            TileFrame::GapReport {
                epoch: 12,
                last_seen_seq: 13,
                expected_seq: 14,
            },
            TileFrame::Error {
                code: 15,
                message: "tile failure".to_string(),
            },
        ];
        for frame in frames {
            assert_eq!(decode_frame(&encode_frame(&frame).unwrap()).unwrap(), frame);
        }
    }

    #[test]
    fn rejects_unknown_version_type_and_encoding() {
        let mut encoded = encode_frame(&TileFrame::TileUpdate {
            epoch: 1,
            seq: 1,
            records: vec![record(0, 0, 1)],
        })
        .unwrap();
        encoded[0] = 2;
        assert_eq!(
            decode_frame(&encoded),
            Err(TileWireError::UnsupportedVersion(2))
        );

        let mut encoded = encode_frame(&TileFrame::EpochAdvance { new_epoch: 1 }).unwrap();
        encoded[1] = 0x77;
        assert_eq!(
            decode_frame(&encoded),
            Err(TileWireError::UnsupportedType(0x77))
        );

        let mut encoded = encode_frame(&TileFrame::TileUpdate {
            epoch: 1,
            seq: 1,
            records: vec![record(0, 0, 1)],
        })
        .unwrap();
        let encoding_offset = HEADER_LEN + UPDATE_BODY_OVERHEAD + 2 + 2;
        encoded[encoding_offset] = 99;
        assert_eq!(
            decode_frame(&encoded),
            Err(TileWireError::UnsupportedEncoding(99))
        );
    }

    #[test]
    fn rejects_truncated_payload_and_trailing_bytes() {
        let mut encoded = encode_frame(&TileFrame::TileUpdate {
            epoch: 1,
            seq: 1,
            records: vec![record(0, 0, 8)],
        })
        .unwrap();
        encoded.truncate(encoded.len() - 1);
        assert_eq!(decode_frame(&encoded), Err(TileWireError::MessageTooShort));

        let mut encoded = encode_frame(&TileFrame::EpochAdvance { new_epoch: 1 }).unwrap();
        encoded.push(0);
        assert_eq!(decode_frame(&encoded), Err(TileWireError::TrailingBytes(1)));
    }

    #[test]
    fn snapshot_packer_caps_messages() {
        let raw_tile_len = 64 * 64 * 4;
        let records = vec![
            record(0, 0, raw_tile_len),
            record(1, 0, raw_tile_len),
            record(2, 0, 8),
        ];
        let chunks = pack_snapshot_chunks(1, 2, 3, 4, 64, records).unwrap();
        assert_eq!(
            chunks.len(),
            2,
            "two raw tiles cannot share one 32KiB message; the tiny tail record can share the second"
        );
        for chunk in &chunks {
            let encoded = encode_frame(chunk).unwrap();
            assert!(encoded.len() <= MAX_DATACHANNEL_MESSAGE_SIZE);
        }
        for (idx, chunk) in chunks.iter().enumerate() {
            let TileFrame::SnapshotChunk {
                chunk_index,
                chunk_count,
                ..
            } = chunk
            else {
                panic!("expected snapshot chunk");
            };
            assert_eq!(*chunk_index, idx as u16);
            assert_eq!(*chunk_count, 2);
        }
    }

    #[test]
    fn update_packer_increments_seq_for_split_messages() {
        let raw_tile_len = 64 * 64 * 4;
        let updates = pack_tile_updates(
            7,
            100,
            vec![record(0, 0, raw_tile_len), record(1, 0, raw_tile_len)],
        )
        .unwrap();
        assert_eq!(updates.len(), 2);
        assert!(matches!(updates[0], TileFrame::TileUpdate { seq: 100, .. }));
        assert!(matches!(updates[1], TileFrame::TileUpdate { seq: 101, .. }));
        for update in &updates {
            assert!(encode_frame(update).unwrap().len() <= MAX_DATACHANNEL_MESSAGE_SIZE);
        }
    }

    #[test]
    fn packer_rejects_single_record_that_cannot_fit() {
        let too_large = MAX_DATACHANNEL_MESSAGE_SIZE;
        let err = pack_tile_updates(1, 1, vec![record(0, 0, too_large)]).unwrap_err();
        assert!(matches!(err, TileWireError::MessageTooLarge(_)));
    }

    #[test]
    fn empty_snapshot_still_emits_one_chunk() {
        let chunks = pack_snapshot_chunks(1, 2, 3, 4, 64, Vec::new()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0],
            TileFrame::SnapshotChunk {
                epoch: 1,
                snapshot_id: 2,
                chunk_index: 0,
                chunk_count: 1,
                grid_w_tiles: 3,
                grid_h_tiles: 4,
                tile_size_px: 64,
                records: Vec::new(),
            }
        );
    }

    #[test]
    fn body_lengths_match_declared_shapes() {
        assert_eq!(
            HEADER_LEN + RESIZE_BODY_LEN,
            encode_frame(&TileFrame::Resize {
                new_epoch: 1,
                grid_w_tiles: 2,
                grid_h_tiles: 3,
                tile_size_px: 64,
            })
            .unwrap()
            .len()
        );
        assert_eq!(
            HEADER_LEN + EPOCH_BODY_LEN,
            encode_frame(&TileFrame::EpochAdvance { new_epoch: 1 })
                .unwrap()
                .len()
        );
        assert_eq!(
            HEADER_LEN + CURSOR_BODY_LEN,
            encode_frame(&TileFrame::CursorState {
                epoch: 1,
                seq: 2,
                x_px: 3,
                y_px: 4,
                visible: false,
            })
            .unwrap()
            .len()
        );
        assert_eq!(
            HEADER_LEN + SUBSCRIBE_BODY_LEN,
            encode_frame(&TileFrame::Subscribe { client_id: 1 })
                .unwrap()
                .len()
        );
        assert_eq!(
            HEADER_LEN + SNAPSHOT_REQUEST_BODY_LEN,
            encode_frame(&TileFrame::SnapshotRequest {
                epoch: 1,
                reason: SnapshotRequestReason::Startup,
            })
            .unwrap()
            .len()
        );
        assert_eq!(
            HEADER_LEN + GAP_REPORT_BODY_LEN,
            encode_frame(&TileFrame::GapReport {
                epoch: 1,
                last_seen_seq: 2,
                expected_seq: 3,
            })
            .unwrap()
            .len()
        );
    }
}
