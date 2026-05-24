use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;
use tracing::{debug, error, info, trace, warn};

/// Soft cap for an individual encrypted frame. The wire format uses a u16 length
/// prefix (max 65535), but in practice our payloads are well under this — capping
/// here lets us reject obvious garbage / oversized frames early instead of allocating
/// up to 64KB per frame for an attacker's choice.
pub const MAX_FRAME_SIZE: usize = 16 * 1024;

/// Maximum time to wait for a single frame's bytes to arrive once we've started
/// reading. Half-open TCP connections (NAT drops, dead peers) otherwise leak the
/// reader task forever. 30s is generous for any legitimate frame.
pub const FRAME_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum time to wait for the whole 3-message Noise XX handshake to complete.
/// A legitimate handshake is sub-second on a healthy network; if we're 10s in
/// without finishing, the peer is misbehaving or gone.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum number of concurrent Noise sessions accepted from a single source IP.
/// Each session costs a tokio task plus a snow transport state, so an unbounded
/// peer could exhaust resources. This is high enough to allow legitimate use
/// (mobile opens one Noise session per local TCP fd, plus separate /stream WS).
pub const MAX_CONNECTIONS_PER_IP: usize = 32;

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
/// Active when OKTO_DEV=1. Generated once; DO NOT rotate.
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

/// Inverse of `to_base32`. Returns `None` on invalid characters.
pub fn from_base32(s: &str) -> Option<Vec<u8>> {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.chars() {
        let c = c.to_ascii_uppercase();
        let v = ALPHA.iter().position(|&x| x as char == c)? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Some(out)
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
    timeout(FRAME_READ_TIMEOUT, stream.read_exact(&mut len_buf))
        .await
        .map_err(|_| anyhow::anyhow!("timed out reading frame length"))??;
    let len = u16::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        anyhow::bail!("frame length {len} exceeds MAX_FRAME_SIZE ({MAX_FRAME_SIZE})");
    }
    let mut buf = vec![0u8; len];
    timeout(FRAME_READ_TIMEOUT, stream.read_exact(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("timed out reading {len}-byte frame body"))??;
    Ok(buf)
}

pub async fn write_noise_frame(stream: &mut tokio::net::TcpStream, data: &[u8]) -> anyhow::Result<()> {
    let len = (data.len() as u16).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(data).await?;
    Ok(())
}

/// Result of a successful Noise XX handshake on the responder side.
/// `remote_static` is the client's 32-byte Curve25519 public key, captured from
/// the handshake state before transitioning to transport mode. Used for
/// per-client identity binding (logging, future allowlist enforcement).
pub struct HandshakeResult {
    pub transport:     snow::TransportState,
    pub remote_static: Option<Vec<u8>>,
}

async fn noise_handshake_inner(
    stream: &mut tokio::net::TcpStream,
    static_private: &[u8],
) -> anyhow::Result<HandshakeResult> {
    let peer = stream.peer_addr().ok();
    debug!("[noise] starting XX handshake peer={peer:?}");
    let builder = snow::Builder::new(NOISE_PATTERN.parse()?);
    let mut hs = builder.local_private_key(static_private).build_responder()?;
    let mut payload = vec![0u8; MAX_FRAME_SIZE];
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
    if msg1_len > MAX_FRAME_SIZE {
        anyhow::bail!("handshake msg1 length {msg1_len} exceeds MAX_FRAME_SIZE");
    }
    debug!("[noise] msg1 len={msg1_len} peer={peer:?}");
    let mut msg1 = vec![0u8; msg1_len];
    stream.read_exact(&mut msg1).await?;
    hs.read_message(&msg1, &mut payload)?;
    let mut msg2 = vec![0u8; MAX_FRAME_SIZE];
    let n = hs.write_message(&[], &mut msg2)?;
    write_noise_frame(stream, &msg2[..n]).await?;
    debug!("[noise] msg2 sent ({n} bytes) peer={peer:?}");
    let msg3 = read_noise_frame(stream).await?;
    hs.read_message(&msg3, &mut payload)?;
    let remote_static = hs.get_remote_static().map(|s| s.to_vec());
    if let Some(ref rs) = remote_static {
        info!("[noise] handshake complete peer={peer:?} client_pub={}", to_base32(rs));
    } else {
        info!("[noise] handshake complete peer={peer:?} client_pub=<absent>");
    }
    Ok(HandshakeResult {
        transport: hs.into_transport_mode()?,
        remote_static,
    })
}

pub async fn noise_handshake(
    stream: &mut tokio::net::TcpStream,
    static_private: &[u8],
) -> anyhow::Result<HandshakeResult> {
    match timeout(HANDSHAKE_TIMEOUT, noise_handshake_inner(stream, static_private)).await {
        Ok(res) => res,
        Err(_)  => anyhow::bail!("noise handshake timed out after {HANDSHAKE_TIMEOUT:?}"),
    }
}

// `expected_initiator_pubkey`, when `Some(_)`, rejects any connection whose
// post-handshake remote static key bytes don't equal this value. Used by the
// remote-agent role to whitelist lair as the only legitimate initiator —
// without it, knowing `(host, port, agent_pubkey)` would be enough for any
// third party to complete the Noise XX handshake.
pub async fn handle_noise_connection(
    mut stream: tokio::net::TcpStream,
    static_private: Arc<Vec<u8>>,
    http_port: u16,
    expected_initiator_pubkey: Option<Arc<Vec<u8>>>,
) -> anyhow::Result<()> {
    let peer = stream.peer_addr().ok();
    let HandshakeResult { transport, remote_static } =
        noise_handshake(&mut stream, &static_private).await?;
    if let Some(expected) = expected_initiator_pubkey.as_deref() {
        match remote_static.as_deref() {
            Some(actual) if actual == expected.as_slice() => {}
            Some(actual) => {
                warn!(
                    "[noise] rejecting peer={peer:?}: initiator pubkey {} does not match expected {}",
                    to_base32(actual),
                    to_base32(expected),
                );
                anyhow::bail!("initiator pubkey not on allowlist");
            }
            None => {
                warn!("[noise] rejecting peer={peer:?}: handshake completed without an initiator pubkey");
                anyhow::bail!("initiator pubkey absent after handshake");
            }
        }
    }
    let transport = Arc::new(Mutex::new(transport));
    debug!("[noise] connecting to local HTTP port {http_port} for peer={peer:?}");
    let local = tokio::net::TcpStream::connect(format!("127.0.0.1:{http_port}")).await
        .map_err(|e| {
            error!("[noise] failed to connect to local HTTP port {http_port}: {e}");
            e
        })?;
    let (mut raw_read, mut raw_write) = stream.into_split();
    let (mut local_read, mut local_write) = local.into_split();
    let transport_enc = transport.clone();
    let transport_dec = transport.clone();
    // Plaintext buffer is sized so a full plaintext read encrypts to ≤ MAX_FRAME_SIZE
    // (ChaChaPoly adds a 16-byte auth tag).
    const PLAIN_BUF_SIZE: usize = MAX_FRAME_SIZE - 64;
    let task_a = tokio::spawn(async move {
        let mut plain = vec![0u8; PLAIN_BUF_SIZE];
        let mut enc   = vec![0u8; MAX_FRAME_SIZE];
        loop {
            let n = local_read.read(&mut plain).await.unwrap_or(0);
            if n == 0 { break; }
            let enc_n = match transport_enc.lock().unwrap().write_message(&plain[..n], &mut enc) {
                Ok(n)  => n,
                Err(e) => { warn!("[noise] proxy encrypt error: {e}"); break; }
            };
            trace!("[noise] proxy out {n}B plain -> {enc_n}B enc");
            let len = (enc_n as u16).to_be_bytes();
            if raw_write.write_all(&len).await.is_err()          { break; }
            if raw_write.write_all(&enc[..enc_n]).await.is_err() { break; }
        }
    });
    let task_b = tokio::spawn(async move {
        let mut len_buf = [0u8; 2];
        let mut enc = vec![0u8; MAX_FRAME_SIZE];
        let mut dec = vec![0u8; MAX_FRAME_SIZE];
        loop {
            // Length prefix can legitimately wait idle on a quiet channel — no timeout here.
            if raw_read.read_exact(&mut len_buf).await.is_err() { break; }
            let len = u16::from_be_bytes(len_buf) as usize;
            if len > MAX_FRAME_SIZE {
                warn!("[noise] dropping connection: frame length {len} exceeds MAX_FRAME_SIZE");
                break;
            }
            // Once the length prefix arrives, the body must follow promptly.
            // A peer that announces a length and stalls is buggy or hostile.
            match timeout(FRAME_READ_TIMEOUT, raw_read.read_exact(&mut enc[..len])).await {
                Ok(Ok(_))  => {}
                Ok(Err(_)) => break,
                Err(_)     => {
                    warn!("[noise] dropping connection: timed out reading {len}-byte frame body");
                    break;
                }
            }
            let dec_n = match transport_dec.lock().unwrap().read_message(&enc[..len], &mut dec) {
                Ok(n)  => n,
                Err(e) => { warn!("[noise] proxy decrypt error: {e}"); break; }
            };
            trace!("[noise] proxy in {len}B enc -> {dec_n}B plain");
            if local_write.write_all(&dec[..dec_n]).await.is_err() { break; }
        }
    });
    let abort_a = task_a.abort_handle();
    let abort_b = task_b.abort_handle();
    tokio::select! { _ = task_a => { abort_b.abort(); } _ = task_b => { abort_a.abort(); } }
    debug!("[noise] proxy session closed peer={peer:?}");
    Ok(())
}

// ── Outbound Noise tunnels (lair → remote agent) ─────────────────────────────

/// Run the Noise XX handshake as the **initiator** (client side), verifying
/// that the responder's static pubkey matches `expected_remote_static`.
/// Returns the transport state ready for `read_message` / `write_message`.
pub async fn noise_handshake_initiator(
    stream: &mut tokio::net::TcpStream,
    static_private: &[u8],
    expected_remote_static: &[u8],
) -> anyhow::Result<snow::TransportState> {
    let mut hs = snow::Builder::new(NOISE_PATTERN.parse()?)
        .local_private_key(static_private)
        .build_initiator()?;

    let mut buf = vec![0u8; MAX_FRAME_SIZE];

    // msg1 → responder
    let n = hs.write_message(&[], &mut buf)?;
    write_noise_frame(stream, &buf[..n]).await?;

    // msg2 ← responder (carries server's static public key inside the encrypted payload)
    let msg2 = read_noise_frame(stream).await?;
    hs.read_message(&msg2, &mut buf)?;

    // msg3 → responder
    let n = hs.write_message(&[], &mut buf)?;
    write_noise_frame(stream, &buf[..n]).await?;

    let server_static = hs.get_remote_static()
        .ok_or_else(|| anyhow::anyhow!("noise initiator: no remote static after handshake"))?;
    if server_static != expected_remote_static {
        anyhow::bail!(
            "noise initiator: remote pubkey mismatch (got {}, expected {})",
            to_base32(server_static),
            to_base32(expected_remote_static),
        );
    }
    Ok(hs.into_transport_mode()?)
}

/// Open an outbound Noise tunnel: bind an ephemeral local TCP listener that
/// accepts ONE inbound plaintext connection and forwards it through Noise to
/// `remote_host:remote_port`. Returns the local port the caller should
/// connect to (typically with `tokio_tungstenite::connect_async`).
///
/// This is the lair-side counterpart of the mobile app's
/// `NativeNoiseConnection` — encrypted point-to-point transport for traffic
/// that crosses the public internet.
pub async fn open_noise_tunnel(
    remote_host:     String,
    remote_port:     u16,
    expected_pubkey: Vec<u8>,
    static_private:  Vec<u8>,
) -> anyhow::Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await
        .map_err(|e| anyhow::anyhow!("bind ephemeral local port: {e}"))?;
    let local_port = listener.local_addr()?.port();

    tokio::spawn(async move {
        // Single inbound connection on the loopback; once accepted we drop the
        // listener so the port can be reclaimed cleanly after the session ends.
        let (local_stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => { warn!("[noise/initiator] accept failed: {e}"); return; }
        };
        drop(listener);

        let mut remote_stream = match tokio::net::TcpStream::connect(
            format!("{remote_host}:{remote_port}")
        ).await {
            Ok(s) => s,
            Err(e) => {
                warn!("[noise/initiator] connect {remote_host}:{remote_port}: {e}");
                return;
            }
        };

        let transport = match timeout(
            HANDSHAKE_TIMEOUT,
            noise_handshake_initiator(&mut remote_stream, &static_private, &expected_pubkey),
        ).await {
            Ok(Ok(t))  => t,
            Ok(Err(e)) => { warn!("[noise/initiator] handshake to {remote_host}:{remote_port}: {e}"); return; }
            Err(_)     => { warn!("[noise/initiator] handshake timeout to {remote_host}:{remote_port}"); return; }
        };

        pipe_noise_initiator(local_stream, remote_stream, transport).await;
        debug!("[noise/initiator] tunnel to {remote_host}:{remote_port} closed");
    });

    Ok(local_port)
}

/// Bidirectional pipe between a plaintext local TCP stream and a
/// Noise-encrypted remote TCP stream (initiator side). Symmetric in spirit
/// to `handle_noise_connection` but with the roles swapped.
async fn pipe_noise_initiator(
    local:     tokio::net::TcpStream,
    remote:    tokio::net::TcpStream,
    transport: snow::TransportState,
) {
    let transport = Arc::new(Mutex::new(transport));
    let (mut local_r, mut local_w)   = local.into_split();
    let (mut remote_r, mut remote_w) = remote.into_split();

    const PLAIN_BUF_SIZE: usize = MAX_FRAME_SIZE - 64;

    let t_enc = transport.clone();
    let task_out = tokio::spawn(async move {
        let mut plain = vec![0u8; PLAIN_BUF_SIZE];
        let mut enc   = vec![0u8; MAX_FRAME_SIZE];
        loop {
            let n = local_r.read(&mut plain).await.unwrap_or(0);
            if n == 0 { break; }
            let enc_n = match t_enc.lock().unwrap().write_message(&plain[..n], &mut enc) {
                Ok(n)  => n,
                Err(_) => break,
            };
            let len = (enc_n as u16).to_be_bytes();
            if remote_w.write_all(&len).await.is_err()          { break; }
            if remote_w.write_all(&enc[..enc_n]).await.is_err() { break; }
        }
    });

    let t_dec = transport.clone();
    let task_in = tokio::spawn(async move {
        let mut len_buf = [0u8; 2];
        let mut enc = vec![0u8; MAX_FRAME_SIZE];
        let mut dec = vec![0u8; MAX_FRAME_SIZE];
        loop {
            if remote_r.read_exact(&mut len_buf).await.is_err() { break; }
            let len = u16::from_be_bytes(len_buf) as usize;
            if len > MAX_FRAME_SIZE { break; }
            match timeout(FRAME_READ_TIMEOUT, remote_r.read_exact(&mut enc[..len])).await {
                Ok(Ok(_))  => {}
                Ok(Err(_)) | Err(_) => break,
            }
            let dec_n = match t_dec.lock().unwrap().read_message(&enc[..len], &mut dec) {
                Ok(n)  => n,
                Err(_) => break,
            };
            if local_w.write_all(&dec[..dec_n]).await.is_err() { break; }
        }
    });

    let abort_out = task_out.abort_handle();
    let abort_in  = task_in.abort_handle();
    tokio::select! {
        _ = task_out => { abort_in.abort();  }
        _ = task_in  => { abort_out.abort(); }
    }
}

// `expected_initiator_pubkey`, when `Some(_)`, restricts the listener to
// handshakes whose initiator static pubkey matches this value (everyone
// else is dropped after the handshake completes). Remote agents pass
// `Some(lair_pubkey)`; lair itself passes `None` because the mobile-facing
// tunnel is gated by QR distribution (tracked separately by the
// client-allowlist TODO in the project root).
pub async fn run_noise_proxy(
    static_private: Vec<u8>,
    noise_port: u16,
    http_port: u16,
    expected_initiator_pubkey: Option<Vec<u8>>,
) {
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{noise_port}"))
        .await.expect("failed to bind Noise port");
    if let Some(ref k) = expected_initiator_pubkey {
        info!(
            "[noise] listening on 0.0.0.0:{noise_port} → 127.0.0.1:{http_port} (allowlist={})",
            to_base32(k),
        );
    } else {
        info!("[noise] listening on 0.0.0.0:{noise_port} → 127.0.0.1:{http_port}");
    }
    let static_private = Arc::new(static_private);
    let expected_initiator_pubkey = expected_initiator_pubkey.map(Arc::new);
    let per_ip_count: Arc<Mutex<HashMap<IpAddr, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };

        // Reserve a slot for this peer IP, refusing if already at the limit. The
        // slot is released by the RAII-style guard below when the task exits.
        let peer_ip = peer.ip();
        let admit = {
            let mut counts = per_ip_count.lock().unwrap();
            let count = counts.entry(peer_ip).or_insert(0);
            if *count >= MAX_CONNECTIONS_PER_IP {
                false
            } else {
                *count += 1;
                true
            }
        };
        if !admit {
            warn!("[noise] rejecting connection from {peer}: per-IP limit ({MAX_CONNECTIONS_PER_IP}) reached");
            drop(stream);
            continue;
        }

        info!("[noise] connection from {peer}");
        let priv_clone = static_private.clone();
        let counts_clone = per_ip_count.clone();
        let expected_clone = expected_initiator_pubkey.clone();
        tokio::spawn(async move {
            // Decrement the per-IP counter when this task ends, regardless of outcome.
            struct ConnGuard {
                counts: Arc<Mutex<HashMap<IpAddr, usize>>>,
                ip:     IpAddr,
            }
            impl Drop for ConnGuard {
                fn drop(&mut self) {
                    let mut c = self.counts.lock().unwrap();
                    if let Some(n) = c.get_mut(&self.ip) {
                        *n = n.saturating_sub(1);
                        if *n == 0 { c.remove(&self.ip); }
                    }
                }
            }
            let _guard = ConnGuard { counts: counts_clone, ip: peer_ip };

            if let Err(e) = handle_noise_connection(stream, priv_clone, http_port, expected_clone).await {
                if e.is::<ProbeClosed>() {
                    debug!("[noise] probe closed (no handshake) from {peer}");
                } else {
                    error!("[noise] error from {peer}: {e}");
                }
            }
        });
    }
}
