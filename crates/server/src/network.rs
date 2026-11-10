//! The server's one TCP connection to the tablet (through `adb reverse`): input and
//! control from the tablet, video to it.

use std::net::SocketAddr;
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Network errors.
#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[allow(dead_code)]
    #[error("packet too large: {size} bytes")]
    PacketTooLarge { size: usize },

    #[error("connection closed")]
    ConnectionClosed,

    #[error("channel closed")]
    ChannelClosed,
}

/// A message received from a TCP client.
#[derive(Debug)]
pub struct TcpMessage {
    pub data: Vec<u8>,
    pub client_id: u64,
}

/// TCP server for input and control packets (client -> server).
pub struct TcpServer {
    listener: TcpListener,
    /// Sender for outgoing messages to clients.
    /// Maps client_id -> sender channel.
    client_senders: Arc<tokio::sync::RwLock<std::collections::HashMap<u64, mpsc::Sender<Vec<u8>>>>>,
    next_client_id: std::sync::atomic::AtomicU64,
}

impl TcpServer {
    /// Bind to the specified address.
    pub async fn bind(addr: SocketAddr) -> Result<Self, NetworkError> {
        let listener = TcpListener::bind(addr).await?;

        // Enable TCP_NODELAY on the listener for lower latency
        // (actual client sockets will also need this)

        Ok(Self {
            listener,
            client_senders: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            next_client_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// Get the local address.
    #[allow(dead_code)]
    pub fn local_addr(&self) -> Result<SocketAddr, NetworkError> {
        Ok(self.listener.local_addr()?)
    }

    /// Accept a new client connection and spawn a task to handle it.
    /// Returns the client_id and their remote address, plus a receiver for their messages.
    pub async fn accept(
        &self,
        msg_tx: mpsc::Sender<TcpMessage>,
    ) -> Result<(u64, SocketAddr), NetworkError> {
        let (stream, addr) = self.listener.accept().await?;

        // Enable TCP_NODELAY for lower latency
        stream.set_nodelay(true)?;

        let client_id = self
            .next_client_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // Create a channel for sending messages to this client
        let (client_tx, client_rx) = mpsc::channel::<Vec<u8>>(64);

        // Store the sender
        {
            let mut senders = self.client_senders.write().await;
            senders.insert(client_id, client_tx);
        }

        // Spawn task to handle this client
        let client_senders = self.client_senders.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_tcp_client(stream, client_id, msg_tx, client_rx).await {
                match e {
                    NetworkError::ConnectionClosed => {
                        info!("TCP client {} disconnected", client_id);
                    }
                    _ => {
                        warn!("TCP client {} error: {}", client_id, e);
                    }
                }
            }

            // Remove the sender when the client disconnects
            let mut senders = client_senders.write().await;
            senders.remove(&client_id);
        });

        info!("TCP client {} connected from {}", client_id, addr);
        Ok((client_id, addr))
    }

    /// Send a message to a specific client.
    pub async fn send_to_client(&self, client_id: u64, data: &[u8]) -> Result<(), NetworkError> {
        let senders = self.client_senders.read().await;
        if let Some(sender) = senders.get(&client_id) {
            sender
                .send(data.to_vec())
                .await
                .map_err(|_| NetworkError::ChannelClosed)?;
            Ok(())
        } else {
            Err(NetworkError::ConnectionClosed)
        }
    }

    /// Check if a client is still connected.
    pub async fn is_client_connected(&self, client_id: u64) -> bool {
        let senders = self.client_senders.read().await;
        senders.contains_key(&client_id)
    }

    /// Disconnect a client.
    pub async fn disconnect_client(&self, client_id: u64) {
        let mut senders = self.client_senders.write().await;
        senders.remove(&client_id);
    }
}

/// Handle a single TCP client connection.
async fn handle_tcp_client(
    stream: TcpStream,
    client_id: u64,
    msg_tx: mpsc::Sender<TcpMessage>,
    mut outgoing_rx: mpsc::Receiver<Vec<u8>>,
) -> Result<(), NetworkError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Keep reads in their own future: read_exact is not cancellation-safe.
    // Selecting each header read against outgoing traffic can discard partial
    // headers and leave the TCP stream permanently out of sync.
    let receive = async {
        loop {
            let mut header = [0u8; 3];
            match reader.read_exact(&mut header).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Err(NetworkError::ConnectionClosed);
                }
                Err(e) => return Err(NetworkError::Io(e)),
            }

            let len = u16::from_le_bytes([header[1], header[2]]) as usize;
            let mut packet = vec![0u8; 3 + len];
            packet[..3].copy_from_slice(&header);
            reader.read_exact(&mut packet[3..]).await?;
            msg_tx
                .send(TcpMessage {
                    data: packet,
                    client_id,
                })
                .await
                .map_err(|_| NetworkError::ChannelClosed)?;
        }
    };

    let send = async {
        while let Some(data) = outgoing_rx.recv().await {
            writer.write_all(&data).await?;
            writer.flush().await?;
        }
        Ok(())
    };

    tokio::select! {
        result = receive => result,
        result = send => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::{sleep, timeout};

    #[tokio::test]
    async fn outgoing_traffic_preserves_partial_incoming_packets() {
        timeout(Duration::from_secs(3), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut peer = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let (msg_tx, mut msg_rx) = mpsc::channel(4);
            let (outgoing_tx, outgoing_rx) = mpsc::channel(4);
            let handler = tokio::spawn(handle_tcp_client(stream, 7, msg_tx, outgoing_rx));

            let packet = [3, 4, 0, 1, 2, 3, 4];
            // Interleave a server write with both a partial header and payload.
            peer.write_all(&packet[..1]).await.unwrap();
            sleep(Duration::from_millis(50)).await;
            outgoing_tx.send(vec![9]).await.unwrap();
            assert_eq!(peer.read_u8().await.unwrap(), 9);

            peer.write_all(&packet[1..4]).await.unwrap();
            sleep(Duration::from_millis(50)).await;
            outgoing_tx.send(vec![8]).await.unwrap();
            assert_eq!(peer.read_u8().await.unwrap(), 8);
            peer.write_all(&packet[4..]).await.unwrap();

            let message = msg_rx.recv().await.unwrap();
            assert_eq!(message.client_id, 7);
            assert_eq!(message.data, packet);

            // A subsequent packet must still be aligned.
            peer.write_all(&packet).await.unwrap();
            assert_eq!(msg_rx.recv().await.unwrap().data, packet);
            drop(outgoing_tx);
            handler.await.unwrap().unwrap();
        })
        .await
        .expect("incoming and outgoing traffic must not block each other");
    }
}
