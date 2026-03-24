import type { TurboModule } from 'react-native';
import { TurboModuleRegistry } from 'react-native';

export interface Spec extends TurboModule {
  /**
   * Establish an SSH tunnel.
   *
   * @param host        SSH server hostname or IP address
   * @param port        SSH server port (typically 22)
   * @param hostPubKey  Base64-encoded 32-byte SHA-256 fingerprint of the server's ECDSA host key ("hk" in the QR)
   * @param clientPrivKey Base64-encoded raw 32-byte Ed25519 private key seed ("ck" in the QR)
   * @returns           The local port number that was bound for forwarding to
   *                    remote localhost:8000
   */
  connect(
    host: string,
    port: number,
    hostPubKey: string,
    clientPrivKey: string,
  ): Promise<number>;

  /**
   * Tear down the active SSH tunnel and free all resources.
   */
  disconnect(): void;
}

const NativeSshTunnel = TurboModuleRegistry.getEnforcing<Spec>('SshTunnel');

export default NativeSshTunnel;
