package com.claudulhu;

import android.util.Base64;

import androidx.annotation.NonNull;
import androidx.annotation.Nullable;

import com.facebook.react.bridge.Promise;
import com.facebook.react.bridge.ReactApplicationContext;
import com.facebook.react.module.annotations.ReactModule;

import com.jcraft.jsch.HostKey;
import com.jcraft.jsch.HostKeyRepository;
import com.jcraft.jsch.JSch;
import com.jcraft.jsch.Session;
import com.jcraft.jsch.UserInfo;
import com.jcraft.jsch.Identity;
import com.jcraft.jsch.JSchException;

import org.bouncycastle.crypto.params.Ed25519PrivateKeyParameters;
import org.bouncycastle.crypto.params.Ed25519PublicKeyParameters;
import org.bouncycastle.crypto.signers.Ed25519Signer;

import java.security.MessageDigest;
import java.util.Arrays;
import java.util.Properties;

/**
 * React Native TurboModule that establishes an SSH tunnel and returns a local
 * port that forwards to remote localhost:8000.
 *
 * Dependencies:
 *   com.jcraft:jsch:0.1.55
 *   org.bouncycastle:bcprov-jdk15on:1.70
 */
@ReactModule(name = SshTunnelModule.NAME)
public class SshTunnelModule extends NativeSshTunnelSpec {

    public static final String NAME = "SshTunnel";

    // -------------------------------------------------------------------------
    // State
    // -------------------------------------------------------------------------

    @Nullable
    private Session activeSession = null;

    // -------------------------------------------------------------------------
    // Constructor
    // -------------------------------------------------------------------------

    public SshTunnelModule(ReactApplicationContext reactContext) {
        super(reactContext);
    }

    @Override
    @NonNull
    public String getName() {
        return NAME;
    }

    // -------------------------------------------------------------------------
    // TurboModule API
    // -------------------------------------------------------------------------

    /**
     * Establish an SSH tunnel.
     *
     * @param host          SSH server hostname / IP
     * @param port          SSH server port (typically 2222)
     * @param hostPubKey    Base64-encoded SHA-256 fingerprint of the server's
     *                      ECDSA P-256 host key wire blob ("hk" in the QR).
     *                      32 bytes. Used for host key pinning.
     * @param clientPrivKey Base64-encoded raw 32-byte Ed25519 private key seed
     *                      ("ck" in the QR). Used for client authentication.
     * @param promise       Resolves with the local port bound for forwarding,
     *                      or rejects with an error message.
     */
    @Override
    public void connect(
            String host,
            double port,
            String hostPubKey,
            String clientPrivKey,
            Promise promise) {

        // Run on a background thread — JSch blocks on network I/O.
        new Thread(() -> {
            try {
                // ------------------------------------------------------------------
                // 1. Decode SHA-256 fingerprint of server's ECDSA host key
                //    "hk" in the QR = base64(SHA256(wire_format_blob))
                // ------------------------------------------------------------------
                byte[] pinnedFingerprint = Base64.decode(hostPubKey, Base64.DEFAULT);
                if (pinnedFingerprint == null || pinnedFingerprint.length != 32) {
                    promise.reject("SSH_KEY_ERROR",
                            "hostPubKey must be a base64-encoded 32-byte SHA-256 fingerprint");
                    return;
                }

                // ------------------------------------------------------------------
                // 2. Decode raw 32-byte Ed25519 private key seed
                //    "ck" in the QR = base64(raw 32-byte seed)
                // ------------------------------------------------------------------
                byte[] seed = Base64.decode(clientPrivKey, Base64.DEFAULT);
                if (seed == null || seed.length != 32) {
                    promise.reject("SSH_KEY_ERROR",
                            "clientPrivKey must be a base64-encoded 32-byte Ed25519 seed");
                    return;
                }
                Ed25519PrivateKeyParameters bcPriv = new Ed25519PrivateKeyParameters(seed, 0);
                Ed25519PublicKeyParameters bcPub = bcPriv.generatePublicKey();

                // ------------------------------------------------------------------
                // 3. Configure JSch
                // ------------------------------------------------------------------
                JSch jsch = new JSch();
                jsch.setHostKeyRepository(new PinnedHostKeyRepository(host, pinnedFingerprint));
                jsch.addIdentity(new Ed25519Identity(bcPriv, bcPub), null);

                // ------------------------------------------------------------------
                // 4. Open SSH session
                // ------------------------------------------------------------------
                int sshPort = (int) port;
                Session session = jsch.getSession("claude", host, sshPort);

                Properties config = new Properties();
                config.put("StrictHostKeyChecking", "yes");
                // ECDSA P-256 host key: well-supported by Android JCE on all API levels.
                // Ed25519 host key verification requires JCE EdDSA (Android API 33+).
                config.put("server_host_key", "ecdsa-sha2-nistp256");
                session.setConfig(config);

                // Prevent JSch from prompting for passwords or pass-phrases.
                session.setUserInfo(new SilentUserInfo());

                session.connect(30_000 /* ms */);

                // ------------------------------------------------------------------
                // 5. Set up local port forward  0 → remote localhost:8000
                //    Port 0 asks the OS for a free ephemeral port.
                // ------------------------------------------------------------------
                int assignedPort = session.setPortForwardingL(0, "localhost", 8000);

                // ------------------------------------------------------------------
                // 6. Persist session and resolve the promise
                // ------------------------------------------------------------------
                synchronized (SshTunnelModule.this) {
                    if (activeSession != null && activeSession.isConnected()) {
                        activeSession.disconnect();
                    }
                    activeSession = session;
                }

                promise.resolve(assignedPort);

            } catch (Exception e) {
                promise.reject("SSH_CONNECT_ERROR", e.getMessage(), e);
            }
        }, "SshTunnelThread").start();
    }

    /**
     * Tear down the active SSH tunnel.  Safe to call when no tunnel is open.
     */
    @Override
    public void disconnect() {
        synchronized (this) {
            if (activeSession != null) {
                try {
                    activeSession.disconnect();
                } catch (Exception ignored) {
                    // Best-effort cleanup.
                }
                activeSession = null;
            }
        }
    }

    // =========================================================================
    // Inner helpers
    // =========================================================================

    // -------------------------------------------------------------------------
    // Host-key pinning
    // -------------------------------------------------------------------------

    /**
     * A {@link HostKeyRepository} that pins the server's host key by its
     * SHA-256 fingerprint.  Rejects any other key, preventing MITM attacks.
     *
     * JSch passes "[host]:port" (not just "host") when connecting on a
     * non-standard port, so we match both forms.
     */
    private static final class PinnedHostKeyRepository implements HostKeyRepository {

        private final String pinnedHost;
        private final byte[] pinnedFingerprint; // SHA-256 of the server's key wire blob

        PinnedHostKeyRepository(String host, byte[] fingerprint) {
            this.pinnedHost = host;
            this.pinnedFingerprint = fingerprint;
        }

        @Override
        public int check(String host, byte[] serverKeyBlob) {
            // JSch passes "[host]:port" for non-standard ports.
            boolean hostMatches = host.equals(pinnedHost)
                    || host.startsWith(pinnedHost + ",")
                    || host.startsWith("[" + pinnedHost + "]:");
            if (!hostMatches) {
                return NOT_INCLUDED;
            }
            try {
                MessageDigest md = MessageDigest.getInstance("SHA-256");
                byte[] serverFingerprint = md.digest(serverKeyBlob);
                return Arrays.equals(pinnedFingerprint, serverFingerprint) ? OK : CHANGED;
            } catch (Exception e) {
                return NOT_INCLUDED;
            }
        }

        @Override
        public void add(HostKey hostkey, UserInfo ui) { /* pinned — never add */ }

        @Override
        public void remove(String host, String type) { /* pinned */ }

        @Override
        public void remove(String host, String type, byte[] key) { /* pinned */ }

        @Override
        public String getKnownHostsRepositoryID() {
            return "PinnedHostKeyRepository";
        }

        @Override
        public HostKey[] getHostKey() {
            return new HostKey[0];
        }

        @Override
        public HostKey[] getHostKey(String host, String type) {
            return new HostKey[0];
        }
    }

    // -------------------------------------------------------------------------
    // Ed25519 Identity (JSch Identity adapter backed by BouncyCastle)
    // -------------------------------------------------------------------------

    /**
     * Bridges a BouncyCastle Ed25519 key pair into the JSch {@link Identity}
     * interface.  JSch 0.1.55 does not support loading Ed25519 keys from raw
     * bytes natively, so we implement the interface ourselves and sign with
     * BouncyCastle.
     */
    private static final class Ed25519Identity implements Identity {

        private static final String SSH_ED25519 = "ssh-ed25519";

        private final Ed25519PrivateKeyParameters privKey;
        private final Ed25519PublicKeyParameters pubKey;

        Ed25519Identity(Ed25519PrivateKeyParameters priv, Ed25519PublicKeyParameters pub) {
            this.privKey = priv;
            this.pubKey = pub;
        }

        @Override
        public boolean setPassphrase(byte[] passphrase) throws JSchException {
            return true; // No passphrase needed.
        }

        @Override
        public byte[] getPublicKeyBlob() {
            // SSH wire format: [uint32 len("ssh-ed25519")] ["ssh-ed25519"] [uint32 32] [32-byte pub]
            byte[] typeBytes = SSH_ED25519.getBytes(java.nio.charset.StandardCharsets.UTF_8);
            byte[] rawPub = pubKey.getEncoded();
            byte[] blob = new byte[4 + typeBytes.length + 4 + rawPub.length];
            writeUint32(blob, 0, typeBytes.length);
            System.arraycopy(typeBytes, 0, blob, 4, typeBytes.length);
            writeUint32(blob, 4 + typeBytes.length, rawPub.length);
            System.arraycopy(rawPub, 0, blob, 4 + typeBytes.length + 4, rawPub.length);
            return blob;
        }

        @Override
        public byte[] getSignature(byte[] data) {
            // Build the SSH signature blob:
            //   [uint32 len("ssh-ed25519")] ["ssh-ed25519"] [uint32 64] [64-byte sig]
            Ed25519Signer signer = new Ed25519Signer();
            signer.init(true, privKey);
            signer.update(data, 0, data.length);
            byte[] rawSig = signer.generateSignature();

            byte[] typeBytes = SSH_ED25519.getBytes(java.nio.charset.StandardCharsets.UTF_8);
            byte[] sigBlob = new byte[4 + typeBytes.length + 4 + rawSig.length];
            writeUint32(sigBlob, 0, typeBytes.length);
            System.arraycopy(typeBytes, 0, sigBlob, 4, typeBytes.length);
            writeUint32(sigBlob, 4 + typeBytes.length, rawSig.length);
            System.arraycopy(rawSig, 0, sigBlob, 4 + typeBytes.length + 4, rawSig.length);
            return sigBlob;
        }

        @Override
        public boolean isEncrypted() {
            return false;
        }

        @Override
        public String getAlgName() {
            return SSH_ED25519;
        }

        @Override
        public String getName() {
            return "Ed25519Identity";
        }

        public String getFingerPrint() {
            return "Ed25519";
        }

        @Override
        public boolean decrypt() {
            return true;
        }

        @Override
        public void clear() { /* nothing sensitive stored beyond what BC holds */ }

        private static void writeUint32(byte[] b, int offset, int value) {
            b[offset]     = (byte) ((value >>> 24) & 0xFF);
            b[offset + 1] = (byte) ((value >>> 16) & 0xFF);
            b[offset + 2] = (byte) ((value >>> 8) & 0xFF);
            b[offset + 3] = (byte) (value & 0xFF);
        }
    }

    // -------------------------------------------------------------------------
    // Silent UserInfo (prevents JSch from prompting the user)
    // -------------------------------------------------------------------------

    private static final class SilentUserInfo implements UserInfo {
        @Override public String getPassphrase() { return null; }
        @Override public String getPassword()   { return null; }
        @Override public boolean promptPassword(String message)   { return false; }
        @Override public boolean promptPassphrase(String message) { return false; }
        @Override public boolean promptYesNo(String message)      { return false; }
        @Override public void showMessage(String message)         { /* no-op */ }
    }
}
