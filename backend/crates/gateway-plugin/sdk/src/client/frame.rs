use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::{Frame, FrameError, Message};

const MAX_METADATA_BYTES: usize = 64 * 1024;

/// 八字节头分别是元数据和载荷的 u32 大端长度，分配内存前校验两者总量。
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    maximum: usize,
) -> Result<Frame, FrameError> {
    let metadata_len = reader.read_u32().await? as usize;
    let payload_len = reader.read_u32().await? as usize;
    check_lengths(metadata_len, payload_len, maximum)?;
    let mut metadata = vec![0; metadata_len];
    reader.read_exact(&mut metadata).await?;
    let message = serde_json::from_slice(&metadata).map_err(|_| FrameError::Metadata)?;
    let mut payload = vec![0; payload_len];
    reader.read_exact(&mut payload).await?;
    Ok(Frame { message, payload })
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
    maximum: usize,
) -> Result<(), FrameError> {
    let metadata = serde_json::to_vec(&frame.message).map_err(|_| FrameError::Metadata)?;
    check_lengths(metadata.len(), frame.payload.len(), maximum)?;
    let metadata_len = u32::try_from(metadata.len()).map_err(|_| FrameError::Length)?;
    let payload_len = u32::try_from(frame.payload.len()).map_err(|_| FrameError::Length)?;
    writer.write_u32(metadata_len).await?;
    writer.write_u32(payload_len).await?;
    writer.write_all(&metadata).await?;
    writer.write_all(&frame.payload).await?;
    writer.flush().await?;
    Ok(())
}

pub(super) fn validate_frame(frame: &Frame, maximum: usize) -> Result<(), FrameError> {
    validate_message(&frame.message, frame.payload.len(), maximum)
}

pub(super) fn validate_message(
    message: &Message,
    payload_len: usize,
    maximum: usize,
) -> Result<(), FrameError> {
    let metadata = serde_json::to_vec(message).map_err(|_| FrameError::Metadata)?;
    check_lengths(metadata.len(), payload_len, maximum)
}

fn check_lengths(metadata: usize, payload: usize, maximum: usize) -> Result<(), FrameError> {
    if metadata == 0
        || metadata > MAX_METADATA_BYTES
        || metadata
            .checked_add(payload)
            .is_none_or(|total| total > maximum)
    {
        return Err(FrameError::Length);
    }
    Ok(())
}
