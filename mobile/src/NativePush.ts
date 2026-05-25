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

  /**
   * Wait for the relay's registration-challenge push to arrive and return the
   * nonce it carries.
   *
   * Resolves with:
   *   - the challenge nonce (string) once the silent push is received, or
   *   - `null` if none arrives within `timeoutMs`.
   *
   * A push that arrives before this is called is latched natively and
   * returned immediately, so the caller may request the challenge first.
   */
  awaitRegistrationChallenge(timeoutMs: number): Promise<string | null>

  /**
   * Which APNs gateway this build's device token resolves on:
   *   - `"sandbox"` for Xcode-signed development builds (and simulators).
   *   - `"production"` for Ad Hoc / TestFlight / App Store builds.
   *
   * Sourced from the `aps-environment` value in the embedded provisioning
   * profile, which is what actually determines the token at registration —
   * the value in `okto.entitlements` is irrelevant unless the signing profile
   * permits the same value. The caller passes the result to the relay so a
   * single relay can serve dev and shipped clients side by side.
   */
  apsEnvironment(): Promise<'sandbox' | 'production'>
}

const NativePush = TurboModuleRegistry.get<Spec>('Push')

export default NativePush
