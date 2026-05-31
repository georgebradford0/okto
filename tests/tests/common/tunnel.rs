//! Test-side Noise transport: connect to lair's Noise port exactly like the
//! mobile client does, then run plain HTTP / WebSocket over the encrypted
//! tunnel.
//!
//! The Noise layer is transparent — after the XX handshake, lair's proxy just
//! forwards decrypted bytes to its loopback HTTP server. So we bridge the
//! encrypted TCP socket to an in-memory `DuplexStream` and hand that ordinary
//! `AsyncRead + AsyncWrite` stream to an HTTP/WS client.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use okto_core::noise::{DEV_STATIC_PUBLIC, NOISE_PATTERN};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// Matches `okto_core::noise::MAX_FRAME_SIZE` (16 KiB). Kept local so the test
/// doesn't depend on that constant's visibility.
const MAX_FRAME_SIZE: usize = 16 * 1024;
/// Plaintext chunk that encrypts to ≤ MAX_FRAME_SIZE (ChaChaPoly adds a tag).
const PLAIN_BUF_SIZE: usize = MAX_FRAME_SIZE - 64;

/// Perform the Noise XX handshake as initiator and return the transport state
/// plus the server's static public key (for QR-pubkey verification).
async fn client_handshake(
    stream: &mut TcpStream,
) -> anyhow::Result<(snow::TransportState, Vec<u8>)> {
    let kp = snow::Builder::new(NOISE_PATTERN.parse()?).generate_keypair()?;
    let mut hs = snow::Builder::new(NOISE_PATTERN.parse()?)
        .local_private_key(&kp.private)
        .build_initiator()?;

    let mut buf = vec![0u8; 65535];

    // msg1 → server
    let n = hs.write_message(&[], &mut buf)?;
    stream.write_all(&(n as u16).to_be_bytes()).await?;
    stream.write_all(&buf[..n]).await?;

    // msg2 ← server (carries server static pubkey)
    let msg2 = read_frame(stream).await?;
    hs.read_message(&msg2, &mut buf)?;

    // msg3 → server
    let n = hs.write_message(&[], &mut buf)?;
    write_frame(stream, &buf[..n]).await?;

    let server_static = hs
        .get_remote_static()
        .ok_or_else(|| anyhow::anyhow!("no remote static after handshake"))?
        .to_vec();
    Ok((hs.into_transport_mode()?, server_static))
}

async fn read_frame(stream: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    anyhow::ensure!(len <= MAX_FRAME_SIZE, "frame {len} exceeds MAX_FRAME_SIZE");
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn write_frame(stream: &mut TcpStream, data: &[u8]) -> anyhow::Result<()> {
    stream.write_all(&(data.len() as u16).to_be_bytes()).await?;
    stream.write_all(data).await?;
    Ok(())
}

/// Open a Noise tunnel to `127.0.0.1:noise_port`, verify the server presents
/// the dev static pubkey, and return a plaintext `DuplexStream` that reads/
/// writes as if directly connected to lair's HTTP server.
pub async fn open_tunnel(noise_port: u16) -> anyhow::Result<DuplexStream> {
    let mut tcp = TcpStream::connect(("127.0.0.1", noise_port))
        .await
        .context("connect to lair noise port")?;
    let (transport, server_static) = client_handshake(&mut tcp).await?;
    anyhow::ensure!(
        server_static == DEV_STATIC_PUBLIC,
        "server static pubkey does not match the dev keypair (is lair running with OKTO_DEV=1?)"
    );

    let (app_side, bridge_side) = tokio::io::duplex(256 * 1024);
    let (mut bridge_rd, mut bridge_wr) = tokio::io::split(bridge_side);
    let (mut tcp_rd, mut tcp_wr) = tcp.into_split();
    let transport = Arc::new(Mutex::new(transport));

    // app → net: read plaintext, encrypt, write length-prefixed frames.
    let enc = transport.clone();
    tokio::spawn(async move {
        let mut plain = vec![0u8; PLAIN_BUF_SIZE];
        let mut out = vec![0u8; MAX_FRAME_SIZE];
        loop {
            let n = match bridge_rd.read(&mut plain).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let m = match enc.lock().unwrap().write_message(&plain[..n], &mut out) {
                Ok(m) => m,
                Err(_) => break,
            };
            if tcp_wr.write_all(&(m as u16).to_be_bytes()).await.is_err() {
                break;
            }
            if tcp_wr.write_all(&out[..m]).await.is_err() {
                break;
            }
        }
    });

    // net → app: read frames, decrypt, write plaintext.
    let dec = transport.clone();
    tokio::spawn(async move {
        let mut len_buf = [0u8; 2];
        let mut inb = vec![0u8; MAX_FRAME_SIZE];
        let mut out = vec![0u8; MAX_FRAME_SIZE];
        loop {
            if tcp_rd.read_exact(&mut len_buf).await.is_err() {
                break;
            }
            let len = u16::from_be_bytes(len_buf) as usize;
            if len > MAX_FRAME_SIZE || tcp_rd.read_exact(&mut inb[..len]).await.is_err() {
                break;
            }
            let m = match dec.lock().unwrap().read_message(&inb[..len], &mut out) {
                Ok(m) => m,
                Err(_) => break,
            };
            if bridge_wr.write_all(&out[..m]).await.is_err() {
                break;
            }
        }
    });

    Ok(app_side)
}

/// One-shot HTTP GET over a fresh tunnel. Returns (status, body).
pub async fn http_get(noise_port: u16, path: &str) -> anyhow::Result<(u16, String)> {
    http_request(noise_port, "GET", path).await
}

/// One-shot HTTP POST (empty body) over a fresh tunnel. Returns (status, body).
pub async fn http_post(noise_port: u16, path: &str) -> anyhow::Result<(u16, String)> {
    http_request(noise_port, "POST", path).await
}

/// Overall ceiling for a single one-shot HTTP request over the tunnel. Bounds
/// the call so a stuck connection surfaces as an error instead of hanging.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// One-shot HTTP request over a fresh tunnel. Reads the response by
/// `Content-Length` rather than waiting for the server to close the socket, so
/// it works regardless of keep-alive. Bounded by `HTTP_TIMEOUT`.
pub async fn http_request(noise_port: u16, method: &str, path: &str) -> anyhow::Result<(u16, String)> {
    tokio::time::timeout(HTTP_TIMEOUT, http_request_inner(noise_port, method, path))
        .await
        .map_err(|_| anyhow::anyhow!("HTTP {method} {path} timed out after {HTTP_TIMEOUT:?}"))?
}

async fn http_request_inner(noise_port: u16, method: &str, path: &str) -> anyhow::Result<(u16, String)> {
    let mut stream = open_tunnel(noise_port).await?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: lair\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await?;

    // Read until the header terminator is in `buf`.
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("connection closed before HTTP headers");
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| anyhow::anyhow!("no status line in: {head:?}"))?;
    let content_len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);

    // Read exactly `content_len` body bytes.
    while buf.len() < header_end + content_len {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = String::from_utf8_lossy(&buf[header_end..header_end + content_len]).to_string();
    Ok((status, body))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A chat WebSocket to lair's `/stream` (or any agent stream path) over the
/// Noise tunnel.
pub struct ChatWs {
    ws: WebSocketStream<DuplexStream>,
}

impl ChatWs {
    /// Connect and perform the WS upgrade for `path` (e.g. `/stream`).
    pub async fn connect(noise_port: u16, path: &str) -> anyhow::Result<ChatWs> {
        let stream = open_tunnel(noise_port).await?;
        let url = format!("ws://lair{path}");
        let (ws, _resp) = tokio_tungstenite::client_async(url, stream)
            .await
            .context("websocket upgrade over tunnel")?;
        Ok(ChatWs { ws })
    }

    /// Send a `user_message` turn.
    pub async fn send_user_message(&mut self, text: &str) -> anyhow::Result<()> {
        let frame = json!({"type":"user_message","text":text}).to_string();
        self.ws.send(Message::Text(frame)).await?;
        Ok(())
    }

    /// Send an `interrupt`.
    pub async fn interrupt(&mut self) -> anyhow::Result<()> {
        self.ws
            .send(Message::Text(json!({"type":"interrupt"}).to_string()))
            .await?;
        Ok(())
    }

    /// Read the next server event as JSON, auto-answering pings. Returns None
    /// on close. Bounded so a stalled stream errors instead of hanging.
    pub async fn next_event(&mut self) -> anyhow::Result<Option<Value>> {
        tokio::time::timeout(Duration::from_secs(20), self.next_event_inner())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for next chat event"))?
    }

    async fn next_event_inner(&mut self) -> anyhow::Result<Option<Value>> {
        loop {
            match self.ws.next().await {
                None => return Ok(None),
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t)?;
                    if v.get("type").and_then(|x| x.as_str()) == Some("ping") {
                        let id = v.get("id").cloned().unwrap_or(json!(0));
                        self.ws
                            .send(Message::Text(json!({"type":"pong","id":id}).to_string()))
                            .await
                            .ok();
                        continue;
                    }
                    return Ok(Some(v));
                }
                Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Ok(_)) => continue, // binary/ping/pong control frames
                Some(Err(e)) => return Err(e.into()),
            }
        }
    }

    /// Drain events until a terminal frame (`done` / `interrupted` / `error`),
    /// returning every event seen including the terminator.
    pub async fn collect_turn(&mut self) -> anyhow::Result<Vec<Value>> {
        let mut events = Vec::new();
        while let Some(ev) = self.next_event().await? {
            let ty = ev.get("type").and_then(|x| x.as_str()).unwrap_or("").to_string();
            events.push(ev);
            if matches!(ty.as_str(), "done" | "interrupted" | "error") {
                break;
            }
        }
        Ok(events)
    }
}

/// Convenience: collect the `type` field of each event in order.
pub fn event_types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e.get("type").and_then(|x| x.as_str()).unwrap_or("").to_string())
        .collect()
}
