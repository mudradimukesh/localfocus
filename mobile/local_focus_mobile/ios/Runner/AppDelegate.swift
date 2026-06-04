import Flutter
import UIKit
import UserNotifications

@main
@objc class AppDelegate: FlutterAppDelegate {
  override func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
  ) -> Bool {
    GeneratedPluginRegistrant.register(with: self)
    if let controller = window?.rootViewController as? FlutterViewController {
      let channel = FlutterMethodChannel(name: "local_focus/native", binaryMessenger: controller.binaryMessenger)
      channel.setMethodCallHandler { call, result in
        switch call.method {
        case "deviceName":
          result(UIDevice.current.name)
        case "usageAccessGranted":
          result(false)
        case "requestUsageAccess":
          result(nil)
        case "latestActivity":
          result(nil)
        case "showNotification":
          let args = call.arguments as? [String: Any]
          let title = args?["title"] as? String ?? "Local Focus"
          let message = args?["message"] as? String ?? "Focus alert"
          self.showLocalNotification(title: title, message: message)
          result(nil)
        case "startPhoneTracking":
          result(nil)
        case "stopPhoneTracking":
          result(nil)
        default:
          result(FlutterMethodNotImplemented)
        }
      }
    }
    return super.application(application, didFinishLaunchingWithOptions: launchOptions)
  }

  private func showLocalNotification(title: String, message: String) {
    let center = UNUserNotificationCenter.current()
    center.requestAuthorization(options: [.alert, .sound, .badge]) { granted, _ in
      guard granted else { return }
      let content = UNMutableNotificationContent()
      content.title = title
      content.body = message
      content.sound = .default
      let request = UNNotificationRequest(
        identifier: "local-focus-\(Date().timeIntervalSince1970)",
        content: content,
        trigger: nil
      )
      center.add(request)
    }
  }
}
