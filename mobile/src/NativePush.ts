import type { TurboModule } from 'react-native'
import { TurboModuleRegistry } from 'react-native'

export type AuthorizationStatus =
  | 'notDetermined'
  | 'denied'
  | 'authorized'
  | 'provisional'
  | 'ephemeral'
  | 'unknown'

export interface Spec extends TurboModule {
  /**
   * Ask the user for notification permission, then register with APNs.
   *
   * Resolves with:
   *   - the hex-encoded APNs device token (string) on success, or
   *   - `null` if the user declined the permission prompt.
   *
   * Rejects if APNs registration itself fails (no entitlement, no internet,
   * simulator without remote-push capability, etc).
   */
  requestPermissionAndRegister(): Promise<string | null>

  /** Current notification permission state — call before re-prompting. */
  getAuthorizationStatus(): Promise<AuthorizationStatus>
}

const NativePush = TurboModuleRegistry.get<Spec>('Push')

export default NativePush
