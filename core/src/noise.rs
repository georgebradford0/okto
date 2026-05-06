use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, error, info, warn};

/// Returned when a connection closes before sending any handshake bytes.
/// This is normal for TCP probes and reconnect races; not a real error.
#[derive(Debug)]
pub struct ProbeClosed;
impl std::fmt::Display for ProbeClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "connection closed before handshake")
    }
}
impl std::error::Error for ProbeClosed {}

pub const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_SHA256";

/// Fixed dev keypair — always the same so the mobile app can hardcode the public key.
/// Active when OCTO_DEV=1. Generated once; DO NOT rotate.
pub const DEV_STATIC_PRIVATE: [u8; 32] = [
    0x6a, 0x58, 0xeb, 0x21, 0x90, 0x00, 0xf0, 0x5f,
    0xd2, 0x6a, 0xf1, 0x58, 0x74, 0xc6, 0x69, 0xbd,
    0x76, 0x01, 0xf8, 0x18, 0x27, 0x11, 0x66, 0xc7,
    0xa2, 0xb1, 0x3e, 0x54, 0x8b, 0xa5, 0x48, 0xbc,
];
pub const DEV_STATIC_PUBLIC: [u8; 32] = [
    0xdf, 0x3b, 0xff, 0xd5, 0xd2, 0xcc, 0x47, 0x19,
    0xd0, 0x3f, 0xbe, 0x27, 0x3f, 0x16, 0x5e, 0xd6,
    0x39, 0x0c, 0x62, 0xab, 0x82, 0x44, 0x77, 0xf2,
    0xed, 0x1c, 0x01, 0xaf, 0xfb, 0x60, 0xa7, 0x71,
];
/// Base32(DEV_STATIC_PUBLIC) — matches the hardcoded pk in mobile/App.tsx
pub const DEV_PUBKEY_BASE32: &str = "34577VOSZRDRTUB7XYTT6FS62Y4QYYVLQJCHP4XNDQA2763AU5YQ";

pub fn to_base32(data: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in data {
        buf = (buf << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHA[((buf >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHA[((buf << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

pub fn load_or_generate_keypair(path: &str) -> (Vec<u8>, Vec<u8>) {
    if let Ok(bytes) = std::fs::read(path) {
        if bytes.len() == 64 {
            info!("[noise] loaded existing keypair from {path}");
            return (bytes[..32].to_vec(), bytes[32..].to_vec());
        }
        warn!("[noise] keypair file {path} is wrong length ({}), regenerating", bytes.len());
    } else {
        info!("[noise] no keypair at {path}, generating new one");
    }
    let builder = snow::Builder::new(NOISE_PATTERN.parse().expect("valid pattern"));
    let kp = builder.generate_keypair().expect("keygen");
    let mut combined = kp.private.clone();
    combined.extend_from_slice(&kp.public);
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, &combined).ok();
    info!("[noise] generated and saved new keypair to {path}");
    (kp.private, kp.public)
}

pub async fn read_noise_frame(stream: &mut tokio::net::TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn write_noise_frame(stream: &mut tokio::net::TcpStream, data: &[u8]) -> anyhow::Result<()> {
    let len = (data.len() as u16).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(data).await?;
    Ok(())
}

pub async fn noise_handshake(
    stream: &mut tokio::net::TcpStream,
    static_private: &[u8],
) -> anyhow::Result<snow::TransportState> {
    let peer = stream.peer_addr().ok();
    debug!("[noise] starting XX handshake peer={peer:?}");
    let builder = snow::Builder::new(NOISE_PATTERN.parse()?);
    let mut hs = builder.local_private_key(static_private).build_responder()?;
    let mut payload = vec![0u8; 65535];
    // Use read() for the first 2-byte length prefix so we can distinguish a
    // clean close (0 bytes → probe) from an EOF mid-handshake (real error).
    let mut len_buf = [0u8; 2];
    let n = stream.read(&mut len_buf).await?;
    if n == 0 {
        debug!("[noise] probe closed before handshake peer={peer:?}");
        return Err(anyhow::Error::new(ProbeClosed));
    }
    if n == 1 {
        stream.read_exact(&mut len_buf[1..]).await?;
    }
    let msg1_len = u16::from_be_bytes(len_buf) as usize;
    debug!("[noise] msg1 len={msg1_len} peer={peer:?}");
    let mut msg1 = vec![0u8; msg1_len];
    stream.read_exact(&mut msg1).await?;
    hs.read_message(&msg1, &mut payload)?;
    let mut msg2 = vec![0u8; 65535];
    let n = hs.write_message(&[], &mut msg2)?;
    write_noise_frame(stream, &msg2[..n]).await?;
    debug!("[noise] msg2 sent ({n} bytes) peer={peer:?}");
    let msg3 = read_noise_frame(stream).await?;
    hs.read_message(&msg3, &mut payload)?;
    info!("[noise] handshake complete peer={peer:?}");
    Ok(hs.into_transport_mode()?)
}

pub async fn handle_noise_connection(
    mut stream: tokio::net::TcpStream,
    static_private: Arc<Vec<u8>>,
    http_port: u16,
) -> anyhow::Result<()> {
    let peer = stream.peer_addr().ok();
    let transport = noise_handshake(&mut stream, &static_private).await?;
    let transport = Arc::new(Mutex::new(transport));
    debug!("[noise] connecting to local HTTP port {http_port} for peer={peer:?}");
    let local = tokio::net::TcpStream::connect(format!("127.0.0.1:{http_port}")).await?;
    let (mut raw_read, mut raw_write) = stream.into_split();
    let (mut local_read, mut local_write) = local.into_split();
    let transport_enc = transport.clone();
    let transport_dec = transport.clone();
    let task_a = tokio::spawn(async move {
        let mut plain = vec![0u8; 65000];
        let mut enc   = vec![0u8; 65535];
        loop {
            let n = local_read.read(&mut plain).await.unwrap_or(0);
            if n == 0 { break; }
            let enc_n = match transport_enc.lock().unwrap().write_message(&plain[..n], &mut enc) {
                Ok(n)  => n,
                Err(_) => break,
            };
            let len = (enc_n as u16).to_be_bytes();
            if raw_write.write_all(&len).await.is_err()          { break; }
            if raw_write.write_all(&enc[..enc_n]).await.is_err() { break; }
        }
    });
    let task_b = tokio::spawn(async move {
        let mut len_buf = [0u8; 2];
        let mut enc = vec![0u8; 65535];
        let mut dec = vec![0u8; 65535];
        loop {
            if raw_read.read_exact(&mut len_buf).await.is_err() { break; }
            let len = u16::from_be_bytes(len_buf) as usize;
            if len > enc.len() { break; }
            if raw_read.read_exact(&mut enc[..len]).await.is_err() { break; }
            let dec_n = match transport_dec.lock().unwrap().read_message(&enc[..len], &mut dec) {
                Ok(n)  => n,
                Err(_) => break,
            };
            if local_write.write_all(&dec[..dec_n]).await.is_err() { break; }
        }
    });
    let abort_a = task_a.abort_handle();
    let abort_b = task_b.abort_handle();
    tokio::select! { _ = task_a => { abort_b.abort(); } _ = task_b => { abort_a.abort(); } }
    debug!("[noise] proxy session closed peer={peer:?}");
    Ok(())
}

pub async fn run_noise_proxy(static_private: Vec<u8>, noise_port: u16, http_port: u16) {
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{noise_port}"))
        .await.expect("failed to bind Noise port");
    info!("[noise] listening on 0.0.0.0:{noise_port} → 127.0.0.1:{http_port}");
    let static_private = Arc::new(static_private);
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        info!("[noise] connection from {peer}");
        let priv_clone = static_private.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_noise_connection(stream, priv_clone, http_port).await {
                if e.is::<ProbeClosed>() {
                    debug!("[noise] probe closed (no handshake) from {peer}");
                } else {
                    error!("[noise] error from {peer}: {e}");
                }
            }
        });
    }
}
