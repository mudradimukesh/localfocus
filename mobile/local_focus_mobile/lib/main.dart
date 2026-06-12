// Local Focus — standalone Android app.
//
// The phone runs the *exact same* Local Focus core as the Mac app: a native
// binary (liblocalfocus.so) is exec'd as a local server on 127.0.0.1:4799, and
// this app shows that identical dashboard in a WebView. A native foreground
// service feeds the phone's own app usage into the server, and an Accessibility
// service enforces the block list. Everything stays on the device.
import 'dart:async';
import 'dart:io' show Platform;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:webview_flutter/webview_flutter.dart';

import 'companion_app.dart';

const MethodChannel _native = MethodChannel('local_focus/native');
const Color _accent = Color(0xFF355C7D);

Future<T?> _invoke<T>(String method, [Map<String, dynamic>? args]) async {
  try {
    return await _native.invokeMethod<T>(method, args);
  } catch (_) {
    return null;
  }
}

void main() {
  WidgetsFlutterBinding.ensureInitialized();
  // Android runs the full standalone experience (embedded server + on-device
  // dashboard + tracking + blocking). iOS cannot run the embedded server or
  // monitor other apps, so it falls back to the Mac companion.
  if (Platform.isAndroid) {
    runApp(const LocalFocusApp());
  } else {
    runApp(const LocalFocusMobileApp());
  }
}

class LocalFocusApp extends StatelessWidget {
  const LocalFocusApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Local Focus',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(colorSchemeSeed: _accent, useMaterial3: true),
      darkTheme: ThemeData(
        colorSchemeSeed: _accent,
        brightness: Brightness.dark,
        useMaterial3: true,
      ),
      home: const HomePage(),
    );
  }
}

class HomePage extends StatefulWidget {
  const HomePage({super.key});

  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage> with WidgetsBindingObserver {
  String _serverUrl = 'http://127.0.0.1:4799';
  String _deviceName = 'Android phone';
  bool _serverReady = false;
  bool _usageGranted = false;
  bool _accessibilityEnabled = false;
  bool _showDashboard = false;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    _boot();
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    super.dispose();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    // The user returns here after granting permissions in system settings.
    if (state == AppLifecycleState.resumed) {
      _refreshPermissions();
    }
  }

  Future<void> _boot() async {
    _serverUrl = await _invoke<String>('serverUrl') ?? _serverUrl;
    _deviceName = await _invoke<String>('deviceName') ?? _deviceName;
    await _invoke('startServer');

    // Wait for the embedded server to accept connections.
    for (var i = 0; i < 40; i++) {
      if (await _invoke<bool>('serverReady') ?? false) {
        _serverReady = true;
        break;
      }
      await Future<void>.delayed(const Duration(milliseconds: 300));
    }

    await _refreshPermissions();
    if (_usageGranted) {
      await _startTracking();
      _showDashboard = true;
    }
    if (mounted) setState(() {});
  }

  Future<void> _refreshPermissions() async {
    final usage = await _invoke<bool>('usageAccessGranted') ?? false;
    final accessibility = await _invoke<bool>('accessibilityEnabled') ?? false;
    if (!mounted) return;
    setState(() {
      _usageGranted = usage;
      _accessibilityEnabled = accessibility;
    });
  }

  Future<void> _startTracking() async {
    await _invoke('startServer');
    await _invoke('startPhoneTracking', {
      'serverUrl': _serverUrl,
      'deviceName': _deviceName,
      'endpoint': 'mobile:self',
    });
  }

  Future<void> _continueToDashboard() async {
    await _refreshPermissions();
    if (!_usageGranted) {
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(
          const SnackBar(content: Text('Usage access is required to track activity.')),
        );
      }
      return;
    }
    await _startTracking();
    if (mounted) setState(() => _showDashboard = true);
  }

  @override
  Widget build(BuildContext context) {
    if (!_serverReady) {
      return const _BootScreen();
    }
    if (_showDashboard) {
      return DashboardScreen(
        url: _serverUrl,
        onOpenSetup: () => setState(() => _showDashboard = false),
      );
    }
    return _SetupScreen(
      usageGranted: _usageGranted,
      accessibilityEnabled: _accessibilityEnabled,
      onGrantUsage: () => _invoke('requestUsageAccess'),
      onEnableBlocking: () => _invoke('openAccessibilitySettings'),
      onIgnoreBattery: () => _invoke('requestBatteryExemption'),
      onRefresh: _refreshPermissions,
      onContinue: _continueToDashboard,
    );
  }
}

class _BootScreen extends StatelessWidget {
  const _BootScreen();

  @override
  Widget build(BuildContext context) {
    return const Scaffold(
      body: Center(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            CircularProgressIndicator(),
            SizedBox(height: 18),
            Text('Starting Local Focus…'),
          ],
        ),
      ),
    );
  }
}

class _SetupScreen extends StatelessWidget {
  const _SetupScreen({
    required this.usageGranted,
    required this.accessibilityEnabled,
    required this.onGrantUsage,
    required this.onEnableBlocking,
    required this.onIgnoreBattery,
    required this.onRefresh,
    required this.onContinue,
  });

  final bool usageGranted;
  final bool accessibilityEnabled;
  final VoidCallback onGrantUsage;
  final VoidCallback onEnableBlocking;
  final VoidCallback onIgnoreBattery;
  final Future<void> Function() onRefresh;
  final VoidCallback onContinue;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Local Focus setup')),
      body: RefreshIndicator(
        onRefresh: onRefresh,
        child: ListView(
          padding: const EdgeInsets.all(18),
          children: [
            const Text(
              'Local Focus runs entirely on this phone — your private activity '
              'dashboard, focus sessions, reports, and app blocking. Nothing '
              'leaves your device.',
            ),
            const SizedBox(height: 18),
            _PermissionTile(
              title: 'Usage access (required)',
              subtitle: 'Lets Local Focus see which app is in the foreground so it '
                  'can build your activity timeline and reports.',
              granted: usageGranted,
              actionLabel: 'Grant usage access',
              onAction: onGrantUsage,
            ),
            _PermissionTile(
              title: 'Accessibility / app blocking (optional)',
              subtitle: 'Lets Local Focus close distracting apps you add to your '
                  'block list by returning you to the home screen.',
              granted: accessibilityEnabled,
              actionLabel: 'Enable blocking',
              onAction: onEnableBlocking,
            ),
            _PermissionTile(
              title: 'Ignore battery optimization (recommended)',
              subtitle: 'Keeps tracking and blocking running reliably in the '
                  'background.',
              granted: null,
              actionLabel: 'Allow background',
              onAction: onIgnoreBattery,
            ),
            const SizedBox(height: 12),
            FilledButton(
              onPressed: usageGranted ? onContinue : null,
              child: const Text('Open dashboard'),
            ),
            const SizedBox(height: 8),
            TextButton(
              onPressed: onRefresh,
              child: const Text('I\'ve granted permissions — re-check'),
            ),
          ],
        ),
      ),
    );
  }
}

class _PermissionTile extends StatelessWidget {
  const _PermissionTile({
    required this.title,
    required this.subtitle,
    required this.granted,
    required this.actionLabel,
    required this.onAction,
  });

  final String title;
  final String subtitle;
  final bool? granted;
  final String actionLabel;
  final VoidCallback onAction;

  @override
  Widget build(BuildContext context) {
    final status = granted == null
        ? const Icon(Icons.chevron_right)
        : Icon(
            granted! ? Icons.check_circle : Icons.radio_button_unchecked,
            color: granted! ? Colors.green : Colors.orange,
          );
    return Card(
      margin: const EdgeInsets.only(bottom: 12),
      child: Padding(
        padding: const EdgeInsets.all(14),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Expanded(
                  child: Text(title, style: const TextStyle(fontWeight: FontWeight.bold)),
                ),
                status,
              ],
            ),
            const SizedBox(height: 6),
            Text(subtitle),
            const SizedBox(height: 10),
            Align(
              alignment: Alignment.centerLeft,
              child: OutlinedButton(onPressed: onAction, child: Text(actionLabel)),
            ),
          ],
        ),
      ),
    );
  }
}

class DashboardScreen extends StatefulWidget {
  const DashboardScreen({super.key, required this.url, required this.onOpenSetup});

  final String url;
  final VoidCallback onOpenSetup;

  @override
  State<DashboardScreen> createState() => _DashboardScreenState();
}

class _DashboardScreenState extends State<DashboardScreen> {
  late final WebViewController _controller;

  @override
  void initState() {
    super.initState();
    _controller = WebViewController()
      ..setJavaScriptMode(JavaScriptMode.unrestricted)
      ..loadRequest(Uri.parse(widget.url));
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: SafeArea(
        child: Column(
          children: [
            Material(
              color: Theme.of(context).colorScheme.surfaceContainerHighest,
              child: Row(
                children: [
                  const SizedBox(width: 12),
                  const Expanded(
                    child: Text(
                      'Local Focus — running on this phone',
                      style: TextStyle(fontWeight: FontWeight.w600),
                    ),
                  ),
                  IconButton(
                    tooltip: 'Reload',
                    icon: const Icon(Icons.refresh),
                    onPressed: () => _controller.reload(),
                  ),
                  IconButton(
                    tooltip: 'Permissions & setup',
                    icon: const Icon(Icons.tune),
                    onPressed: widget.onOpenSetup,
                  ),
                ],
              ),
            ),
            Expanded(child: WebViewWidget(controller: _controller)),
          ],
        ),
      ),
    );
  }
}
