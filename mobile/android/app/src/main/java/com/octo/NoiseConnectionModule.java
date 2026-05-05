package com.octo;

import android.util.Log;

import androidx.annotation.NonNull;
import androidx.annotation.Nullable;

import com.facebook.react.bridge.Promise;
import com.facebook.react.bridge.ReactApplicationContext;
import com.facebook.react.module.annotations.ReactModule;

import org.bouncycastle.crypto.agreement.X25519Agreement;
import org.bouncycastle.crypto.generators.X25519KeyPairGenerator;
import org.bouncycastle.crypto.modes.ChaCha20Poly1305;
import org.bouncycastle.crypto.params.AEADParameters;
import org.bouncycastle.crypto.params.KeyParameter;
import org.bouncycastle.crypto.params.X25519KeyGenerationParameters;
import org.bouncycastle.crypto.params.X25519PrivateKeyParameters;
import org.bouncycastle.crypto.params.X25519PublicKeyParameters;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicBoolean;

import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;

/**
 * React Native TurboModule that establishes a Noise_XX_25519_ChaChaPoly_SHA256
 * secure channel and exposes it as a local TCP port (mirroring the SSH tunnel
 * API so the rest of the app is unchanged).
 *
 * For each incoming local connection a fresh Noise session is opened to the
 * server, so WebSocket and HTTP connections each get their own independently
 * authenticated, forward-secret channel.
 *
 * Dependencies (already present):
 *   org.bouncycastle:bcprov-jdk15on:1.70
 */
@ReactModule(name = NoiseConnectionModule.NAME)
public class NoiseConnectionModule extends NativeNoiseConnectionSpec {

    public static final String NAME = "NoiseConnection";
    private static final String TAG = "NoiseConn";

    // Noise_XX_25519_ChaChaPoly_SHA256 — protocol name is exactly 32 bytes.
    private static final byte[] PROTOCOL_NAME =
            "Noise_XX_25519_ChaChaPoly_SHA256".getBytes(java.nio.charset.StandardCharsets.UTF_8);

    @Nullable private volatile ServerSocket localServer = null;

    private final AtomicBoolean running = new AtomicBoolean(false);

    // Tracks all sockets held by active proxy threads so disconnect() can close
    // them immediately, preventing fd recycling races on the JVM layer.
    private final java.util.Set<Socket> proxySockets =
            java.util.Collections.synchronizedSet(new java.util.HashSet<>());

    public NoiseConnectionModule(ReactApplicationContext ctx) { super(ctx); }

    @Override @NonNull public String getName() { return NAME; }

    // ─── TurboModule API ──────────────────────────────────────────────────────

    @Override
    public void connect(String host, double port, String serverPubKeyB32, Promise promise) {
        new Thread(() -> {
            try {
                byte[] pk = base32Decode(serverPubKeyB32.toUpperCase().trim());
                if (pk == null || pk.length != 32) {
                    promise.reject("NOISE_ERROR", "serverPubKey must be a 52-char base32 Curve25519 key");
                    return;
                }

                ServerSocket ss = new ServerSocket(0, 50, InetAddress.getLoopbackAddress());
                // Install the new server socket and close the old one atomically so
                // the previous acceptLoop unblocks and exits without a separate disconnect().
                ServerSocket old = localServer;
                localServer = ss;
                running.set(true);
                if (old != null) { try { old.close(); } catch (IOException ignored) {} }

                new Thread(() -> acceptLoop(ss, host, (int) port, pk), "NoiseAccept").start();

                promise.resolve(ss.getLocalPort());
            } catch (Throwable t) {
                Log.e(TAG, "connect failed", t);
                promise.reject("NOISE_CONNECT_ERROR", t.getMessage(), new Exception(t));
            }
        }, "NoiseConnect").start();
    }

    @Override
    public void disconnect() {
        running.set(false);
        ServerSocket ss = localServer;
        localServer = null;
        if (ss != null) {
            try { ss.close(); } catch (IOException ignored) {}
        }
        // Close all active proxy sockets so their threads exit immediately and
        // the OS cannot recycle those ports/fds for new connections.
        for (Socket s : proxySockets) { closeQuietly(s); }
        proxySockets.clear();
    }

    // ─── Accept loop ──────────────────────────────────────────────────────────

    private void acceptLoop(ServerSocket ss, String host, int port, byte[] serverPub) {
        while (running.get()) {
            try {
                Socket local = ss.accept();
                new Thread(() -> proxy(local, host, port, serverPub), "NoiseProxy").start();
            } catch (IOException e) {
                if (running.get()) Log.e(TAG, "accept error", e);
                break;
            }
        }
    }

    // ─── Per-connection proxy ─────────────────────────────────────────────────

    private void proxy(Socket local, String host, int port, byte[] serverPub) {
        proxySockets.add(local);
        Socket remote = null;
        try {
            remote = new Socket(host, port);
            proxySockets.add(remote);
            remote.setTcpNoDelay(true);

            InputStream  ris = remote.getInputStream();
            OutputStream ros = remote.getOutputStream();

            NoiseTransport noise = handshake(ris, ros, serverPub);

            // After handshake the two directions are independent; use separate threads.
            final Socket   rFinal = remote;
            final Socket   lFinal = local;
            final NoiseTransport nFinal = noise;

            Thread send = new Thread(() -> {
                try {
                    InputStream lis = lFinal.getInputStream();
                    byte[] plain = new byte[65000];
                    int n;
                    while ((n = lis.read(plain)) > 0) {
                        byte[] enc = nFinal.encrypt(plain, n);
                        ros.write(int16Be(enc.length));
                        ros.write(enc);
                        ros.flush();
                    }
                } catch (Exception e) {
                    Log.d(TAG, "send done: " + e.getMessage());
                } finally {
                    closeQuietly(rFinal);
                }
            }, "NoiseSend");

            Thread recv = new Thread(() -> {
                try {
                    OutputStream los = lFinal.getOutputStream();
                    byte[] lenBuf = new byte[2];
                    while (true) {
                        readFully(ris, lenBuf, 2);
                        int len = ((lenBuf[0] & 0xff) << 8) | (lenBuf[1] & 0xff);
                        byte[] enc = new byte[len];
                        readFully(ris, enc, len);
                        byte[] dec = nFinal.decrypt(enc);
                        los.write(dec);
                        los.flush();
                    }
                } catch (Exception e) {
                    Log.d(TAG, "recv done: " + e.getMessage());
                } finally {
                    closeQuietly(lFinal);
                }
            }, "NoiseRecv");

            send.start();
            recv.start();
            send.join();
            recv.join();

        } catch (Exception e) {
            Log.e(TAG, "proxy error", e);
        } finally {
            proxySockets.remove(local);
            proxySockets.remove(remote);
            closeQuietly(local);
            closeQuietly(remote);
        }
    }

    // ─── Noise_XX Handshake (Initiator) ──────────────────────────────────────

    private NoiseTransport handshake(InputStream ris, OutputStream ros, byte[] serverPub) throws Exception {
        // State
        byte[] h  = Arrays.copyOf(PROTOCOL_NAME, 32); // exactly 32 bytes
        byte[] ck = Arrays.copyOf(h, 32);
        // MixHash(empty prologue) — required by Noise spec §5.6 even when prologue is empty.
        // snow (server) calls this unconditionally; without it h diverges immediately.
        h = mixHash(h, new byte[0]);
        byte[] k  = null;
        long   n  = 0;

        // Generate ephemeral keypair
        X25519KeyPairGenerator gen = new X25519KeyPairGenerator();
        gen.init(new X25519KeyGenerationParameters(new SecureRandom()));
        org.bouncycastle.crypto.AsymmetricCipherKeyPair ePair = gen.generateKeyPair();
        byte[] ePriv = ((X25519PrivateKeyParameters) ePair.getPrivate()).getEncoded();
        byte[] ePub  = ((X25519PublicKeyParameters)  ePair.getPublic()).getEncoded();

        // Static keypair (fresh per session — server sees our pubkey but we don't enforce TOFU yet)
        gen.init(new X25519KeyGenerationParameters(new SecureRandom()));
        org.bouncycastle.crypto.AsymmetricCipherKeyPair sPair = gen.generateKeyPair();
        byte[] sPriv = ((X25519PrivateKeyParameters) sPair.getPrivate()).getEncoded();
        byte[] sPub  = ((X25519PublicKeyParameters)  sPair.getPublic()).getEncoded();

        // ── Message 1: → e ────────────────────────────────────────────────────
        h = mixHash(h, ePub);
        writeFrame(ros, ePub);

        // ── Message 2: ← e, ee, s, es ─────────────────────────────────────────
        // Payload = rePub(32) || encRs(48) || encEmpty(16) = 96 bytes
        byte[] msg2 = readFrame(ris);
        if (msg2.length != 96) throw new Exception("msg2 unexpected length: " + msg2.length);

        byte[] rePub = slice(msg2, 0, 32);
        h = mixHash(h, rePub);

        byte[] ee = dh(ePriv, rePub);
        byte[][] mk = mixKey(ck, k, ee);
        ck = mk[0]; k = mk[1]; n = 0;

        byte[] encRs = slice(msg2, 32, 48);
        byte[] rsPub = chachaDecrypt(k, n++, h, encRs);
        h = mixHash(h, encRs);

        // Verify the decrypted server static pubkey against what was in the QR
        if (!Arrays.equals(rsPub, serverPub)) {
            throw new Exception("Server identity mismatch — wrong server or MITM attack");
        }

        byte[] es = dh(ePriv, rsPub);
        mk = mixKey(ck, k, es);
        ck = mk[0]; k = mk[1]; n = 0;

        byte[] encEmpty2 = slice(msg2, 80, 16);
        chachaDecrypt(k, n++, h, encEmpty2); // MAC-only check on empty payload
        h = mixHash(h, encEmpty2);

        // ── Message 3: → s, se ────────────────────────────────────────────────
        byte[] encS = chachaEncrypt(k, n++, h, sPub);
        h = mixHash(h, encS);

        byte[] se = dh(sPriv, rePub);
        mk = mixKey(ck, k, se);
        ck = mk[0]; k = mk[1]; n = 0;

        byte[] encEmpty3 = chachaEncrypt(k, n++, h, new byte[0]);
        h = mixHash(h, encEmpty3);

        writeFrame(ros, concat(encS, encEmpty3));

        // ── Split ─────────────────────────────────────────────────────────────
        byte[] tempKey  = hmacSha256(ck, new byte[0]);
        byte[] sendKey  = hmacSha256(tempKey, new byte[]{1});
        byte[] recvKey  = hmacSha256(tempKey, concat(sendKey, new byte[]{2}));

        return new NoiseTransport(sendKey, recvKey);
    }

    // ─── NoiseTransport ───────────────────────────────────────────────────────

    private static final class NoiseTransport {
        private final byte[] sendKey;
        private final byte[] recvKey;
        private long sendN = 0;
        private long recvN = 0;

        NoiseTransport(byte[] sendKey, byte[] recvKey) {
            this.sendKey = sendKey;
            this.recvKey = recvKey;
        }

        synchronized byte[] encrypt(byte[] plain, int len) throws Exception {
            return chachaEncrypt(sendKey, sendN++, EMPTY, Arrays.copyOf(plain, len));
        }

        synchronized byte[] decrypt(byte[] ciphertext) throws Exception {
            return chachaDecrypt(recvKey, recvN++, EMPTY, ciphertext);
        }

        private static final byte[] EMPTY = new byte[0];
    }

    // ─── Noise primitives ─────────────────────────────────────────────────────

    private static byte[] mixHash(byte[] h, byte[] data) throws Exception {
        MessageDigest md = MessageDigest.getInstance("SHA-256");
        md.update(h);
        md.update(data);
        return md.digest();
    }

    /** Returns [ck_new, k_new]; sets n = 0 implicitly (caller resets n). */
    private static byte[][] mixKey(byte[] ck, @Nullable byte[] ignored, byte[] ikm) throws Exception {
        byte[] tempKey = hmacSha256(ck, ikm);
        byte[] ckNew   = hmacSha256(tempKey, new byte[]{1});
        byte[] kNew    = hmacSha256(tempKey, concat(ckNew, new byte[]{2}));
        return new byte[][]{ckNew, kNew};
    }

    private static byte[] chachaEncrypt(byte[] key, long nonce, byte[] aad, byte[] plain)
            throws Exception {
        ChaCha20Poly1305 cipher = new ChaCha20Poly1305();
        cipher.init(true, new AEADParameters(new KeyParameter(key), 128, encodeNonce(nonce), aad));
        byte[] out = new byte[cipher.getOutputSize(plain.length)];
        int off = cipher.processBytes(plain, 0, plain.length, out, 0);
        cipher.doFinal(out, off);
        return out;
    }

    private static byte[] chachaDecrypt(byte[] key, long nonce, byte[] aad, byte[] ciphertext)
            throws Exception {
        ChaCha20Poly1305 cipher = new ChaCha20Poly1305();
        cipher.init(false, new AEADParameters(new KeyParameter(key), 128, encodeNonce(nonce), aad));
        byte[] out = new byte[cipher.getOutputSize(ciphertext.length)];
        int off = cipher.processBytes(ciphertext, 0, ciphertext.length, out, 0);
        cipher.doFinal(out, off);
        return out;
    }

    /** Noise nonce encoding: 4 zero bytes || 8-byte little-endian counter = 12 bytes */
    private static byte[] encodeNonce(long n) {
        byte[] nonce = new byte[12];
        // bytes 0-3 are zero; bytes 4-11 are LE uint64
        for (int i = 0; i < 8; i++) {
            nonce[4 + i] = (byte) (n & 0xff);
            n >>= 8;
        }
        return nonce;
    }

    private static byte[] dh(byte[] privBytes, byte[] pubBytes) {
        X25519PrivateKeyParameters priv = new X25519PrivateKeyParameters(privBytes, 0);
        X25519PublicKeyParameters  pub  = new X25519PublicKeyParameters(pubBytes,  0);
        X25519Agreement agreement = new X25519Agreement();
        agreement.init(priv);
        byte[] secret = new byte[32];
        agreement.calculateAgreement(pub, secret, 0);
        return secret;
    }

    private static byte[] hmacSha256(byte[] key, byte[] data) throws Exception {
        Mac mac = Mac.getInstance("HmacSHA256");
        mac.init(new SecretKeySpec(key, "HmacSHA256"));
        mac.update(data);
        return mac.doFinal();
    }

    // ─── I/O helpers ──────────────────────────────────────────────────────────

    private static void writeFrame(OutputStream os, byte[] data) throws IOException {
        os.write(int16Be(data.length));
        os.write(data);
        os.flush();
    }

    private static byte[] readFrame(InputStream is) throws IOException {
        byte[] lenBuf = new byte[2];
        readFully(is, lenBuf, 2);
        int len = ((lenBuf[0] & 0xff) << 8) | (lenBuf[1] & 0xff);
        byte[] buf = new byte[len];
        readFully(is, buf, len);
        return buf;
    }

    private static void readFully(InputStream is, byte[] buf, int len) throws IOException {
        int off = 0;
        while (off < len) {
            int n = is.read(buf, off, len - off);
            if (n < 0) throw new IOException("stream closed");
            off += n;
        }
    }

    private static byte[] int16Be(int v) {
        return new byte[]{(byte) (v >> 8), (byte) (v & 0xff)};
    }

    private static byte[] concat(byte[] a, byte[] b) {
        byte[] out = new byte[a.length + b.length];
        System.arraycopy(a, 0, out, 0, a.length);
        System.arraycopy(b, 0, out, a.length, b.length);
        return out;
    }

    private static byte[] slice(byte[] src, int off, int len) {
        return Arrays.copyOfRange(src, off, off + len);
    }

    private static void closeQuietly(java.io.Closeable c) {
        if (c != null) try { c.close(); } catch (IOException ignored) {}
    }

    // ─── Base32 decode (uppercase, no padding) ────────────────────────────────

    private static final String BASE32_ALPHA = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

    @Nullable
    private static byte[] base32Decode(String s) {
        byte[] out = new byte[s.length() * 5 / 8];
        int buf = 0, bits = 0, idx = 0;
        for (char c : s.toCharArray()) {
            int v = BASE32_ALPHA.indexOf(c);
            if (v < 0) continue;
            buf = (buf << 5) | v;
            bits += 5;
            if (bits >= 8) {
                bits -= 8;
                if (idx >= out.length) return null;
                out[idx++] = (byte) ((buf >> bits) & 0xff);
            }
        }
        return Arrays.copyOf(out, idx);
    }
}
