import UIKit
import React
import React_RCTAppDelegate
import ReactAppDependencyProvider
import UserNotifications

@main
class AppDelegate: UIResponder, UIApplicationDelegate {
  var window: UIWindow?

  var reactNativeDelegate: ReactNativeDelegate?
  var reactNativeFactory: RCTReactNativeFactory?

  func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
  ) -> Bool {
    let delegate = ReactNativeDelegate()
    let factory = RCTReactNativeFactory(delegate: delegate)
    delegate.dependencyProvider = RCTAppDependencyProvider()

    reactNativeDelegate = delegate
    reactNativeFactory = factory

    window = UIWindow(frame: UIScreen.main.bounds)

    factory.startReactNative(
      withModuleName: "okto",
      in: window,
      launchOptions: launchOptions
    )

    // Make Push the UNUserNotificationCenter delegate so foreground pushes
    // surface as banners instead of being silently dropped.
    UNUserNotificationCenter.current().delegate = Push.shared

    return true
  }

  // ── APNs registration callbacks ────────────────────────────────────────────
  //
  // Forwarded into PushModule.swift so the JS-side promise from
  // `Push.requestPermissionAndRegister()` can resolve with the token.

  func application(
    _ application: UIApplication,
    didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data
  ) {
    Push.handleRegistration(token: deviceToken)
  }

  func application(
    _ application: UIApplication,
    didFailToRegisterForRemoteNotificationsWithError error: Error
  ) {
    Push.handleRegistrationError(error)
  }

  // Silent (content-available) pushes — the relay uses these to deliver
  // registration-challenge nonces. Forwarded into PushModule, which hands the
  // nonce to the waiting JS registration flow. Requires the
  // `remote-notification` UIBackgroundMode (see Info.plist).
  func application(
    _ application: UIApplication,
    didReceiveRemoteNotification userInfo: [AnyHashable: Any],
    fetchCompletionHandler completionHandler: @escaping (UIBackgroundFetchResult) -> Void
  ) {
    Push.handleRemoteNotification(userInfo)
    completionHandler(.noData)
  }
}

class ReactNativeDelegate: RCTDefaultReactNativeFactoryDelegate {
  override func sourceURL(for bridge: RCTBridge) -> URL? {
    self.bundleURL()
  }

  override func bundleURL() -> URL? {
#if DEBUG
    RCTBundleURLProvider.sharedSettings().jsBundleURL(forBundleRoot: "index")
#else
    Bundle.main.url(forResource: "main", withExtension: "jsbundle")
#endif
  }
}
