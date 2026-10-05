//! Length-prefixed postcard framing: `u32 LE length | postcard(message)`.

use serde::{Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};

/// Upper bound for a single frame. Larger payloads go through blob methods.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

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
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    decode(&body)
}

/// Incremental decoder for non-blocking readers: push bytes, pop complete frames.
#[derive(Default)]
pub struct FrameBuf {
    buf: Vec<u8>,
}

impl FrameBuf {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Returns the next complete frame body, if any.
    pub fn next_body(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if len > MAX_FRAME {
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
        let mut len = [0u8; 4];
        r.read_exact(&mut len).await?;
        let len = u32::from_le_bytes(len) as usize;
        if len > MAX_FRAME {
            return Err(FrameError::TooLarge(len));
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await?;
        Ok(body)
    }

    pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
        r: &mut R,
    ) -> Result<T, FrameError> {
        decode(&read_body(r).await?)
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
}
