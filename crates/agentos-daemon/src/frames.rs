//! Async side of the host⇄guest framing: u32 LE length + JSON
//! (mirrors the sync implementation in the guest agent).

use agentos_core::protocol::{GuestMessage, HostMessage};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_FRAME: usize = 1 << 20;

pub async fn read_guest_frame(
    r: &mut (impl AsyncRead + Unpin),
) -> std::io::Result<Option<GuestMessage>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other(format!("oversized frame: {len}")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(std::io::Error::other)
}

pub async fn write_host_frame(
    w: &mut (impl AsyncWrite + Unpin),
    msg: &HostMessage,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    w.write_all(&(body.len() as u32).to_le_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await
}
