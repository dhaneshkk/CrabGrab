//! Receiver side of the CrabGrab Windows -> WSL frame bridge.
//!
//! The Windows sender connects *to* this receiver: WSL2 forwards Windows' `127.0.0.1` to the
//! distro in both NAT (default `localhostForwarding`) and mirrored networking modes, so no
//! firewall rule or host-IP lookup is needed.
//!
//! ```no_run
//! let listener = std::net::TcpListener::bind(("0.0.0.0", crabgrab_bridge::protocol::DEFAULT_PORT)).unwrap();
//! let (stream, _) = listener.accept().unwrap();
//! let mut frames = crabgrab_bridge::FrameReader::new(stream).unwrap();
//! while let Some(frame) = frames.next_frame().unwrap() {
//!     println!("{} {}x{}", frame.header.frame_id, frame.header.width, frame.header.height);
//! }
//! ```

pub mod protocol;

use std::io::{BufReader, Read};

use protocol::{FrameHeader, ProtocolError};

/// A received frame. `data` is BGRA8, `header.stride` bytes per row.
pub struct Frame<'a> {
    pub header: FrameHeader,
    pub data: &'a [u8],
}

impl<'a> Frame<'a> {
    /// Pixel at (x, y) as [b, g, r, a]
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = y as usize * self.header.stride as usize + x as usize * 4;
        [self.data[i], self.data[i + 1], self.data[i + 2], self.data[i + 3]]
    }

    /// Converts to tightly packed RGB8 (e.g. for PPM/PNG encoders or ML pipelines)
    pub fn to_rgb8(&self) -> Vec<u8> {
        let (w, h, stride) = (self.header.width as usize, self.header.height as usize, self.header.stride as usize);
        let mut out = Vec::with_capacity(w * h * 3);
        for row in self.data.chunks_exact(stride).take(h) {
            for px in row[..w * 4].chunks_exact(4) {
                out.extend_from_slice(&[px[2], px[1], px[0]]);
            }
        }
        out
    }
}

/// Reads the hello, then frames, reusing one payload buffer.
pub struct FrameReader<R: Read> {
    reader: BufReader<R>,
    buffer: Vec<u8>,
}

impl<R: Read> FrameReader<R> {
    pub fn new(inner: R) -> Result<Self, ProtocolError> {
        let mut reader = BufReader::with_capacity(1 << 20, inner);
        protocol::read_hello(&mut reader)?;
        Ok(FrameReader { reader, buffer: Vec::new() })
    }

    /// `Ok(None)` when the sender ends the stream or disconnects cleanly between frames.
    pub fn next_frame(&mut self) -> Result<Option<Frame<'_>>, ProtocolError> {
        match protocol::read_message(&mut self.reader, &mut self.buffer)? {
            Some(header) => Ok(Some(Frame { header, data: &self.buffer })),
            None => Ok(None),
        }
    }
}
