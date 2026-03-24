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

import org.bouncycastle.crypto.params.AsymmetricKeyParameter;
import org.bouncycastle.crypto.params.Ed25519PrivateKeyParameters;
import org.bouncycastle.crypto.params.Ed25519PublicKeyParameters;
import org.bouncycastle.crypto.signers.Ed25519Signer;
import org.bouncycastle.crypto.util.OpenSSHPrivateKeyUtil;

import java.nio.charset.StandardCharsets;
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
     * @param port          SSH server port (typically 22)
     * @param hostPubKey    Base64-encoded OpenSSH wire-format public key — the
     *                      second field of an ssh-ed25519 .pub file. Decodes to
     *                      the standard 51-byte blob: [uint32 11]["ssh-ed25519"]
     *                      [uint32 32][32-byte key]. Matches what the Docker
     *                      container prints in the QR code ("hk" field).
     * @param clientPrivKey Base64-encoded contents of the OpenSSH Ed25519 private
     *                      key PEM file (including BEGIN/END headers). Matches the
     *                      "ck" field in the QR code.
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
                // 1. Parse host public key from OpenSSH wire format
                //    "hk" in the QR = awk '{print $2}' ssh_host_ed25519_key.pub
                //    Decodes to [uint32 len]["ssh-ed25519"][uint32 32][32-byte key]
                // ------------------------------------------------------------------
                byte[] wireFormat = Base64.decode(hostPubKey, Base64.DEFAULT);
                byte[] rawPub = PinnedHostKeyRepository.extractEd25519PublicKey(wireFormat);
                if (rawPub == null) {
                    promise.reject("SSH_KEY_ERROR",
                            "hostPubKey is not a valid Ed25519 SSH wire-format public key");
                    return;
                }

                // ------------------------------------------------------------------
                // 2. Parse client private key from OpenSSH PEM file
                //    "ck" in the QR = base64 -w0 of the entire private key file
                // ------------------------------------------------------------------
                byte[] pemBytes = Base64.decode(clientPrivKey, Base64.DEFAULT);
                String pemStr = new String(pemBytes, StandardCharsets.UTF_8);
                // Strip PEM armour and whitespace to get the raw base64 blob.
                String inner = pemStr
                        .replace("-----BEGIN OPENSSH PRIVATE KEY-----", "")
                        .replace("-----END OPENSSH PRIVATE KEY-----", "")
                        .replaceAll("\\s+", "");
                byte[] blob = Base64.decode(inner, Base64.DEFAULT);
                AsymmetricKeyParameter keyParam = OpenSSHPrivateKeyUtil.parsePrivateKeyBlob(blob);
                if (!(keyParam instanceof Ed25519PrivateKeyParameters)) {
                    promise.reject("SSH_KEY_ERROR",
                            "clientPrivKey is not an Ed25519 private key");
                    return;
                }
                Ed25519PrivateKeyParameters bcPriv = (Ed25519PrivateKeyParameters) keyParam;
                Ed25519PublicKeyParameters bcPub = bcPriv.generatePublicKey();

                // ------------------------------------------------------------------
                // 3. Configure JSch
                // ------------------------------------------------------------------
                JSch jsch = new JSch();
                jsch.setHostKeyRepository(new PinnedHostKeyRepository(host, rawPub));
                jsch.addIdentity(new Ed25519Identity(bcPriv, bcPub), null);

                // ------------------------------------------------------------------
                // 4. Open SSH session
                // ------------------------------------------------------------------
                int sshPort = (int) port;
                Session session = jsch.getSession("claude", host, sshPort);

                Properties config = new Properties();
                config.put("StrictHostKeyChecking", "yes");
                // Restrict key exchange / host-key algorithms to Ed25519 so JSch
                // doesn't try RSA/ECDSA and fail before reaching our host-key check.
                config.put("server_host_key", "ssh-ed25519");
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
     * A {@link HostKeyRepository} that accepts exactly one Ed25519 host key
     * (supplied as 32 raw bytes) for a specific host.  All other keys are
     * rejected so that the SSH session fails rather than silently connecting
     * to an unexpected server.
     */
    private static final class PinnedHostKeyRepository implements HostKeyRepository {

        private final String pinnedHost;
        private final byte[] pinnedRawPub; // 32-byte Ed25519 public key

        PinnedHostKeyRepository(String host, byte[] rawPub) {
            this.pinnedHost = host;
            this.pinnedRawPub = rawPub;
        }

        @Override
        public int check(String host, byte[] serverKeyBlob) {
            // serverKeyBlob is the raw SSH wire-format key blob:
            //   [uint32 len] "ssh-ed25519" [uint32 32] [32 bytes public key]
            byte[] serverRaw = extractEd25519PublicKey(serverKeyBlob);
            if (serverRaw == null) {
                return NOT_INCLUDED; // Force a rejection via StrictHostKeyChecking.
            }
            if (!host.equals(pinnedHost) && !host.startsWith(pinnedHost + ",")) {
                return NOT_INCLUDED;
            }
            return Arrays.equals(pinnedRawPub, serverRaw) ? OK : CHANGED;
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

        /**
         * Parse the 32-byte Ed25519 public key out of an SSH wire-format blob.
         *
         * Wire format:
         *   4 bytes  — length of key-type string
         *   N bytes  — key-type string (e.g. "ssh-ed25519")
         *   4 bytes  — length of public key bytes (should be 32)
         *   32 bytes — raw public key
         */
        @Nullable
        static byte[] extractEd25519PublicKey(byte[] blob) {
            try {
                if (blob == null || blob.length < 4) return null;
                int typeLen = readUint32(blob, 0);
                if (typeLen < 0 || 4 + typeLen + 4 > blob.length) return null;
                String keyType = new String(blob, 4, typeLen, java.nio.charset.StandardCharsets.UTF_8);
                if (!"ssh-ed25519".equals(keyType)) return null;
                int keyOffset = 4 + typeLen;
                int keyLen = readUint32(blob, keyOffset);
                if (keyLen != 32 || keyOffset + 4 + 32 > blob.length) return null;
                return Arrays.copyOfRange(blob, keyOffset + 4, keyOffset + 4 + 32);
            } catch (Exception e) {
                return null;
            }
        }

        private static int readUint32(byte[] b, int offset) {
            return ((b[offset] & 0xFF) << 24)
                    | ((b[offset + 1] & 0xFF) << 16)
                    | ((b[offset + 2] & 0xFF) << 8)
                    | (b[offset + 3] & 0xFF);
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
