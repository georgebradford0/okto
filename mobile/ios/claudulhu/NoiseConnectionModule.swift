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

// MARK: - Handshake runner

private func runHandshake(remoteIn: InputStream, remoteOut: OutputStream, serverPub: Data) throws -> NoiseTransport {
    let proto = "Noise_XX_25519_ChaChaPoly_SHA256".data(using: .utf8)! // exactly 32 bytes
    var hs    = HandshakeState(proto)

    let ePriv = Curve25519.KeyAgreement.PrivateKey()
    let ePub  = Data(ePriv.publicKey.rawRepresentation)
    let sPriv = Curve25519.KeyAgreement.PrivateKey()
    let sPub  = Data(sPriv.publicKey.rawRepresentation)

    // Message 1: → e
    hs.mixHash(ePub)
    try ioWriteFrame(remoteOut, ePub)

    // Message 2: ← e, ee, s, es  (96 bytes)
    let msg2 = try ioReadFrame(remoteIn)
    guard msg2.count == 96 else { throw NoiseError.badFrame("msg2 length \(msg2.count)") }

    let rePub = msg2.prefix(32)
    hs.mixHash(Data(rePub))

    let ee = try x25519(priv: ePriv, pubBytes: Data(rePub))
    hs.mixKey(ee)

    let encRs = Data(msg2[32..<80])
    let rsPub = try hs.decryptAndHash(encRs)

    guard rsPub == serverPub else { throw NoiseError.identityMismatch }

    let es = try x25519(priv: ePriv, pubBytes: rsPub)
    hs.mixKey(es)

    let encEmpty2 = Data(msg2[80...])
    _ = try hs.decryptAndHash(encEmpty2)

    // Message 3: → s, se
    let encS     = try hs.encryptAndHash(sPub)
    let se       = try x25519(priv: sPriv, pubBytes: Data(rePub))
    hs.mixKey(se)
    let encEmpty3 = try hs.encryptAndHash(Data())
    try ioWriteFrame(remoteOut, encS + encEmpty3)

    let (sendKey, recvKey) = hs.split()
    return NoiseTransport(sendKey: sendKey, recvKey: recvKey)
}

// MARK: - Stream I/O helpers (NSStream over BSD fd)

private func ioWriteFrame(_ s: OutputStream, _ data: Data) throws {
    var packet = Data([UInt8(data.count >> 8), UInt8(data.count & 0xff)]) + data
    var off = 0
    while off < packet.count {
        let n = packet.withUnsafeBytes { ptr in
            s.write(ptr.baseAddress!.advanced(by: off).assumingMemoryBound(to: UInt8.self),
                    maxLength: packet.count - off)
        }
        guard n > 0 else { throw NoiseError.ioError }
        off += n
    }
}

private func ioReadFrame(_ s: InputStream) throws -> Data {
    var lenBuf = Data(count: 2)
    try ioReadFully(s, &lenBuf, 2)
    let len = Int(lenBuf[0]) << 8 | Int(lenBuf[1])
    var buf = Data(count: len)
    try ioReadFully(s, &buf, len)
    return buf
}

private func ioReadFully(_ s: InputStream, _ buf: inout Data, _ count: Int) throws {
    var off = 0
    while off < count {
        let n = buf.withUnsafeMutableBytes { ptr in
            s.read(ptr.baseAddress!.advanced(by: off).assumingMemoryBound(to: UInt8.self),
                   maxLength: count - off)
        }
        guard n > 0 else { throw NoiseError.ioError }
        off += n
    }
}

private func ioWriteAll(_ s: OutputStream, _ data: Data) throws {
    var off = 0
    while off < data.count {
        let n = data.withUnsafeBytes { ptr in
            s.write(ptr.baseAddress!.advanced(by: off).assumingMemoryBound(to: UInt8.self),
                    maxLength: data.count - off)
        }
        guard n > 0 else { throw NoiseError.ioError }
        off += n
    }
}

// MARK: - BSD socket helpers

private func makeServerSocket() -> (fd: Int32, port: Int) {
    let fd = Darwin.socket(AF_INET, SOCK_STREAM, IPPROTO_TCP)
    guard fd >= 0 else { return (-1, -1) }
    var yes: Int32 = 1
    Darwin.setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &yes, socklen_t(MemoryLayout<Int32>.size))

    var addr = sockaddr_in()
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_addr   = in_addr(s_addr: 0x0100007f) // 127.0.0.1 big-endian
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

private func connectSocket(host: String, port: Int) -> Int32 {
    let fd = Darwin.socket(AF_INET, SOCK_STREAM, IPPROTO_TCP)
    guard fd >= 0 else { return -1 }
    var yes: Int32 = 1
    Darwin.setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &yes, socklen_t(MemoryLayout<Int32>.size))

    var addr = sockaddr_in()
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_port   = CFSwapInt16HostToBig(UInt16(port))
    guard inet_pton(AF_INET, host, &addr.sin_addr) == 1 else { Darwin.close(fd); return -1 }

    let ok = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
        }
    }
    guard ok == 0 else { Darwin.close(fd); return -1 }
    return fd
}

private func streams(for fd: Int32) -> (InputStream, OutputStream)? {
    var rs: Unmanaged<CFReadStream>?
    var ws: Unmanaged<CFWriteStream>?
    CFStreamCreatePairWithSocket(kCFAllocatorDefault, CFSocketNativeHandle(fd), &rs, &ws)
    guard let rsRetained = rs, let wsRetained = ws else { return nil }
    // Set close-native-socket on one stream so the fd is freed when both streams close.
    CFReadStreamSetProperty(rsRetained.takeUnretainedValue(),
                            CFStreamPropertyKey(kCFStreamPropertyShouldCloseNativeSocket), kCFBooleanTrue)
    let ins  = rsRetained.takeRetainedValue() as InputStream
    let outs = wsRetained.takeRetainedValue() as OutputStream
    ins.open(); outs.open()
    return (ins, outs)
}

// MARK: - React Native Module

@objc(NoiseConnection)
final class NoiseConnection: NSObject {

    private var listenFd: Int32 = -1
    private var remoteHost = ""
    private var remotePort = 0
    private var remotePub  = Data()
    private let stateLock  = NSLock()
    private var active     = false

    @objc static func requiresMainQueueSetup() -> Bool { false }

    @objc func connect(
        _ host: String,
        port: Double,
        serverPubKey: String,
        resolve: @escaping (Any?) -> Void,
        reject:  @escaping (String?, String?, Error?) -> Void
    ) {
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            guard let self else { return }
            do {
                guard let pk = base32Decode(serverPubKey.uppercased().trimmingCharacters(in: .whitespaces)),
                      pk.count == 32 else {
                    reject("NOISE_ERROR", "serverPubKey must be a 52-char base32 Curve25519 key", nil)
                    return
                }
                self.remoteHost = host
                self.remotePort = Int(port)
                self.remotePub  = pk

                let (fd, localPort) = makeServerSocket()
                guard fd >= 0, localPort > 0 else { throw NoiseError.ioError }

                self.stateLock.lock()
                self.listenFd = fd
                self.active   = true
                self.stateLock.unlock()

                DispatchQueue.global(qos: .utility).async { [weak self] in self?.acceptLoop(fd: fd) }

                resolve(localPort)
            } catch {
                reject("NOISE_CONNECT_ERROR", error.localizedDescription, error as NSError)
            }
        }
    }

    @objc func disconnect() {
        stateLock.lock()
        active = false
        let fd = listenFd; listenFd = -1
        stateLock.unlock()
        if fd >= 0 { Darwin.close(fd) }
    }

    private func acceptLoop(fd: Int32) {
        while true {
            stateLock.lock(); let alive = active; stateLock.unlock()
            guard alive else { break }
            let clientFd = Darwin.accept(fd, nil, nil)
            if clientFd < 0 { break }
            let h = remoteHost; let p = remotePort; let pk = remotePub
            DispatchQueue.global(qos: .utility).async {
                self.proxyConnection(localFd: clientFd, host: h, port: p, serverPub: pk)
            }
        }
    }

    private func proxyConnection(localFd: Int32, host: String, port: Int, serverPub: Data) {
        guard let (localIn, localOut) = streams(for: localFd) else {
            Darwin.close(localFd); return
        }
        let remoteFd = connectSocket(host: host, port: port)
        guard remoteFd >= 0, let (remoteIn, remoteOut) = streams(for: remoteFd) else {
            localIn.close(); localOut.close(); Darwin.close(localFd); return
        }

        do {
            let noise = try runHandshake(remoteIn: remoteIn, remoteOut: remoteOut, serverPub: serverPub)
            let g = DispatchGroup()

            // local → encrypt → remote
            g.enter()
            DispatchQueue.global(qos: .utility).async {
                defer { g.leave(); remoteOut.close() }
                var buf = Data(count: 65000)
                while true {
                    let n = buf.withUnsafeMutableBytes {
                        localIn.read($0.baseAddress!.assumingMemoryBound(to: UInt8.self), maxLength: 65000)
                    }
                    guard n > 0,
                          let enc = try? noise.encrypt(buf.prefix(n)),
                          (try? ioWriteFrame(remoteOut, enc)) != nil else { break }
                }
            }

            // remote → decrypt → local
            g.enter()
            DispatchQueue.global(qos: .utility).async {
                defer { g.leave(); localOut.close() }
                while true {
                    guard let enc = try? ioReadFrame(remoteIn),
                          let dec = try? noise.decrypt(enc),
                          (try? ioWriteAll(localOut, dec)) != nil else { break }
                }
            }

            g.wait()
        } catch {
            NSLog("[NoiseConnection] session error: \(error)")
        }
        localIn.close(); localOut.close()
        remoteIn.close(); remoteOut.close()
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
