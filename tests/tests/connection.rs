use std::sync::Arc;

use claudulhu_core::noise::{
    handle_noise_connection, noise_handshake, read_noise_frame, write_noise_frame,
    DEV_STATIC_PRIVATE, DEV_STATIC_PUBLIC, NOISE_PATTERN,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Perform the Noise XX handshake as the initiator (client side).
/// Returns the transport state and the server's received static public key.
/// The caller is responsible for verifying the static key matches the QR code value.
async fn client_handshake(
    stream: &mut TcpStream,
) -> anyhow::Result<(snow::TransportState, Vec<u8>)> {
    let kp = snow::Builder::new(NOISE_PATTERN.parse()?)
        .generate_keypair()?;
    let mut hs = snow::Builder::new(NOISE_PATTERN.parse()?)
        .local_private_key(&kp.private)
        .build_initiator()?;

    let mut buf = vec![0u8; 65535];

    // msg1 → server
    let n = hs.write_message(&[], &mut buf)?;
    stream.write_all(&(n as u16).to_be_bytes()).await?;
    stream.write_all(&buf[..n]).await?;

    // msg2 ← server (contains server's static public key)
    let msg2 = read_noise_frame(stream).await?;
    hs.read_message(&msg2, &mut buf)?;

    // msg3 → server
    let n = hs.write_message(&[], &mut buf)?;
    write_noise_frame(stream, &buf[..n]).await?;

    let server_static = hs.get_remote_static()
        .ok_or_else(|| anyhow::anyhow!("no remote static key after handshake"))?
        .to_vec();

    Ok((hs.into_transport_mode()?, server_static))
}

/// Verify that the Noise XX handshake completes and the resulting session
/// keys produce a working encrypted channel.
#[tokio::test]
async fn noise_handshake_completes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        noise_handshake(&mut stream, &DEV_STATIC_PRIVATE)
            .await
            .expect("server handshake failed")
    });

    let mut client_stream = TcpStream::connect(addr).await.unwrap();
    let (mut client_ts, _) = client_handshake(&mut client_stream)
        .await
        .expect("client handshake failed");

    let mut server_ts = server_task.await.unwrap();

    // Verify the shared session works: client encrypts, server decrypts.
    let plaintext = b"hello rulyeh";
    let mut ciphertext = vec![0u8; plaintext.len() + 64];
    let enc_len = client_ts.write_message(plaintext, &mut ciphertext).unwrap();

    let mut decrypted = vec![0u8; plaintext.len() + 64];
    let dec_len = server_ts
        .read_message(&ciphertext[..enc_len], &mut decrypted)
        .unwrap();

    assert_eq!(&decrypted[..dec_len], plaintext);

    // And the reverse: server encrypts, client decrypts.
    let reply = b"hello client";
    let mut ciphertext2 = vec![0u8; reply.len() + 64];
    let enc_len2 = server_ts.write_message(reply, &mut ciphertext2).unwrap();

    let mut decrypted2 = vec![0u8; reply.len() + 64];
    let dec_len2 = client_ts
        .read_message(&ciphertext2[..enc_len2], &mut decrypted2)
        .unwrap();

    assert_eq!(&decrypted2[..dec_len2], reply);
}

/// Verify that the client can read the server's static public key after the
/// handshake. This is how QR-code authentication works: the client compares
/// the received key against the one encoded in the QR code and rejects the
/// connection if they differ. Noise XX completes at the protocol level
/// regardless — rejection is the application's responsibility.
#[tokio::test]
async fn noise_handshake_exposes_server_static_key() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = noise_handshake(&mut stream, &DEV_STATIC_PRIVATE).await;
    });

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let (_, received_key) = client_handshake(&mut stream).await.unwrap();

    // Client can verify this against the key from the QR code.
    assert_eq!(received_key, DEV_STATIC_PUBLIC);

    // Connecting to a server with a different key would yield a different
    // received_key, and the application should close the connection.
}

/// Verify the full path the mobile client takes: Noise tunnel → HTTP proxy →
/// server. A minimal HTTP backend responds with "pong"; the test encrypts an
/// HTTP GET, tunnels it through `handle_noise_connection`, and decrypts the
/// response frames — mirroring what the iOS/Android NoiseConnectionModule does
/// for every WebSocket connection the app opens.
#[tokio::test]
async fn noise_proxy_forwards_http() {
    // Minimal HTTP backend: read the request and reply with a fixed response.
    let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_port = http_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut conn, _) = http_listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        conn.read(&mut buf).await.unwrap();
        conn.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong",
        )
        .await
        .unwrap();
        // drop conn — server closes, proxy task_a exits, proxy shuts down
    });

    // Noise proxy in front of the HTTP backend.
    let noise_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let noise_addr = noise_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = noise_listener.accept().await.unwrap();
        handle_noise_connection(stream, Arc::new(DEV_STATIC_PRIVATE.to_vec()), http_port)
            .await
            .ok();
    });

    // Connect as Noise client (mobile role) and complete the handshake.
    let mut stream = TcpStream::connect(noise_addr).await.unwrap();
    let (mut transport, received_key) = client_handshake(&mut stream).await.unwrap();
    assert_eq!(received_key, DEV_STATIC_PUBLIC, "server key should match QR-code value");

    // Encrypt the HTTP request and write it as a Noise frame — identical to
    // what the iOS proxy's "local → encrypt → remote" loop does.
    let req = b"GET /ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let mut enc = vec![0u8; req.len() + 64];
    let n = transport.write_message(req, &mut enc).unwrap();
    write_noise_frame(&mut stream, &enc[..n]).await.unwrap();

    // Collect decrypted frames until the proxy closes the connection — mirrors
    // the iOS "remote → decrypt → local" loop.
    let mut response = Vec::new();
    loop {
        match read_noise_frame(&mut stream).await {
            Ok(frame) => {
                let mut dec = vec![0u8; frame.len() + 64];
                let dec_n = transport.read_message(&frame, &mut dec).unwrap();
                response.extend_from_slice(&dec[..dec_n]);
            }
            Err(_) => break, // proxy closed the connection — response complete
        }
    }

    let response_str = std::str::from_utf8(&response).unwrap();
    assert!(
        response_str.starts_with("HTTP/1.1 200"),
        "unexpected status: {response_str:?}",
    );
    assert!(
        response_str.ends_with("pong"),
        "missing response body: {response_str:?}",
    );
}
