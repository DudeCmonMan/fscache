pub mod broadcast_layer;
pub mod client;
pub mod protocol;
pub mod recent_logs;
pub mod server;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

pub type IpcFramedReader = FramedRead<OwnedReadHalf, LengthDelimitedCodec>;
pub type IpcFramedWriter = FramedWrite<OwnedWriteHalf, LengthDelimitedCodec>;

pub async fn send_msg<T: serde::Serialize>(
    writer: &mut IpcFramedWriter,
    msg: &T,
) -> anyhow::Result<()> {
    let json = serde_json::to_vec(msg)?;
    writer.send(Bytes::from(json)).await?;
    Ok(())
}

/// Returns `None` on clean stream close.
pub async fn recv_msg<T: serde::de::DeserializeOwned>(
    reader: &mut IpcFramedReader,
) -> anyhow::Result<Option<T>> {
    match reader.next().await {
        Some(Ok(frame)) => Ok(Some(serde_json::from_slice(&frame)?)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

pub fn framed_split(stream: tokio::net::UnixStream) -> (IpcFramedReader, IpcFramedWriter) {
    let (r, w) = stream.into_split();
    (
        FramedRead::new(r, LengthDelimitedCodec::new()),
        FramedWrite::new(w, LengthDelimitedCodec::new()),
    )
}
