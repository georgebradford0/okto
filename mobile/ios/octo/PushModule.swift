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
final class Push: NSObject {

    // Pending JS promise for an in-flight requestPermissionAndRegister().
    // Cleared as soon as APNs returns success or error. Guarded by `lock`
    // because AppDelegate's callbacks fire on the main thread and our
    // RN method calls land on the bridge queue.
    private static var pending: (resolve: (Any?) -> Void, reject: (String?, String?, Error?) -> Void)?
    private static let lock = NSLock()

    @objc static func requiresMainQueueSetup() -> Bool { false }

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
}
