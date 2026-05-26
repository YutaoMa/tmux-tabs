//! Length-prefixed JSON frame encoding for the server socket protocol.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_FRAME_BYTES: usize = 1_048_576;

#[derive(thiserror::Error, Debug)]
pub enum FrameError {
    #[error("frame exceeds maximum size of 1 MiB")]
    TooLarge,
    #[error("frame deserialization failed")]
    Deserialize(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Serialize a message and prepend its `u32` length, producing the bytes that
/// should be written to a socket as a single frame.
///
/// # Errors
/// Returns [`FrameError::TooLarge`] if the encoded frame exceeds 1 MiB or
/// [`FrameError::Deserialize`] if serialization fails.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let data = serde_json::to_vec(msg)?;
    let len = match u32::try_from(data.len()) {
        Ok(n) if data.len() <= MAX_FRAME_BYTES => n,
        _ => return Err(FrameError::TooLarge),
    };
    let mut buf = Vec::with_capacity(4 + data.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&data);
    Ok(buf)
}

/// Write a length-prefixed JSON frame.
///
/// # Errors
/// Returns [`FrameError::TooLarge`] if the encoded frame exceeds 1 MiB,
/// [`FrameError::Deserialize`] if serialization fails, or [`FrameError::Io`]
/// if the underlying writer fails.
pub async fn write_frame<W: AsyncWriteExt + Unpin, T: Serialize>(
    writer: &mut W,
    msg: &T,
) -> Result<(), FrameError> {
    let buf = encode_frame(msg)?;
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a length-prefixed JSON frame. Returns `None` on clean peer close.
///
/// # Errors
/// Returns [`FrameError::TooLarge`] if the announced frame exceeds 1 MiB,
/// [`FrameError::Deserialize`] if the payload is not valid JSON, or
/// [`FrameError::Io`] for other reader failures.
pub async fn read_frame<R: AsyncReadExt + Unpin, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> Result<Option<T>, FrameError> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if is_peer_closed(e.kind()) => return Ok(None),
        Err(e) => return Err(FrameError::Io(e)),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let msg = serde_json::from_slice(&buf)?;
    Ok(Some(msg))
}

fn is_peer_closed(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
    )
}
