use anyhow::Result;
use canopee_protocol::{NodeCommand, NodeResponse};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub async fn socket_connectable(socket: &Path) -> bool {
    UnixStream::connect(socket).await.is_ok()
}

pub async fn send_command(socket: &Path, command: NodeCommand) -> Result<NodeResponse> {
    let mut stream = UnixStream::connect(socket).await?;
    let bytes = bincode::serialize(&command)?;
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;

    let size = stream.read_u32().await?;
    let mut buffer = vec![0u8; size as usize];
    stream.read_exact(&mut buffer).await?;
    Ok(bincode::deserialize(&buffer)?)
}