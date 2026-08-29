// Companion mode (used on iOS / non-Android): connects to a Local Focus
// instance running on a Mac over WiFi. iOS cannot run the embedded server
// or monitor other apps, so this is the most an iPhone can do.
import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:shared_preferences/shared_preferences.dart';

const defaultServerUrl = '';
const nativeChannelName = 'local_focus/native';

const _connectCodeAlphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';

/// Decodes the 8-character connection code shown in the Local Focus desktop
/// dashboard back into a server URL. The desktop encodes four IPv4 address
/// octets plus a checksum byte as Crockford base32 (the port is always 4799).
/// Returns null when the code is malformed or fails its checksum.
String? decodeConnectCode(String raw) {
  var cleaned = raw.toUpperCase().replaceAll(RegExp(r'[^0-9A-Z]'), '');
  // Map Crockford's visually ambiguous letters onto their digits.
  cleaned = cleaned.replaceAll('I', '1').replaceAll('L', '1').replaceAll('O', '0');
  if (cleaned.length != 8) return null;
  final bits = StringBuffer();
  for (final char in cleaned.split('')) {
    final index = _connectCodeAlphabet.indexOf(char);
    if (index < 0) return null;
    bits.write(index.toRadixString(2).padLeft(5, '0'));
  }
  final binary = bits.toString();
  final bytes = <int>[];
  for (var i = 0; i < 40; i += 8) {
    bytes.add(int.parse(binary.substring(i, i + 8), radix: 2));
  }
  final checksum = (bytes[0] + bytes[1] + bytes[2] + bytes[3]) & 0xff;
  if (checksum != bytes[4]) return null;
  return 'http://${bytes[0]}.${bytes[1]}.${bytes[2]}.${bytes[3]}:4799';
}


// Switchable look-and-feel "templates", each an original take on a 2026 trend:
// Vibrant (Gen-Z violet→pink), Cyber (neon dark-first), Clay (soft tactile
// pastel), Minimal (crisp high-contrast), Professional (refined navy/slate).
enum FocusTemplate { vibrant, cyber, clay, minimal, professional }

extension FocusTemplateInfo on FocusTemplate {
  String get label => switch (this) {
        FocusTemplate.vibrant => '✨ Vibrant',
        FocusTemplate.cyber => '🌌 Cyber',
        FocusTemplate.clay => '🫧 Clay',
        FocusTemplate.minimal => '◾ Minimal',
        FocusTemplate.professional => '💼 Professional',
      };
}

/// Current template, listened to by the app so changes apply live.
final ValueNotifier<FocusTemplate> focusTemplate =
    ValueNotifier<FocusTemplate>(FocusTemplate.vibrant);

FocusTemplate _parseTemplate(String? value) => FocusTemplate.values.firstWhere(
      (t) => t.name == value,
      orElse: () => FocusTemplate.vibrant,
    );

Future<void> loadFocusTemplate() async {
  try {
    final prefs = await SharedPreferences.getInstance();
    focusTemplate.value = _parseTemplate(prefs.getString('focusTemplate'));
  } catch (_) {}
}

Future<void> setFocusTemplate(FocusTemplate template) async {
  focusTemplate.value = template;
  HapticFeedback.selectionClick();
  try {
    final prefs = await SharedPreferences.getInstance();
    await prefs.setString('focusTemplate', template.name);
  } catch (_) {}
}

ThemeMode focusThemeMode(FocusTemplate template) => switch (template) {
      FocusTemplate.cyber => ThemeMode.dark,
      FocusTemplate.clay => ThemeMode.light,
      FocusTemplate.minimal => ThemeMode.light,
      FocusTemplate.professional => ThemeMode.light,
      FocusTemplate.vibrant => ThemeMode.system,
    };

/// Gradient used for the app bar and hero accents, derived from the live scheme.
LinearGradient focusGradientOf(ColorScheme scheme) => LinearGradient(
      colors: [scheme.primary, scheme.secondary],
      begin: Alignment.topLeft,
      end: Alignment.bottomRight,
    );

ThemeData buildLocalFocusTheme(FocusTemplate template, Brightness brightness) {
  late final Brightness effective;
  late final Color primary, secondary, bg, surface, fieldFill, navBg;
  double cardElevation = 0;
  Color cardShadow = Colors.transparent;
  double cardRadius = 24;
  double btnRadius = 18;
  Color cardBorder = Colors.transparent;
  switch (template) {
    case FocusTemplate.cyber:
      effective = Brightness.dark;
      primary = const Color(0xFFB66BFF); // neon violet
      secondary = const Color(0xFF22D3EE); // neon cyan
      bg = const Color(0xFF07060F);
      surface = const Color(0xFF130F24);
      fieldFill = const Color(0xFF1B1533);
      navBg = const Color(0xFF0D0A1B);
      break;
    case FocusTemplate.clay:
      effective = Brightness.light;
      primary = const Color(0xFF8B7CF6); // soft violet
      secondary = const Color(0xFFF59EBC); // soft pink
      bg = const Color(0xFFEDE9F6);
      surface = const Color(0xFFFBF9FF);
      fieldFill = const Color(0xFFF0EBFB);
      navBg = const Color(0xFFFBF9FF);
      cardElevation = 6; // soft tactile clay lift
      cardShadow = const Color(0xFF8B7CF6).withValues(alpha: 0.20);
      break;
    case FocusTemplate.vibrant:
      effective = brightness;
      final dark = brightness == Brightness.dark;
      primary = dark ? const Color(0xFFA78BFA) : const Color(0xFF7C3AED);
      secondary = dark ? const Color(0xFFF472B6) : const Color(0xFFEC4899);
      bg = dark ? const Color(0xFF120E22) : const Color(0xFFF5F2FF);
      surface = dark ? const Color(0xFF1D1838) : Colors.white;
      fieldFill = dark ? const Color(0xFF171331) : const Color(0xFFF1EBFF);
      navBg = dark ? const Color(0xFF1D1838) : Colors.white;
      break;
    case FocusTemplate.minimal:
      // Crisp, high-contrast, near-black accent, flat surfaces, hairline borders.
      effective = Brightness.light;
      primary = const Color(0xFF111111);
      secondary = const Color(0xFF111111);
      bg = const Color(0xFFFFFFFF);
      surface = const Color(0xFFFFFFFF);
      fieldFill = const Color(0xFFF4F4F5);
      navBg = const Color(0xFFFFFFFF);
      cardBorder = const Color(0xFFE4E4E7);
      cardRadius = 14;
      btnRadius = 10;
      break;
    case FocusTemplate.professional:
      // Refined, restrained navy/slate; understated.
      effective = Brightness.light;
      primary = const Color(0xFF1E40AF);
      secondary = const Color(0xFF2563EB);
      bg = const Color(0xFFF8FAFC);
      surface = const Color(0xFFFFFFFF);
      fieldFill = const Color(0xFFF1F5F9);
      navBg = const Color(0xFFFFFFFF);
      cardBorder = const Color(0xFFE2E8F0);
      cardRadius = 16;
      btnRadius = 12;
      break;
  }
  final scheme = ColorScheme.fromSeed(
    seedColor: primary,
    primary: primary,
    secondary: secondary,
    error: const Color(0xFFF43F5E),
    surface: surface,
    brightness: effective,
  );
  final cardShape = RoundedRectangleBorder(
    borderRadius: BorderRadius.circular(cardRadius),
    side: cardBorder == Colors.transparent
        ? BorderSide.none
        : BorderSide(color: cardBorder),
  );
  final buttonShape = RoundedRectangleBorder(borderRadius: BorderRadius.circular(btnRadius));
  final fieldBorder = OutlineInputBorder(
    borderRadius: BorderRadius.circular(16),
    borderSide: BorderSide.none,
  );
  return ThemeData(
    colorScheme: scheme,
    useMaterial3: true,
    scaffoldBackgroundColor: bg,
    cardTheme: CardThemeData(
      elevation: cardElevation,
      shadowColor: cardShadow,
      margin: EdgeInsets.zero,
      color: surface,
      surfaceTintColor: Colors.transparent,
      shape: cardShape,
    ),
    appBarTheme: AppBarTheme(
      backgroundColor: Colors.transparent,
      foregroundColor: scheme.onSurface,
      elevation: 0,
      scrolledUnderElevation: 0,
      centerTitle: false,
      titleTextStyle: TextStyle(
        fontSize: 22,
        fontWeight: FontWeight.w900,
        color: scheme.onSurface,
      ),
    ),
    filledButtonTheme: FilledButtonThemeData(
      style: FilledButton.styleFrom(
        shape: buttonShape,
        padding: const EdgeInsets.symmetric(vertical: 14, horizontal: 18),
        textStyle: const TextStyle(fontWeight: FontWeight.w800, fontSize: 15),
      ),
    ),
    elevatedButtonTheme: ElevatedButtonThemeData(
      style: ElevatedButton.styleFrom(
        shape: buttonShape,
        padding: const EdgeInsets.symmetric(vertical: 14, horizontal: 18),
        textStyle: const TextStyle(fontWeight: FontWeight.w800),
      ),
    ),
    outlinedButtonTheme: OutlinedButtonThemeData(
      style: OutlinedButton.styleFrom(
        shape: buttonShape,
        padding: const EdgeInsets.symmetric(vertical: 13, horizontal: 16),
        textStyle: const TextStyle(fontWeight: FontWeight.w700),
      ),
    ),
    textButtonTheme: TextButtonThemeData(
      style: TextButton.styleFrom(
        foregroundColor: scheme.primary,
        textStyle: const TextStyle(fontWeight: FontWeight.w800),
      ),
    ),
    inputDecorationTheme: InputDecorationTheme(
      filled: true,
      fillColor: fieldFill,
      isDense: true,
      border: fieldBorder,
      enabledBorder: fieldBorder,
      focusedBorder: OutlineInputBorder(
        borderRadius: BorderRadius.circular(16),
        borderSide: BorderSide(color: scheme.primary, width: 2),
      ),
    ),
    navigationBarTheme: NavigationBarThemeData(
      backgroundColor: navBg,
      surfaceTintColor: Colors.transparent,
      indicatorColor: primary.withValues(alpha: 0.20),
      elevation: 0,
      labelTextStyle: WidgetStatePropertyAll(
        TextStyle(fontSize: 12, fontWeight: FontWeight.w700, color: scheme.onSurface),
      ),
    ),
    chipTheme: ChipThemeData(
      shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(999)),
      side: BorderSide.none,
    ),
    snackBarTheme: SnackBarThemeData(
      behavior: SnackBarBehavior.floating,
      shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(16)),
    ),
  );
}

class LocalFocusMobileApp extends StatelessWidget {
  const LocalFocusMobileApp({super.key});

  @override
  Widget build(BuildContext context) {
    return ValueListenableBuilder<FocusTemplate>(
      valueListenable: focusTemplate,
      builder: (context, template, _) {
        return MaterialApp(
          title: 'Local Focus',
          debugShowCheckedModeBanner: false,
          themeMode: focusThemeMode(template),
          theme: buildLocalFocusTheme(template, Brightness.light),
          darkTheme: buildLocalFocusTheme(template, Brightness.dark),
          home: const MobileShell(),
        );
      },
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

  // Tell the native layer whether a focus session is active so it can warn the
  // user (via a scheduled local notification) if they leave the app on iOS.
  static Future<void> setFocusState({
    required bool active,
    required int alertDelaySeconds,
    required String task,
  }) async {
    try {
      await _channel.invokeMethod<void>('setFocusState', {
        'active': active,
        'alertDelaySeconds': alertDelaySeconds,
        'task': task,
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
    required this.highFocusMode,
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
  final bool highFocusMode;

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
      highFocusMode: value['highFocusMode'] == true,
    );
  }
}

class BlockRule {
  const BlockRule({
    required this.target,
    required this.mode,
    required this.hasPassword,
  });

  final String target;
  final String mode; // 'full' or 'password'
  final bool hasPassword;

  static BlockRule fromJson(dynamic value) {
    final map = value is Map ? value : <String, dynamic>{};
    return BlockRule(
      target: stringValue(map['target'], ''),
      mode: stringValue(map['mode'], 'full'),
      hasPassword: map['hasPassword'] == true,
    );
  }
}

class ReportArchive {
  const ReportArchive({required this.archivedAt, required this.report});

  final int archivedAt;
  final SummaryReport report;

  static ReportArchive fromJson(dynamic value) {
    final map = value is Map ? value : <String, dynamic>{};
    return ReportArchive(
      archivedAt: intValue(map['archivedAt']),
      report: SummaryReport.fromJson(map['report']),
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
  final _codeController = TextEditingController();
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
  final _journalController = TextEditingController();
  final _blockController = TextEditingController();

  int _tab = 0;
  bool _loading = true;
  bool _busy = false;
  bool _connected = false;
  bool _autoTrack = false;
  bool _pollAlerts = true;
  bool _usageAccess = false;
  String _endpoint = '';
  String _status =
      'Enter the connection code from the Local Focus desktop dashboard, then connect.';
  String _manualCategory = 'productive';
  int _since = currentSeconds();

  FocusSession? _focus;
  SummaryReport? _report;
  FocusReport? _focusReport;
  ReportPeriod _period = ReportPeriod.day;
  final List<Map<String, dynamic>> _events = [];
  final List<String> _activityLog = [];

  List<BlockRule> _blocks = [];
  List<ReportArchive> _reportHistory = [];
  bool _journalEnabled = true;
  String _journalReminderMode = 'evening';
  String _journalDate = '';
  String _journalStatus = 'Loading journal...';

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
    _codeController.dispose();
    _deviceController.dispose();
    _taskController.dispose();
    _targetsController.dispose();
    _minutesController.dispose();
    _alertController.dispose();
    _redirectController.dispose();
    _manualAppController.dispose();
    _manualTitleController.dispose();
    _manualSourceController.dispose();
    _journalController.dispose();
    _blockController.dispose();
    super.dispose();
  }

  Future<void> _load() async {
    final prefs = await SharedPreferences.getInstance();
    final nativeName = await NativeBridge.deviceName();
    _prefs = prefs;
    final savedServerUrl = prefs.getString('serverUrl') ?? defaultServerUrl;
    _serverController.text = savedServerUrl;
    // Restore the previously typed connection code so it's already filled in.
    _codeController.text = prefs.getString('connectCode') ?? '';
    _deviceController.text = prefs.getString('deviceName') ?? nativeName;
    _endpoint =
        prefs.getString('endpoint') ?? endpointForName(_deviceController.text);
    _autoTrack = prefs.getBool('autoTrack') ?? false;
    _pollAlerts = prefs.getBool('pollAlerts') ?? true;
    _usageAccess = await NativeBridge.usageAccessGranted();
    final hasSavedConnection = savedServerUrl.trim().isNotEmpty;
    setState(() {
      _loading = false;
      _status = hasSavedConnection
          ? 'Reconnecting to your saved Local Focus...'
          : 'Enter the connection code from the Local Focus desktop dashboard, then connect.';
    });
    // Auto-reconnect with the saved connection so the code only has to be
    // entered once.
    if (hasSavedConnection) {
      await _connect(silent: true);
    }
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

  Future<void> _applyConnectCode() async {
    final url = decodeConnectCode(_codeController.text);
    if (url == null) {
      setState(() {
        _status =
            'That connection code is not valid. Re-enter the 8-character code '
            'shown in the Local Focus desktop dashboard.';
      });
      return;
    }
    _serverController.text = url;
    _endpoint = endpointForName(_deviceController.text);
    await _savePrefs();
    // Remember the code so the user never has to type it again.
    final prefs = _prefs;
    if (prefs != null) {
      await prefs.setString('connectCode', _codeController.text.trim().toUpperCase());
    }
    await _connect();
  }

  Future<void> _showConnectCodeDialog() async {
    final submitted = await showDialog<bool>(
      context: context,
      builder: (dialogContext) {
        return AlertDialog(
          title: const Text('Connect with code'),
          content: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              const Text(
                'Enter the 8-character connection code shown in the Local Focus '
                'app on your computer.',
              ),
              const SizedBox(height: 12),
              TextField(
                controller: _codeController,
                autofocus: true,
                textCapitalization: TextCapitalization.characters,
                decoration: const InputDecoration(
                  labelText: 'Connection code',
                  hintText: 'XXXX-XXXX',
                ),
                onSubmitted: (_) => Navigator.of(dialogContext).pop(true),
              ),
            ],
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.of(dialogContext).pop(false),
              child: const Text('Cancel'),
            ),
            FilledButton(
              onPressed: () => Navigator.of(dialogContext).pop(true),
              child: const Text('Connect'),
            ),
          ],
        );
      },
    );
    if (submitted == true) {
      await _applyConnectCode();
    }
  }

  Future<void> _connect({bool silent = false}) async {
    if (_busy) return;
    if (_serverController.text.trim().isEmpty) {
      if (!silent) {
        setState(() {
          _status = 'Enter a connection code or direct link first.';
        });
      }
      return;
    }
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
      await _loadJournal();
    } catch (error) {
      _connected = false;
      await NativeBridge.stopPhoneTracking();
      _status = silent
          ? 'Could not connect. Re-enter the connection code or check the direct link.'
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
      final blocks = (state is Map && state['blockedRules'] is List)
          ? (state['blockedRules'] as List).map(BlockRule.fromJson).toList()
          : <BlockRule>[];
      final journal = state is Map ? state['journal'] : null;
      final journalSettings = journal is Map ? journal['settings'] : null;
      var history = _reportHistory;
      try {
        final historyJson = await api.getJson('/api/report/history');
        if (historyJson is List) {
          history = historyJson.map(ReportArchive.fromJson).toList();
        }
      } catch (_) {}
      if (!mounted) return;
      setState(() {
        _focus = focus;
        _report = SummaryReport.fromJson(report);
        _focusReport = focusReport;
        _blocks = blocks;
        _reportHistory = history;
        if (journalSettings is Map) {
          _journalEnabled = journalSettings['enabled'] != false;
          _journalReminderMode =
              stringValue(journalSettings['reminderMode'], _journalReminderMode);
        }
        _status = 'Connected to ${api.baseUrl}.';
      });
      _syncFocusStateToNative();
    } catch (error) {
      if (!mounted) return;
      setState(() {
        _connected = false;
        _status = 'Lost connection: $error';
      });
    }
  }

  // Keep the native layer informed of focus state so it can warn the user if
  // they leave the app during a session (the only phone-side distraction signal
  // available on iOS).
  void _syncFocusStateToNative() {
    final focus = _focus;
    final active = _connected && _pollAlerts && focus != null && !focus.paused;
    NativeBridge.setFocusState(
      active: active,
      alertDelaySeconds: focus?.alertDelaySeconds ?? 60,
      task: focus?.task ?? '',
    );
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

  Future<void> _toggleHighFocus(bool enabled) async {
    final api = _api;
    if (api == null) return;
    await _runAction(() async {
      await api.getJson('/api/focus/high-focus', {'enabled': enabled ? '1' : '0'});
      await _refresh();
    }, enabled ? 'High-Focus mode on.' : 'High-Focus mode off.');
  }

  Future<void> _addBlock() async {
    final api = _api;
    final keyword = _blockController.text.trim();
    if (api == null || keyword.isEmpty) return;
    await _runAction(() async {
      await api.getJson('/api/block/add', {'keyword': keyword, 'mode': 'full'});
      _blockController.clear();
      await _refresh();
    }, 'Blocked "$keyword".');
  }

  Future<void> _removeBlock(String target) async {
    final api = _api;
    if (api == null || target.isEmpty) return;
    await _runAction(() async {
      await api.getJson('/api/block/remove', {'keyword': target});
      await _refresh();
    }, 'Removed "$target".');
  }

  Future<void> _loadJournal({String? date}) async {
    final api = _api;
    if (api == null || !_connected) return;
    try {
      final query = <String, String>{};
      if (date != null && date.isNotEmpty) query['date'] = date;
      final response = await api.getJson(
        '/api/journal/entry',
        query.isEmpty ? null : query,
      );
      if (!mounted) return;
      setState(() {
        _journalDate =
            stringValue(response is Map ? response['date'] : null, _journalDate);
        _journalController.text =
            stringValue(response is Map ? response['text'] : null, '');
        _journalStatus = 'Loaded entry for $_journalDate.';
      });
    } catch (error) {
      if (!mounted) return;
      setState(() => _journalStatus = 'Could not load journal: $error');
    }
  }

  Future<void> _saveJournal() async {
    final api = _api;
    if (api == null) return;
    setState(() => _journalStatus = 'Saving...');
    try {
      final response = await api.postJson('/api/journal/save', {
        'date': _journalDate,
        'text': _journalController.text,
      });
      if (!mounted) return;
      setState(() {
        _journalDate =
            stringValue(response is Map ? response['date'] : null, _journalDate);
        _journalStatus = 'Saved entry for $_journalDate.';
      });
    } catch (error) {
      if (!mounted) return;
      setState(() => _journalStatus = 'Could not save journal: $error');
    }
  }

  Future<void> _saveJournalSettings({bool? enabled, String? reminderMode}) async {
    final api = _api;
    if (api == null) return;
    final nextEnabled = enabled ?? _journalEnabled;
    final nextMode = reminderMode ?? _journalReminderMode;
    setState(() {
      _journalEnabled = nextEnabled;
      _journalReminderMode = nextMode;
    });
    await _runAction(() async {
      await api.getJson('/api/journal/settings', {
        'enabled': nextEnabled ? '1' : '0',
        'reminderMode': nextMode,
      });
    }, 'Journal settings saved.');
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
        foregroundColor: Colors.white,
        flexibleSpace: DecoratedBox(
          decoration: BoxDecoration(
            gradient: focusGradientOf(Theme.of(context).colorScheme),
          ),
        ),
        title: const Text(
          'Local Focus',
          style: TextStyle(
            color: Colors.white,
            fontWeight: FontWeight.w900,
            fontSize: 22,
          ),
        ),
        actions: [
          IconButton(
            tooltip: 'Refresh',
            onPressed: _busy ? null : _refresh,
            icon: const Icon(Icons.refresh, color: Colors.white),
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
              blocks: _blocks,
              blockController: _blockController,
              taskController: _taskController,
              targetsController: _targetsController,
              minutesController: _minutesController,
              alertController: _alertController,
              redirectController: _redirectController,
              onConnect: _showConnectCodeDialog,
              onStart: _startFocus,
              onPause: _pauseFocus,
              onStop: _stopFocus,
              onToggleHighFocus: _toggleHighFocus,
              onAddBlock: _addBlock,
              onRemoveBlock: _removeBlock,
            ),
            _JournalPage(
              connected: _connected,
              busy: _busy,
              enabled: _journalEnabled,
              reminderMode: _journalReminderMode,
              date: _journalDate,
              status: _journalStatus,
              controller: _journalController,
              onReload: () => _loadJournal(),
              onSave: _saveJournal,
              onEnabledChanged: (value) =>
                  _saveJournalSettings(enabled: value),
              onReminderModeChanged: (value) =>
                  _saveJournalSettings(reminderMode: value),
            ),
            _ReportPage(
              period: _period,
              report: _report,
              focusReport: _focusReport,
              focus: _focus,
              history: _reportHistory,
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
                _syncFocusStateToNative();
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
              codeController: _codeController,
              deviceController: _deviceController,
              onUseCode: _applyConnectCode,
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
            icon: Icon(Icons.menu_book_outlined),
            selectedIcon: Icon(Icons.menu_book),
            label: 'Journal',
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
    required this.blocks,
    required this.blockController,
    required this.taskController,
    required this.targetsController,
    required this.minutesController,
    required this.alertController,
    required this.redirectController,
    required this.onConnect,
    required this.onStart,
    required this.onPause,
    required this.onStop,
    required this.onToggleHighFocus,
    required this.onAddBlock,
    required this.onRemoveBlock,
  });

  final bool connected;
  final bool busy;
  final String status;
  final FocusSession? focus;
  final SummaryReport? report;
  final List<BlockRule> blocks;
  final TextEditingController blockController;
  final TextEditingController taskController;
  final TextEditingController targetsController;
  final TextEditingController minutesController;
  final TextEditingController alertController;
  final TextEditingController redirectController;
  final VoidCallback onConnect;
  final VoidCallback onStart;
  final VoidCallback onPause;
  final VoidCallback onStop;
  final ValueChanged<bool> onToggleHighFocus;
  final VoidCallback onAddBlock;
  final ValueChanged<String> onRemoveBlock;

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
        // Hide the start form when a session is already running (e.g. started on
        // the Mac); the running controls below take over instead.
        if (focus == null) ...[
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
        ],
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
                    const SizedBox(height: 4),
                    SwitchListTile(
                      contentPadding: EdgeInsets.zero,
                      title: const Text('High-Focus mode'),
                      subtitle: const Text(
                        'Fully block outside-focus apps and websites, not just warn.',
                      ),
                      value: focus!.highFocusMode,
                      onChanged:
                          busy ? null : (value) => onToggleHighFocus(value),
                    ),
                  ],
                ),
        ),
        const SizedBox(height: 12),
        if (report != null) DailySummary(report: report!),
        const SizedBox(height: 12),
        SectionCard(
          title: 'Blocked apps & sites',
          subtitle:
              'Blocking applies on the Mac whenever Local Focus is running.',
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Row(
                children: [
                  Expanded(
                    child: LabeledField(
                      label: 'App name or website',
                      controller: blockController,
                    ),
                  ),
                  const SizedBox(width: 10),
                  FilledButton(
                    onPressed: busy ? null : onAddBlock,
                    child: const Text('Block'),
                  ),
                ],
              ),
              const SizedBox(height: 6),
              if (blocks.isEmpty)
                const EmptyState(text: 'No blocked apps or sites yet.')
              else
                Wrap(
                  spacing: 8,
                  runSpacing: 8,
                  children: [
                    for (final rule in blocks)
                      InputChip(
                        label: Text(
                          rule.hasPassword
                              ? '${rule.target} (password)'
                              : rule.target,
                        ),
                        onDeleted:
                            busy ? null : () => onRemoveBlock(rule.target),
                      ),
                  ],
                ),
            ],
          ),
        ),
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
    required this.history,
    required this.onPeriodChanged,
  });

  final ReportPeriod period;
  final SummaryReport? report;
  final FocusReport? focusReport;
  final FocusSession? focus;
  final List<ReportArchive> history;
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
        const SizedBox(height: 12),
        SectionCard(
          title: 'Report history',
          subtitle: 'Previously archived reports.',
          child: history.isEmpty
              ? const EmptyState(text: 'No previous reports yet.')
              : Column(
                  children: [
                    for (final archive in history)
                      DataRowLine(
                        title: _formatArchiveTime(archive.archivedAt),
                        detail:
                            'Productive ${formatDuration(archive.report.productiveSeconds)} - Distracted ${formatDuration(archive.report.distractingSeconds)} - Idle ${formatDuration(archive.report.idleSeconds)}',
                        seconds: archive.report.totalSeconds,
                      ),
                  ],
                ),
        ),
      ],
    );
  }

  static String _formatArchiveTime(int epochSeconds) {
    if (epochSeconds <= 0) return 'Archived report';
    final dt = DateTime.fromMillisecondsSinceEpoch(
      epochSeconds * 1000,
    ).toLocal();
    String two(int n) => n.toString().padLeft(2, '0');
    return '${dt.year}-${two(dt.month)}-${two(dt.day)} ${two(dt.hour)}:${two(dt.minute)}';
  }
}

class _JournalPage extends StatelessWidget {
  const _JournalPage({
    required this.connected,
    required this.busy,
    required this.enabled,
    required this.reminderMode,
    required this.date,
    required this.status,
    required this.controller,
    required this.onReload,
    required this.onSave,
    required this.onEnabledChanged,
    required this.onReminderModeChanged,
  });

  final bool connected;
  final bool busy;
  final bool enabled;
  final String reminderMode;
  final String date;
  final String status;
  final TextEditingController controller;
  final VoidCallback onReload;
  final VoidCallback onSave;
  final ValueChanged<bool> onEnabledChanged;
  final ValueChanged<String> onReminderModeChanged;

  @override
  Widget build(BuildContext context) {
    final disabled = busy || !connected;
    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
        SectionCard(
          title: 'Daily journal',
          subtitle: date.isEmpty
              ? 'Reflect on your day. Entries are saved on the Mac.'
              : 'Entry for $date.',
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              TextField(
                controller: controller,
                minLines: 6,
                maxLines: 12,
                decoration: const InputDecoration(
                  hintText:
                      'What mattered today? What pulled focus? What should tomorrow remember?',
                ),
              ),
              const SizedBox(height: 10),
              Row(
                children: [
                  Expanded(
                    child: FilledButton.icon(
                      onPressed: disabled ? null : onSave,
                      icon: const Icon(Icons.save),
                      label: const Text('Save entry'),
                    ),
                  ),
                  const SizedBox(width: 10),
                  IconButton.outlined(
                    tooltip: 'Reload entry',
                    onPressed: disabled ? null : onReload,
                    icon: const Icon(Icons.refresh),
                  ),
                ],
              ),
              const SizedBox(height: 8),
              Text(status, style: Theme.of(context).textTheme.bodySmall),
            ],
          ),
        ),
        const SizedBox(height: 12),
        SectionCard(
          title: 'Journal reminders',
          subtitle: 'Get nudged to reflect, and choose when.',
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              SwitchListTile(
                contentPadding: EdgeInsets.zero,
                title: const Text('Daily journaling reminder'),
                value: enabled,
                onChanged: disabled ? null : onEnabledChanged,
              ),
              const SizedBox(height: 4),
              Align(
                alignment: Alignment.centerLeft,
                child: Text(
                  'Remind me',
                  style: Theme.of(context).textTheme.bodySmall,
                ),
              ),
              const SizedBox(height: 6),
              SegmentedButton<String>(
                segments: const [
                  ButtonSegment(value: 'evening', label: Text('This evening')),
                  ButtonSegment(
                    value: 'next_morning',
                    label: Text('Next morning'),
                  ),
                ],
                selected: {
                  reminderMode == 'next_morning' ? 'next_morning' : 'evening',
                },
                onSelectionChanged: disabled
                    ? null
                    : (value) => onReminderModeChanged(value.first),
              ),
            ],
          ),
        ),
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
    required this.codeController,
    required this.deviceController,
    required this.onUseCode,
    required this.onConnect,
    required this.onSave,
  });

  final bool connected;
  final bool busy;
  final String endpoint;
  final TextEditingController serverController;
  final TextEditingController codeController;
  final TextEditingController deviceController;
  final VoidCallback onUseCode;
  final VoidCallback onConnect;
  final VoidCallback onSave;

  @override
  Widget build(BuildContext context) {
    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
        SectionCard(
          title: 'Theme',
          subtitle: 'Pick a look. Saved on this device.',
          child: ValueListenableBuilder<FocusTemplate>(
            valueListenable: focusTemplate,
            builder: (context, current, _) => Wrap(
              spacing: 10,
              runSpacing: 10,
              children: [
                for (final t in FocusTemplate.values)
                  ChoiceChip(
                    label: Text(t.label),
                    selected: current == t,
                    onSelected: (_) => setFocusTemplate(t),
                  ),
              ],
            ),
          ),
        ),
        const SizedBox(height: 12),
        SectionCard(
          title: 'Connect with a code',
          subtitle:
              'Open Local Focus on your computer and enter the 8-character connection code it shows.',
          child: Column(
            children: [
              LabeledField(
                label: 'Connection code',
                controller: codeController,
                keyboardType: TextInputType.visiblePassword,
              ),
              const SizedBox(height: 10),
              SizedBox(
                width: double.infinity,
                child: FilledButton.icon(
                  onPressed: busy ? null : onUseCode,
                  icon: const Icon(Icons.link),
                  label: const Text('Connect with code'),
                ),
              ),
            ],
          ),
        ),
        const SizedBox(height: 12),
        SectionCard(
          title: 'Device and manual link',
          subtitle:
              'Name this device. You can paste the direct link instead of using a code.',
          child: Column(
            children: [
              LabeledField(label: 'Phone name', controller: deviceController),
              LabeledField(
                label: 'Direct link (optional)',
                controller: serverController,
                keyboardType: TextInputType.url,
              ),
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
            'Local Focus does not scan for nearby devices. It only talks to the exact Local Focus link you scan or paste, and all data stays on your devices.',
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
