import Flutter
import UIKit
import UserNotifications

@main
@objc class AppDelegate: FlutterAppDelegate {
  // Last-known focus state pushed from Dart. Used to decide, at background time,
  // whether to nag the user that they left their focus task. iOS cannot see
  // other apps, so "the user left this app during a focus session" is the only
  // phone-side distraction signal available.
  private var focusActive = false
  private var focusAlertDelay: Double = 60
  private var focusTask = ""
  private let distractionReminderId = "focus-distraction-reminder"

  override func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
  ) -> Bool {
    GeneratedPluginRegistrant.register(with: self)

    // Become the notification delegate so alerts are presented even while the
    // app is foregrounded, and request permission up front so the first alert
    // is never silently dropped.
    let center = UNUserNotificationCenter.current()
    center.delegate = self
    center.requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in }

    // Observe app background/foreground transitions to drive the phone-side
    // "you left focus" reminder. Using NotificationCenter avoids overriding
    // FlutterAppDelegate's own lifecycle methods.
    NotificationCenter.default.addObserver(
      self,
      selector: #selector(handleDidEnterBackground),
      name: UIApplication.didEnterBackgroundNotification,
      object: nil
    )
    NotificationCenter.default.addObserver(
      self,
      selector: #selector(handleWillEnterForeground),
      name: UIApplication.willEnterForegroundNotification,
      object: nil
    )

    if let controller = window?.rootViewController as? FlutterViewController {
      let channel = FlutterMethodChannel(name: "local_focus/native", binaryMessenger: controller.binaryMessenger)
      channel.setMethodCallHandler { [weak self] call, result in
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
          self?.showLocalNotification(title: title, message: message)
          result(nil)
        case "setFocusState":
          let args = call.arguments as? [String: Any]
          self?.focusActive = (args?["active"] as? Bool) ?? false
          if let delay = (args?["alertDelaySeconds"] as? NSNumber)?.doubleValue {
            self?.focusAlertDelay = delay
          }
          self?.focusTask = (args?["task"] as? String) ?? ""
          // If focus was turned off while backgrounded, stop nagging immediately.
          if self?.focusActive == false {
            self?.cancelDistractionReminder()
          }
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

  // The user backgrounded the app. If a focus session is active, that counts as
  // a phone-side distraction — schedule a repeating reminder to get back.
  @objc private func handleDidEnterBackground() {
    if focusActive {
      scheduleDistractionReminder()
    }
  }

  // The user came back — they are no longer distracted, so stop nagging.
  @objc private func handleWillEnterForeground() {
    cancelDistractionReminder()
  }

  private func scheduleDistractionReminder() {
    let center = UNUserNotificationCenter.current()
    center.removePendingNotificationRequests(withIdentifiers: [distractionReminderId])
    let task = focusTask.isEmpty ? "your focus task" : focusTask
    let content = UNMutableNotificationContent()
    content.title = "Focus warning"
    content.body = "You left Local Focus during \"\(task)\". Get back to focus."
    content.sound = .default
    if #available(iOS 15.0, *) {
      content.interruptionLevel = .timeSensitive
    }
    // UNTimeIntervalNotificationTrigger requires >= 60s when it repeats; nag at
    // the focus warn-after cadence (at least once a minute), like the Mac.
    let interval = max(60, focusAlertDelay)
    let trigger = UNTimeIntervalNotificationTrigger(timeInterval: interval, repeats: true)
    let request = UNNotificationRequest(identifier: distractionReminderId, content: content, trigger: trigger)
    center.add(request)
  }

  private func cancelDistractionReminder() {
    let center = UNUserNotificationCenter.current()
    center.removePendingNotificationRequests(withIdentifiers: [distractionReminderId])
    center.removeDeliveredNotifications(withIdentifiers: [distractionReminderId])
  }

  private func showLocalNotification(title: String, message: String) {
    let center = UNUserNotificationCenter.current()
    center.getNotificationSettings { settings in
      let fire = {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = message
        content.sound = .default
        // Time-sensitive notifications break through and pop prominently instead
        // of landing quietly in Notification Center.
        if #available(iOS 15.0, *) {
          content.interruptionLevel = .timeSensitive
        }
        let request = UNNotificationRequest(
          identifier: "local-focus-\(Date().timeIntervalSince1970)",
          content: content,
          trigger: nil
        )
        center.add(request)
      }
      switch settings.authorizationStatus {
      case .notDetermined:
        center.requestAuthorization(options: [.alert, .sound, .badge]) { granted, _ in
          if granted { fire() }
        }
      case .denied:
        return
      default:
        fire()
      }
    }
  }

  // Present notifications as a banner with sound even when the app is in the
  // foreground (iOS suppresses them by default without this). FlutterAppDelegate
  // already conforms to UNUserNotificationCenterDelegate, so this is an override.
  override func userNotificationCenter(
    _ center: UNUserNotificationCenter,
    willPresent notification: UNNotification,
    withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
  ) {
    if #available(iOS 14.0, *) {
      completionHandler([.banner, .list, .sound])
    } else {
      completionHandler([.alert, .sound])
    }
  }
}
