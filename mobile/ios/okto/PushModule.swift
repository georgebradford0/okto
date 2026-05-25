import Foundation
import UIKit
import UserNotifications

// React Native bridge for iOS push notifications.
//
// JS calls `requestPermissionAndRegister()`, which:
//   1. Asks the user for notification permission via UNUserNotificationCenter.
//   2. Calls UIApplication.registerForRemoteNotifications() to ask APNs for a
//      device token.
//   3. Resolves the JS promise with the hex-encoded token once AppDelegate's
//      `application:didRegisterForRemoteNotificationsWithDeviceToken:` fires.
//
// AppDelegate forwards both success and failure callbacks back here via
// `Push.handleRegistration(token:)` / `Push.handleRegistrationError(_:)`.

@objc(Push)
final class Push: NSObject, UNUserNotificationCenterDelegate {

    // Single shared instance so AppDelegate can install us as the
    // UNUserNotificationCenterDelegate at launch — that delegate must be a
    // real object reference, not the class. RN bridge calls still work
    // through the same shared instance.
    @objc static let shared = Push()

    // Pending JS promise for an in-flight requestPermissionAndRegister().
    // Cleared as soon as APNs returns success or error. Guarded by `lock`
    // because AppDelegate's callbacks fire on the main thread and our
    // RN method calls land on the bridge queue.
    private static var pending: (resolve: (Any?) -> Void, reject: (String?, String?, Error?) -> Void)?
    private static let lock = NSLock()

    @objc static func requiresMainQueueSetup() -> Bool { false }

    // Show banner + play sound even when the app is foregrounded. Without
    // this iOS silently delivers the push to the app and suppresses any UI,
    // which makes a "task complete" notification invisible to the user
    // sitting in the chat waiting for it.
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        completionHandler([.banner, .sound, .badge])
    }

    // Called by AppDelegate when APNs returns the device token.
    @objc static func handleRegistration(token: Data) {
        let hex = token.map { String(format: "%02x", $0) }.joined()
        print("[push] APNs token registered: \(hex.prefix(8))…\(hex.suffix(8)) (\(hex.count / 2) bytes)")

        lock.lock()
        let p = pending
        pending = nil
        lock.unlock()

        p?.resolve(hex)
    }

    // Called by AppDelegate when APNs rejects the registration request
    // (no entitlement, simulator without remote-push capability, no internet, …).
    @objc static func handleRegistrationError(_ error: Error) {
        print("[push] APNs registration error: \(error.localizedDescription)")

        lock.lock()
        let p = pending
        pending = nil
        lock.unlock()

        p?.reject("PUSH_REGISTRATION_FAILED", error.localizedDescription, error as NSError)
    }

    @objc func requestPermissionAndRegister(
        _ resolve: @escaping (Any?) -> Void,
        rejecter reject: @escaping (String?, String?, Error?) -> Void
    ) {
        let center = UNUserNotificationCenter.current()
        center.requestAuthorization(options: [.alert, .sound, .badge]) { granted, error in
            if let error = error {
                reject("PUSH_PERMISSION_ERROR", error.localizedDescription, error as NSError)
                return
            }
            guard granted else {
                resolve(nil)   // user declined — JS treats nil as "no token"
                return
            }

            // Stash the promise *before* registering, since AppDelegate's
            // callback can fire before the dispatch returns on fast networks.
            Push.lock.lock()
            // If a previous call is already pending, reject it — only one
            // in-flight registration at a time (RN won't normally send two
            // back-to-back, but defensively handle it).
            if let prev = Push.pending {
                prev.reject("PUSH_SUPERSEDED", "registration superseded by a new request", nil)
            }
            Push.pending = (resolve, reject)
            Push.lock.unlock()

            DispatchQueue.main.async {
                UIApplication.shared.registerForRemoteNotifications()
            }
        }
    }

    @objc func getAuthorizationStatus(
        _ resolve: @escaping (Any?) -> Void,
        rejecter reject: @escaping (String?, String?, Error?) -> Void
    ) {
        UNUserNotificationCenter.current().getNotificationSettings { settings in
            let s: String
            switch settings.authorizationStatus {
            case .notDetermined: s = "notDetermined"
            case .denied:        s = "denied"
            case .authorized:    s = "authorized"
            case .provisional:   s = "provisional"
            case .ephemeral:     s = "ephemeral"
            @unknown default:    s = "unknown"
            }
            resolve(s)
        }
    }

    // ── APNs gateway (sandbox vs production) ──────────────────────────────
    //
    // The relay needs to know which APNs gateway resolves a given device
    // token. The canonical source is the `aps-environment` entitlement baked
    // into the embedded provisioning profile at sign time — NOT what's in
    // okto.entitlements. A Xcode-signed development build always gets a
    // sandbox token even if the entitlements file says "production"; only a
    // distribution-signed build (Ad Hoc, TestFlight, App Store) gets a
    // production token. JS reads this value and passes it on /register so the
    // relay picks the right gateway per device.

    private static let apsEnvironmentString: String = computeApsEnvironment()

    private static func computeApsEnvironment() -> String {
        #if targetEnvironment(simulator)
        // Simulators don't issue real APNs tokens; the value is moot.
        return "sandbox"
        #else
        guard let url = Bundle.main.url(forResource: "embedded", withExtension: "mobileprovision"),
              let data = try? Data(contentsOf: url) else {
            // No embedded profile (Mac Catalyst / unusual packaging). Assume
            // shipped app — safer to over-pick production than to spam sandbox.
            return "production"
        }
        // The file is a CMS (PKCS7) envelope around an XML plist. We don't
        // need to verify the signature here — extract the plist by locating
        // its delimiters in the byte stream. isoLatin1 preserves every byte
        // 1:1 so range offsets back into the original data are stable.
        guard let raw = String(data: data, encoding: .isoLatin1),
              let start = raw.range(of: "<plist"),
              let end   = raw.range(of: "</plist>") else {
            return "production"
        }
        let plistText = String(raw[start.lowerBound..<end.upperBound])
        guard let plistData = plistText.data(using: .isoLatin1),
              let plist     = try? PropertyListSerialization.propertyList(from: plistData, options: [], format: nil),
              let dict      = plist as? [String: Any],
              let ents      = dict["Entitlements"] as? [String: Any],
              let env       = ents["aps-environment"] as? String else {
            return "production"
        }
        return env == "development" ? "sandbox" : "production"
        #endif
    }

    @objc func apsEnvironment(
        _ resolve: @escaping (Any?) -> Void,
        rejecter reject: @escaping (String?, String?, Error?) -> Void
    ) {
        resolve(Push.apsEnvironmentString)
    }

    // ── Registration-challenge handling ────────────────────────────────────
    //
    // The relay proves a device controls its APNs token by sending a silent
    // push carrying a nonce; `/register` then requires that nonce echoed back.
    // AppDelegate forwards every remote notification here. If it is a
    // challenge push we hand the nonce to a waiting JS promise, or latch it
    // until JS asks — the push can land before JS calls `await`.
    //
    // `challengeGeneration` identifies the current waiter so a stale timeout
    // (whose waiter was already consumed or superseded) cannot settle a newer
    // call's promise.

    private static var latchedNonce: String?
    private static var challengeWaiter: ((Any?) -> Void)?
    private static var challengeGeneration: Int = 0

    // Called by AppDelegate for every received remote notification.
    @objc static func handleRemoteNotification(_ userInfo: [AnyHashable: Any]) {
        guard let nonce = userInfo["okto_challenge"] as? String else { return }
        print("[push] registration challenge received")

        lock.lock()
        if let waiter = challengeWaiter {
            challengeWaiter = nil
            challengeGeneration += 1
            lock.unlock()
            waiter(nonce)
        } else {
            latchedNonce = nonce
            lock.unlock()
        }
    }

    @objc func awaitRegistrationChallenge(
        _ timeoutMs: NSNumber,
        resolver resolve: @escaping (Any?) -> Void,
        rejecter reject: @escaping (String?, String?, Error?) -> Void
    ) {
        Push.lock.lock()
        // Push already arrived before JS asked? hand it over immediately.
        if let n = Push.latchedNonce {
            Push.latchedNonce = nil
            Push.lock.unlock()
            resolve(n)
            return
        }
        // Supersede any previous waiter so its JS promise doesn't hang.
        if let prev = Push.challengeWaiter {
            Push.challengeWaiter = nil
            Push.challengeGeneration += 1
            prev(nil)
        }
        Push.challengeGeneration += 1
        let myGeneration = Push.challengeGeneration
        Push.challengeWaiter = resolve
        Push.lock.unlock()

        // Timeout: resolve null if this same call is still the waiter.
        let seconds = max(0, timeoutMs.doubleValue / 1000.0)
        DispatchQueue.main.asyncAfter(deadline: .now() + seconds) {
            Push.lock.lock()
            if Push.challengeWaiter != nil && Push.challengeGeneration == myGeneration {
                let waiter = Push.challengeWaiter
                Push.challengeWaiter = nil
                Push.challengeGeneration += 1
                Push.lock.unlock()
                waiter?(nil)
            } else {
                Push.lock.unlock()
            }
        }
    }
}
