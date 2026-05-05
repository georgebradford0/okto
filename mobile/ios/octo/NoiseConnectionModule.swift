import Foundation
import CryptoKit
import Darwin

// ── Noise_XX_25519_ChaChaPoly_SHA256 — iOS/Swift implementation ───────────────
//
// Implements the initiator (client) role.
// DH = X25519, AEAD = ChaCha20-Poly1305, HASH = SHA-256
//
// After handshake a NoiseTransport encrypts/decrypts transport messages.
// Each local TCP connection gets its own Noise session (no multiplexing needed).
//
// I/O uses raw BSD send()/recv() on blocking sockets throughout — CFStreamCreatePairWithSocket
// internally sets the fd to non-blocking mode which breaks synchronous reads.

// MARK: - Crypto primitives

private func sha256cat(_ a: Data, _ b: Data) -> Data {
    Data(SHA256.hash(data: a + b))
}

private func hmacSha256(key: Data, data: Data) -> Data {
    let k = SymmetricKey(data: key)
    return Data(HMAC<SHA256>.authenticationCode(for: data, using: k))
}

/// Noise HKDF — returns (ck_new, k_new)
private func noiseHKDF(ck: Data, ikm: Data) -> (Data, Data) {
    let temp = hmacSha256(key: ck, data: ikm)
    let out1 = hmacSha256(key: temp, data: Data([0x01]))
    let out2 = hmacSha256(key: temp, data: out1 + Data([0x02]))
    return (out1, out2)
}

/// Noise nonce: 4 zero bytes + 8-byte little-endian counter
private func encodeNonce(_ n: UInt64) -> Data {
    var nonce = Data(count: 12)
    var v = n
    for i in 0..<8 { nonce[4 + i] = UInt8(v & 0xff); v >>= 8 }
    return nonce
}

private enum NoiseError: Error {
    case identityMismatch
    case badFrame(String)
    case ioError
}

private func chachaEncrypt(key: Data, nonce n: UInt64, aad: Data, plain: Data) throws -> Data {
    let sym    = SymmetricKey(data: key)
    let nonceV = try ChaChaPoly.Nonce(data: encodeNonce(n))
    let sealed = try ChaChaPoly.seal(plain, using: sym, nonce: nonceV, authenticating: aad)
    return sealed.ciphertext + sealed.tag
}

private func chachaDecrypt(key: Data, nonce n: UInt64, aad: Data, ct: Data) throws -> Data {
    guard ct.count >= 16 else { throw NoiseError.badFrame("ciphertext too short") }
    let sym    = SymmetricKey(data: key)
    let nonceV = try ChaChaPoly.Nonce(data: encodeNonce(n))
    let body   = ct.prefix(ct.count - 16)
    let tag    = ct.suffix(16)
    let box    = try ChaChaPoly.SealedBox(nonce: nonceV, ciphertext: body, tag: tag)
    return try ChaChaPoly.open(box, using: sym, authenticating: aad)
}

private func x25519(priv: Curve25519.KeyAgreement.PrivateKey, pubBytes: Data) throws -> Data {
    let pub = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: pubBytes)
    let ss  = try priv.sharedSecretFromKeyAgreement(with: pub)
    return ss.withUnsafeBytes { Data($0) }
}

// MARK: - Handshake state machine

private struct HandshakeState {
    var h:  Data
    var ck: Data
    var k:  Data? = nil
    var n:  UInt64 = 0

    init(_ protoName: Data) { h = protoName; ck = protoName }

    mutating func mixHash(_ data: Data) { h = sha256cat(h, data) }

    mutating func mixKey(_ ikm: Data) {
        let (c, k2) = noiseHKDF(ck: ck, ikm: ikm)
        ck = c; k = k2; n = 0
    }

    mutating func encryptAndHash(_ plain: Data) throws -> Data {
        guard let key = k else { mixHash(plain); return plain }
        let ct = try chachaEncrypt(key: key, nonce: n, aad: h, plain: plain)
        n += 1; mixHash(ct); return ct
    }

    mutating func decryptAndHash(_ ciphertext: Data) throws -> Data {
        guard let key = k else { mixHash(ciphertext); return ciphertext }
        let plain = try chachaDecrypt(key: key, nonce: n, aad: h, ct: ciphertext)
        n += 1; mixHash(ciphertext); return plain
    }

    func split() -> (sendKey: Data, recvKey: Data) {
        let temp    = hmacSha256(key: ck, data: Data())
        let sendKey = hmacSha256(key: temp, data: Data([0x01]))
        let recvKey = hmacSha256(key: temp, data: sendKey + Data([0x02]))
        return (sendKey, recvKey)
    }
}

// MARK: - Transport

private final class NoiseTransport {
    private let sendKey: Data
    private let recvKey: Data
    private var sendN: UInt64 = 0
    private var recvN: UInt64 = 0
    private let lock = NSLock()

    init(sendKey: Data, recvKey: Data) { self.sendKey = sendKey; self.recvKey = recvKey }

    func encrypt(_ plain: Data) throws -> Data {
        lock.lock(); defer { lock.unlock() }
        let n = sendN; sendN += 1
        return try chachaEncrypt(key: sendKey, nonce: n, aad: Data(), plain: plain)
    }

    func decrypt(_ ciphertext: Data) throws -> Data {
        lock.lock(); defer { lock.unlock() }
        let n = recvN; recvN += 1
        return try chachaDecrypt(key: recvKey, nonce: n, aad: Data(), ct: ciphertext)
    }
}

// MARK: - BSD socket I/O helpers (blocking, no CFStream/NSStream)

private func fdWriteAll(_ fd: Int32, _ data: Data) throws {
    var off = 0
    while off < data.count {
        let n = data.withUnsafeBytes { ptr in
            Darwin.send(fd, ptr.baseAddress!.advanced(by: off), data.count - off, 0)
        }
        if n <= 0 {
            throw NoiseError.ioError
        }
        off += n
    }
}

private func fdReadFully(_ fd: Int32, _ count: Int) throws -> Data {
    var buf = Data(count: count)
    var off = 0
    while off < count {
        let n = buf.withUnsafeMutableBytes { ptr in
            Darwin.recv(fd, ptr.baseAddress!.advanced(by: off), count - off, 0)
        }
        if n <= 0 {
            throw NoiseError.ioError
        }
        off += n
    }
    return buf
}

/// Write a 2-byte-length-framed message.
private func fdWriteFrame(_ fd: Int32, _ data: Data) throws {
    try fdWriteAll(fd, Data([UInt8(data.count >> 8), UInt8(data.count & 0xff)]) + data)
}

/// Read a 2-byte-length-framed message.
private func fdReadFrame(_ fd: Int32) throws -> Data {
    let lenBuf = try fdReadFully(fd, 2)
    let len    = Int(lenBuf[0]) << 8 | Int(lenBuf[1])
    return try fdReadFully(fd, len)
}

// MARK: - Handshake runner

private func runHandshake(remoteFd: Int32, serverPub: Data) throws -> NoiseTransport {
    let proto = "Noise_XX_25519_ChaChaPoly_SHA256".data(using: .utf8)! // exactly 32 bytes
    var hs    = HandshakeState(proto)

    let ePriv = Curve25519.KeyAgreement.PrivateKey()
    let ePub  = Data(ePriv.publicKey.rawRepresentation)
    let sPriv = Curve25519.KeyAgreement.PrivateKey()
    let sPub  = Data(sPriv.publicKey.rawRepresentation)

    // MixHash(empty prologue) — required by Noise spec §5.6 even when prologue is empty.
    // snow (server) calls this unconditionally; without it h diverges immediately.
    hs.mixHash(Data())

    // Message 1: → e
    // Noise spec: after each token/payload, (Encrypt|Decrypt)AndHash is called.
    // For M1's empty payload with no key, this is just MixHash("").
    // snow calls encrypt_and_mix_hash("") at the end of every write_message, even
    // for the empty payload.  Without this the hash state diverges from snow's.
    hs.mixHash(ePub)
    hs.mixHash(Data())                   // M1 empty-payload: MixHash("") per Noise spec §5.2
    try fdWriteFrame(remoteFd, ePub)

    // Message 2: ← e, ee, s, es  (96 bytes)
    let msg2 = try fdReadFrame(remoteFd)
    guard msg2.count == 96 else { throw NoiseError.badFrame("msg2 length \(msg2.count)") }

    let rePub = Data(msg2.prefix(32))
    hs.mixHash(rePub)

    let ee = try x25519(priv: ePriv, pubBytes: rePub)
    hs.mixKey(ee)

    let encRs = Data(msg2[32..<80])
    let rsPub = try hs.decryptAndHash(encRs)

    guard rsPub == serverPub else { throw NoiseError.identityMismatch }

    let es = try x25519(priv: ePriv, pubBytes: rsPub)
    hs.mixKey(es)

    let encEmpty2 = Data(msg2[80...])
    _ = try hs.decryptAndHash(encEmpty2)

    // Message 3: → s, se
    let encS      = try hs.encryptAndHash(sPub)
    let se        = try x25519(priv: sPriv, pubBytes: rePub)
    hs.mixKey(se)
    let encEmpty3 = try hs.encryptAndHash(Data())
    try fdWriteFrame(remoteFd, encS + encEmpty3)

    let (sendKey, recvKey) = hs.split()
    return NoiseTransport(sendKey: sendKey, recvKey: recvKey)
}

// MARK: - BSD socket helpers

private func makeServerSocket() -> (fd: Int32, port: Int) {
    let fd = Darwin.socket(AF_INET, SOCK_STREAM, IPPROTO_TCP)
    guard fd >= 0 else { return (-1, -1) }
    var yes: Int32 = 1
    Darwin.setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &yes, socklen_t(MemoryLayout<Int32>.size))

    var addr = sockaddr_in()
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_addr   = in_addr(s_addr: 0x0100007f) // 127.0.0.1 in host byte order on LE
    addr.sin_port   = 0

    let bindOK = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
        }
    }
    guard bindOK == 0 else { Darwin.close(fd); return (-1, -1) }
    guard Darwin.listen(fd, 50) == 0 else { Darwin.close(fd); return (-1, -1) }

    var out = sockaddr_in()
    var outLen = socklen_t(MemoryLayout<sockaddr_in>.size)
    withUnsafeMutablePointer(to: &out) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            Darwin.getsockname(fd, $0, &outLen)
        }
    }
    return (fd, Int(CFSwapInt16BigToHost(out.sin_port)))
}

/// TCP connect with a wall-clock timeout via non-blocking connect + select().
/// Returns a connected, blocking-mode fd, or -1 on failure/timeout.
private func connectSocket(host: String, port: Int, timeoutSecs: Int = 15) -> Int32 {
    let fd = Darwin.socket(AF_INET, SOCK_STREAM, IPPROTO_TCP)
    guard fd >= 0 else { return -1 }
    var yes: Int32 = 1
    Darwin.setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &yes, socklen_t(MemoryLayout<Int32>.size))

    var addr = sockaddr_in()
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_port   = CFSwapInt16HostToBig(UInt16(port))
    guard inet_pton(AF_INET, host, &addr.sin_addr) == 1 else { Darwin.close(fd); return -1 }

    // Set non-blocking for the connect call so we can apply our own timeout.
    let flags = fcntl(fd, F_GETFL, 0)
    fcntl(fd, F_SETFL, flags | O_NONBLOCK)

    let result = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
        }
    }

    if result == 0 {
        // Immediate connect (e.g. loopback) — restore blocking and return.
        fcntl(fd, F_SETFL, flags)
        return fd
    }
    guard errno == EINPROGRESS else { Darwin.close(fd); return -1 }

    // Wait for writability (connection complete) with timeout.
    var writeFds = fd_set(); var errFds = fd_set()
    withUnsafeMutablePointer(to: &writeFds) { __darwin_fd_set(fd, $0) }
    withUnsafeMutablePointer(to: &errFds)   { __darwin_fd_set(fd, $0) }
    var tv = timeval(tv_sec: timeoutSecs, tv_usec: 0)
    let n = withUnsafeMutablePointer(to: &writeFds) { wPtr in
        withUnsafeMutablePointer(to: &errFds) { ePtr in
            select(fd + 1, nil, wPtr, ePtr, &tv)
        }
    }
    guard n > 0 else { Darwin.close(fd); return -1 }  // timeout or select error

    // Check whether connect succeeded.
    var optErr: Int32 = 0; var optLen = socklen_t(MemoryLayout<Int32>.size)
    getsockopt(fd, SOL_SOCKET, SO_ERROR, &optErr, &optLen)
    guard optErr == 0 else { Darwin.close(fd); return -1 }

    fcntl(fd, F_SETFL, flags)  // restore blocking mode
    return fd
}

// MARK: - React Native Module

@objc(NoiseConnection)
final class NoiseConnection: NSObject {

    private var listenFd: Int32 = -1
    private let stateLock  = NSLock()
    private var active     = false
    // Monotonically incremented by disconnect(). Each connect() captures its
    // value at call time; any in-flight probe or setup that sees a stale
    // generation aborts without touching shared state. This prevents a slow
    // probe from a superseded connect() call from clobbering the listen socket
    // that a subsequent connect() already installed.
    private var connectGeneration: Int = 0

    // Tracks all fds held by active proxyConnection threads so disconnect()
    // can close them immediately, preventing fd recycling races.
    private var proxyFds = Set<Int32>()
    private let proxyFdsLock = NSLock()

    @objc static func requiresMainQueueSetup() -> Bool { false }

    @objc func constantsToExport() -> [AnyHashable: Any]! {
        #if targetEnvironment(simulator)
        return ["isSimulator": true]
        #else
        return ["isSimulator": false]
        #endif
    }

    @objc func connect(
        _ host: String,
        port: Double,
        serverPubKey: String,
        resolve: @escaping (Any?) -> Void,
        reject:  @escaping (String?, String?, Error?) -> Void
    ) {
        // Snapshot generation before going async. A disconnect() that fires while
        // the probe is in flight will increment connectGeneration, causing the
        // stale connect() to abort at each checkpoint below.
        stateLock.lock()
        let myGen = connectGeneration
        stateLock.unlock()

        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            guard let self else { return }
            do {
                guard let pk = base32Decode(serverPubKey.uppercased().trimmingCharacters(in: .whitespaces)),
                      pk.count == 32 else {
                    reject("NOISE_ERROR", "serverPubKey must be a 52-char base32 Curve25519 key", nil)
                    return
                }
                // Probe: verify the remote host is actually reachable before resolving.
                // connectSocket uses a non-blocking connect with a 15s timeout, so if
                // a carrier firewall silently drops the packets this rejects visibly in
                // JS (via the .catch handler) instead of hanging indefinitely.
                print("[noise-proxy] probing \(host):\(Int(port))…")
                let probeFd = connectSocket(host: host, port: Int(port))
                guard probeFd >= 0 else {
                    reject("NOISE_CONNECT_ERROR", "Cannot reach \(host):\(Int(port)) — check firewall / cellular data (errno \(errno))", nil)
                    return
                }
                // Probe succeeded; close this fd — the accept loop will open real connections.
                Darwin.close(probeFd)
                print("[noise-proxy] probe OK, host is reachable")

                // Generation check after probe: a disconnect() may have fired while
                // we were blocked in connectSocket(). Abort rather than installing a
                // listen socket that would clobber the one a newer connect() already set.
                self.stateLock.lock()
                guard self.connectGeneration == myGen else {
                    self.stateLock.unlock()
                    print("[noise-proxy] connect() cancelled after probe — superseded by disconnect()")
                    reject("NOISE_CONNECT_ERROR", "Connection superseded", nil)
                    return
                }
                self.stateLock.unlock()

                let (fd, localPort) = makeServerSocket()
                guard fd >= 0, localPort > 0 else { throw NoiseError.ioError }

                // Final generation check under lock before installing the listen socket.
                self.stateLock.lock()
                guard self.connectGeneration == myGen else {
                    self.stateLock.unlock()
                    Darwin.close(fd)
                    print("[noise-proxy] connect() cancelled before install — superseded by disconnect()")
                    reject("NOISE_CONNECT_ERROR", "Connection superseded", nil)
                    return
                }
                let oldFd = self.listenFd
                self.listenFd = fd
                self.active   = true
                self.stateLock.unlock()

                // Close the old listen socket (if any) after installing the new one.
                if oldFd >= 0 { Darwin.close(oldFd) }

                DispatchQueue.global(qos: .utility).async { [weak self] in
                    self?.acceptLoop(fd: fd, host: host, port: Int(port), serverPub: pk, generation: myGen)
                }

                resolve(localPort)
            } catch {
                reject("NOISE_CONNECT_ERROR", error.localizedDescription, error as NSError)
            }
        }
    }

    @objc func disconnect() {
        stateLock.lock()
        active = false
        connectGeneration += 1   // invalidates any in-flight connect() calls
        let fd = listenFd; listenFd = -1
        stateLock.unlock()
        if fd >= 0 { Darwin.close(fd) }

        // Close all active proxy fds so their threads exit immediately and
        // the OS cannot recycle those fd numbers for new connections.
        proxyFdsLock.lock()
        let fds = proxyFds
        proxyFds.removeAll()
        proxyFdsLock.unlock()
        fds.forEach { Darwin.close($0) }
    }

    private func acceptLoop(fd: Int32, host: String, port: Int, serverPub: Data, generation: Int) {
        while true {
            stateLock.lock()
            let alive = active && connectGeneration == generation
            stateLock.unlock()
            guard alive else { break }
            let clientFd = Darwin.accept(fd, nil, nil)
            if clientFd < 0 { break }
            DispatchQueue.global(qos: .utility).async {
                self.proxyConnection(localFd: clientFd, host: host, port: port, serverPub: serverPub, generation: generation)
            }
        }
    }

    private func proxyConnection(localFd: Int32, host: String, port: Int, serverPub: Data, generation: Int) {
        // Register localFd immediately so disconnect() can close it even while we
        // are blocked inside connectSocket(). Without this, a disconnect() that fires
        // between accept() and the insert below would leave localFd open and let this
        // closure run against a stale session.
        proxyFdsLock.lock(); proxyFds.insert(localFd); proxyFdsLock.unlock()

        print("[noise-proxy] local client accepted; connecting to \(host):\(port)…")
        let remoteFd = connectSocket(host: host, port: port)
        guard remoteFd >= 0 else {
            print("[noise-proxy] connectSocket to \(host):\(port) FAILED (errno=\(errno))")
            proxyFdsLock.lock()
            let owned = proxyFds.remove(localFd) != nil
            proxyFdsLock.unlock()
            if owned { Darwin.close(localFd) }
            return
        }

        // Check generation after connectSocket — disconnect() may have closed localFd
        // while we were blocked. Abort without touching any new-session state.
        stateLock.lock()
        let stillCurrent = connectGeneration == generation
        stateLock.unlock()
        guard stillCurrent else {
            Darwin.close(remoteFd)   // remoteFd was never in proxyFds; close it directly
            print("[noise-proxy] proxyConnection cancelled after connectSocket — stale generation")
            return
        }

        proxyFdsLock.lock(); proxyFds.insert(remoteFd); proxyFdsLock.unlock()

        // Close both fds, but only if disconnect() hasn't already done so.
        // disconnect() removes fds from proxyFds before closing them, so if
        // remove() returns nil the fd was already closed — don't close again
        // or the OS may have recycled that number for a new server socket.
        // Called by each proxy direction thread when it exits so the other
        // thread gets unblocked immediately (no g.wait() deadlock).
        let closeBoth = { [weak self] in
            guard let self else { return }
            self.proxyFdsLock.lock()
            let ownLocal  = self.proxyFds.remove(localFd)  != nil
            let ownRemote = self.proxyFds.remove(remoteFd) != nil
            self.proxyFdsLock.unlock()
            if ownLocal  { Darwin.close(localFd) }
            if ownRemote { Darwin.close(remoteFd) }
        }

        print("[noise-proxy] TCP connected to \(host):\(port); starting handshake…")

        do {
            let noise = try runHandshake(remoteFd: remoteFd, serverPub: serverPub)
            print("[noise-proxy] handshake complete; proxying data")
            let g = DispatchGroup()

            // local → encrypt → remote
            g.enter()
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

            // remote → decrypt → local
            g.enter()
            DispatchQueue.global(qos: .utility).async {
                defer { g.leave(); closeBoth() }
                while true {
                    guard let enc = try? fdReadFrame(remoteFd),
                          let dec = try? noise.decrypt(enc),
                          (try? fdWriteAll(localFd, dec)) != nil else { break }
                }
            }

            g.wait()
        } catch {
            print("[noise-proxy] handshake/proxy error: \(error)")
            closeBoth()
        }
    }
}

// MARK: - Base32 decode (no padding, uppercase)

private func base32Decode(_ s: String) -> Data? {
    let alpha = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
    var buf: UInt32 = 0; var bits = 0
    var out = Data()
    for ch in s {
        guard let idx = alpha.firstIndex(of: ch) else { continue }
        buf = (buf << 5) | UInt32(alpha.distance(from: alpha.startIndex, to: idx))
        bits += 5
        if bits >= 8 { bits -= 8; out.append(UInt8((buf >> bits) & 0xff)) }
    }
    return out
}
