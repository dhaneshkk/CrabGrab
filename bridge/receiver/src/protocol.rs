//! CrabGrab bridge wire protocol, version 1.
//!
//! This file is the single source of truth for the protocol: the Windows/macOS sender
//! (`examples/wsl_bridge_sender.rs`) includes it with `#[path]`, and the Linux receiver crate
//! compiles it as a module. It depends on `std` only.
//!
//! Stream layout (all integers little-endian):
//!
//! ```text
//! hello   := magic "CGRB" (4) | version u16 | flags u16                      (8 bytes, once)
//! message := kind u32 | width u32 | height u32 | stride u32
//!            | frame_id u64 | timestamp_ns u64 | payload_len u64             (40 bytes)
//!            | payload [payload_len bytes]
//! ```
//!
//! `kind` 1 = video frame, BGRA 8-bit per channel, rows `stride` bytes apart.
//! `kind` 2 = end of stream (all other fields zero, no payload).

use std::io::{self, Read, Write};

pub const MAGIC: [u8; 4] = *b"CGRB";
pub const VERSION: u16 = 1;
pub const DEFAULT_PORT: u16 = 7878;
pub const HELLO_LEN: usize = 8;
pub const HEADER_LEN: usize = 40;

/// Largest frame edge accepted, in pixels. Bounds the receiver's allocation so a corrupt or
/// hostile header cannot make it allocate gigabytes.
pub const MAX_DIMENSION: u32 = 16_384;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MessageKind {
    VideoBgra8,
    End,
}

impl MessageKind {
    pub fn to_u32(self) -> u32 {
        match self {
            MessageKind::VideoBgra8 => 1,
            MessageKind::End => 2,
        }
    }

    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(MessageKind::VideoBgra8),
            2 => Some(MessageKind::End),
            _ => None,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub kind: MessageKind,
    pub width: u32,
    pub height: u32,
    /// Bytes between the starts of consecutive rows (>= width * 4)
    pub stride: u32,
    pub frame_id: u64,
    /// Capture time relative to the start of the stream
    pub timestamp_ns: u64,
    pub payload_len: u64,
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    BadMagic([u8; 4]),
    UnsupportedVersion(u16),
    UnknownKind(u32),
    BadGeometry { width: u32, height: u32, stride: u32, payload_len: u64 },
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::Io(e) => write!(f, "i/o error: {}", e),
            ProtocolError::BadMagic(m) => write!(f, "bad magic {:?} (is the peer a CrabGrab bridge sender?)", m),
            ProtocolError::UnsupportedVersion(v) => write!(f, "unsupported protocol version {} (expected {})", v, VERSION),
            ProtocolError::UnknownKind(k) => write!(f, "unknown message kind {}", k),
            ProtocolError::BadGeometry { width, height, stride, payload_len } => write!(
                f,
                "inconsistent frame geometry: {}x{} stride {} payload {}",
                width, height, stride, payload_len
            ),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<io::Error> for ProtocolError {
    fn from(e: io::Error) -> Self {
        ProtocolError::Io(e)
    }
}

pub fn encode_hello() -> [u8; HELLO_LEN] {
    let mut out = [0u8; HELLO_LEN];
    out[0..4].copy_from_slice(&MAGIC);
    out[4..6].copy_from_slice(&VERSION.to_le_bytes());
    // bytes 6..8: flags, reserved (zero)
    out
}

/// Returns the peer's flags on success.
pub fn decode_hello(bytes: &[u8; HELLO_LEN]) -> Result<u16, ProtocolError> {
    let magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if magic != MAGIC {
        return Err(ProtocolError::BadMagic(magic));
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }
    Ok(u16::from_le_bytes([bytes[6], bytes[7]]))
}

impl FrameHeader {
    pub fn video(width: u32, height: u32, frame_id: u64, timestamp_ns: u64) -> Self {
        let stride = width * 4;
        FrameHeader {
            kind: MessageKind::VideoBgra8,
            width,
            height,
            stride,
            frame_id,
            timestamp_ns,
            payload_len: stride as u64 * height as u64,
        }
    }

    pub fn end() -> Self {
        FrameHeader { kind: MessageKind::End, width: 0, height: 0, stride: 0, frame_id: 0, timestamp_ns: 0, payload_len: 0 }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&self.kind.to_u32().to_le_bytes());
        out[4..8].copy_from_slice(&self.width.to_le_bytes());
        out[8..12].copy_from_slice(&self.height.to_le_bytes());
        out[12..16].copy_from_slice(&self.stride.to_le_bytes());
        out[16..24].copy_from_slice(&self.frame_id.to_le_bytes());
        out[24..32].copy_from_slice(&self.timestamp_ns.to_le_bytes());
        out[32..40].copy_from_slice(&self.payload_len.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8; HEADER_LEN]) -> Result<Self, ProtocolError> {
        let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let u64_at = |i: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[i..i + 8]);
            u64::from_le_bytes(b)
        };
        let kind_raw = u32_at(0);
        let kind = MessageKind::from_u32(kind_raw).ok_or(ProtocolError::UnknownKind(kind_raw))?;
        let header = FrameHeader {
            kind,
            width: u32_at(4),
            height: u32_at(8),
            stride: u32_at(12),
            frame_id: u64_at(16),
            timestamp_ns: u64_at(24),
            payload_len: u64_at(32),
        };
        header.validate()?;
        Ok(header)
    }

    /// Checks the geometry is self-consistent and bounded. Every decoded header passes this, so a
    /// receiver can size its buffer from `payload_len` without further checks.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        let ok = match self.kind {
            MessageKind::End => self.payload_len == 0,
            MessageKind::VideoBgra8 => {
                self.width > 0
                    && self.height > 0
                    && self.width <= MAX_DIMENSION
                    && self.height <= MAX_DIMENSION
                    && self.stride as u64 >= self.width as u64 * 4
                    && self.stride as u64 <= MAX_DIMENSION as u64 * 4
                    && self.payload_len == self.stride as u64 * self.height as u64
            }
        };
        if ok {
            Ok(())
        } else {
            Err(ProtocolError::BadGeometry {
                width: self.width,
                height: self.height,
                stride: self.stride,
                payload_len: self.payload_len,
            })
        }
    }
}

pub fn write_hello<W: Write>(w: &mut W) -> io::Result<()> {
    w.write_all(&encode_hello())
}

pub fn read_hello<R: Read>(r: &mut R) -> Result<u16, ProtocolError> {
    let mut bytes = [0u8; HELLO_LEN];
    r.read_exact(&mut bytes)?;
    decode_hello(&bytes)
}

/// Writes one message. `payload.len()` must equal `header.payload_len`.
pub fn write_message<W: Write>(w: &mut W, header: &FrameHeader, payload: &[u8]) -> io::Result<()> {
    if payload.len() as u64 != header.payload_len {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "payload length does not match header"));
    }
    w.write_all(&header.encode())?;
    w.write_all(payload)
}

/// Reads one message into `payload` (resized to fit; reuse it across calls to avoid reallocating).
/// Returns `Ok(None)` on a clean end of stream: an End message, or EOF between messages.
pub fn read_message<R: Read>(r: &mut R, payload: &mut Vec<u8>) -> Result<Option<FrameHeader>, ProtocolError> {
    let mut bytes = [0u8; HEADER_LEN];
    // Distinguish clean EOF (no bytes of the next header) from a truncated header
    let mut filled = 0;
    while filled < HEADER_LEN {
        match r.read(&mut bytes[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(ProtocolError::Io(io::ErrorKind::UnexpectedEof.into())),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let header = FrameHeader::decode(&bytes)?;
    if header.kind == MessageKind::End {
        return Ok(None);
    }
    payload.resize(header.payload_len as usize, 0);
    r.read_exact(payload)?;
    Ok(Some(header))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn hello_roundtrip() {
        assert_eq!(decode_hello(&encode_hello()).unwrap(), 0);
    }

    #[test]
    fn hello_rejects_bad_magic_and_version() {
        let mut h = encode_hello();
        h[0] = b'X';
        assert!(matches!(decode_hello(&h), Err(ProtocolError::BadMagic(_))));
        let mut h = encode_hello();
        h[4] = 9;
        assert!(matches!(decode_hello(&h), Err(ProtocolError::UnsupportedVersion(9))));
    }

    #[test]
    fn header_roundtrip() {
        let h = FrameHeader::video(1920, 1080, 42, 123_456_789);
        assert_eq!(FrameHeader::decode(&h.encode()).unwrap(), h);
        assert_eq!(h.payload_len, 1920 * 1080 * 4);
        let e = FrameHeader::end();
        assert_eq!(FrameHeader::decode(&e.encode()).unwrap(), e);
    }

    #[test]
    fn header_rejects_bad_geometry() {
        let mut h = FrameHeader::video(4, 4, 0, 0);
        h.payload_len += 1;
        assert!(FrameHeader::decode(&h.encode()).is_err());
        let mut h = FrameHeader::video(4, 4, 0, 0);
        h.stride = 8; // < width * 4
        h.payload_len = 32;
        assert!(FrameHeader::decode(&h.encode()).is_err());
        let mut h = FrameHeader::video(4, 4, 0, 0);
        h.width = MAX_DIMENSION + 1;
        assert!(FrameHeader::decode(&h.encode()).is_err());
        let mut raw = FrameHeader::end().encode();
        raw[0] = 77;
        assert!(matches!(FrameHeader::decode(&raw), Err(ProtocolError::UnknownKind(77))));
    }

    #[test]
    fn stream_roundtrip_and_eof_handling() {
        let mut wire = Vec::new();
        write_hello(&mut wire).unwrap();
        let h1 = FrameHeader::video(2, 1, 1, 10);
        write_message(&mut wire, &h1, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        write_message(&mut wire, &FrameHeader::end(), &[]).unwrap();

        let mut r = Cursor::new(wire);
        read_hello(&mut r).unwrap();
        let mut buf = Vec::new();
        assert_eq!(read_message(&mut r, &mut buf).unwrap(), Some(h1));
        assert_eq!(buf, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(read_message(&mut r, &mut buf).unwrap(), None); // End message
        assert_eq!(read_message(&mut r, &mut buf).unwrap(), None); // clean EOF
    }

    #[test]
    fn truncated_header_is_an_error() {
        let bytes = FrameHeader::video(1, 1, 0, 0).encode();
        let mut r = Cursor::new(bytes[..10].to_vec());
        assert!(read_message(&mut r, &mut Vec::new()).is_err());
    }

    #[test]
    fn write_rejects_mismatched_payload() {
        let h = FrameHeader::video(2, 2, 0, 0);
        assert!(write_message(&mut Vec::new(), &h, &[0; 3]).is_err());
    }
}
