import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:shared_preferences/shared_preferences.dart';

const defaultServerUrl = 'http://192.168.4.22:4799';
const nativeChannelName = 'local_focus/native';

void main() {
  runApp(const LocalFocusMobileApp());
}

class LocalFocusMobileApp extends StatelessWidget {
  const LocalFocusMobileApp({super.key});

  @override
  Widget build(BuildContext context) {
    const accent = Color(0xff2d6a4f);
    const warn = Color(0xffb53d3d);
    const idle = Color(0xffb7791f);
    return MaterialApp(
      title: 'Local Focus',
      debugShowCheckedModeBanner: false,
      themeMode: ThemeMode.system,
      theme: ThemeData(
        colorScheme: ColorScheme.fromSeed(
          seedColor: accent,
          primary: accent,
          error: warn,
          tertiary: idle,
          brightness: Brightness.light,
        ),
        scaffoldBackgroundColor: const Color(0xfff6f7f2),
        useMaterial3: true,
        cardTheme: const CardThemeData(
          elevation: 0,
          margin: EdgeInsets.zero,
          shape: RoundedRectangleBorder(
            borderRadius: BorderRadius.all(Radius.circular(10)),
          ),
        ),
      ),
      darkTheme: ThemeData(
        colorScheme: ColorScheme.fromSeed(
          seedColor: accent,
          primary: const Color(0xff7cc7a2),
          error: const Color(0xffff8d8d),
          tertiary: const Color(0xffffc56d),
          brightness: Brightness.dark,
        ),
        scaffoldBackgroundColor: const Color(0xff121512),
        useMaterial3: true,
        cardTheme: const CardThemeData(
          elevation: 0,
          margin: EdgeInsets.zero,
          shape: RoundedRectangleBorder(
            borderRadius: BorderRadius.all(Radius.circular(10)),
          ),
        ),
      ),
      home: const MobileShell(),
    );
  }
}

enum ReportPeriod { day, week, month, year }

class NativeBridge {
  static const _channel = MethodChannel(nativeChannelName);

  static Future<String> deviceName() async {
    try {
      return await _channel.invokeMethod<String>('deviceName') ?? 'Phone';
    } catch (_) {
      return 'Phone';
    }
  }

  static Future<bool> usageAccessGranted() async {
    try {
      return await _channel.invokeMethod<bool>('usageAccessGranted') ?? false;
    } catch (_) {
      return false;
    }
  }

  static Future<void> requestUsageAccess() async {
    try {
      await _channel.invokeMethod<void>('requestUsageAccess');
    } catch (_) {}
  }

  static Future<Map<String, dynamic>?> latestActivity() async {
    try {
      final value = await _channel.invokeMethod<dynamic>('latestActivity');
      if (value is Map) {
        return value.map((key, value) => MapEntry('$key', value));
      }
    } catch (_) {}
    return null;
  }

  static Future<void> showNotification(String title, String message) async {
    try {
      await _channel.invokeMethod<void>('showNotification', {
        'title': title,
        'message': message,
      });
    } catch (_) {}
  }

  static Future<void> startPhoneTracking({
    required String serverUrl,
    required String deviceName,
    required String endpoint,
  }) async {
    try {
      await _channel.invokeMethod<void>('startPhoneTracking', {
        'serverUrl': serverUrl,
        'deviceName': deviceName,
        'endpoint': endpoint,
      });
    } catch (_) {}
  }

  static Future<void> stopPhoneTracking() async {
    try {
      await _channel.invokeMethod<void>('stopPhoneTracking');
    } catch (_) {}
  }
}

class LocalFocusApi {
  LocalFocusApi(String baseUrl) : baseUrl = normalizeBaseUrl(baseUrl);

  final String baseUrl;
  final HttpClient _client = HttpClient()
    ..connectionTimeout = const Duration(seconds: 4);

  static String normalizeBaseUrl(String value) {
    var trimmed = value.trim();
    if (trimmed.isEmpty) return defaultServerUrl;
    if (!trimmed.startsWith('http://') && !trimmed.startsWith('https://')) {
      trimmed = 'http://$trimmed';
    }
    return trimmed.endsWith('/')
        ? trimmed.substring(0, trimmed.length - 1)
        : trimmed;
  }

  Uri uri(String path, [Map<String, String>? query]) {
    final base = Uri.parse(baseUrl);
    return base.replace(
      path: path,
      queryParameters: query == null || query.isEmpty ? null : query,
    );
  }

  Future<dynamic> getJson(String path, [Map<String, String>? query]) async {
    final request = await _client.getUrl(uri(path, query));
    final response = await request.close();
    final body = await utf8.decodeStream(response);
    if (response.statusCode < 200 || response.statusCode >= 300) {
      throw HttpException('HTTP ${response.statusCode}: $body');
    }
    return jsonDecode(body);
  }

  Future<dynamic> postJson(String path, Map<String, dynamic> body) async {
    final encoded = jsonEncode(body);
    final request = await _client.postUrl(uri(path));
    request.headers.contentType = ContentType.json;
    request.headers.contentLength = utf8.encode(encoded).length;
    request.write(encoded);
    final response = await request.close();
    final text = await utf8.decodeStream(response);
    if (response.statusCode < 200 || response.statusCode >= 300) {
      throw HttpException('HTTP ${response.statusCode}: $text');
    }
    return jsonDecode(text);
  }

  void close() => _client.close(force: true);
}

class FocusSession {
  const FocusSession({
    required this.task,
    required this.target,
    required this.startedAt,
    required this.durationMinutes,
    required this.alertDelaySeconds,
    required this.alertAction,
    required this.redirectApp,
    required this.paused,
    required this.remainingSeconds,
  });

  final String task;
  final String target;
  final int startedAt;
  final int durationMinutes;
  final int alertDelaySeconds;
  final String alertAction;
  final String redirectApp;
  final bool paused;
  final int remainingSeconds;

  static FocusSession? fromJson(dynamic value) {
    if (value is! Map) return null;
    return FocusSession(
      task: stringValue(value['task'], 'Focus session'),
      target: stringValue(value['target'], ''),
      startedAt: intValue(value['startedAt']),
      durationMinutes: intValue(value['durationMinutes'], 25),
      alertDelaySeconds: intValue(value['alertDelaySeconds'], 60),
      alertAction: stringValue(value['alertAction'], 'alert'),
      redirectApp: stringValue(value['redirectApp'], ''),
      paused: value['paused'] == true,
      remainingSeconds: intValue(value['remainingSeconds']),
    );
  }
}

class SummaryReport {
  const SummaryReport({
    required this.productiveSeconds,
    required this.distractingSeconds,
    required this.idleSeconds,
    required this.topApps,
  });

  final int productiveSeconds;
  final int distractingSeconds;
  final int idleSeconds;
  final List<ActivityRow> topApps;

  int get totalSeconds => productiveSeconds + distractingSeconds + idleSeconds;

  static SummaryReport fromJson(dynamic value) {
    final map = value is Map ? value : <String, dynamic>{};
    final apps = value is Map && value['topApps'] is List
        ? (value['topApps'] as List).map(ActivityRow.fromJson).toList()
        : <ActivityRow>[];
    return SummaryReport(
      productiveSeconds: intValue(map['productiveMinutes']) * 60,
      distractingSeconds: intValue(map['distractingMinutes']) * 60,
      idleSeconds: intValue(map['idleMinutes']) * 60,
      topApps: apps,
    );
  }
}

class FocusReport {
  const FocusReport({
    required this.productiveSeconds,
    required this.distractingSeconds,
    required this.idleSeconds,
    required this.targets,
    required this.outside,
  });

  final int productiveSeconds;
  final int distractingSeconds;
  final int idleSeconds;
  final List<TargetRow> targets;
  final List<ActivityRow> outside;

  int get totalSeconds => productiveSeconds + distractingSeconds + idleSeconds;

  static FocusReport fromJson(dynamic value) {
    final map = value is Map ? value : <String, dynamic>{};
    final targets = map['targetBreakdown'] is List
        ? (map['targetBreakdown'] as List).map(TargetRow.fromJson).toList()
        : <TargetRow>[];
    final outside = map['topDistractions'] is List
        ? (map['topDistractions'] as List).map(ActivityRow.fromJson).toList()
        : <ActivityRow>[];
    return FocusReport(
      productiveSeconds: intValue(map['productiveSeconds']),
      distractingSeconds: intValue(map['distractingSeconds']),
      idleSeconds: intValue(map['idleSeconds']),
      targets: targets,
      outside: outside,
    );
  }
}

class TargetRow {
  const TargetRow({
    required this.target,
    required this.seconds,
    required this.idleSeconds,
    required this.totalSeconds,
  });

  final String target;
  final int seconds;
  final int idleSeconds;
  final int totalSeconds;

  static TargetRow fromJson(dynamic value) {
    final map = value is Map ? value : <String, dynamic>{};
    return TargetRow(
      target: stringValue(map['target'], 'Target'),
      seconds: intValue(map['seconds']),
      idleSeconds: intValue(map['idleSeconds']),
      totalSeconds: intValue(map['totalSeconds']),
    );
  }
}

class ActivityRow {
  const ActivityRow({
    required this.app,
    required this.source,
    required this.seconds,
  });

  final String app;
  final String source;
  final int seconds;

  static ActivityRow fromJson(dynamic value) {
    final map = value is Map ? value : <String, dynamic>{};
    return ActivityRow(
      app: stringValue(map['app'], 'Activity'),
      source: stringValue(map['source'], ''),
      seconds: intValue(map['seconds'], intValue(map['minutes']) * 60),
    );
  }
}

class MobileShell extends StatefulWidget {
  const MobileShell({super.key});

  @override
  State<MobileShell> createState() => _MobileShellState();
}

class _MobileShellState extends State<MobileShell> {
  SharedPreferences? _prefs;
  LocalFocusApi? _api;
  Timer? _refreshTimer;
  Timer? _eventTimer;
  Timer? _activityTimer;

  final _serverController = TextEditingController(text: defaultServerUrl);
  final _deviceController = TextEditingController(text: 'Phone');
  final _taskController = TextEditingController(text: 'Deep work on phone');
  final _targetsController = TextEditingController(
    text: 'Safari, Chrome, Notes, https://claude.ai/, https://chatgpt.com',
  );
  final _minutesController = TextEditingController(text: '25');
  final _alertController = TextEditingController(text: '1');
  final _redirectController = TextEditingController(text: '');
  final _manualAppController = TextEditingController(text: 'Safari');
  final _manualTitleController = TextEditingController(text: 'Phone browser');
  final _manualSourceController = TextEditingController(
    text: 'https://claude.ai/chat',
  );

  int _tab = 0;
  bool _loading = true;
  bool _busy = false;
  bool _connected = false;
  bool _autoTrack = false;
  bool _pollAlerts = true;
  bool _usageAccess = false;
  String _endpoint = '';
  String _status = 'Set the laptop URL and connect.';
  String _manualCategory = 'productive';
  int _since = currentSeconds();

  FocusSession? _focus;
  SummaryReport? _report;
  FocusReport? _focusReport;
  ReportPeriod _period = ReportPeriod.day;
  final List<Map<String, dynamic>> _events = [];
  final List<String> _activityLog = [];

  @override
  void initState() {
    super.initState();
    _load();
  }

  @override
  void dispose() {
    _refreshTimer?.cancel();
    _eventTimer?.cancel();
    _activityTimer?.cancel();
    _api?.close();
    _serverController.dispose();
    _deviceController.dispose();
    _taskController.dispose();
    _targetsController.dispose();
    _minutesController.dispose();
    _alertController.dispose();
    _redirectController.dispose();
    _manualAppController.dispose();
    _manualTitleController.dispose();
    _manualSourceController.dispose();
    super.dispose();
  }

  Future<void> _load() async {
    final prefs = await SharedPreferences.getInstance();
    final nativeName = await NativeBridge.deviceName();
    _prefs = prefs;
    _serverController.text = prefs.getString('serverUrl') ?? defaultServerUrl;
    _deviceController.text = prefs.getString('deviceName') ?? nativeName;
    _endpoint =
        prefs.getString('endpoint') ?? endpointForName(_deviceController.text);
    _autoTrack = prefs.getBool('autoTrack') ?? false;
    _pollAlerts = prefs.getBool('pollAlerts') ?? true;
    _usageAccess = await NativeBridge.usageAccessGranted();
    setState(() {
      _loading = false;
    });
    await _connect(silent: true);
  }

  Future<void> _savePrefs() async {
    final prefs = _prefs;
    if (prefs == null) return;
    await prefs.setString('serverUrl', _serverController.text.trim());
    await prefs.setString('deviceName', _deviceController.text.trim());
    await prefs.setString('endpoint', _endpoint);
    await prefs.setBool('autoTrack', _autoTrack);
    await prefs.setBool('pollAlerts', _pollAlerts);
  }

  Future<void> _connect({bool silent = false}) async {
    if (_busy) return;
    setState(() {
      _busy = true;
      if (!silent) _status = 'Connecting to Local Focus...';
    });
    try {
      _api?.close();
      _api = LocalFocusApi(_serverController.text);
      final name = _deviceController.text.trim().isEmpty
          ? 'Phone'
          : _deviceController.text.trim();
      _endpoint = _endpoint.isEmpty ? endpointForName(name) : _endpoint;
      final response = await _api!.postJson('/api/mobile/register', {
        'name': name,
        'kind': 'phone',
        'endpoint': _endpoint,
      });
      _endpoint = stringValue(response['endpoint'], _endpoint);
      _connected = true;
      _since = currentSeconds();
      _status = 'Connected to ${_api!.baseUrl}.';
      await _savePrefs();
      _startTimers();
      await _syncNativePhoneTracker();
      await _refresh();
    } catch (error) {
      _connected = false;
      await NativeBridge.stopPhoneTracking();
      _status = silent
          ? 'Could not connect. Check the laptop URL and Wi-Fi.'
          : 'Connection failed: $error';
    } finally {
      if (mounted) {
        setState(() {
          _busy = false;
        });
      }
    }
  }

  void _startTimers() {
    _refreshTimer?.cancel();
    _eventTimer?.cancel();
    _activityTimer?.cancel();
    _refreshTimer = Timer.periodic(
      const Duration(seconds: 10),
      (_) => _refresh(),
    );
    _eventTimer = Timer.periodic(
      const Duration(seconds: 5),
      (_) => _pollEvents(),
    );
    _activityTimer = Timer.periodic(
      const Duration(seconds: 5),
      (_) => _postPhoneActivity(),
    );
  }

  Future<void> _refresh() async {
    final api = _api;
    if (api == null || !_connected) return;
    try {
      final state = await api.getJson('/api/state');
      final report = await api.getJson('/api/report');
      final focus = FocusSession.fromJson(state is Map ? state['focus'] : null);
      final focusReport = await _fetchFocusReport(api, focus);
      if (!mounted) return;
      setState(() {
        _focus = focus;
        _report = SummaryReport.fromJson(report);
        _focusReport = focusReport;
        _status = 'Connected to ${api.baseUrl}.';
      });
    } catch (error) {
      if (!mounted) return;
      setState(() {
        _connected = false;
        _status = 'Lost connection: $error';
      });
    }
  }

  Future<FocusReport> _fetchFocusReport(
    LocalFocusApi api,
    FocusSession? focus,
  ) async {
    final window = periodWindow(_period);
    final target = focus?.target.trim().isNotEmpty == true
        ? focus!.target
        : _targetsController.text.trim();
    final response = await api.getJson('/api/focus-report', {
      'target': target,
      'since': '${window.start}',
      'until': '${window.end}',
      'period': _period.name,
    });
    return FocusReport.fromJson(response);
  }

  Future<void> _startFocus() async {
    final api = _api;
    if (api == null) {
      await _connect();
      return;
    }
    final task = _taskController.text.trim().isEmpty
        ? 'Phone focus'
        : _taskController.text.trim();
    final minutes = int.tryParse(_minutesController.text.trim()) ?? 25;
    final alertMinutes = int.tryParse(_alertController.text.trim()) ?? 1;
    await _runAction(() async {
      await api.getJson('/api/focus/start', {
        'task': task,
        'target': _targetsController.text.trim(),
        'minutes': '$minutes',
        'alertSeconds': '${alertMinutes.clamp(1, 60) * 60}',
        'alertAction': _redirectController.text.trim().isEmpty
            ? 'alert'
            : 'switch',
        'redirectApp': _redirectController.text.trim(),
      });
      await _refresh();
    }, 'Focus started.');
  }

  Future<void> _pauseFocus() async {
    final api = _api;
    if (api == null) return;
    await _runAction(() async {
      await api.getJson('/api/focus/pause');
      await _refresh();
    }, _focus?.paused == true ? 'Focus resumed.' : 'Focus paused.');
  }

  Future<void> _stopFocus() async {
    final api = _api;
    if (api == null) return;
    await _runAction(() async {
      await api.getJson('/api/focus/stop');
      await _refresh();
    }, 'Focus stopped.');
  }

  Future<void> _sendManualActivity() async {
    await _postActivity({
      'device': _deviceController.text.trim(),
      'app': _manualAppController.text.trim().isEmpty
          ? 'Phone activity'
          : _manualAppController.text.trim(),
      'title': _manualTitleController.text.trim(),
      'source': _manualSourceController.text.trim().isEmpty
          ? 'mobile:${_deviceController.text.trim()}'
          : _manualSourceController.text.trim(),
      'category': _manualCategory,
      'timestamp': currentSeconds(),
    });
  }

  Future<void> _postPhoneActivity() async {
    if (!_connected || !_autoTrack) return;
    final granted = await NativeBridge.usageAccessGranted();
    if (granted != _usageAccess && mounted) {
      setState(() => _usageAccess = granted);
    }
    if (!granted && Platform.isAndroid) return;
    final activity = await NativeBridge.latestActivity();
    if (activity == null) return;
    final app = stringValue(activity['app'], '').trim();
    if (app.isEmpty || app == 'Local Focus Mobile') return;
    await _postActivity({
      'device': _deviceController.text.trim(),
      'app': app,
      'title': stringValue(activity['title'], app),
      'source': stringValue(
        activity['source'],
        'mobile:${_deviceController.text.trim()}',
      ),
      if (stringValue(activity['category'], '').isNotEmpty)
        'category': stringValue(activity['category'], ''),
      'timestamp': currentSeconds(),
    }, quiet: true);
  }

  Future<void> _syncNativePhoneTracker() async {
    final api = _api;
    if (!_autoTrack || api == null || !_connected) {
      await NativeBridge.stopPhoneTracking();
      return;
    }
    await NativeBridge.startPhoneTracking(
      serverUrl: api.baseUrl,
      deviceName: _deviceController.text.trim(),
      endpoint: _endpoint,
    );
  }

  Future<void> _postActivity(
    Map<String, dynamic> body, {
    bool quiet = false,
  }) async {
    final api = _api;
    if (api == null || !_connected) return;
    try {
      final response = await api.postJson('/api/mobile/activity', body);
      final category = stringValue(
        response['category'],
        stringValue(body['category'], 'tracked'),
      );
      final label = '${body['app']} - $category';
      if (!mounted) return;
      setState(() {
        _activityLog.insert(0, label);
        if (_activityLog.length > 8) _activityLog.removeLast();
        if (!quiet) _status = 'Sent phone activity: $label';
      });
      if (!quiet) await _refresh();
    } catch (error) {
      if (!mounted) return;
      setState(() {
        if (!quiet) _status = 'Could not send phone activity: $error';
      });
    }
  }

  Future<void> _pollEvents() async {
    final api = _api;
    if (api == null || !_connected || !_pollAlerts || _endpoint.isEmpty) return;
    try {
      final response = await api.getJson('/api/device/events', {
        'since': '$_since',
        'device': _endpoint,
      });
      if (response is! List || response.isEmpty) return;
      final events = response.whereType<Map>().map((event) {
        return event.map((key, value) => MapEntry('$key', value));
      }).toList();
      final maxSince = events
          .map((event) => intValue(event['timestamp'], _since))
          .fold<int>(_since, (a, b) => a > b ? a : b);
      for (final event in events) {
        await NativeBridge.showNotification(
          'Local Focus',
          stringValue(event['message'], 'Focus alert'),
        );
      }
      if (!mounted) return;
      setState(() {
        _since = maxSince;
        _events.insertAll(0, events);
        if (_events.length > 20) _events.removeRange(20, _events.length);
      });
    } catch (_) {}
  }

  Future<void> _runAction(
    Future<void> Function() action,
    String success,
  ) async {
    if (_busy) return;
    setState(() {
      _busy = true;
      _status = 'Working...';
    });
    try {
      await action();
      _status = success;
    } catch (error) {
      _status = 'Action failed: $error';
    } finally {
      if (mounted) {
        setState(() {
          _busy = false;
        });
      }
    }
  }

  @override
  Widget build(BuildContext context) {
    if (_loading) {
      return const Scaffold(body: Center(child: CircularProgressIndicator()));
    }
    return Scaffold(
      appBar: AppBar(
        title: const Text('Local Focus'),
        actions: [
          IconButton(
            tooltip: 'Refresh',
            onPressed: _busy ? null : _refresh,
            icon: const Icon(Icons.refresh),
          ),
        ],
      ),
      body: SafeArea(
        child: IndexedStack(
          index: _tab,
          children: [
            _FocusPage(
              connected: _connected,
              busy: _busy,
              status: _status,
              focus: _focus,
              report: _report,
              taskController: _taskController,
              targetsController: _targetsController,
              minutesController: _minutesController,
              alertController: _alertController,
              redirectController: _redirectController,
              onConnect: () => _connect(),
              onStart: _startFocus,
              onPause: _pauseFocus,
              onStop: _stopFocus,
            ),
            _ReportPage(
              period: _period,
              report: _report,
              focusReport: _focusReport,
              focus: _focus,
              onPeriodChanged: (period) {
                setState(() => _period = period);
                _refresh();
              },
            ),
            _TrackingPage(
              endpoint: _endpoint,
              autoTrack: _autoTrack,
              pollAlerts: _pollAlerts,
              usageAccess: _usageAccess,
              activityLog: _activityLog,
              events: _events,
              manualCategory: _manualCategory,
              manualAppController: _manualAppController,
              manualTitleController: _manualTitleController,
              manualSourceController: _manualSourceController,
              onAutoTrackChanged: (value) async {
                setState(() => _autoTrack = value);
                await _savePrefs();
                await _syncNativePhoneTracker();
                await _postPhoneActivity();
              },
              onPollAlertsChanged: (value) async {
                setState(() => _pollAlerts = value);
                await _savePrefs();
              },
              onRequestUsageAccess: () async {
                await NativeBridge.requestUsageAccess();
                final granted = await NativeBridge.usageAccessGranted();
                setState(() => _usageAccess = granted);
              },
              onManualCategoryChanged: (value) =>
                  setState(() => _manualCategory = value),
              onSendManualActivity: _sendManualActivity,
            ),
            _SettingsPage(
              connected: _connected,
              busy: _busy,
              endpoint: _endpoint,
              serverController: _serverController,
              deviceController: _deviceController,
              onConnect: () => _connect(),
              onSave: () async {
                _endpoint = endpointForName(_deviceController.text);
                await _savePrefs();
                await _connect();
              },
            ),
          ],
        ),
      ),
      bottomNavigationBar: NavigationBar(
        selectedIndex: _tab,
        onDestinationSelected: (index) => setState(() => _tab = index),
        destinations: const [
          NavigationDestination(
            icon: Icon(Icons.flag_outlined),
            selectedIcon: Icon(Icons.flag),
            label: 'Focus',
          ),
          NavigationDestination(
            icon: Icon(Icons.insert_chart_outlined),
            selectedIcon: Icon(Icons.insert_chart),
            label: 'Reports',
          ),
          NavigationDestination(
            icon: Icon(Icons.phone_android_outlined),
            selectedIcon: Icon(Icons.phone_android),
            label: 'Tracking',
          ),
          NavigationDestination(
            icon: Icon(Icons.settings_outlined),
            selectedIcon: Icon(Icons.settings),
            label: 'Settings',
          ),
        ],
      ),
    );
  }
}

class _FocusPage extends StatelessWidget {
  const _FocusPage({
    required this.connected,
    required this.busy,
    required this.status,
    required this.focus,
    required this.report,
    required this.taskController,
    required this.targetsController,
    required this.minutesController,
    required this.alertController,
    required this.redirectController,
    required this.onConnect,
    required this.onStart,
    required this.onPause,
    required this.onStop,
  });

  final bool connected;
  final bool busy;
  final String status;
  final FocusSession? focus;
  final SummaryReport? report;
  final TextEditingController taskController;
  final TextEditingController targetsController;
  final TextEditingController minutesController;
  final TextEditingController alertController;
  final TextEditingController redirectController;
  final VoidCallback onConnect;
  final VoidCallback onStart;
  final VoidCallback onPause;
  final VoidCallback onStop;

  @override
  Widget build(BuildContext context) {
    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
        StatusBanner(
          connected: connected,
          status: status,
          onConnect: onConnect,
        ),
        const SizedBox(height: 12),
        SectionCard(
          title: 'Focus setup',
          subtitle: 'Start or update a focus session from your phone.',
          child: Column(
            children: [
              LabeledField(label: 'Focus task', controller: taskController),
              LabeledField(
                label: 'Focus apps and websites',
                controller: targetsController,
                minLines: 2,
              ),
              Row(
                children: [
                  Expanded(
                    child: LabeledField(
                      label: 'Minutes',
                      controller: minutesController,
                      keyboardType: TextInputType.number,
                    ),
                  ),
                  const SizedBox(width: 10),
                  Expanded(
                    child: LabeledField(
                      label: 'Warn after minutes',
                      controller: alertController,
                      keyboardType: TextInputType.number,
                    ),
                  ),
                ],
              ),
              LabeledField(
                label: 'Move-to app on laptop optional',
                controller: redirectController,
              ),
              const SizedBox(height: 8),
              FilledButton.icon(
                onPressed: busy ? null : onStart,
                icon: const Icon(Icons.play_arrow),
                label: const Text('Start Focus'),
              ),
            ],
          ),
        ),
        const SizedBox(height: 12),
        SectionCard(
          title: 'Current focus session',
          subtitle: focus == null ? 'No active session.' : focus!.target,
          child: focus == null
              ? const EmptyState(
                  text:
                      'Start a focus session above to track phone and laptop activity together.',
                )
              : Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      focus!.task,
                      style: Theme.of(context).textTheme.titleLarge,
                    ),
                    const SizedBox(height: 6),
                    Text(
                      '${formatDuration(focus!.remainingSeconds)} remaining',
                    ),
                    const SizedBox(height: 10),
                    Row(
                      children: [
                        Expanded(
                          child: OutlinedButton.icon(
                            onPressed: busy ? null : onPause,
                            icon: Icon(
                              focus!.paused ? Icons.play_arrow : Icons.pause,
                            ),
                            label: Text(focus!.paused ? 'Resume' : 'Pause'),
                          ),
                        ),
                        const SizedBox(width: 10),
                        Expanded(
                          child: OutlinedButton.icon(
                            onPressed: busy ? null : onStop,
                            icon: const Icon(Icons.stop),
                            label: const Text('Stop'),
                          ),
                        ),
                      ],
                    ),
                  ],
                ),
        ),
        const SizedBox(height: 12),
        if (report != null) DailySummary(report: report!),
      ],
    );
  }
}

class _ReportPage extends StatelessWidget {
  const _ReportPage({
    required this.period,
    required this.report,
    required this.focusReport,
    required this.focus,
    required this.onPeriodChanged,
  });

  final ReportPeriod period;
  final SummaryReport? report;
  final FocusReport? focusReport;
  final FocusSession? focus;
  final ValueChanged<ReportPeriod> onPeriodChanged;

  @override
  Widget build(BuildContext context) {
    final activeReport = focusReport;
    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
        SectionCard(
          title: 'Focus report for',
          subtitle: 'Tap a period to generate the matching report.',
          child: SegmentedButton<ReportPeriod>(
            segments: const [
              ButtonSegment(
                value: ReportPeriod.day,
                label: Text('Day'),
                icon: Icon(Icons.today),
              ),
              ButtonSegment(
                value: ReportPeriod.week,
                label: Text('Week'),
                icon: Icon(Icons.view_week),
              ),
              ButtonSegment(
                value: ReportPeriod.month,
                label: Text('Month'),
                icon: Icon(Icons.calendar_view_month),
              ),
              ButtonSegment(
                value: ReportPeriod.year,
                label: Text('Year'),
                icon: Icon(Icons.event),
              ),
            ],
            selected: {period},
            onSelectionChanged: (value) => onPeriodChanged(value.first),
          ),
        ),
        const SizedBox(height: 12),
        SectionCard(
          title: 'Productive vs distracted',
          subtitle: focus?.target.trim().isEmpty == false
              ? focus!.target
              : 'Current focus targets',
          child: activeReport == null
              ? const EmptyState(
                  text: 'Connect to Local Focus to load report data.',
                )
              : Column(
                  children: [
                    MetricGrid(
                      items: [
                        MetricItem(
                          'Total time',
                          activeReport.totalSeconds,
                          Icons.timer,
                        ),
                        MetricItem(
                          'Productive',
                          activeReport.productiveSeconds,
                          Icons.check_circle_outline,
                        ),
                        MetricItem(
                          'Distracted',
                          activeReport.distractingSeconds,
                          Icons.warning_amber,
                        ),
                        MetricItem(
                          'Idle',
                          activeReport.idleSeconds,
                          Icons.bedtime_outlined,
                        ),
                      ],
                    ),
                    const SizedBox(height: 14),
                    DurationStack(
                      productiveSeconds: activeReport.productiveSeconds,
                      distractingSeconds: activeReport.distractingSeconds,
                      idleSeconds: activeReport.idleSeconds,
                    ),
                  ],
                ),
        ),
        const SizedBox(height: 12),
        SectionCard(
          title: 'Time on focus apps and websites',
          subtitle: 'Split by active and idle time.',
          child: activeReport == null || activeReport.targets.isEmpty
              ? const EmptyState(text: 'No focus target time yet.')
              : Column(
                  children: activeReport.targets.map((target) {
                    return DataRowLine(
                      title: target.target,
                      detail:
                          'Active ${formatDuration(target.seconds)} - Idle ${formatDuration(target.idleSeconds)}',
                      seconds: target.totalSeconds,
                    );
                  }).toList(),
                ),
        ),
        const SizedBox(height: 12),
        SectionCard(
          title: 'Top outside-focus activity',
          subtitle: 'Grouped by app or website.',
          child: activeReport == null || activeReport.outside.isEmpty
              ? const EmptyState(text: 'No outside-focus activity found.')
              : Column(
                  children: activeReport.outside.map((row) {
                    return DataRowLine(
                      title: row.source.isEmpty ? row.app : row.source,
                      detail: row.app,
                      seconds: row.seconds,
                      warning: row.seconds >= 15 * 60,
                    );
                  }).toList(),
                ),
        ),
        if (report != null) ...[
          const SizedBox(height: 12),
          SectionCard(
            title: 'Last 24 hours',
            subtitle: 'Overall laptop and phone activity.',
            child: DailySummary(report: report!),
          ),
        ],
      ],
    );
  }
}

class _TrackingPage extends StatelessWidget {
  const _TrackingPage({
    required this.endpoint,
    required this.autoTrack,
    required this.pollAlerts,
    required this.usageAccess,
    required this.activityLog,
    required this.events,
    required this.manualCategory,
    required this.manualAppController,
    required this.manualTitleController,
    required this.manualSourceController,
    required this.onAutoTrackChanged,
    required this.onPollAlertsChanged,
    required this.onRequestUsageAccess,
    required this.onManualCategoryChanged,
    required this.onSendManualActivity,
  });

  final String endpoint;
  final bool autoTrack;
  final bool pollAlerts;
  final bool usageAccess;
  final List<String> activityLog;
  final List<Map<String, dynamic>> events;
  final String manualCategory;
  final TextEditingController manualAppController;
  final TextEditingController manualTitleController;
  final TextEditingController manualSourceController;
  final ValueChanged<bool> onAutoTrackChanged;
  final ValueChanged<bool> onPollAlertsChanged;
  final VoidCallback onRequestUsageAccess;
  final ValueChanged<String> onManualCategoryChanged;
  final VoidCallback onSendManualActivity;

  @override
  Widget build(BuildContext context) {
    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
        SectionCard(
          title: 'Phone tracking',
          subtitle: Platform.isAndroid
              ? 'Android can track foreground apps after Usage Access is granted.'
              : 'iPhone tracking requires Apple Screen Time entitlements; receiver and manual activity work now.',
          child: Column(
            children: [
              SwitchListTile(
                contentPadding: EdgeInsets.zero,
                title: const Text('Track this phone'),
                subtitle: const Text(
                  'Posts phone foreground activity to the Local Focus report.',
                ),
                value: autoTrack,
                onChanged: onAutoTrackChanged,
              ),
              if (Platform.isAndroid)
                ListTile(
                  contentPadding: EdgeInsets.zero,
                  leading: Icon(
                    usageAccess ? Icons.verified_user : Icons.lock_open,
                  ),
                  title: Text(
                    usageAccess
                        ? 'Usage Access enabled'
                        : 'Usage Access required',
                  ),
                  subtitle: const Text('Needed for phone app tracking.'),
                  trailing: TextButton(
                    onPressed: onRequestUsageAccess,
                    child: const Text('Open'),
                  ),
                ),
              SwitchListTile(
                contentPadding: EdgeInsets.zero,
                title: const Text('Receive focus alerts'),
                subtitle: Text(
                  endpoint.isEmpty ? 'Register the phone first.' : endpoint,
                ),
                value: pollAlerts,
                onChanged: onPollAlertsChanged,
              ),
            ],
          ),
        ),
        const SizedBox(height: 12),
        SectionCard(
          title: 'Send activity test',
          subtitle: 'Use this to test phone reporting immediately.',
          child: Column(
            children: [
              LabeledField(label: 'App', controller: manualAppController),
              LabeledField(label: 'Title', controller: manualTitleController),
              LabeledField(
                label: 'Website or source',
                controller: manualSourceController,
              ),
              SegmentedButton<String>(
                segments: const [
                  ButtonSegment(value: 'productive', label: Text('Productive')),
                  ButtonSegment(
                    value: 'distracting',
                    label: Text('Distracted'),
                  ),
                  ButtonSegment(value: 'idle', label: Text('Idle')),
                ],
                selected: {manualCategory},
                onSelectionChanged: (value) =>
                    onManualCategoryChanged(value.first),
              ),
              const SizedBox(height: 10),
              FilledButton.icon(
                onPressed: onSendManualActivity,
                icon: const Icon(Icons.send),
                label: const Text('Send Activity'),
              ),
            ],
          ),
        ),
        const SizedBox(height: 12),
        SectionCard(
          title: 'Recent phone samples',
          subtitle: 'Last samples sent by this app.',
          child: activityLog.isEmpty
              ? const EmptyState(text: 'No phone activity sent yet.')
              : Column(
                  children: activityLog.map((item) {
                    return ListTile(
                      contentPadding: EdgeInsets.zero,
                      leading: const Icon(Icons.phone_android),
                      title: Text(item),
                    );
                  }).toList(),
                ),
        ),
        const SizedBox(height: 12),
        SectionCard(
          title: 'Receiver alerts',
          subtitle: 'Alerts sent to this phone.',
          child: events.isEmpty
              ? const EmptyState(text: 'No receiver alerts yet.')
              : Column(
                  children: events.map((event) {
                    return ListTile(
                      contentPadding: EdgeInsets.zero,
                      leading: const Icon(Icons.notifications_active_outlined),
                      title: Text(stringValue(event['event'], 'Alert')),
                      subtitle: Text(
                        stringValue(event['message'], 'Focus alert'),
                      ),
                    );
                  }).toList(),
                ),
        ),
      ],
    );
  }
}

class _SettingsPage extends StatelessWidget {
  const _SettingsPage({
    required this.connected,
    required this.busy,
    required this.endpoint,
    required this.serverController,
    required this.deviceController,
    required this.onConnect,
    required this.onSave,
  });

  final bool connected;
  final bool busy;
  final String endpoint;
  final TextEditingController serverController;
  final TextEditingController deviceController;
  final VoidCallback onConnect;
  final VoidCallback onSave;

  @override
  Widget build(BuildContext context) {
    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
        SectionCard(
          title: 'Laptop connection',
          subtitle:
              'Use the Local Focus connect URL from the desktop dashboard.',
          child: Column(
            children: [
              LabeledField(
                label: 'Laptop URL',
                controller: serverController,
                keyboardType: TextInputType.url,
              ),
              LabeledField(label: 'Phone name', controller: deviceController),
              Align(
                alignment: Alignment.centerLeft,
                child: Text(
                  endpoint.isEmpty
                      ? 'Endpoint will be created on connect.'
                      : 'Endpoint: $endpoint',
                  style: Theme.of(context).textTheme.bodySmall,
                ),
              ),
              const SizedBox(height: 10),
              Row(
                children: [
                  Expanded(
                    child: FilledButton.icon(
                      onPressed: busy ? null : onSave,
                      icon: const Icon(Icons.save),
                      label: const Text('Save and Connect'),
                    ),
                  ),
                  const SizedBox(width: 10),
                  IconButton.outlined(
                    tooltip: 'Connect',
                    onPressed: busy ? null : onConnect,
                    icon: Icon(connected ? Icons.cloud_done : Icons.cloud_off),
                  ),
                ],
              ),
            ],
          ),
        ),
        const SizedBox(height: 12),
        const SectionCard(
          title: 'Install notes',
          subtitle:
              'Android can be installed with an APK. iPhone runs from Xcode or TestFlight.',
          child: Text(
            'Keep the laptop and phone on the same Wi-Fi. The desktop app keeps all data local and the phone posts to the laptop URL you set above.',
          ),
        ),
      ],
    );
  }
}

class SectionCard extends StatelessWidget {
  const SectionCard({
    super.key,
    required this.title,
    required this.subtitle,
    required this.child,
  });

  final String title;
  final String subtitle;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              title,
              style: Theme.of(
                context,
              ).textTheme.titleMedium?.copyWith(fontWeight: FontWeight.w800),
            ),
            const SizedBox(height: 4),
            Text(subtitle, style: Theme.of(context).textTheme.bodySmall),
            const SizedBox(height: 14),
            child,
          ],
        ),
      ),
    );
  }
}

class StatusBanner extends StatelessWidget {
  const StatusBanner({
    super.key,
    required this.connected,
    required this.status,
    required this.onConnect,
  });

  final bool connected;
  final String status;
  final VoidCallback onConnect;

  @override
  Widget build(BuildContext context) {
    final color = connected
        ? Theme.of(context).colorScheme.primary
        : Theme.of(context).colorScheme.error;
    return Container(
      padding: const EdgeInsets.all(14),
      decoration: BoxDecoration(
        color: color.withValues(alpha: 0.10),
        border: Border.all(color: color.withValues(alpha: 0.25)),
        borderRadius: BorderRadius.circular(10),
      ),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Icon(connected ? Icons.wifi_tethering : Icons.wifi_off, color: color),
          const SizedBox(width: 10),
          Expanded(child: Text(status)),
          TextButton(onPressed: onConnect, child: const Text('Connect')),
        ],
      ),
    );
  }
}

class LabeledField extends StatelessWidget {
  const LabeledField({
    super.key,
    required this.label,
    required this.controller,
    this.keyboardType,
    this.minLines = 1,
  });

  final String label;
  final TextEditingController controller;
  final TextInputType? keyboardType;
  final int minLines;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.only(bottom: 10),
      child: TextField(
        controller: controller,
        keyboardType: keyboardType,
        minLines: minLines,
        maxLines: minLines > 1 ? 4 : 1,
        decoration: InputDecoration(
          labelText: label,
          border: const OutlineInputBorder(),
          isDense: true,
        ),
      ),
    );
  }
}

class DailySummary extends StatelessWidget {
  const DailySummary({super.key, required this.report});

  final SummaryReport report;

  @override
  Widget build(BuildContext context) {
    return SectionCard(
      title: 'Today at a glance',
      subtitle: 'Total time is productive plus distracted plus idle.',
      child: Column(
        children: [
          MetricGrid(
            items: [
              MetricItem('Total time', report.totalSeconds, Icons.timer),
              MetricItem(
                'Productive',
                report.productiveSeconds,
                Icons.check_circle_outline,
              ),
              MetricItem(
                'Distracted',
                report.distractingSeconds,
                Icons.warning_amber,
              ),
              MetricItem('Idle', report.idleSeconds, Icons.bedtime_outlined),
            ],
          ),
          const SizedBox(height: 14),
          DurationStack(
            productiveSeconds: report.productiveSeconds,
            distractingSeconds: report.distractingSeconds,
            idleSeconds: report.idleSeconds,
          ),
          if (report.topApps.isNotEmpty) ...[
            const SizedBox(height: 12),
            ...report.topApps.take(5).map((row) {
              return DataRowLine(
                title: row.source.isEmpty ? row.app : row.source,
                detail: row.app,
                seconds: row.seconds,
              );
            }),
          ],
        ],
      ),
    );
  }
}

class MetricGrid extends StatelessWidget {
  const MetricGrid({super.key, required this.items});

  final List<MetricItem> items;

  @override
  Widget build(BuildContext context) {
    return GridView.count(
      crossAxisCount: 2,
      childAspectRatio: 2.8,
      mainAxisSpacing: 8,
      crossAxisSpacing: 8,
      shrinkWrap: true,
      physics: const NeverScrollableScrollPhysics(),
      children: items.map((item) => MetricTile(item: item)).toList(),
    );
  }
}

class MetricItem {
  const MetricItem(this.label, this.seconds, this.icon);
  final String label;
  final int seconds;
  final IconData icon;
}

class MetricTile extends StatelessWidget {
  const MetricTile({super.key, required this.item});

  final MetricItem item;

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.all(10),
      decoration: BoxDecoration(
        border: Border.all(
          color: Theme.of(context).dividerColor.withValues(alpha: 0.55),
        ),
        borderRadius: BorderRadius.circular(8),
      ),
      child: Row(
        children: [
          Icon(item.icon, size: 20),
          const SizedBox(width: 8),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                Text(
                  item.label,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: Theme.of(context).textTheme.labelMedium,
                ),
                Text(
                  formatDuration(item.seconds),
                  style: Theme.of(context).textTheme.titleMedium?.copyWith(
                    fontWeight: FontWeight.w800,
                  ),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}

class DurationStack extends StatelessWidget {
  const DurationStack({
    super.key,
    required this.productiveSeconds,
    required this.distractingSeconds,
    required this.idleSeconds,
  });

  final int productiveSeconds;
  final int distractingSeconds;
  final int idleSeconds;

  @override
  Widget build(BuildContext context) {
    final total = (productiveSeconds + distractingSeconds + idleSeconds).clamp(
      1,
      1 << 31,
    );
    return Column(
      children: [
        ClipRRect(
          borderRadius: BorderRadius.circular(999),
          child: SizedBox(
            height: 18,
            child: Row(
              children: [
                _segment(context, productiveSeconds / total, productiveColor),
                _segment(context, distractingSeconds / total, distractingColor),
                _segment(context, idleSeconds / total, idleColor),
              ],
            ),
          ),
        ),
        const SizedBox(height: 10),
        Wrap(
          spacing: 12,
          runSpacing: 8,
          children: const [
            LegendDot(label: 'Productive', color: productiveColor),
            LegendDot(label: 'Distracted', color: distractingColor),
            LegendDot(label: 'Idle', color: idleColor),
          ],
        ),
      ],
    );
  }

  Widget _segment(BuildContext context, double flex, Color color) {
    return Flexible(
      flex: (flex * 1000).round().clamp(1, 1000),
      child: Container(color: color),
    );
  }
}

class LegendDot extends StatelessWidget {
  const LegendDot({super.key, required this.label, required this.color});
  final String label;
  final Color color;

  @override
  Widget build(BuildContext context) {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Container(
          width: 10,
          height: 10,
          decoration: BoxDecoration(color: color, shape: BoxShape.circle),
        ),
        const SizedBox(width: 6),
        Text(label, style: Theme.of(context).textTheme.labelMedium),
      ],
    );
  }
}

class DataRowLine extends StatelessWidget {
  const DataRowLine({
    super.key,
    required this.title,
    required this.detail,
    required this.seconds,
    this.warning = false,
  });

  final String title;
  final String detail;
  final int seconds;
  final bool warning;

  @override
  Widget build(BuildContext context) {
    final color = warning
        ? Theme.of(context).colorScheme.error
        : Theme.of(context).colorScheme.onSurface;
    return ListTile(
      contentPadding: EdgeInsets.zero,
      title: Text(
        title,
        maxLines: 2,
        overflow: TextOverflow.ellipsis,
        style: TextStyle(color: color),
      ),
      subtitle: Text(detail, maxLines: 1, overflow: TextOverflow.ellipsis),
      trailing: Text(
        formatDuration(seconds),
        style: TextStyle(fontWeight: FontWeight.w800, color: color),
      ),
    );
  }
}

class EmptyState extends StatelessWidget {
  const EmptyState({super.key, required this.text});
  final String text;

  @override
  Widget build(BuildContext context) {
    return Container(
      width: double.infinity,
      padding: const EdgeInsets.all(14),
      decoration: BoxDecoration(
        border: Border.all(
          color: Theme.of(context).dividerColor.withValues(alpha: 0.55),
        ),
        borderRadius: BorderRadius.circular(8),
      ),
      child: Text(text),
    );
  }
}

class PeriodWindow {
  const PeriodWindow(this.start, this.end);
  final int start;
  final int end;
}

const productiveColor = Color(0xff2f855a);
const distractingColor = Color(0xffc24141);
const idleColor = Color(0xffb7791f);

PeriodWindow periodWindow(ReportPeriod period) {
  final now = DateTime.now();
  DateTime start;
  DateTime end;
  switch (period) {
    case ReportPeriod.day:
      start = DateTime(now.year, now.month, now.day);
      end = start.add(const Duration(days: 1));
      break;
    case ReportPeriod.week:
      start = DateTime(
        now.year,
        now.month,
        now.day,
      ).subtract(Duration(days: now.weekday - 1));
      end = start.add(const Duration(days: 7));
      break;
    case ReportPeriod.month:
      start = DateTime(now.year, now.month);
      end = DateTime(now.year, now.month + 1);
      break;
    case ReportPeriod.year:
      start = DateTime(now.year);
      end = DateTime(now.year + 1);
      break;
  }
  return PeriodWindow(
    start.millisecondsSinceEpoch ~/ 1000,
    end.millisecondsSinceEpoch ~/ 1000,
  );
}

String endpointForName(String name) {
  final slug = name
      .trim()
      .toLowerCase()
      .replaceAll(RegExp(r'[^a-z0-9]+'), '-')
      .replaceAll(RegExp(r'^-+|-+$'), '');
  return 'mobile:${slug.isEmpty ? 'phone' : slug}';
}

String formatDuration(int seconds) {
  if (seconds <= 0) return '0s';
  if (seconds < 60) return '${seconds}s';
  if (seconds >= 3600) {
    final hours = seconds ~/ 3600;
    final minutes = (seconds % 3600) ~/ 60;
    return minutes == 0 ? '${hours}h' : '${hours}h ${minutes}m';
  }
  final minutes = seconds ~/ 60;
  final rest = seconds % 60;
  return rest == 0 ? '${minutes}m' : '${minutes}m ${rest}s';
}

int currentSeconds() => DateTime.now().millisecondsSinceEpoch ~/ 1000;

String stringValue(dynamic value, String fallback) {
  if (value == null) return fallback;
  final text = '$value'.trim();
  return text.isEmpty ? fallback : text;
}

int intValue(dynamic value, [int fallback = 0]) {
  if (value is int) return value;
  if (value is num) return value.round();
  return int.tryParse('$value') ?? fallback;
}
