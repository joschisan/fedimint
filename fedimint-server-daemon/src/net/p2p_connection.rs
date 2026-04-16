use std::time::Duration;

use async_trait::async_trait;
use fedimint_core::encoding::{Decodable, Encodable};
use iroh::Watcher as _;
use iroh::endpoint::{Connection, RecvStream};

/// Maximum size of a p2p message in bytes. The largest message we expect to
/// receive is a signed session outcome.
const MAX_P2P_MESSAGE_SIZE: usize = 10_000_000;

pub type DynP2PConnection<M> = Box<dyn IP2PConnection<M>>;

pub type DynIP2PFrame<M> = Box<dyn IP2PFrame<M>>;

#[async_trait]
pub trait IP2PFrame<M>: Send + 'static {
    /// Read the entire frame from the connection and deserialize it into a
    /// message. This is *not* required to be cancel-safe.
    async fn read_to_end(&mut self) -> anyhow::Result<M>;

    fn into_dyn(self) -> DynIP2PFrame<M>
    where
        Self: Sized,
    {
        Box::new(self)
    }
}

#[async_trait]
pub trait IP2PConnection<M>: Send + 'static {
    /// Send a message over the connection. This is *not* required to be
    /// cancel-safe.
    async fn send(&mut self, message: M) -> anyhow::Result<()>;

    /// Receive a p2p frame from the connection. This is *required* to be
    /// cancel-safe.
    async fn receive(&mut self) -> anyhow::Result<DynIP2PFrame<M>>;

    /// Get the round-trip time of the connection.
    fn rtt(&self) -> Option<Duration>;

    fn into_dyn(self) -> DynP2PConnection<M>
    where
        Self: Sized,
    {
        Box::new(self)
    }
}

pub struct IrohRecvFrame {
    stream: RecvStream,
}

impl IrohRecvFrame {
    pub fn new(stream: RecvStream) -> Self {
        Self { stream }
    }
}

#[async_trait]
impl<M> IP2PFrame<M> for IrohRecvFrame
where
    M: Decodable + Send + 'static,
{
    async fn read_to_end(&mut self) -> anyhow::Result<M> {
        let bytes = self.stream.read_to_end(MAX_P2P_MESSAGE_SIZE).await?;

        Ok(M::consensus_decode_whole(&bytes)?)
    }
}

pub struct IrohP2PConnection {
    connection: Connection,
}

impl IrohP2PConnection {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }
}

#[async_trait]
impl<M> IP2PConnection<M> for IrohP2PConnection
where
    M: Encodable + Decodable + Send + 'static,
{
    async fn send(&mut self, message: M) -> anyhow::Result<()> {
        let mut sink = self.connection.open_uni().await?;

        sink.write_all(&message.consensus_encode_to_vec()).await?;

        sink.finish()?;

        Ok(())
    }

    async fn receive(&mut self) -> anyhow::Result<DynIP2PFrame<M>> {
        let stream = self.connection.accept_uni().await?;

        let frame = IrohRecvFrame::new(stream);

        Ok(frame.into_dyn())
    }

    fn rtt(&self) -> Option<Duration> {
        let paths = self.connection.paths();
        paths
            .peek()
            .iter()
            .find(|p| p.is_selected())
            .and_then(|p| p.rtt())
    }
}
