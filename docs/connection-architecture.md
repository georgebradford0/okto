# Connection Architecture: Mobile → Lair

This document traces every step of establishing a connection from the mobile app to a
lair server, including the Noise handshake, the local proxy model, and all the
React Native state that drives it.

---

## 1. Big Picture

```
Mobile (React Native)
│
│  [QR scan]  →  host, port, base32-pubkey
│
│  NoiseConnection.connect(host, port, pk)
│    → TCP probe (verify reachable)
│    → bind 127.0.0.1:0  (random localPort)
│    → spawn acceptLoop
│    → resolve(localPort)
│
│  tunnelPort = localPort
│
│  WebSocket ws://127.0.0.1:{tunnelPort}/stream
│       ↓
│  [acceptLoop accepts]
│  proxyConnection
│    → TCP connect to host:port
│    → Noise_XX handshake
│    → bidirectional encrypted proxy
│       ↕ (all traffic encrypted)
└── lair server (port 9000)
      → run_noise_proxy
      → handle_noise_connection
      → TCP connect to 127.0.0.1:8000 (axum HTTP)
      → bidirectional plaintext proxy
         ↕
      axum HTTP (port 8000)
        GET /history
        GET /stream  (WebSocket)
        POST /message
```

Traffic path in full:

```
App JS ←→ WebSocket ←→ iOS TCP (127.0.0.1:tunnelPort)
                           ↓ accept
                       proxyConnection
                           ↓ TCP connect + Noise handshake
                       host:9000 (lair noise proxy)
                           ↓ decrypt
                       127.0.0.1:8000 (axum)
```

The mobile app never speaks plaintext directly to the server. Everything flows through
the local proxy which encrypts it on the way out and decrypts it on the way in.

---

## 2. QR Code Format

The QR code encodes a colon-delimited string:

```
2:<host>:<port>:<base32-pubkey>
```

Example:
```
2:10.42.0.1:9000:34577VOSZRDRTUB7XYTT6FS62Y4QYYVLQJCHP4XNDQA2763AU5YQ
```

Parsed in `App.tsx`:

```ts
function parseQrData(raw: string): NoiseConnectionInfo | null {
  const parts = raw.split(':')
  if (parts[0] === '2' && parts.length === 4) {
    const [, host, portStr, pk] = parts
    const port = parseInt(portStr, 10)
    if (!host || isNaN(port) || !pk) return null
    return { v: 2, host, port, pk }
  }
  return null
}
```

The parsed `{ host, port, pk }` is saved to AsyncStorage as `masterConnection` and
reloaded on every app launch. Version prefix `2` distinguishes this from the old SSH
format (`1`).

The server generates the QR data at startup. `PUBLIC_HOST` and `NOISE_PORT` control what
goes in it; if `PUBLIC_HOST` is unset, the server detects its outbound IP via a UDP
socket to 8.8.8.8:

```rust
// lair/src/main.rs
let public_host = std::env::var("PUBLIC_HOST")
    .ok()
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| {
        std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| { s.connect("8.8.8.8:80")?; s.local_addr() })
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string())
    });
```

---

## 3. Server Startup: Noise Proxy and HTTP

`lair/src/main.rs` starts two things:

```rust
// Noise proxy on port 9000
tokio::spawn(run_noise_proxy(static_private, noise_port, http_port));

// Axum HTTP on port 8000
axum::serve(listener, app).await.unwrap();
```

`run_noise_proxy` (in `core/src/noise.rs`) binds a TCP listener and spawns a new task
per incoming connection:

```rust
pub async fn run_noise_proxy(static_private: Vec<u8>, noise_port: u16, http_port: u16) {
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{noise_port}"))
        .await.expect("failed to bind Noise port");
    let static_private = Arc::new(static_private);
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        println!("[noise] connection from {peer}");          // <-- these are the logs you see
        let priv_clone = static_private.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_noise_connection(stream, priv_clone, http_port).await {
                eprintln!("[noise] error from {peer}: {e}"); // <-- "early eof" appears here
            }
        });
    }
}
```

Every TCP connection to port 9000 — including the mobile probe — triggers
`"[noise] connection from ..."`. If the connection closes before the handshake
completes (e.g. a probe), tokio's `read_exact` returns an unexpected-EOF error
which prints `"[noise] error from ...: early eof"`.

---

## 4. Server-Side Noise Handshake

`handle_noise_connection` first completes the Noise XX handshake, then splices
the decrypted stream to the local HTTP port:

```rust
pub async fn handle_noise_connection(
    mut stream: tokio::net::TcpStream,
    static_private: Arc<Vec<u8>>,
    http_port: u16,
) -> anyhow::Result<()> {
    // 1. Handshake
    let transport = noise_handshake(&mut stream, &static_private).await?;
    let transport = Arc::new(Mutex::new(transport));

    // 2. Connect to local HTTP
    let local = tokio::net::TcpStream::connect(format!("127.0.0.1:{http_port}")).await?;

    let (mut raw_read,   mut raw_write)   = stream.into_split();
    let (mut local_read, mut local_write) = local.into_split();

    let transport_enc = transport.clone();
    let transport_dec = transport.clone();

    // task_a: local HTTP → encrypt → remote client
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

    // task_b: remote client → decrypt → local HTTP
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

    // Exit when either direction closes
    tokio::select! { _ = task_a => {} _ = task_b => {} }
    Ok(())
}
```

The handshake itself (responder role):

```rust
pub async fn noise_handshake(
    stream: &mut tokio::net::TcpStream,
    static_private: &[u8],
) -> anyhow::Result<snow::TransportState> {
    let builder = snow::Builder::new(NOISE_PATTERN.parse()?);
    let mut hs = builder.local_private_key(static_private).build_responder()?;
    let mut payload = vec![0u8; 65535];

    let msg1 = read_noise_frame(stream).await?;   // ← e  (32 bytes, client ephemeral pub)
    hs.read_message(&msg1, &mut payload)?;

    let mut msg2 = vec![0u8; 65535];
    let n = hs.write_message(&[], &mut msg2)?;    // → e, ee, s, es  (96 bytes)
    write_noise_frame(stream, &msg2[..n]).await?;

    let msg3 = read_noise_frame(stream).await?;   // ← s, se  (client static pub + MAC)
    hs.read_message(&msg3, &mut payload)?;

    Ok(hs.into_transport_mode()?)
}
```

Frame format used throughout (both handshake and transport):

```rust
// 2-byte big-endian length prefix + payload
pub async fn read_noise_frame(stream: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn write_noise_frame(stream: &mut TcpStream, data: &[u8]) -> anyhow::Result<()> {
    let len = (data.len() as u16).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(data).await?;
    Ok(())
}
```

---

## 5. iOS Native Module: `NoiseConnection.connect()`

The native module (`mobile/ios/octo/NoiseConnectionModule.swift`) acts as a local
TCP proxy. `connect()` doesn't actually connect to the server in the JS sense — it
sets up a local listening socket and returns its port. The actual server connection
happens only when the app opens a WebSocket to that local port.

### 5a. Probe

```swift
// Verify the server is reachable before committing.
// connectSocket() is a non-blocking connect with 15-second timeout.
let probeFd = connectSocket(host: host, port: Int(port))
guard probeFd >= 0 else {
    reject("NOISE_CONNECT_ERROR", "Cannot reach \(host):\(Int(port)) — …", nil)
    return
}
Darwin.close(probeFd)  // close immediately — this is the "early eof" the server sees
```

The probe opens a real TCP connection to `host:9000`, which is why the server logs
`"[noise] connection from ..."` followed immediately by `"early eof"` — the probe
closes the fd without sending any Noise handshake bytes.

### 5b. Local listen socket

```swift
let (fd, localPort) = makeServerSocket()
// makeServerSocket() binds 127.0.0.1:0 and listens — OS assigns a free port.
// Returns (fd, assignedPort).
```

The listen socket is bound to `127.0.0.1` only — unreachable outside the device.
Port 0 means the OS picks a free ephemeral port.

### 5c. Installing state (with generation check)

```swift
self.stateLock.lock()
// Generation check: abort if disconnect() fired while we were probing.
guard self.connectGeneration == myGen else {
    self.stateLock.unlock()
    Darwin.close(fd)
    reject("NOISE_CONNECT_ERROR", "Connection superseded", nil)
    return
}
let oldFd = self.listenFd
self.listenFd = fd
self.active   = true
self.stateLock.unlock()

if oldFd >= 0 { Darwin.close(oldFd) }
```

`myGen` is captured at the top of `connect()` before the async dispatch. `disconnect()`
increments `connectGeneration`, so any call that was still in-flight when `disconnect()`
ran will fail these checks and abort rather than clobbering state.

### 5d. Accept loop

```swift
DispatchQueue.global(qos: .utility).async { [weak self] in
    self?.acceptLoop(fd: fd, host: host, port: Int(port), serverPub: pk, generation: myGen)
}
resolve(localPort)   // <-- Promise resolves here; App.tsx gets tunnelPort
```

```swift
private func acceptLoop(fd: Int32, host: String, port: Int, serverPub: Data, generation: Int) {
    while true {
        stateLock.lock()
        let alive = active && connectGeneration == generation
        stateLock.unlock()
        guard alive else { break }
        let clientFd = Darwin.accept(fd, nil, nil)
        if clientFd < 0 { break }
        DispatchQueue.global(qos: .utility).async {
            self.proxyConnection(localFd: clientFd, host: host, port: port,
                                 serverPub: serverPub, generation: generation)
        }
    }
}
```

The accept loop blocks on `Darwin.accept()`. When `disconnect()` closes the listen fd,
`accept()` returns -1 and the loop exits. The generation check at the top of each
iteration handles the case where `active` was re-set to true by a new `connect()`
using the same fd number.

---

## 6. iOS Native Module: `proxyConnection()`

One `proxyConnection` is spawned per WebSocket or HTTP request that the app opens.

```swift
private func proxyConnection(localFd: Int32, host: String, port: Int,
                              serverPub: Data, generation: Int) {
    // Register localFd immediately so disconnect() can close it even while
    // we are blocked inside connectSocket().
    proxyFdsLock.lock(); proxyFds.insert(localFd); proxyFdsLock.unlock()

    let remoteFd = connectSocket(host: host, port: port)  // TCP connect to server
    guard remoteFd >= 0 else {
        // Clean up: remove localFd from set and close it if still ours.
        proxyFdsLock.lock()
        let owned = proxyFds.remove(localFd) != nil
        proxyFdsLock.unlock()
        if owned { Darwin.close(localFd) }
        return
    }

    // Generation check: disconnect() may have closed localFd while we were
    // blocked in connectSocket(). Don't continue with a stale session.
    stateLock.lock()
    let stillCurrent = connectGeneration == generation
    stateLock.unlock()
    guard stillCurrent else {
        Darwin.close(remoteFd)   // never added to proxyFds; close directly
        return
    }

    proxyFdsLock.lock(); proxyFds.insert(remoteFd); proxyFdsLock.unlock()

    // ... Noise handshake + bidirectional proxy ...
}
```

### 6a. Noise handshake (client/initiator side)

The handshake is a full Swift implementation of Noise_XX (no external library):

```
Message 1  (→ e)          client sends ephemeral public key (32 bytes)
Message 2  (← e, ee, s, es)  server sends ephemeral pub + encrypted static pub (96 bytes)
Message 3  (→ s, se)      client sends encrypted static pub + MAC
Split      →  sendKey, recvKey  (two independent ChaCha20 cipher streams)
```

Key verification in message 2:

```swift
let rsPub = try hs.decryptAndHash(encRs)    // decrypt server's static key
guard rsPub == serverPub else {             // compare against key from QR code
    throw NoiseError.identityMismatch       // abort if wrong server
}
```

This is the TOFU/pinning check. If the server's key doesn't match the QR, the
connection is rejected. Since the server keypair is stored in `/data/noise_key.bin`
and persists across restarts, the same QR code works indefinitely unless you delete
the key file.

### 6b. Transport encryption

After handshake, each direction has its own independent nonce counter and ChaCha20-Poly1305 key:

```swift
final class NoiseTransport {
    private let sendKey: Data
    private let recvKey: Data
    private var sendN: UInt64 = 0   // monotonically incremented
    private var recvN: UInt64 = 0
    private let lock = NSLock()

    func encrypt(_ plain: Data) throws -> Data {
        lock.lock(); defer { lock.unlock() }
        let ct = try chachaEncrypt(key: sendKey, nonce: sendN, aad: Data(), plain: plain)
        sendN += 1
        return ct
    }
    func decrypt(_ ct: Data) throws -> Data {
        lock.lock(); defer { lock.unlock() }
        let plain = try chachaDecrypt(key: recvKey, nonce: recvN, aad: Data(), ct: ct)
        recvN += 1
        return plain
    }
}
```

Nonce encoding (4 zero bytes + 8-byte little-endian counter = 12 bytes total):

```swift
private func encodeNonce(_ n: UInt64) -> Data {
    var nonce = Data(count: 12)
    var v = n
    for i in 0..<8 { nonce[4 + i] = UInt8(v & 0xff); v >>= 8 }
    return nonce
}
```

### 6c. Bidirectional proxy

Two DispatchQueue tasks run simultaneously on the connected fds:

```swift
// local (WebSocket client) → encrypt → remote (server)
DispatchQueue.global(qos: .utility).async {
    defer { g.leave(); closeBoth() }
    var buf = Data(count: 65000)
    while true {
        let n = buf.withUnsafeMutableBytes {
            Darwin.recv(localFd, $0.baseAddress!, 65000, 0)
        }
        guard n > 0,
              let enc = try? noise.encrypt(buf.prefix(n)),
              (try? fdWriteFrame(remoteFd, enc)) != nil else { break }
    }
}

// remote (server) → decrypt → local (WebSocket client)
DispatchQueue.global(qos: .utility).async {
    defer { g.leave(); closeBoth() }
    while true {
        guard let enc = try? fdReadFrame(remoteFd),
              let dec = try? noise.decrypt(enc),
              (try? fdWriteAll(localFd, dec)) != nil else { break }
    }
}

g.wait()   // block until both directions exit
```

`fdWriteFrame` / `fdReadFrame` apply the 2-byte length prefix:

```swift
private func fdWriteFrame(_ fd: Int32, _ data: Data) throws {
    var len = UInt16(data.count).bigEndian
    try fdWriteAll(fd, Data(bytes: &len, count: 2))
    try fdWriteAll(fd, data)
}

private func fdReadFrame(_ fd: Int32) throws -> Data {
    var lenBuf = Data(count: 2)
    try lenBuf.withUnsafeMutableBytes { _ = try fdReadFully(fd, $0) }
    let len = Int(UInt16(bigEndian: lenBuf.withUnsafeBytes { $0.load(as: UInt16.self) }))
    return try fdReadFully(fd, count: len)
}
```

### 6d. `closeBoth` and fd cleanup

Both proxy threads call `closeBoth` on exit (via `defer`). The closure uses atomic
set removal to guarantee each fd is closed exactly once, even if both threads exit
simultaneously or `disconnect()` already closed one:

```swift
let closeBoth = { [weak self] in
    guard let self else { return }
    self.proxyFdsLock.lock()
    let ownLocal  = self.proxyFds.remove(localFd)  != nil
    let ownRemote = self.proxyFds.remove(remoteFd) != nil
    self.proxyFdsLock.unlock()
    if ownLocal  { Darwin.close(localFd) }
    if ownRemote { Darwin.close(remoteFd) }
}
```

---

## 7. `disconnect()`

`disconnect()` is called by the React effect both before connecting (to clear any
prior state) and in the effect cleanup (when dependencies change):

```swift
@objc func disconnect() {
    stateLock.lock()
    active = false
    connectGeneration += 1   // invalidates all in-flight connect() calls
    let fd = listenFd; listenFd = -1
    stateLock.unlock()
    if fd >= 0 { Darwin.close(fd) }   // unblocks acceptLoop's Darwin.accept()

    // Close all active proxy fds so proxy threads exit immediately.
    proxyFdsLock.lock()
    let fds = proxyFds
    proxyFds.removeAll()
    proxyFdsLock.unlock()
    fds.forEach { Darwin.close($0) }  // unblocks recv()/read() in proxy threads
}
```

Effect of closing each fd type:
- **listenFd closed** → `Darwin.accept(fd, nil, nil)` returns -1 → acceptLoop breaks
- **localFd closed** → `Darwin.recv(localFd, ...)` returns ≤0 → send thread breaks → `closeBoth()` closes remoteFd → recv thread breaks
- **remoteFd closed** → `fdReadFrame(remoteFd)` throws → recv thread breaks → `closeBoth()` closes localFd → send thread breaks

---

## 8. React Native Connection Effect

Everything in App.tsx is driven by a single `useEffect` that re-runs when `conn`,
`activeChild`, or `reconnectKey` changes:

```ts
useEffect(() => {
    setTunnelPort(null)
    setTunnelError(null)

    const target = activeChild
      ? { host: activeChild.host, port: activeChild.port, pk: activeChild.pubkey }
      : conn
      ? { host: conn.host, port: conn.port, pk: conn.pk }
      : null

    if (!target) return

    let live = true
    NoiseConnection.disconnect()                    // cancel any prior session
    NoiseConnection.connect(target.host, target.port, target.pk)
      .then(port => {
        if (!live) return                           // effect already re-ran; discard
        setTunnelPort(port)                         // triggers ChatPane to mount
      })
      .catch(e => {
        if (!live) return
        if (activeChild) setActiveChild(null)       // fall back to master
        else setTunnelError(e?.message ?? String(e))
      })

    return () => {
      live = false
      NoiseConnection?.disconnect()                 // cleanup when effect re-runs
    }
}, [conn, activeChild, reconnectKey])
```

`reconnectKey` is bumped every time the app comes to the foreground:

```ts
useEffect(() => {
    const sub = AppState.addEventListener('change', state => {
      if (state === 'active') setReconnectKey(k => k + 1)
    })
    return () => sub.remove()
}, [])
```

### What `tunnelPort` gates

`tunnelPort` is the only thing that allows the chat UI to render. Until it is set, the
app shows the connecting spinner. Once set, `ChatPane` mounts with
`baseUrl = http://127.0.0.1:{tunnelPort}` and immediately calls `loadHistory()`.

---

## 9. HTTP API via the Tunnel

Once `tunnelPort` is set, all HTTP traffic goes through the local proxy:

```
App JS                  Local proxy (Swift)        lair (axum port 8000)
  │                          │                           │
  │  GET /history            │                           │
  ├─────────────────────────►│  encrypt → frame → send   │
  │                          ├──────────────────────────►│
  │                          │        recv → decrypt     │
  │◄─────────────────────────┤◄──────────────────────────┤
  │  200 { messages: [...] } │                           │
```

### `GET /history`

```rust
async fn history_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cost = *state.last_cost_usd.lock().unwrap();
    let msgs = messages_to_history(&state.messages.lock().unwrap(), cost);
    Json(serde_json::json!({ "messages": msgs }))
}
```

Returns `{ messages: HistMsg[], is_streaming?: bool }`.
The `is_streaming` field is checked by `loadHistory()`:

```ts
if (data.is_streaming) {
    reattachStream()   // open a WebSocket to tail the running loop
} else {
    updateStatus('ready')
}
```

**Note**: lair's history response does not include `is_streaming`. That field is only
returned by the child server. When connected to lair, `loadHistory()` will always
take the `else` branch and set status to `ready`, never calling `reattachStream()`.

### `GET /stream` (WebSocket)

This is the primary interaction endpoint. Used for both sending messages and watching
an already-running loop.

**Sending a message** — the app sends `{ "text": "..." }`:

```ts
const ws = new WebSocket(wsUrl)
ws.onopen = () => ws.send(JSON.stringify({ text }))
```

The server loop in `handle_stream`:

```rust
let text = loop {
    match ws_rx.next().await {
        Some(Ok(WsMessage::Text(t))) => {
            match serde_json::from_str::<serde_json::Value>(&t)
                .ok()
                .and_then(|v| v["text"].as_str().map(str::to_string))
            {
                Some(t) => break t,     // got the text, start the agent loop
                None    => return,      // no "text" field → close connection immediately
            }
        }
        ...
    }
};
```

**Watching a running stream** — the app sends `{ "type": "watch" }`:

```ts
ws.onopen = () => ws.send(JSON.stringify({ type: 'watch' }))
```

**This is a bug.** The server's `handle_stream` loop only looks for `v["text"]`. A
message with `{ "type": "watch" }` has no `"text"` field, so `and_then(|v| v["text"]
.as_str())` returns `None`, and `handle_stream` returns immediately — closing the
WebSocket. The `reattachStream` path (called when `is_streaming` is true) silently
fails: the WebSocket opens, sends `watch`, and the server closes it without sending
any events.

### Server-side streaming events

Once the agent loop starts, events are sent as JSON text frames:

| Event | Fields |
|-------|--------|
| `text` | `{ type: "text", text: "..." }` |
| `tool_use` | `{ type: "tool_use", tool: "...", input: {...} }` |
| `tool_output` | `{ type: "tool_output", line: "..." }` |
| `tool_result` | `{ type: "tool_result", tool_use_id: "...", output: "..." }` |
| `done` | `{ type: "done", cost_usd: 0.0012 }` |
| `interrupted` | `{ type: "interrupted", cost_usd: ... }` |
| `error` | `{ type: "error", message: "..." }` |

Interrupt: client sends `{ "type": "interrupt" }` at any time; the server's listener
task sees it and sets `aborted = true`.

---

## 10. Reconnect Sequences

### App foregrounds (most common trigger)

```
AppState → 'active'
  → setReconnectKey(k+1)
    → effect cleanup: live=false, disconnect()
    → effect re-runs:
        setTunnelPort(null)      ChatPane unmounts, spinner shows
        disconnect()             kills any in-flight connect() (generation++)
        connect(host, port, pk)  probe + new listen socket
          → .then(port):
              setTunnelPort(port)  ChatPane mounts
              loadHistory()
              if is_streaming → reattachStream()
```

### New QR scan

```
handleQrScanned(raw)
  → parseQrData → { host, port, pk }
  → AsyncStorage.setItem('masterConnection', ...)
  → setConn(parsed)
    → connection effect re-runs (conn changed)
    → same sequence as above
```

### Child container opened

```
setActiveChild(child)
  → connection effect re-runs (activeChild changed)
  → target = { host: child.host, port: child.port, pk: child.pubkey }
  → connect to child's noise port
  → on success: ChildChatScreen mounts with tunnelPort
  → on failure: setActiveChild(null) → effect re-runs → falls back to master
```

---

## 11. Keypair Persistence and Identity

The server keypair is stored in `/data/noise_key.bin` (first 32 bytes = private,
last 32 bytes = public):

```rust
pub fn load_or_generate_keypair(path: &str) -> (Vec<u8>, Vec<u8>) {
    if let Ok(bytes) = std::fs::read(path) {
        if bytes.len() == 64 {
            return (bytes[..32].to_vec(), bytes[32..].to_vec());
        }
    }
    // Generate fresh keypair if file missing or corrupt
    let builder = snow::Builder::new(NOISE_PATTERN.parse().expect("valid pattern"));
    let kp = builder.generate_keypair().expect("keygen");
    let mut combined = kp.private.clone();
    combined.extend_from_slice(&kp.public);
    std::fs::write(path, &combined).ok();
    (kp.private, kp.public)
}
```

If the key file is deleted or the PVC is re-provisioned, a new keypair is generated
and the old QR codes become invalid (the client will throw `identityMismatch` at
message 2 of the handshake). This is the expected and correct behavior — it's the
server proving its identity.

In dev mode (`OCTO_DEV=1`), a fixed hardcoded keypair is used and the public key
is baked into the iOS simulator path in `App.tsx`.

Child containers inherit the parent's keypair via the `NOISE_PRIVATE_KEY` env var
(hex-encoded 64-byte combined key), so all children share the same Noise identity as
lair.

---

## 12. Known Gaps

### `watch` command not handled

`reattachStream()` sends `{ "type": "watch" }` to `/stream`, but `handle_stream` only
accepts `{ "text": "..." }` as the first message. Any other JSON causes it to return
immediately, silently closing the WebSocket. The `reattachStream()` path is therefore
broken for lair. (It may work on child servers if they have a different
`handle_stream` implementation.)

### `is_streaming` not returned by lair `/history`

`history_handler` in lair returns `{ messages }` with no `is_streaming` field.
`loadHistory()` will therefore never call `reattachStream()` even if the agent loop is
actively running. The screen won't show streaming output for a loop that started
before the app connected.

### No reconnect on tunnel drop

If the encrypted tunnel drops mid-session (network change, server restart) without
the app backgrounding, the WebSocket will error and the status will go to `error`.
The only automatic reconnect trigger is `AppState → 'active'`. The user has to
background and foreground the app to reconnect.
