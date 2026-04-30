import type { TurboModule } from 'react-native';
import { TurboModuleRegistry } from 'react-native';

export interface Spec extends TurboModule {
  /**
   * Establish a Noise_XX_25519_ChaChaPoly_SHA256 tunnel.
   *
   * Connects to host:port, performs the Noise_XX handshake using the server's
   * static public key for authentication, then binds a local TCP port that
   * transparently proxies to the remote server (one Noise session per local
   * connection, so WebSocket + HTTP all work unchanged).
   *
   * @param host          Server IP or hostname
   * @param port          Server Noise TCP port (typically 9000)
   * @param serverPubKey  Base32-encoded 32-byte Curve25519 static public key
   *                      ("pk" field in the QR code)
   * @returns             Local TCP port bound for forwarding to server
   */
  connect(host: string, port: number, serverPubKey: string): Promise<number>;

  /**
   * Tear down the proxy listener and all active connections.
   */
  disconnect(): void;

}

const NativeNoiseConnection = TurboModuleRegistry.get<Spec>('NoiseConnection');

export default NativeNoiseConnection;
