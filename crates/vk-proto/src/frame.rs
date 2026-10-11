//! Length-prefixed postcard framing: `u32 LE length | postcard(message)`.

use serde::{Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};

/// Upper bound for a single frame. Larger payloads go through blob methods.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

/// Upper bound for a frame from a holder (output arrives in 64 KiB chunks; status and
/// markers are small).
pub const MAX_HOLDER_FRAME: usize = 16 * 1024 * 1024;

/// Upper bound for a render-stream frame from a client: a piggybacked JSON-RPC command
/// (`ClientFrame::Command`) may be as long as a control-socket line (16 MiB), plus framing.
pub const MAX_CLIENT_FRAME: usize = 17 * 1024 * 1024;

/// A frame body buffer starts at most this large and grows as its bytes arrive, so a length
/// header alone (from a peer that never sends the body) cannot make the reader allocate the
/// whole frame up front.
const INITIAL_BODY_ALLOC: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("decode: {0}")]
    Decode(#[from] postcard::Error),
    #[error("frame too large: {0} bytes")]
    TooLarge(usize),
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let mut buf = vec![0u8; 4];
    let body = postcard::to_extend(msg, Vec::new())?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    buf[..4].copy_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    Ok(buf)
}

pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    Ok(postcard::from_bytes(body)?)
}

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), FrameError> {
    w.write_all(&encode(msg)?)?;
    Ok(())
}

pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T, FrameError> {
    read_frame_max(r, MAX_FRAME)
}

/// [`read_frame`] that refuses frames longer than `max` bytes.
pub fn read_frame_max<R: Read, T: DeserializeOwned>(
    r: &mut R,
    max: usize,
) -> Result<T, FrameError> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > max.min(MAX_FRAME) {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = Vec::with_capacity(len.min(INITIAL_BODY_ALLOC));
    (&mut *r).take(len as u64).read_to_end(&mut body)?;
    if body.len() < len {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
    }
    decode(&body)
}

/// Incremental decoder for non-blocking readers: push bytes, pop complete frames. The buffer
/// holds only bytes that arrived.
#[derive(Default)]
pub struct FrameBuf {
    buf: Vec<u8>,
    /// Frame size limit; `0` means [`MAX_FRAME`].
    max: usize,
}

impl FrameBuf {
    /// A decoder that refuses frames longer than `max` bytes.
    pub fn with_max(max: usize) -> Self {
        FrameBuf {
            buf: Vec::new(),
            max,
        }
    }

    fn limit(&self) -> usize {
        if self.max == 0 {
            MAX_FRAME
        } else {
            self.max.min(MAX_FRAME)
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Returns the next complete frame body, if any.
    pub fn next_body(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if len > self.limit() {
            return Err(FrameError::TooLarge(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let body = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Ok(Some(body))
    }

    pub fn next_frame<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError> {
        match self.next_body()? {
            Some(b) => Ok(Some(decode(&b)?)),
            None => Ok(None),
        }
    }
}

#[cfg(feature = "tokio")]
pub mod asyncio {
    use super::*;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
        w: &mut W,
        msg: &T,
    ) -> Result<(), FrameError> {
        w.write_all(&encode(msg)?).await?;
        Ok(())
    }

    pub async fn read_body<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>, FrameError> {
        read_body_max(r, MAX_FRAME).await
    }

    /// One frame body of at most `max` bytes. The buffer grows as the bytes arrive.
    pub async fn read_body_max<R: AsyncRead + Unpin>(
        r: &mut R,
        max: usize,
    ) -> Result<Vec<u8>, FrameError> {
        let mut len = [0u8; 4];
        r.read_exact(&mut len).await?;
        let len = u32::from_le_bytes(len) as usize;
        if len > max.min(MAX_FRAME) {
            return Err(FrameError::TooLarge(len));
        }
        let mut body = Vec::with_capacity(len.min(INITIAL_BODY_ALLOC));
        (&mut *r).take(len as u64).read_to_end(&mut body).await?;
        if body.len() < len {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
        Ok(body)
    }

    pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
        r: &mut R,
    ) -> Result<T, FrameError> {
        decode(&read_body(r).await?)
    }

    /// [`read_frame`] that refuses frames longer than `max` bytes.
    pub async fn read_frame_max<R: AsyncRead + Unpin, T: DeserializeOwned>(
        r: &mut R,
        max: usize,
    ) -> Result<T, FrameError> {
        decode(&read_body_max(r, max).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_incremental() {
        let a = encode(&("hello".to_string(), 7u32)).unwrap();
        let b = encode(&("world".to_string(), 9u32)).unwrap();
        let mut fb = FrameBuf::default();
        let all = [a.clone(), b].concat();
        for chunk in all.chunks(3) {
            fb.push(chunk);
        }
        let x: (String, u32) = fb.next_frame().unwrap().unwrap();
        let y: (String, u32) = fb.next_frame().unwrap().unwrap();
        assert_eq!(x, ("hello".into(), 7));
        assert_eq!(y, ("world".into(), 9));
        assert!(fb.next_frame::<(String, u32)>().unwrap().is_none());
        let z: (String, u32) = read_frame(&mut &a[..]).unwrap();
        assert_eq!(z.1, 7);
    }

    #[test]
    fn a_length_header_alone_does_not_allocate_the_frame() {
        // Claims 60 MiB, sends 3 bytes: an EOF error, not a 60 MiB buffer.
        let mut data = ((60u32 << 20).to_le_bytes()).to_vec();
        data.extend_from_slice(b"abc");
        match read_frame::<_, Vec<u8>>(&mut &data[..]) {
            Err(FrameError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("expected EOF, got {other:?}"),
        }
    }

    #[test]
    fn per_kind_limits_refuse_long_frames() {
        let big = encode(&vec![7u8; 2048]).unwrap();
        assert!(matches!(
            read_frame_max::<_, Vec<u8>>(&mut &big[..], 1024),
            Err(FrameError::TooLarge(_))
        ));
        let v: Vec<u8> = read_frame_max(&mut &big[..], 4096).unwrap();
        assert_eq!(v.len(), 2048);
        let mut fb = FrameBuf::with_max(1024);
        fb.push(&big);
        assert!(matches!(fb.next_body(), Err(FrameError::TooLarge(_))));
    }

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn async_reader_checks_the_limit_and_eof() {
        let big = encode(&vec![1u8; 2048]).unwrap();
        assert!(matches!(
            asyncio::read_frame_max::<_, Vec<u8>>(&mut &big[..], 1024).await,
            Err(FrameError::TooLarge(_))
        ));
        let mut short = ((1u32 << 20).to_le_bytes()).to_vec();
        short.extend_from_slice(b"xy");
        assert!(matches!(
            asyncio::read_body(&mut &short[..]).await,
            Err(FrameError::Io(_))
        ));
    }
}
