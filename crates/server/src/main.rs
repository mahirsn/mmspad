//! mmspad server: one tablet on USB, reached through `adb reverse`.
//!
//! One TCP connection on 127.0.0.1 carries input and control from the tablet and, once
//! the tablet asks for a picture, the screen's video to it.

mod config;
mod network;

use anyhow::Result;
use config::ServerConfig;
use mmspad_capture::{select_monitor, ScreenCapture};
use mmspad_encoder::{EncoderConfig, VideoEncoder};
use mmspad_input::{TabletDescriptor, VirtualTablet};
use mmspad_protocol::{
    decode_packet, encode_control, encode_video, ControlPacket, DecodedPacket, VideoPacket,
    MAX_FRAGMENT_DATA, PROTOCOL_VERSION,
};
use network::{TcpMessage, TcpServer};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{info, warn};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    info!("mmspad v{}", env!("CARGO_PKG_VERSION"));
    let config = ServerConfig::from_args();
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run(config))
}

/// Screen capture, its encoder thread and the channels between them.
struct VideoPipe {
    width: u32,
    height: u32,
    capture: ScreenCapture,
    frame_tx: mpsc::SyncSender<mmspad_capture::Frame>,
    encoded_rx: mpsc::Receiver<mmspad_encoder::EncodedFrame>,
    keyframe: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl VideoPipe {
    /// Picks the screen through the portal (asked once, then remembered) and starts
    /// capturing and encoding it.
    async fn start(config: &ServerConfig) -> Result<Self> {
        let token_file = std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state"))
            .join("mmspad/restore-token");
        let token = std::fs::read_to_string(&token_file).ok();
        let monitor = select_monitor(token.as_deref().map(str::trim)).await?;
        if let Some(t) = &monitor.restore_token {
            let _ = std::fs::create_dir_all(token_file.parent().unwrap());
            let _ = std::fs::write(&token_file, t);
        }
        info!("Capturing monitor {}x{} (node {})", monitor.width, monitor.height, monitor.node_id);
        let capture = ScreenCapture::start(&monitor)?;
        // The portal reports the screen in logical pixels; the frames come at the
        // real resolution (2560x1600 under 125% scaling). Encode what arrives, or
        // the picture is a cropped, zoomed-in corner of the screen.
        let mut first = None;
        for _ in 0..200 {
            if let Some(f) = capture.try_recv_frame() {
                first = Some(f);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (width, height) = first.as_ref().map(|f| (f.width, f.height)).unwrap_or((monitor.width, monitor.height));
        let fps = config.fps;
        let mut encoder = VideoEncoder::new(EncoderConfig {
            width,
            height,
            fps,
            bitrate: config.bitrate,
            keyframe_interval: config.keyframe_interval,
        })?;
        let (frame_tx, frame_rx) = mpsc::sync_channel::<mmspad_capture::Frame>(1);
        let (encoded_tx, encoded_rx) = mpsc::sync_channel(2);
        let keyframe = Arc::new(AtomicBool::new(false));
        let wants_keyframe = keyframe.clone();
        let handle = std::thread::spawn(move || {
            let _monitor = monitor; // keeps the portal session alive
            let mut pts = 0u64;
            while let Ok(frame) = frame_rx.recv() {
                if wants_keyframe.swap(false, Ordering::SeqCst) {
                    encoder.request_keyframe();
                }
                match encoder.encode(&frame) {
                    Ok(frames) => {
                        for mut encoded in frames {
                            encoded.pts = pts;
                            pts += 1_000_000 / fps as u64;
                            if encoded_tx.send(encoded).is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => warn!("Encoding error: {}", e),
                }
            }
        });
        Ok(Self { width, height, capture, frame_tx, encoded_rx, keyframe, handle })
    }

    fn stop(self) {
        // Closing the frame channel stops the encoder thread.
        drop(self.frame_tx);
        if let Err(e) = self.handle.join() {
            warn!("Encoder thread panicked: {:?}", e);
        }
    }
}

async fn run(config: ServerConfig) -> Result<()> {
    info!("{:?}", config);
    // The picture's size, sent to the tablet; unknown until it asks for a picture. Input
    // is normalized, so it does not depend on this.
    let (mut width, mut height) = (1920, 1080);
    let mut tablet = VirtualTablet::new(&TabletDescriptor::default())?;

    let addr: SocketAddr = format!("127.0.0.1:{}", config.port).parse()?;
    info!("Listening on {}", addr);
    let tcp_server = TcpServer::bind(addr).await?;
    let (tcp_msg_tx, mut tcp_msg_rx) = tokio_mpsc::channel::<TcpMessage>(256);

    let mut client: Option<u64> = None;
    let mut video: Option<VideoPipe> = None;
    let mut sequence: u32 = 0;
    let mut last_heartbeat = Instant::now();
    let mut last_frame = Instant::now();
    let frame_interval = Duration::from_micros(1_000_000 / config.fps as u64);

    loop {
        tokio::select! {
            result = tcp_server.accept(tcp_msg_tx.clone()) => match result {
                Ok((id, addr)) => {
                    if let Some(old) = client.replace(id) {
                        info!("Replacing client {}", old);
                        tcp_server.disconnect_client(old).await;
                    }
                    // The client reads ConnectAck as its first packet; a heartbeat
                    // sent before the handshake would be taken for the reply.
                    last_heartbeat = Instant::now();
                    info!("Client {} connected from {}", id, addr);
                }
                Err(e) => warn!("Failed to accept a connection: {}", e),
            },

            Some(msg) = tcp_msg_rx.recv() => {
                let Some(id) = client.filter(|&c| c == msg.client_id) else { continue };
                match decode_packet(&msg.data) {
                    Ok(DecodedPacket::Control(ControlPacket::RequestKeyframe)) => {
                        match &video {
                            Some(v) => v.keyframe.store(true, Ordering::SeqCst),
                            None => match VideoPipe::start(&config).await {
                                Ok(v) => {
                                    (width, height) = (v.width, v.height);
                                    v.keyframe.store(true, Ordering::SeqCst);
                                    video = Some(v);
                                    // A second ConnectAck tells the client the picture's size.
                                    let ack = encode_control(&ControlPacket::ConnectAck { width, height })?;
                                    let _ = tcp_server.send_to_client(id, &ack).await;
                                }
                                Err(e) => warn!("Could not start video: {}", e),
                            },
                        }
                    }
                    Ok(DecodedPacket::Control(ControlPacket::Connect { version })) => {
                        if version != PROTOCOL_VERSION {
                            let nack = encode_control(&ControlPacket::ConnectNack {
                                reason: format!("protocol version {version}, this server speaks {PROTOCOL_VERSION}"),
                            })?;
                            let _ = tcp_server.send_to_client(id, &nack).await;
                            tcp_server.disconnect_client(id).await;
                            client = None;
                            continue;
                        }
                        let ack = encode_control(&ControlPacket::ConnectAck { width, height })?;
                        let _ = tcp_server.send_to_client(id, &ack).await;
                        info!("Client {} handshake complete", id);
                    }
                    Ok(DecodedPacket::Control(ControlPacket::Disconnect)) => {
                        info!("Client {} disconnected", id);
                        tcp_server.disconnect_client(id).await;
                        client = None;
                        video.take().map(VideoPipe::stop); // video only while a tablet shows it
                    }
                    Ok(DecodedPacket::Control(_)) => {}
                    Ok(DecodedPacket::Input(input)) => {
                        if let Err(e) = tablet.process_input(input.timestamp, &input.event) {
                            warn!("Failed to process input: {}", e);
                        }
                    }
                    Ok(DecodedPacket::Video(_)) => {}
                    Err(e) => warn!("Bad packet from the tablet: {}", e),
                }
            }

            _ = tokio::time::sleep(Duration::from_micros(100)) => {
                // The last touch frame of a burst has no later event to report it.
                if let Err(e) = tablet.flush() {
                    warn!("Failed to process input: {}", e);
                }
                if let Some(id) = client {
                    if !tcp_server.is_client_connected(id).await {
                        info!("Client {} disconnected", id);
                        client = None;
                        video.take().map(VideoPipe::stop);
                    }
                }
            }
        }

        let Some(id) = client else { continue };

        if let Some(v) = &video {
            while let Some(frame) = v.capture.try_recv_frame() {
                // The compositor sends a frame whenever the screen changes; only --fps
                // of them are encoded, which is most of the CPU this costs.
                if last_frame.elapsed() < frame_interval {
                    continue;
                }
                last_frame = Instant::now();
                // A full channel means the encoder is still busy: drop the frame.
                let _ = v.frame_tx.try_send(frame);
            }
        }

        while let Some(Ok(encoded)) = video.as_ref().map(|v| v.encoded_rx.try_recv()) {
            let data = &encoded.data;
            let fragment_count = data.len().div_ceil(MAX_FRAGMENT_DATA) as u16;
            for (i, chunk) in data.chunks(MAX_FRAGMENT_DATA).enumerate() {
                let wire = encode_video(&VideoPacket {
                    sequence,
                    timestamp: encoded.pts,
                    is_keyframe: encoded.is_keyframe,
                    fragment_index: i as u16,
                    fragment_count,
                    data: chunk.to_vec(),
                })?;
                if tcp_server.send_to_client(id, &wire).await.is_err() {
                    break;
                }
            }
            sequence = sequence.wrapping_add(1);
        }

        if last_heartbeat.elapsed() > Duration::from_secs(1) {
            let heartbeat = encode_control(&ControlPacket::Heartbeat)?;
            let _ = tcp_server.send_to_client(id, &heartbeat).await;
            last_heartbeat = Instant::now();
        }
    }
}

