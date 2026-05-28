use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const APP_NAME: &str = "local-focus";
const SAMPLE_SECONDS: u64 = 5;
const DISTRACTION_SECONDS: i64 = 90;
const DEFAULT_ALERT_DELAY_SECONDS: u64 = 60;
const IDLE_SECONDS: u64 = 60;

#[derive(Clone, Debug)]
struct Config {
    productive_keywords: Vec<String>,
    distracting_keywords: Vec<String>,
    blocked_keywords: Vec<String>,
}

#[derive(Clone, Debug)]
struct ActivitySample {
    timestamp: i64,
    app: String,
    title: String,
    source: String,
    category: String,
}

#[derive(Clone, Debug)]
struct FocusSession {
    task: String,
    target: String,
    started_at: i64,
    duration_minutes: u64,
    break_minutes: u64,
    paused_at: Option<i64>,
    paused_total_seconds: i64,
    pomodoro_alerted_at: Option<i64>,
    alert_delay_seconds: u64,
    alert_action: String,
    redirect_app: String,
}

#[derive(Default)]
struct AppState {
    config: Config,
    focus: Option<FocusSession>,
    last_distraction_at: i64,
    last_focus_mismatch_at: i64,
    focus_mismatch_started_at: Option<i64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            productive_keywords: vec![
                "code".into(),
                "terminal".into(),
                "editor".into(),
                "docs".into(),
                "figma".into(),
                "notion".into(),
                "calendar".into(),
                "github".into(),
                "jira".into(),
                "linear".into(),
            ],
            distracting_keywords: vec![
                "youtube".into(),
                "netflix".into(),
                "reddit".into(),
                "instagram".into(),
                "tiktok".into(),
                "x.com".into(),
                "twitter".into(),
                "facebook".into(),
                "game".into(),
                "steam".into(),
            ],
            blocked_keywords: Vec::new(),
        }
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let data_dir = data_dir()?;
    fs::create_dir_all(&data_dir)?;
    ensure_config(&data_dir)?;

    match args.get(1).map(String::as_str) {
        Some("track") => run_tracker(data_dir),
        Some("focus") => {
            let task = args.get(2).cloned().unwrap_or_else(|| "Focus session".into());
            let minutes = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(25);
            let target = args.get(4).cloned().unwrap_or_default();
            start_focus(data_dir, task, target, minutes, 5)
        }
        Some("report") => print_report(data_dir),
        Some("serve") | None => run_app(data_dir),
        Some("data-dir") => {
            println!("{}", data_dir.display());
            Ok(())
        }
        _ => {
            print_help();
            Ok(())
        }
    }
}

fn run_app(data_dir: PathBuf) -> io::Result<()> {
    let config = load_config(&data_dir).unwrap_or_default();
    let state = Arc::new(Mutex::new(AppState {
        config,
        focus: load_focus(&data_dir),
        last_distraction_at: 0,
        last_focus_mismatch_at: 0,
        focus_mismatch_started_at: None,
    }));

    {
        let tracker_dir = data_dir.clone();
        let tracker_state = Arc::clone(&state);
        thread::spawn(move || {
            if let Err(error) = tracking_loop(tracker_dir, tracker_state) {
                eprintln!("tracking stopped: {error}");
            }
        });
    }

    {
        let focus_dir = data_dir.clone();
        let focus_state = Arc::clone(&state);
        thread::spawn(move || {
            if let Err(error) = focus_loop(focus_dir, focus_state) {
                eprintln!("focus monitor stopped: {error}");
            }
        });
    }

    {
        let daily_dir = data_dir.clone();
        let daily_state = Arc::clone(&state);
        thread::spawn(move || {
            if let Err(error) = daily_report_loop(daily_dir, daily_state) {
                eprintln!("daily report logger stopped: {error}");
            }
        });
    }

    let listener = TcpListener::bind("127.0.0.1:4799")?;
    println!("Local Focus is running at http://127.0.0.1:4799");
    println!("Data stays on this machine: {}", data_dir.display());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let request_dir = data_dir.clone();
                let request_state = Arc::clone(&state);
                thread::spawn(move || {
                    if let Err(error) = handle_http(stream, request_dir, request_state) {
                        eprintln!("request failed: {error}");
                    }
                });
            }
            Err(error) => eprintln!("connection failed: {error}"),
        }
    }

    Ok(())
}

fn run_tracker(data_dir: PathBuf) -> io::Result<()> {
    let state = Arc::new(Mutex::new(AppState {
        config: load_config(&data_dir).unwrap_or_default(),
        focus: load_focus(&data_dir),
        last_distraction_at: 0,
        last_focus_mismatch_at: 0,
        focus_mismatch_started_at: None,
    }));
    tracking_loop(data_dir, state)
}

fn tracking_loop(data_dir: PathBuf, state: Arc<Mutex<AppState>>) -> io::Result<()> {
    loop {
        let raw = foreground_activity();
        let (config, focus) = state
            .lock()
            .map(|s| (s.config.clone(), s.focus.clone()))
            .unwrap_or_default();
        let category = classify(&config, &raw.0, &raw.1);
        let mut sample = ActivitySample {
            timestamp: now(),
            app: raw.0,
            title: raw.1,
            source: raw.2,
            category,
        };
        apply_focus_productivity_gate(&focus, &mut sample);
        if system_idle_seconds().is_some_and(|seconds| seconds >= IDLE_SECONDS) {
            sample.category = "idle".into();
        }

        append_sample(&data_dir, &sample)?;
        detect_distraction(&data_dir, &state, &sample)?;
        thread::sleep(Duration::from_secs(SAMPLE_SECONDS));
    }
}

fn focus_loop(data_dir: PathBuf, state: Arc<Mutex<AppState>>) -> io::Result<()> {
    loop {
        thread::sleep(Duration::from_secs(10));
        let focus = state.lock().ok().and_then(|s| s.focus.clone());
        if let Some(session) = focus {
            if session.paused_at.is_some() {
                continue;
            }
            let elapsed = focus_elapsed_seconds(&session, now());
            let target = (session.duration_minutes * 60) as i64;
            if elapsed >= target && session.pomodoro_alerted_at.is_none() {
                os_alert(
                    "Focus complete",
                    &format!(
                        "{} Pomodoro is complete. Focus monitoring is still active until you Pause or Stop. Take a {} minute break when you are ready.",
                        session.task, session.break_minutes
                    ),
                );
                let mut completed = session.clone();
                completed.pomodoro_alerted_at = Some(now());
                save_focus(&data_dir, &completed)?;
                if let Ok(mut state) = state.lock() {
                    state.focus = Some(completed);
                }
            }
        }
    }
}

fn daily_report_loop(data_dir: PathBuf, state: Arc<Mutex<AppState>>) -> io::Result<()> {
    loop {
        maybe_log_previous_day_report(&data_dir, &state)?;
        thread::sleep(Duration::from_secs(5 * 60));
    }
}

fn start_focus(
    data_dir: PathBuf,
    task: String,
    target: String,
    duration_minutes: u64,
    break_minutes: u64,
) -> io::Result<()> {
    let session = FocusSession {
        task,
        target,
        started_at: now(),
        duration_minutes,
        break_minutes,
        paused_at: None,
        paused_total_seconds: 0,
        pomodoro_alerted_at: None,
        alert_delay_seconds: DEFAULT_ALERT_DELAY_SECONDS,
        alert_action: "alert".into(),
        redirect_app: String::new(),
    };
    save_focus(&data_dir, &session)?;
    append_focus_session(&data_dir, &session)?;
    let target_note = if session.target.trim().is_empty() {
        String::new()
    } else {
        format!(" in {}", session.target)
    };
    notify(
        "Focus started",
        &format!("{} minutes: {}{}", duration_minutes, session.task, target_note),
    );
    println!("Started focus session: {}", session.task);
    Ok(())
}

fn detect_distraction(
    data_dir: &PathBuf,
    state: &Arc<Mutex<AppState>>,
    sample: &ActivitySample,
) -> io::Result<()> {
    let mut guard = match state.lock() {
        Ok(guard) => guard,
        Err(_) => return Ok(()),
    };

    let focused = guard.focus.is_some();
    let paused = guard
        .focus
        .as_ref()
        .is_some_and(|focus| focus.paused_at.is_some());
    if paused {
        return Ok(());
    }

    let distracting = sample.category == "distracting";
    let enough_time = sample.timestamp - guard.last_distraction_at >= DISTRACTION_SECONDS;
    let focus_mismatch = guard
        .focus
        .as_ref()
        .filter(|focus| !focus_targets(focus).is_empty())
        .is_some_and(|focus| !matches_focus_target(focus, sample));

    if focused && focus_mismatch {
        let alert_delay = guard
            .focus
            .as_ref()
            .map(|focus| focus.alert_delay_seconds.max(1) as i64)
            .unwrap_or(DEFAULT_ALERT_DELAY_SECONDS as i64);
        let mismatch_started_at = match guard.focus_mismatch_started_at {
            Some(started_at) => started_at,
            None => {
                guard.focus_mismatch_started_at = Some(sample.timestamp);
                sample.timestamp
            }
        };
        let mismatch_duration = sample.timestamp - mismatch_started_at;
        let alert_cooldown_passed = sample.timestamp - guard.last_focus_mismatch_at >= alert_delay;

        if mismatch_duration >= alert_delay && alert_cooldown_passed {
            let focus = guard.focus.as_ref().expect("focus checked above");
            let message = format!(
                "You have been outside your focus apps/sites for over {}. Allowed: '{}'. Current activity: {}",
                human_duration(alert_delay as u64),
                focus.target,
                sample.app
            );
            if focus.alert_action == "switch" && !focus.redirect_app.trim().is_empty() {
                os_alert_then_activate("Focus warning", &message, &focus.redirect_app);
            } else {
                os_alert("Focus warning", &message);
            }
            guard.last_focus_mismatch_at = sample.timestamp;
            append_event(data_dir, "focus_target_mismatch", &message)?;
        }
    } else {
        guard.focus_mismatch_started_at = None;
    }

    if focused && distracting && enough_time {
        let task = guard
            .focus
            .as_ref()
            .map(|f| f.task.clone())
            .unwrap_or_else(|| "your task".into());
        let message = format!("You are in focus mode for {task}. Current activity: {}", sample.app);
        os_alert("Distraction warning", &message);
        guard.last_distraction_at = sample.timestamp;
        append_event(data_dir, "distraction_alert", &message)?;
    }

    Ok(())
}

fn matches_focus_target(focus: &FocusSession, sample: &ActivitySample) -> bool {
    let targets = focus_targets(focus);
    if targets.is_empty() {
        return true;
    }

    let haystack = normalize_match_text(&format!(
        "{} {} {}",
        sample.app, sample.title, sample.source
    ));

    targets.iter().any(|target| {
        let normalized = normalize_match_text(target);
        let domain = domain_from_url(target).map(|domain| normalize_match_text(&domain));
        haystack.contains(&normalized)
            || domain
                .as_ref()
                .is_some_and(|domain| !domain.is_empty() && haystack.contains(domain))
    })
}

fn apply_focus_productivity_gate(focus: &Option<FocusSession>, sample: &mut ActivitySample) {
    let Some(focus) = focus else {
        return;
    };
    if focus.paused_at.is_some() {
        return;
    }
    if focus_targets(focus).is_empty() {
        return;
    }

    if matches_focus_target(focus, sample) {
        sample.category = "productive".into();
    } else if sample.category == "productive" {
        sample.category = "distracting".into();
    } else {
        sample.category = "distracting".into();
    }
}

#[cfg(target_os = "macos")]
fn system_idle_seconds() -> Option<u64> {
    let output = Command::new("ioreg")
        .args(["-c", "IOHIDSystem"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let marker = "\"HIDIdleTime\" = ";
    let value = text
        .lines()
        .find_map(|line| line.split_once(marker).map(|(_, value)| value.trim()))?;
    value.parse::<u64>().ok().map(|nanos| nanos / 1_000_000_000)
}

#[cfg(target_os = "windows")]
fn system_idle_seconds() -> Option<u64> {
    let script = r#"
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class IdleTime {
  [StructLayout(LayoutKind.Sequential)]
  struct LASTINPUTINFO {
    public uint cbSize;
    public uint dwTime;
  }
  [DllImport("user32.dll")]
  static extern bool GetLastInputInfo(ref LASTINPUTINFO plii);
  public static uint Seconds() {
    LASTINPUTINFO info = new LASTINPUTINFO();
    info.cbSize = (uint)Marshal.SizeOf(info);
    GetLastInputInfo(ref info);
    return ((uint)Environment.TickCount - info.dwTime) / 1000;
  }
}
'@
[IdleTime]::Seconds()
"#;
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn system_idle_seconds() -> Option<u64> {
    let output = Command::new("xprintidle").output().ok()?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok()
        .map(|millis| millis / 1000)
}

fn focus_elapsed_seconds(focus: &FocusSession, at: i64) -> i64 {
    let active_until = focus.paused_at.unwrap_or(at);
    (active_until - focus.started_at - focus.paused_total_seconds).max(0)
}

fn focus_targets(focus: &FocusSession) -> Vec<String> {
    focus
        .target
        .split([',', '\n'])
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn human_duration(seconds: u64) -> String {
    if seconds == 60 {
        "1 minute".into()
    } else if seconds % 60 == 0 {
        format!("{} minutes", seconds / 60)
    } else if seconds == 1 {
        "1 second".into()
    } else {
        format!("{seconds} seconds")
    }
}

fn normalize_match_text(value: &str) -> String {
    value
        .trim()
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.")
        .to_lowercase()
}

fn domain_from_url(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))?;
    without_scheme
        .split('/')
        .next()
        .map(|domain| domain.trim_start_matches("www.").to_string())
        .filter(|domain| !domain.is_empty())
}

fn foreground_activity() -> (String, String, String) {
    platform_foreground_activity().unwrap_or_else(|| {
        (
            "Unknown".into(),
            "Unknown activity".into(),
            "local".into(),
        )
    })
}

#[cfg(target_os = "macos")]
fn platform_foreground_activity() -> Option<(String, String, String)> {
    let script = r#"tell application "System Events"
set frontApp to name of first application process whose frontmost is true
try
set windowTitle to name of front window of first application process whose frontmost is true
on error
set windowTitle to frontApp
end try
end tell
return frontApp & "||" & windowTitle"#;

    let output = Command::new("osascript").arg("-e").arg(script).output().ok()?;
    let (app, title, fallback_source) = parse_activity(&String::from_utf8_lossy(&output.stdout))?;
    let source = active_browser_url(&app).unwrap_or(fallback_source);
    Some((app, title, source))
}

#[cfg(target_os = "macos")]
fn active_browser_url(app: &str) -> Option<String> {
    let script = match app {
        "Safari" => r#"tell application "Safari" to get URL of current tab of front window"#,
        "Google Chrome" => {
            r#"tell application "Google Chrome" to get URL of active tab of front window"#
        }
        "Brave Browser" => {
            r#"tell application "Brave Browser" to get URL of active tab of front window"#
        }
        "Microsoft Edge" => {
            r#"tell application "Microsoft Edge" to get URL of active tab of front window"#
        }
        "Arc" => r#"tell application "Arc" to get URL of active tab of front window"#,
        _ => return None,
    };

    let output = Command::new("osascript").arg("-e").arg(script).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let url = clean(&String::from_utf8_lossy(&output.stdout));
    if url == "Unknown" {
        None
    } else {
        Some(url)
    }
}

#[cfg(target_os = "windows")]
fn platform_foreground_activity() -> Option<(String, String, String)> {
    let script = r#"
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class WinApi {
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int count);
}
"@
$handle = [WinApi]::GetForegroundWindow()
$title = New-Object System.Text.StringBuilder 512
[void][WinApi]::GetWindowText($handle, $title, $title.Capacity)
$p = Get-Process | Where-Object {$_.MainWindowHandle -eq $handle} | Select-Object -First 1
($p.ProcessName + "||" + $title.ToString())
"#;
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .output()
        .ok()?;
    parse_activity(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(target_os = "linux")]
fn platform_foreground_activity() -> Option<(String, String, String)> {
    let window_id = Command::new("sh")
        .arg("-c")
        .arg("xdotool getactivewindow 2>/dev/null")
        .output()
        .ok()?;
    let window_id = String::from_utf8_lossy(&window_id.stdout).trim().to_string();
    if window_id.is_empty() {
        return None;
    }

    let title = Command::new("sh")
        .arg("-c")
        .arg(format!("xdotool getwindowname {window_id} 2>/dev/null"))
        .output()
        .ok()?;
    let app = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "xprop -id {window_id} WM_CLASS 2>/dev/null | sed 's/.*= //; s/\"//g'"
        ))
        .output()
        .ok()?;

    let app = clean(&String::from_utf8_lossy(&app.stdout));
    let title = clean(&String::from_utf8_lossy(&title.stdout));
    let source = source_from_title(&title);
    Some((app, title, source))
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn platform_foreground_activity() -> Option<(String, String, String)> {
    None
}

fn parse_activity(value: &str) -> Option<(String, String, String)> {
    let mut parts = value.trim().splitn(3, "||");
    let app = clean(parts.next()?);
    let title = clean(parts.next().unwrap_or(""));
    let source = parts
        .next()
        .map(clean)
        .filter(|value| value != "Unknown")
        .unwrap_or_else(|| source_from_title(&title));
    Some((app, title, source))
}

fn source_from_title(title: &str) -> String {
    let lower = title.to_lowercase();
    for token in lower.split_whitespace() {
        if token.contains('.') && !token.ends_with('.') {
            return token
                .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-')
                .to_string();
        }
    }
    "local".into()
}

fn classify(config: &Config, app: &str, title: &str) -> String {
    let haystack = format!("{} {}", app, title).to_lowercase();
    if config.blocked_keywords.iter().any(|k| haystack.contains(k)) {
        return "distracting".into();
    }
    if config
        .distracting_keywords
        .iter()
        .any(|k| haystack.contains(k))
    {
        return "distracting".into();
    }
    if config
        .productive_keywords
        .iter()
        .any(|k| haystack.contains(k))
    {
        return "productive".into();
    }
    "distracting".into()
}

fn append_sample(data_dir: &PathBuf, sample: &ActivitySample) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("activity.jsonl"))?;
    writeln!(
        file,
        "{{\"timestamp\":{},\"app\":\"{}\",\"title\":\"{}\",\"source\":\"{}\",\"category\":\"{}\"}}",
        sample.timestamp,
        json_escape(&sample.app),
        json_escape(&sample.title),
        json_escape(&sample.source),
        json_escape(&sample.category)
    )
}

fn append_event(data_dir: &PathBuf, kind: &str, message: &str) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("events.jsonl"))?;
    writeln!(
        file,
        "{{\"timestamp\":{},\"kind\":\"{}\",\"message\":\"{}\"}}",
        now(),
        json_escape(kind),
        json_escape(message)
    )
}

fn append_focus_session(data_dir: &PathBuf, focus: &FocusSession) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("focus_sessions.jsonl"))?;
    writeln!(
        file,
        "{{\"task\":\"{}\",\"target\":\"{}\",\"startedAt\":{},\"durationMinutes\":{},\"alertDelaySeconds\":{},\"alertAction\":\"{}\",\"redirectApp\":\"{}\"}}",
        json_escape(&focus.task),
        json_escape(&focus.target),
        focus.started_at,
        focus.duration_minutes,
        focus.alert_delay_seconds,
        json_escape(&focus.alert_action),
        json_escape(&focus.redirect_app)
    )
}

fn focus_sessions_json(
    data_dir: &PathBuf,
    since: Option<i64>,
    until: Option<i64>,
    current_focus: Option<FocusSession>,
) -> io::Result<String> {
    let path = data_dir.join("focus_sessions.jsonl");
    let mut rows = Vec::new();
    if path.exists() {
        let reader = BufReader::new(File::open(path)?);
        for line in reader.lines().map_while(Result::ok) {
            let started_at = json_number(&line, "startedAt").unwrap_or(0);
            if started_at == 0
                || since.is_some_and(|value| started_at < value)
                || until.is_some_and(|value| started_at >= value)
            {
                continue;
            }
            rows.push(line);
        }
    }

    if let Some(focus) = current_focus {
        if since.map_or(true, |value| focus.started_at >= value)
            && until.map_or(true, |value| focus.started_at < value)
            && !rows
                .iter()
                .any(|line| json_number(line, "startedAt") == Some(focus.started_at))
        {
            rows.push(format!(
                "{{\"task\":\"{}\",\"target\":\"{}\",\"startedAt\":{},\"durationMinutes\":{},\"alertDelaySeconds\":{},\"alertAction\":\"{}\",\"redirectApp\":\"{}\"}}",
                json_escape(&focus.task),
                json_escape(&focus.target),
                focus.started_at,
                focus.duration_minutes,
                focus.alert_delay_seconds,
                json_escape(&focus.alert_action),
                json_escape(&focus.redirect_app)
            ));
        }
    }

    rows.sort_by_key(|line| json_number(line, "startedAt").unwrap_or(0));
    Ok(format!("[{}]", rows.join(",")))
}

fn load_samples(data_dir: &PathBuf) -> io::Result<Vec<ActivitySample>> {
    let path = data_dir.join("activity.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }

    let reader = BufReader::new(File::open(path)?);
    let mut samples = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        if let Some(sample) = parse_sample(&line) {
            samples.push(sample);
        }
    }
    Ok(samples)
}

fn parse_sample(line: &str) -> Option<ActivitySample> {
    Some(ActivitySample {
        timestamp: json_number(line, "timestamp")?,
        app: json_string(line, "app")?,
        title: json_string(line, "title")?,
        source: json_string(line, "source")?,
        category: json_string(line, "category")?,
    })
}

fn handle_http(
    mut stream: TcpStream,
    data_dir: PathBuf,
    state: Arc<Mutex<AppState>>,
) -> io::Result<()> {
    let mut buffer = [0; 4096];
    let read = stream.read(&mut buffer)?;
    let request = String::from_utf8_lossy(&buffer[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    if path.starts_with("/api/focus/start") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let task = params
            .get("task")
            .cloned()
            .unwrap_or_else(|| "Focus session".into());
        let minutes = params
            .get("minutes")
            .and_then(|v| v.parse().ok())
            .unwrap_or(25);
        let target = params
            .get("target")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let alert_delay_seconds = params
            .get("alertSeconds")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_ALERT_DELAY_SECONDS)
            .clamp(10, 60 * 60);
        let alert_action = params
            .get("alertAction")
            .filter(|action| action.as_str() == "switch")
            .cloned()
            .unwrap_or_else(|| "alert".into());
        let redirect_app = params
            .get("redirectApp")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let session = FocusSession {
            task,
            target,
            started_at: now(),
            duration_minutes: minutes,
            break_minutes: 5,
            paused_at: None,
            paused_total_seconds: 0,
            pomodoro_alerted_at: None,
            alert_delay_seconds,
            alert_action,
            redirect_app,
        };
        save_focus(&data_dir, &session)?;
        append_focus_session(&data_dir, &session)?;
        if let Ok(mut state) = state.lock() {
            state.focus = Some(session.clone());
        }
        let target_note = if session.target.trim().is_empty() {
            String::new()
        } else {
            format!(" in {}", session.target)
        };
        notify(
            "Focus started",
            &format!("{} minutes: {}{}", minutes, session.task, target_note),
        );
        write_response(&mut stream, "application/json", "{\"ok\":true}")?;
    } else if path.starts_with("/api/focus/pause") {
        let updated = {
            let mut guard = state
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "state lock poisoned"))?;
            if let Some(mut focus) = guard.focus.clone() {
                let current = now();
                if let Some(paused_at) = focus.paused_at {
                    focus.paused_total_seconds += current - paused_at;
                    focus.paused_at = None;
                    notify("Focus resumed", &focus.task);
                } else {
                    focus.paused_at = Some(current);
                    notify("Focus paused", &focus.task);
                }
                guard.focus = Some(focus.clone());
                Some(focus)
            } else {
                None
            }
        };

        if let Some(focus) = updated {
            save_focus(&data_dir, &focus)?;
        }
        write_response(&mut stream, "application/json", "{\"ok\":true}")?;
    } else if path.starts_with("/api/focus/stop") {
        clear_focus(&data_dir)?;
        if let Ok(mut state) = state.lock() {
            state.focus = None;
        }
        write_response(&mut stream, "application/json", "{\"ok\":true}")?;
    } else if path.starts_with("/api/block/add") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let keyword = params
            .get("keyword")
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_default();

        if !keyword.is_empty() {
            let mut config = load_config(&data_dir).unwrap_or_default();
            if !config.blocked_keywords.iter().any(|k| k == &keyword) {
                config.blocked_keywords.push(keyword.clone());
                save_config(&data_dir, &config)?;
            }
            if let Ok(mut state) = state.lock() {
                state.config = config;
            }
            append_event(&data_dir, "blocked_keyword_added", &keyword)?;
        }

        write_response(&mut stream, "application/json", "{\"ok\":true}")?;
    } else if path == "/api/timeline" {
        write_response(&mut stream, "application/json", &timeline_json(&data_dir)?)?;
    } else if path == "/api/report/reset" {
        reset_report(&data_dir)?;
        write_response(&mut stream, "application/json", "{\"ok\":true}")?;
    } else if path == "/api/report/history" {
        write_response(&mut stream, "application/json", &report_history_json(&data_dir)?)?;
    } else if path.starts_with("/api/focus-sessions") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let since = params.get("since").and_then(|value| value.parse::<i64>().ok());
        let until = params.get("until").and_then(|value| value.parse::<i64>().ok());
        let focus = state.lock().ok().and_then(|s| s.focus.clone());
        write_response(
            &mut stream,
            "application/json",
            &focus_sessions_json(&data_dir, since, until, focus)?,
        )?;
    } else if path.starts_with("/api/focus-report") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let target = params
            .get("target")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let since = params.get("since").and_then(|value| value.parse::<i64>().ok());
        let until = params.get("until").and_then(|value| value.parse::<i64>().ok());
        let period = params
            .get("period")
            .map(|value| value.as_str())
            .unwrap_or("window");
        write_response(
            &mut stream,
            "application/json",
            &focus_report_json(&data_dir, &target, since, until, period)?,
        )?;
    } else if path == "/api/report" {
        write_response(&mut stream, "application/json", &report_json(&data_dir)?)?;
    } else if path == "/api/state" {
        let focus = state.lock().ok().and_then(|s| s.focus.clone());
        write_response(&mut stream, "application/json", &state_json(focus))?;
    } else {
        write_response(&mut stream, "text/html; charset=utf-8", &index_html())?;
    }

    Ok(())
}

fn write_response(stream: &mut TcpStream, content_type: &str, body: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n{}",
        body.len(),
        body
    )
}

fn timeline_json(data_dir: &PathBuf) -> io::Result<String> {
    let samples = load_samples(data_dir)?;
    let mut segments = Vec::new();
    let mut current: Option<ActivitySample> = None;
    let mut current_start = 0;
    let mut last_timestamp = 0;

    for sample in samples.into_iter().rev().take(1500).collect::<Vec<_>>().into_iter().rev() {
        match &current {
            Some(active)
                if active.app == sample.app
                    && active.title == sample.title
                    && active.category == sample.category =>
            {
                last_timestamp = sample.timestamp;
            }
            Some(active) => {
                segments.push(segment_json(active, current_start, last_timestamp));
                current_start = sample.timestamp;
                last_timestamp = sample.timestamp;
                current = Some(sample);
            }
            None => {
                current_start = sample.timestamp;
                last_timestamp = sample.timestamp;
                current = Some(sample);
            }
        }
    }

    if let Some(active) = current {
        segments.push(segment_json(&active, current_start, last_timestamp));
    }

    Ok(format!("[{}]", segments.join(",")))
}

fn report_json(data_dir: &PathBuf) -> io::Result<String> {
    let samples = load_samples(data_dir)?;
    let since = report_window_start(data_dir)?.max(now() - 24 * 60 * 60);
    let recent: Vec<_> = samples.into_iter().filter(|s| s.timestamp >= since).collect();
    let total = recent.len().max(1) as f64;
    let productive = recent.iter().filter(|s| s.category == "productive").count() as f64;
    let idle = recent.iter().filter(|s| s.category == "idle").count() as f64;
    let distracting = recent
        .iter()
        .filter(|s| s.category == "distracting")
        .count() as f64;
    let score = ((productive * 100.0 - distracting * 40.0 - idle * 10.0) / total)
        .clamp(0.0, 100.0)
        .round();

    let mut app_counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for sample in &recent {
        *app_counts
            .entry((sample.app.clone(), sample.source.clone()))
            .or_default() += 1;
    }
    let mut apps: Vec<_> = app_counts.into_iter().collect();
    apps.sort_by(|a, b| b.1.cmp(&a.1));
    let app_json = apps
        .into_iter()
        .take(10)
        .map(|((app, source), count)| {
            format!(
                "{{\"app\":\"{}\",\"source\":\"{}\",\"seconds\":{},\"minutes\":{}}}",
                json_escape(&app),
                json_escape(&source),
                count as u64 * SAMPLE_SECONDS,
                count as u64 * SAMPLE_SECONDS / 60
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    Ok(format!(
        "{{\"score\":{},\"productiveMinutes\":{},\"distractingMinutes\":{},\"idleMinutes\":{},\"topApps\":[{}]}}",
        score as u64,
        productive as u64 * SAMPLE_SECONDS / 60,
        distracting as u64 * SAMPLE_SECONDS / 60,
        idle as u64 * SAMPLE_SECONDS / 60,
        app_json
    ))
}

fn focus_report_json(
    data_dir: &PathBuf,
    target_text: &str,
    since_override: Option<i64>,
    until_override: Option<i64>,
    period: &str,
) -> io::Result<String> {
    let samples = load_samples(data_dir)?;
    let since = since_override
        .unwrap_or(report_window_start(data_dir)?.max(now() - 24 * 60 * 60))
        .max(0);
    let recent: Vec<_> = samples
        .into_iter()
        .filter(|s| s.timestamp >= since && until_override.map_or(true, |until| s.timestamp < until))
        .collect();
    let targets = target_list_from_text(target_text);
    let target_json = targets
        .iter()
        .map(|target| format!("\"{}\"", json_escape(target)))
        .collect::<Vec<_>>()
        .join(",");

    let mut target_seconds: BTreeMap<String, u64> = targets
        .iter()
        .map(|target| (target.clone(), 0))
        .collect::<BTreeMap<_, _>>();
    let mut target_idle_seconds: BTreeMap<String, u64> = targets
        .iter()
        .map(|target| (target.clone(), 0))
        .collect::<BTreeMap<_, _>>();
    let mut outside_seconds = 0;
    let mut productive_seconds = 0;
    let mut distracting_seconds = 0;
    let mut idle_seconds = 0;
    let mut distraction_counts: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut hourly: BTreeMap<i64, (u64, u64, u64)> = BTreeMap::new();
    let mut hourly_details: BTreeMap<i64, BTreeMap<(String, String, String, String), u64>> =
        BTreeMap::new();

    for sample in &recent {
        let seconds = SAMPLE_SECONDS;
        let bucket = sample.timestamp - sample.timestamp.rem_euclid(60 * 60);
        let entry = hourly.entry(bucket).or_default();
        *hourly_details
            .entry(bucket)
            .or_default()
            .entry((
                sample.app.clone(),
                sample.title.clone(),
                sample.source.clone(),
                sample.category.clone(),
            ))
            .or_default() += seconds;
        if sample.category == "productive" {
            productive_seconds += seconds;
            entry.0 += seconds;
        } else if sample.category == "idle" {
            idle_seconds += seconds;
            entry.2 += seconds;
        } else {
            distracting_seconds += seconds;
            entry.1 += seconds;
        }

        if let Some(target) = targets
            .iter()
            .find(|target| sample_matches_target_text(sample, target))
        {
            if sample.category == "idle" {
                *target_idle_seconds.entry(target.clone()).or_default() += seconds;
            } else {
                *target_seconds.entry(target.clone()).or_default() += seconds;
            }
        } else {
            if sample.category == "idle" {
                idle_seconds += 0;
            } else {
                outside_seconds += seconds;
                let (app, source) = report_activity_key(sample);
                *distraction_counts
                    .entry((app, source))
                    .or_default() += seconds;
            }
        }
    }

    let mut target_rows = target_seconds.into_iter().collect::<Vec<_>>();
    target_rows.sort_by(|a, b| {
        let a_total = a.1 + target_idle_seconds.get(&a.0).copied().unwrap_or(0);
        let b_total = b.1 + target_idle_seconds.get(&b.0).copied().unwrap_or(0);
        b_total.cmp(&a_total).then_with(|| a.0.cmp(&b.0))
    });
    let target_rows_json = target_rows
        .iter()
        .map(|(target, seconds)| {
            let idle = target_idle_seconds.get(target).copied().unwrap_or(0);
            let total = seconds + idle;
            format!(
                "{{\"target\":\"{}\",\"seconds\":{},\"idleSeconds\":{},\"totalSeconds\":{},\"minutes\":{},\"idleMinutes\":{},\"totalMinutes\":{}}}",
                json_escape(target),
                seconds,
                idle,
                total,
                seconds / 60,
                idle / 60,
                total / 60
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    let mut distractions = distraction_counts.into_iter().collect::<Vec<_>>();
    distractions.sort_by(|a, b| b.1.cmp(&a.1));
    let distraction_json = distractions
        .into_iter()
        .take(5)
        .map(|((app, source), seconds)| {
            format!(
                "{{\"app\":\"{}\",\"source\":\"{}\",\"seconds\":{},\"minutes\":{}}}",
                json_escape(&app),
                json_escape(&source),
                seconds,
                seconds / 60
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    let hourly_json = hourly
        .into_iter()
        .map(|(hour, (productive, distracting, idle))| {
            let mut details = hourly_details
                .remove(&hour)
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<_>>();
            details.sort_by(|a, b| b.1.cmp(&a.1));
            let details_json = details
                .into_iter()
                .take(12)
                .map(|((app, title, source, category), seconds)| {
                    format!(
                        "{{\"app\":\"{}\",\"title\":\"{}\",\"source\":\"{}\",\"category\":\"{}\",\"seconds\":{}}}",
                        json_escape(&app),
                        json_escape(&title),
                        json_escape(&source),
                        json_escape(&category),
                        seconds
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{{\"hour\":{},\"productiveSeconds\":{},\"distractingSeconds\":{},\"idleSeconds\":{},\"items\":[{}]}}",
                hour, productive, distracting, idle, details_json
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    let focused_seconds = target_rows.iter().map(|(_, seconds)| *seconds).sum::<u64>();
    let total_seconds = focused_seconds + outside_seconds + idle_seconds;
    let focus_percent = if total_seconds == 0 {
        0
    } else {
        (focused_seconds * 100 / total_seconds).min(100)
    };
    let score = if total_seconds == 0 {
        0
    } else {
        ((productive_seconds as f64 * 100.0
            - distracting_seconds as f64 * 40.0
            - idle_seconds as f64 * 10.0)
            / total_seconds as f64)
            .clamp(0.0, 100.0)
            .round() as u64
    };

    Ok(format!(
        "{{\"period\":\"{}\",\"windowStart\":{},\"generatedAt\":{},\"targets\":[{}],\"focusSeconds\":{},\"outsideSeconds\":{},\"idleSeconds\":{},\"productiveSeconds\":{},\"distractingSeconds\":{},\"focusPercent\":{},\"score\":{},\"targetBreakdown\":[{}],\"topDistractions\":[{}],\"hourly\":[{}]}}",
        json_escape(period),
        since,
        now(),
        target_json,
        focused_seconds,
        outside_seconds,
        idle_seconds,
        productive_seconds,
        distracting_seconds,
        focus_percent,
        score,
        target_rows_json,
        distraction_json,
        hourly_json
    ))
}

fn target_list_from_text(target_text: &str) -> Vec<String> {
    target_text
        .split([',', '\n'])
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn sample_matches_target_text(sample: &ActivitySample, target: &str) -> bool {
    let haystack = normalize_match_text(&format!(
        "{} {} {}",
        sample.app, sample.title, sample.source
    ));
    let normalized = normalize_match_text(target);
    let domain = domain_from_url(target).map(|domain| normalize_match_text(&domain));
    haystack.contains(&normalized)
        || domain
            .as_ref()
            .is_some_and(|domain| !domain.is_empty() && haystack.contains(domain))
}

fn report_activity_key(sample: &ActivitySample) -> (String, String) {
    if let Some((domain, source)) = website_report_key(&sample.source) {
        return (domain, source);
    }

    (sample.app.clone(), sample.source.clone())
}

fn website_report_key(source: &str) -> Option<(String, String)> {
    let trimmed = source.trim();
    let (scheme, rest) = trimmed
        .strip_prefix("https://")
        .map(|rest| ("https", rest))
        .or_else(|| trimmed.strip_prefix("http://").map(|rest| ("http", rest)))?;
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches(':');
    if host.is_empty() {
        return None;
    }

    let display_host = host.to_string();
    let domain = host.trim_start_matches("www.").to_string();
    Some((domain, format!("{scheme}://{display_host}/")))
}

fn reset_report(data_dir: &PathBuf) -> io::Result<()> {
    let archived_at = now();
    let report = report_json(data_dir)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("report_history.jsonl"))?;
    writeln!(
        file,
        "{{\"archivedAt\":{},\"report\":{}}}",
        archived_at,
        report
    )?;
    fs::write(data_dir.join("report_start.txt"), archived_at.to_string())
}

fn report_history_json(data_dir: &PathBuf) -> io::Result<String> {
    let path = data_dir.join("report_history.jsonl");
    if !path.exists() {
        return Ok("[]".into());
    }

    let reader = BufReader::new(File::open(path)?);
    let mut lines = reader
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    lines.reverse();
    lines.truncate(20);
    Ok(format!("[{}]", lines.join(",")))
}

fn maybe_log_previous_day_report(
    data_dir: &PathBuf,
    state: &Arc<Mutex<AppState>>,
) -> io::Result<()> {
    let Some((previous_day, previous_start, today_start)) = local_day_window() else {
        return Ok(());
    };
    let marker_path = data_dir.join("last_daily_focus_report.txt");
    let last_logged = fs::read_to_string(&marker_path).unwrap_or_default();
    if last_logged.trim() == previous_day {
        return Ok(());
    }

    let target = state
        .lock()
        .ok()
        .and_then(|state| state.focus.as_ref().map(|focus| focus.target.clone()))
        .or_else(|| load_focus(data_dir).map(|focus| focus.target))
        .unwrap_or_default();
    let report = focus_report_json(
        data_dir,
        &target,
        Some(previous_start),
        Some(today_start),
        "day",
    )?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("daily_focus_reports.jsonl"))?;
    writeln!(
        file,
        "{{\"day\":\"{}\",\"archivedAt\":{},\"report\":{}}}",
        json_escape(&previous_day),
        now(),
        report
    )?;
    fs::write(marker_path, previous_day)
}

fn local_day_window() -> Option<(String, i64, i64)> {
    #[cfg(target_os = "macos")]
    {
        let today = command_text("date", &["+%Y-%m-%d"])?;
        let yesterday = command_text("date", &["-v-1d", "+%Y-%m-%d"])?;
        let today_start = command_text(
            "date",
            &["-j", "-f", "%Y-%m-%d %H:%M:%S", &format!("{today} 00:00:00"), "+%s"],
        )?
        .parse()
        .ok()?;
        let yesterday_start = command_text(
            "date",
            &[
                "-j",
                "-f",
                "%Y-%m-%d %H:%M:%S",
                &format!("{yesterday} 00:00:00"),
                "+%s",
            ],
        )?
        .parse()
        .ok()?;
        return Some((yesterday, yesterday_start, today_start));
    }

    #[cfg(target_os = "linux")]
    {
        let today = command_text("date", &["+%Y-%m-%d"])?;
        let yesterday = command_text("date", &["-d", "yesterday", "+%Y-%m-%d"])?;
        let today_start = command_text("date", &["-d", &format!("{today} 00:00:00"), "+%s"])?
            .parse()
            .ok()?;
        let yesterday_start =
            command_text("date", &["-d", &format!("{yesterday} 00:00:00"), "+%s"])?
                .parse()
                .ok()?;
        return Some((yesterday, yesterday_start, today_start));
    }

    #[cfg(target_os = "windows")]
    {
        let script = "$today=Get-Date -Hour 0 -Minute 0 -Second 0 -Millisecond 0; $y=$today.AddDays(-1); \"$($y.ToString('yyyy-MM-dd'))|$([int][double]::Parse((Get-Date $y -UFormat %s)))|$([int][double]::Parse((Get-Date $today -UFormat %s)))\"";
        let value = command_text("powershell", &["-NoProfile", "-Command", script])?;
        let mut parts = value.split('|');
        let day = parts.next()?.to_string();
        let start = parts.next()?.parse().ok()?;
        let end = parts.next()?.parse().ok()?;
        return Some((day, start, end));
    }
}

fn command_text(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn report_window_start(data_dir: &PathBuf) -> io::Result<i64> {
    let path = data_dir.join("report_start.txt");
    if !path.exists() {
        return Ok(0);
    }

    let value = fs::read_to_string(path)?;
    Ok(value.trim().parse().unwrap_or(0))
}

fn state_json(focus: Option<FocusSession>) -> String {
    match focus {
        Some(focus) => {
            let elapsed = focus_elapsed_seconds(&focus, now());
            let remaining = ((focus.duration_minutes * 60) as i64 - elapsed).max(0);
            format!(
                "{{\"focus\":{{\"task\":\"{}\",\"target\":\"{}\",\"startedAt\":{},\"durationMinutes\":{},\"alertDelaySeconds\":{},\"alertAction\":\"{}\",\"redirectApp\":\"{}\",\"paused\":{},\"remainingSeconds\":{}}}}}",
                json_escape(&focus.task),
                json_escape(&focus.target),
                focus.started_at,
                focus.duration_minutes,
                focus.alert_delay_seconds,
                json_escape(&focus.alert_action),
                json_escape(&focus.redirect_app),
                focus.paused_at.is_some(),
                remaining
            )
        }
        None => "{\"focus\":null}".into(),
    }
}

fn segment_json(sample: &ActivitySample, start: i64, end: i64) -> String {
    let category = match sample.category.as_str() {
        "productive" => "productive",
        "idle" => "idle",
        _ => "distracting",
    };
    format!(
        "{{\"start\":{},\"end\":{},\"durationSeconds\":{},\"app\":\"{}\",\"title\":\"{}\",\"source\":\"{}\",\"category\":\"{}\"}}",
        start,
        end,
        (end - start + SAMPLE_SECONDS as i64).max(SAMPLE_SECONDS as i64),
        json_escape(&sample.app),
        json_escape(&sample.title),
        json_escape(&sample.source),
        category
    )
}

fn print_report(data_dir: PathBuf) -> io::Result<()> {
    println!("{}", report_json(&data_dir)?);
    Ok(())
}

fn index_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Local Focus</title>
<style>
:root { color-scheme: light dark; --bg:#f6f6f1; --ink:#202124; --muted:#686b63; --line:#d9dbd2; --good:#24734d; --warn:#9b6418; --bad:#a8323b; --panel:#ffffff; --panel-soft:#f0f1ea; --accent:#355c7d; --shadow:0 18px 45px rgba(32,33,36,.08); }
@media (prefers-color-scheme: dark) { :root { --bg:#171816; --ink:#f1f1e9; --muted:#aeb0a8; --line:#34362f; --panel:#22231f; --panel-soft:#1c1d19; --shadow:0 18px 45px rgba(0,0,0,.22); } }
* { box-sizing: border-box; }
body { margin:0; font:14px/1.4 system-ui, -apple-system, Segoe UI, sans-serif; background:var(--bg); color:var(--ink); }
header { display:flex; align-items:center; justify-content:space-between; gap:16px; padding:18px 24px; border-bottom:1px solid var(--line); background:color-mix(in srgb, var(--panel) 82%, transparent); backdrop-filter:blur(12px); position:sticky; top:0; z-index:20; }
.header-actions { display:grid; gap:8px; justify-items:end; }
.header-actions button { padding:7px 11px; }
h1 { margin:0; font-size:20px; }
main { max-width:1180px; margin:0 auto; padding:24px; display:grid; gap:18px; }
.bar { display:flex; flex-wrap:wrap; gap:10px; align-items:center; }
input, select, textarea, button { border:1px solid var(--line); border-radius:8px; padding:10px 12px; background:var(--panel); color:var(--ink); }
textarea { min-height:88px; resize:vertical; font:inherit; }
button { cursor:pointer; font-weight:700; }
button:disabled { cursor:not-allowed; opacity:.55; }
.focus-shell { background:linear-gradient(180deg, color-mix(in srgb, var(--panel) 92%, var(--panel-soft)), var(--panel)); border:1px solid var(--line); border-radius:12px; padding:18px; display:grid; gap:16px; box-shadow:var(--shadow); }
.focus-shell-head { display:flex; align-items:center; justify-content:space-between; gap:14px; }
.focus-title { display:flex; align-items:center; gap:12px; }
.focus-mark { width:42px; height:42px; border-radius:10px; background:linear-gradient(135deg, var(--accent), var(--good)); color:white; display:grid; place-items:center; font-weight:850; letter-spacing:.04em; }
.focus-shell h2 { margin:0; font-size:18px; }
.control-shell { background:var(--panel); border:1px solid var(--line); border-radius:12px; padding:16px; display:grid; gap:14px; }
.control-shell h2 { margin:0; font-size:16px; }
.report-calendar { display:grid; gap:12px; }
.calendar-head { display:grid; grid-template-columns:auto 1fr auto; gap:10px; align-items:center; }
.calendar-title { text-align:center; font-weight:800; }
.calendar-actions { display:grid; grid-template-columns:repeat(3, minmax(0, 1fr)); gap:10px; }
.calendar-actions button, .week-button, .day-button { min-height:40px; }
.calendar-grid { display:grid; grid-template-columns:64px repeat(7, minmax(0, 1fr)); gap:6px; align-items:stretch; }
.calendar-label { color:var(--muted); font-size:12px; font-weight:750; text-align:center; padding:4px; }
.week-button, .day-button { width:100%; padding:8px 6px; }
.day-button.outside { color:var(--muted); opacity:.65; }
.day-button.selected { background:var(--good); border-color:var(--good); color:white; }
.focus-task-window { border:1px solid var(--line); border-radius:10px; padding:12px; background:var(--panel-soft); display:grid; gap:8px; }
.focus-task-window.disabled { opacity:.55; }
.focus-session-list { display:grid; gap:8px; }
.focus-session-row { border:1px solid var(--line); border-radius:8px; padding:9px; background:var(--panel); }
.block-fields { display:grid; grid-template-columns:minmax(0, 1fr) auto; gap:12px; align-items:end; }
.block-fields button { min-height:42px; min-width:140px; white-space:nowrap; }
.focus-layout { display:grid; grid-template-columns:minmax(0, 1.3fr) minmax(320px, .7fr); gap:16px; align-items:start; }
.focus-layout.editor-collapsed { grid-template-columns:minmax(0, 520px); }
.focus-layout.editor-collapsed .focus-form { display:none; }
.focus-form { display:grid; grid-template-columns:repeat(2, minmax(0, 1fr)); gap:12px; align-items:end; }
.focus-form .field-wide { grid-column:1 / -1; }
.target-builder { display:grid; gap:8px; }
.target-entry { display:grid; grid-template-columns:minmax(0, 1fr) auto; gap:8px; }
.target-entry button { min-width:96px; }
.target-list-editor { display:flex; flex-wrap:wrap; gap:8px; min-height:38px; padding:8px; border:1px solid var(--line); border-radius:8px; background:var(--panel-soft); }
.target-list-editor.empty::before { content:"Add up to 10 focus apps or websites."; color:var(--muted); }
.target-remove { display:inline-flex; align-items:center; gap:6px; max-width:100%; border:1px solid color-mix(in srgb, var(--accent) 35%, var(--line)); border-radius:999px; padding:5px 9px; background:var(--panel); color:var(--ink); font:inherit; font-weight:650; overflow-wrap:anywhere; }
.target-remove span { color:var(--muted); font-weight:850; }
.focus-actions { display:flex; flex-wrap:wrap; gap:10px; align-items:center; justify-content:flex-end; }
.focus-side { border:1px solid var(--line); border-radius:10px; padding:14px; background:var(--panel-soft); display:grid; gap:12px; }
.focus-side h3 { margin:0; font-size:13px; color:var(--muted); text-transform:uppercase; letter-spacing:.06em; }
.quick-metrics { display:grid; grid-template-columns:repeat(3, minmax(0, 1fr)); gap:8px; }
.quick-metric { border:1px solid var(--line); border-radius:8px; padding:10px; background:var(--panel); }
.quick-metric span { color:var(--muted); display:block; font-size:11px; font-weight:700; }
.quick-metric strong { display:block; margin-top:2px; font-size:16px; }
.status-chip { border:1px solid var(--line); border-radius:999px; padding:6px 10px; background:color-mix(in srgb, var(--line) 25%, transparent); color:var(--muted); font-weight:700; }
.status-chip.running { color:var(--good); border-color:color-mix(in srgb, var(--good) 45%, var(--line)); background:color-mix(in srgb, var(--good) 10%, transparent); }
.status-chip.paused { color:var(--warn); border-color:color-mix(in srgb, var(--warn) 45%, var(--line)); background:color-mix(in srgb, var(--warn) 12%, transparent); }
.focus-details-toggle { padding:6px 10px; }
.top-actions { display:flex; flex-wrap:wrap; gap:8px; justify-content:flex-end; }
.top-actions button { white-space:nowrap; }
.focus-details { display:none; border:1px solid var(--line); border-radius:10px; padding:14px; color:var(--muted); overflow-wrap:anywhere; background:var(--panel); }
.focus-details.open { display:grid; gap:10px; }
.detail-grid { display:grid; grid-template-columns:repeat(3, minmax(0, 1fr)); gap:10px; }
.detail-card { border:1px solid var(--line); border-radius:8px; padding:10px; background:var(--panel-soft); min-width:0; }
.detail-card span { color:var(--muted); display:block; font-size:11px; font-weight:750; text-transform:uppercase; letter-spacing:.05em; }
.detail-card strong { display:block; margin-top:4px; color:var(--ink); overflow-wrap:anywhere; }
.target-chips { display:flex; flex-wrap:wrap; gap:6px; }
.target-chip { max-width:100%; border:1px solid color-mix(in srgb, var(--accent) 35%, var(--line)); border-radius:999px; padding:5px 9px; background:color-mix(in srgb, var(--accent) 8%, transparent); color:var(--ink); overflow-wrap:anywhere; }
.field { display:grid; gap:4px; }
.field label { color:var(--muted); font-size:12px; font-weight:650; }
.field input, .field select, .field textarea { width:100%; min-width:150px; }
.field-wide input { min-width:280px; }
.source-toggle { display:inline; max-width:100%; padding:0; border:0; background:transparent; color:var(--ink); font:inherit; font-weight:500; text-align:left; overflow-wrap:anywhere; }
.source-toggle:hover { text-decoration:underline; }
.focus-btn { transition: background .15s ease, border-color .15s ease, color .15s ease; }
.focus-idle { border-color:var(--good); color:var(--good); }
.focus-running { background:var(--good); border-color:var(--good); color:white; }
.focus-paused { background:var(--warn); border-color:var(--warn); color:white; }
.focus-stop-active { border-color:var(--bad); color:var(--bad); }
.grid { display:grid; grid-template-columns:repeat(4, minmax(0, 1fr)); gap:12px; }
.metric, .timeline, .apps, .explain, .history, .report { background:var(--panel); border:1px solid var(--line); border-radius:10px; padding:16px; }
.metric strong { display:block; font-size:28px; }
.muted { color:var(--muted); }
.explain { display:none; }
.explain.open { display:block; }
.history { display:none; }
.history.open { display:block; }
.report { display:none; }
.report.open { display:grid; gap:16px; }
.explain-grid { display:grid; grid-template-columns:repeat(5, minmax(0, 1fr)); gap:12px; }
.history-grid { display:grid; grid-template-columns:repeat(4, minmax(0, 1fr)); gap:10px; }
.report-grid { display:grid; grid-template-columns:repeat(4, minmax(0, 1fr)); gap:12px; }
.report-two { display:grid; grid-template-columns:1.2fr 1fr; gap:16px; align-items:start; }
.report h2, .report h3 { margin:0; }
.report-card { border:1px solid var(--line); border-radius:8px; padding:14px; min-width:0; }
.report-card strong { display:block; font-size:24px; margin-top:4px; }
.target-list { display:grid; gap:12px; margin-top:12px; }
.target-row { display:grid; gap:8px; border-top:1px solid var(--line); padding-top:12px; }
.target-head { display:flex; align-items:baseline; justify-content:space-between; gap:12px; }
.target-name { min-width:0; font-weight:700; overflow-wrap:anywhere; }
.target-total { color:var(--ink); font-weight:750; white-space:nowrap; }
.target-stack { display:flex; height:16px; overflow:hidden; border-radius:999px; background:color-mix(in srgb, var(--line) 55%, transparent); }
.target-active { background:var(--good); min-width:2px; }
.target-idle { background:var(--warn); min-width:2px; }
.target-meta { display:flex; flex-wrap:wrap; gap:8px; }
.meta-pill { border:1px solid var(--line); border-radius:999px; padding:3px 8px; color:var(--muted); font-size:12px; }
.bar-row { display:grid; grid-template-columns:minmax(110px, 1fr) 2fr 72px; gap:10px; align-items:center; margin:10px 0; }
.bar-track { height:12px; background:color-mix(in srgb, var(--line) 55%, transparent); border-radius:999px; overflow:hidden; }
.bar-fill { height:100%; background:var(--good); border-radius:999px; min-width:2px; }
.bar-fill.bad { background:var(--bad); }
.split-chart { min-height:170px; border-radius:8px; background:conic-gradient(var(--good) var(--focus-angle), var(--bad) 0); border:1px solid var(--line); display:grid; place-items:center; }
.split-chart span { background:var(--panel); border:1px solid var(--line); border-radius:999px; padding:18px 20px; font-weight:750; }
.hour-bars { display:grid; grid-template-columns:repeat(12, minmax(34px, 1fr)); gap:10px; align-items:end; min-height:150px; }
.hour-bar { display:grid; align-items:end; height:120px; gap:2px; }
.hour-segment { position:relative; border-radius:4px 4px 0 0; min-height:2px; cursor:default; }
.hour-click { border:0; background:transparent; padding:0; color:inherit; width:100%; cursor:pointer; }
.hour-click.selected .hour-bar { outline:2px solid color-mix(in srgb, var(--accent) 75%, var(--line)); outline-offset:3px; border-radius:6px; }
.hour-detail { margin-top:12px; display:grid; gap:12px; }
.hour-detail-head { display:flex; flex-wrap:wrap; gap:10px; justify-content:space-between; align-items:flex-start; }
.hour-detail-title h3 { margin:0; }
.hour-summary { display:flex; flex-wrap:wrap; gap:8px; }
.hour-summary .meta-pill strong { color:var(--ink); margin-left:4px; }
.detail-stack { display:flex; height:18px; overflow:hidden; border-radius:999px; background:color-mix(in srgb, var(--line) 55%, transparent); }
.detail-stack span { min-width:2px; }
.detail-good { background:var(--good); }
.detail-idle { background:var(--warn); }
.detail-bad { background:var(--bad); }
.activity-mix { display:grid; gap:10px; }
.activity-row { display:grid; grid-template-columns:minmax(0, 1fr) 110px; gap:12px; align-items:center; border-top:1px solid var(--line); padding-top:10px; }
.activity-main { min-width:0; }
.activity-title { display:flex; flex-wrap:wrap; gap:8px; align-items:center; }
.activity-title strong { overflow-wrap:anywhere; }
.activity-bar { display:grid; gap:4px; }
.activity-bar-track { height:8px; border-radius:999px; background:color-mix(in srgb, var(--line) 55%, transparent); overflow:hidden; }
.activity-bar-fill { height:100%; border-radius:999px; min-width:2px; }
.hour-segment:hover::after { content:attr(data-tip); position:absolute; left:50%; bottom:calc(100% + 8px); transform:translateX(-50%); z-index:10; width:max-content; max-width:220px; padding:6px 8px; border:1px solid var(--line); border-radius:6px; background:var(--panel); color:var(--ink); box-shadow:0 8px 24px color-mix(in srgb, var(--ink) 18%, transparent); font-size:12px; font-weight:650; white-space:normal; }
.hour-segment:hover::before { content:""; position:absolute; left:50%; bottom:100%; transform:translateX(-50%); border:5px solid transparent; border-top-color:var(--line); z-index:11; }
.hour-good, .hour-bad { border-radius:4px 4px 0 0; min-height:2px; }
.hour-good { background:var(--good); }
.hour-bad { background:var(--bad); }
.insights { display:grid; gap:8px; }
.insights p { margin:0; padding:10px 12px; border:1px solid var(--line); border-radius:8px; }
.explain h2 { margin:0 0 12px; font-size:16px; }
.history h2 { margin:0 0 12px; font-size:16px; }
.explain h3, .history h3 { margin:0 0 4px; font-size:13px; }
.explain p, .history p { margin:0; color:var(--muted); }
.timeline { display:grid; gap:10px; }
.item { display:grid; grid-template-columns:120px 1fr 96px; gap:12px; align-items:start; border-top:1px solid var(--line); padding-top:10px; }
.item.long-attention { border-left:4px solid var(--bad); padding-left:10px; background:color-mix(in srgb, var(--bad) 7%, transparent); }
.item.long-attention.long-idle { border-left-color:var(--warn); background:color-mix(in srgb, var(--warn) 8%, transparent); }
.long-note { display:inline-block; margin-top:4px; border-radius:999px; padding:2px 7px; font-size:11px; font-weight:700; color:var(--bad); background:color-mix(in srgb, var(--bad) 14%, transparent); }
.long-idle .long-note { color:var(--warn); background:color-mix(in srgb, var(--warn) 16%, transparent); }
.tag { width:max-content; border-radius:999px; padding:2px 8px; font-size:12px; }
.productive { color:var(--good); background:color-mix(in srgb, var(--good) 15%, transparent); }
.distracting { color:var(--bad); background:color-mix(in srgb, var(--bad) 14%, transparent); }
.idle { color:var(--warn); background:color-mix(in srgb, var(--warn) 16%, transparent); }
.two { display:grid; grid-template-columns:2fr 1fr; gap:18px; }
@media (max-width:980px) { .focus-layout, .control-shell { grid-template-columns:1fr; } .focus-actions { justify-content:flex-start; } }
@media (max-width:900px) { .focus-shell-head { align-items:start; display:grid; } .top-actions { justify-content:flex-start; } .block-fields { grid-template-columns:minmax(0, 1fr) auto; } }
@media (max-width:760px) { header, .two, .grid, .item, .explain-grid, .history-grid, .report-grid, .report-two, .bar-row, .focus-form, .detail-grid, .block-fields, .activity-row, .calendar-actions { grid-template-columns:1fr; display:grid; } header { align-items:start; } .header-actions { justify-items:start; } .hour-bars { grid-template-columns:repeat(6, minmax(12px, 1fr)); } .focus-shell-head { align-items:start; display:grid; } .quick-metrics { grid-template-columns:1fr; } .calendar-grid { grid-template-columns:48px repeat(7, minmax(28px, 1fr)); gap:4px; } }
</style>
</head>
<body>
<header>
  <div><h1>Local Focus</h1><div class="muted">Private activity timeline, focus sessions, and reports. All data stays on this device.</div></div>
  <div class="header-actions">
    <div id="focusState" class="status-chip"></div>
    <button id="explainToggle" onclick="toggleExplain()" aria-expanded="false">Explain</button>
  </div>
</header>
<main>
  <section class="focus-shell">
    <div class="focus-shell-head">
      <div class="focus-title">
        <div class="focus-mark">LF</div>
        <div><h2>Focus setup</h2><div class="muted">Choose what counts as focused work. Everything else is tracked as distraction.</div></div>
      </div>
      <div class="top-actions">
        <button id="focusEditorToggle" class="focus-details-toggle" onclick="toggleFocusEditor()" aria-expanded="true">Hide edit details</button>
        <button id="focusDetailsToggle" class="focus-details-toggle" onclick="toggleFocusDetails()" aria-expanded="false">Show focus details</button>
      </div>
    </div>
    <div id="focusDetails" class="focus-details"></div>
    <div id="focusEditor" class="focus-layout">
      <div class="focus-form">
        <div class="field field-wide target-builder">
          <label for="targetInput">Focus apps and websites</label>
          <div class="target-entry">
            <input id="targetInput" placeholder="Pages, https://claude.ai/" aria-label="Focus app or website">
            <button type="button" onclick="addFocusTarget()">Add</button>
          </div>
          <div id="targetListEditor" class="target-list-editor empty" aria-live="polite"></div>
          <input id="target" type="hidden" aria-label="Focus targets">
        </div>
        <div class="field"><label for="minutes">Focus timer</label><input id="minutes" type="number" min="1" max="180" value="25" aria-label="Minutes"></div>
        <div class="field"><label for="alertMinutes">Warn after</label><input id="alertMinutes" type="number" min="1" max="60" value="1" aria-label="Alert after minutes" title="Alert after minutes outside focus"></div>
        <div class="field"><label for="alertAction">Warning action</label><select id="alertAction" aria-label="After delay action" title="After delay action">
          <option value="alert">Show alert</option>
          <option value="switch">Move to app</option>
        </select></div>
        <div class="field"><label for="redirectApp">App to move to</label><input id="redirectApp" placeholder="Pages" aria-label="Move focus to app"></div>
      </div>
      <aside class="focus-side">
        <h3>Current session</h3>
        <div class="quick-metrics">
          <div class="quick-metric"><span>Status</span><strong id="quickStatus">Off</strong></div>
          <div class="quick-metric"><span>Warn after</span><strong id="quickDelay">1m</strong></div>
          <div class="quick-metric"><span>Action</span><strong id="quickAction">Alert</strong></div>
        </div>
        <div class="focus-actions">
          <button id="startFocus" class="focus-btn focus-idle" onclick="startFocus()">Start focus</button>
          <button id="pauseFocus" class="focus-btn" onclick="pauseFocus()" disabled>Pause</button>
          <button id="stopFocus" class="focus-btn" onclick="stopFocus()" disabled>Stop</button>
          <button onclick="resetReport()">Refresh</button>
        </div>
      </aside>
    </div>
  </section>
  <section class="control-shell" aria-label="Reports">
    <div>
      <h2>Reports</h2>
      <div class="muted">Click a year, month, week, or date to generate that report.</div>
    </div>
    <div class="report-calendar">
      <div class="calendar-head">
        <button type="button" onclick="moveCalendarMonth(-1)" aria-label="Previous month">Prev</button>
        <div id="calendarTitle" class="calendar-title"></div>
        <button type="button" onclick="moveCalendarMonth(1)" aria-label="Next month">Next</button>
      </div>
      <div class="calendar-actions">
        <button id="yearReportButton" type="button" onclick="generateCalendarReport('year')"></button>
        <button id="monthReportButton" type="button" onclick="generateCalendarReport('month')"></button>
        <button id="selectedWeekButton" type="button" onclick="generateCalendarReport('week')"></button>
      </div>
      <div id="calendarGrid" class="calendar-grid" aria-label="Report calendar"></div>
      <div id="focusTaskWindow" class="focus-task-window">
        <div><strong>Report window</strong><div class="muted" id="focusTaskWindowHint">Focus tasks created for the selected date.</div></div>
        <div id="focusSessionList" class="focus-session-list"></div>
      </div>
    </div>
  </section>
  <section class="control-shell" aria-label="Distraction rules">
    <div>
      <h2>Add distraction rule</h2>
      <div class="muted">Mark keywords, apps, or sites as distracting and eligible for alerts.</div>
    </div>
    <div class="block-fields">
      <div class="field"><label for="blockKeyword">Block keyword, app, or site</label><input id="blockKeyword" placeholder="youtube, reddit, games" aria-label="Block keyword"></div>
      <button onclick="addBlock()">Add block</button>
    </div>
  </section>
  <section class="explain" id="explainPanel">
    <h2>Report meaning</h2>
    <div class="explain-grid">
      <div><h3>Total time</h3><p>All tracked time in the current report window: productive, distracted, and idle.</p></div>
      <div><h3>Productive</h3><p>During a targeted focus session, only activity matching one of your focus apps or sites counts here. Outside targeted focus, productive keywords are used.</p></div>
      <div><h3>Distracted</h3><p>Any activity that is not productive. During targeted focus, every app or site outside your focus list is tracked here.</p></div>
      <div><h3>Idle</h3><p>If there is no keyboard or mouse input for 60 seconds, time is tracked as idle even when the focused app or website matches your focus list.</p></div>
      <div><h3>Blocked</h3><p>Blocked keywords are treated as distracted activity and can still trigger OS-level warnings.</p></div>
    </div>
  </section>
  <section class="grid" id="metrics"></section>
  <section class="report" id="focusReportPanel" aria-live="polite"></section>
  <section class="bar">
    <button id="historyToggle" onclick="toggleHistory()" aria-expanded="false">Previous reports</button>
  </section>
  <section class="history" id="historyPanel">
    <h2>Previous reports</h2>
    <div id="historyList"></div>
  </section>
  <section class="two">
    <section class="timeline"><h2>Timeline</h2><div id="timeline"></div></section>
    <section class="apps"><h2>Top apps and URLs</h2><div id="apps"></div></section>
  </section>
</main>
<script>
const focusDraftKey = 'local-focus-draft';
let focusEditorManuallyOpened = false;
let focusTargets = [];
let currentFocusReport = null;
let calendarDate = new Date();
let selectedReportDate = new Date();
const fmtTime = seconds => new Date(seconds * 1000).toLocaleTimeString([], {hour:'2-digit', minute:'2-digit'});
const minutes = seconds => Math.max(1, Math.round(seconds / 60));
async function startFocus() {
  saveFocusDraft();
  const task = encodeURIComponent('Focus session');
  const target = encodeURIComponent(document.querySelector('#target').value || '');
  const mins = encodeURIComponent(document.querySelector('#minutes').value || '25');
  const alertSeconds = encodeURIComponent(Math.max(1, Number(document.querySelector('#alertMinutes').value || '1')) * 60);
  const alertAction = encodeURIComponent(document.querySelector('#alertAction').value || 'alert');
  const redirectApp = encodeURIComponent(document.querySelector('#redirectApp').value || '');
  await fetch(`/api/focus/start?task=${task}&target=${target}&minutes=${mins}&alertSeconds=${alertSeconds}&alertAction=${alertAction}&redirectApp=${redirectApp}`);
  refresh();
}
async function stopFocus() { await fetch('/api/focus/stop'); refresh(); }
async function pauseFocus() { await fetch('/api/focus/pause'); refresh(); }
async function resetReport() {
  await fetch('/api/report/reset');
  document.querySelector('#focusReportPanel').classList.remove('open');
  document.querySelector('#focusReportPanel').innerHTML = '';
  refresh();
}
async function addBlock() {
  const input = document.querySelector('#blockKeyword');
  const keyword = encodeURIComponent(input.value || '');
  if (!keyword) return;
  await fetch(`/api/block/add?keyword=${keyword}`);
  input.value = '';
  refresh();
}
function saveFocusDraft() {
  localStorage.setItem(focusDraftKey, JSON.stringify({
    target: document.querySelector('#target').value,
    minutes: document.querySelector('#minutes').value,
    alertMinutes: document.querySelector('#alertMinutes').value,
    alertAction: document.querySelector('#alertAction').value,
    redirectApp: document.querySelector('#redirectApp').value
  }));
}
function restoreFocusDraft() {
  try {
    const draft = JSON.parse(localStorage.getItem(focusDraftKey) || '{}');
    if (draft.target) setFocusTargets(draft.target);
    if (draft.minutes) document.querySelector('#minutes').value = draft.minutes;
    if (draft.alertMinutes) document.querySelector('#alertMinutes').value = draft.alertMinutes;
    if (draft.alertAction) document.querySelector('#alertAction').value = draft.alertAction;
    if (draft.redirectApp) document.querySelector('#redirectApp').value = draft.redirectApp;
  } catch {}
  ['#minutes', '#alertMinutes', '#alertAction', '#redirectApp'].forEach(selector => {
    document.querySelector(selector).addEventListener('input', saveFocusDraft);
    document.querySelector(selector).addEventListener('change', saveFocusDraft);
  });
  document.querySelector('#targetInput').addEventListener('keydown', event => {
    if (event.key === 'Enter') {
      event.preventDefault();
      addFocusTarget();
    }
  });
}
function setFocusTargets(value) {
  focusTargets = String(value || '').split(/[,\n]/).map(item => item.trim()).filter(Boolean).slice(0, 10);
  syncFocusTargets();
}
function syncFocusTargets() {
  document.querySelector('#target').value = focusTargets.join(', ');
  const editor = document.querySelector('#targetListEditor');
  editor.classList.toggle('empty', focusTargets.length === 0);
  editor.innerHTML = focusTargets.map((target, index) => `
    <button type="button" class="target-remove" onclick="removeFocusTarget(${index})">${escapeHtml(shortenSource(target))} <span aria-hidden="true">x</span></button>
  `).join('');
  saveFocusDraft();
}
function addFocusTarget() {
  const input = document.querySelector('#targetInput');
  const value = input.value.trim();
  if (!value || focusTargets.length >= 10) return;
  for (const target of value.split(/[,\n]/).map(item => item.trim()).filter(Boolean)) {
    if (focusTargets.length >= 10) break;
    if (!focusTargets.some(existing => existing.toLowerCase() === target.toLowerCase())) {
      focusTargets.push(target);
    }
  }
  input.value = '';
  syncFocusTargets();
}
function removeFocusTarget(index) {
  focusTargets.splice(index, 1);
  syncFocusTargets();
}
function toggleExplain() {
  const panel = document.querySelector('#explainPanel');
  const button = document.querySelector('#explainToggle');
  const open = panel.classList.toggle('open');
  button.setAttribute('aria-expanded', String(open));
  button.textContent = open ? 'Hide explanation' : 'Explain';
}
function toggleHistory() {
  const panel = document.querySelector('#historyPanel');
  const button = document.querySelector('#historyToggle');
  const open = panel.classList.toggle('open');
  button.setAttribute('aria-expanded', String(open));
  button.textContent = open ? 'Hide previous reports' : 'Previous reports';
}
function toggleFocusDetails() {
  const panel = document.querySelector('#focusDetails');
  const button = document.querySelector('#focusDetailsToggle');
  const open = panel.classList.toggle('open');
  button.setAttribute('aria-expanded', String(open));
  button.textContent = open ? 'Hide focus details' : 'Show focus details';
}
function setFocusEditorOpen(open, manual = false) {
  const editor = document.querySelector('#focusEditor');
  const button = document.querySelector('#focusEditorToggle');
  if (manual) focusEditorManuallyOpened = open;
  editor.classList.toggle('editor-collapsed', !open);
  button.setAttribute('aria-expanded', String(open));
  button.textContent = open ? 'Hide edit details' : 'Edit focus details';
}
function toggleFocusEditor() {
  const editor = document.querySelector('#focusEditor');
  setFocusEditorOpen(editor.classList.contains('editor-collapsed'), true);
}
async function runCalendarReport(period, dateValue = selectedReportDate) {
  const panel = document.querySelector('#focusReportPanel');
  const target = document.querySelector('#target').value || '';
  const windowRange = calendarPeriodWindow(period, dateValue);
  const since = Math.floor(windowRange.since.getTime() / 1000);
  const until = Math.floor(windowRange.until.getTime() / 1000);
  setFocusTaskWindow(period, windowRange);
  try {
    const report = await fetch(`/api/focus-report?target=${encodeURIComponent(target)}&since=${since}&until=${until}&period=${encodeURIComponent(period)}`).then(r => r.json());
    currentFocusReport = report;
    panel.innerHTML = renderFocusReport(report);
    panel.classList.add('open');
  } catch (error) {
    panel.innerHTML = `<p class="muted">Could not generate report.</p>`;
    panel.classList.add('open');
  }
}
function calendarPeriodWindow(period, dateValue) {
  const start = new Date(dateValue);
  start.setHours(0, 0, 0, 0);
  if (period === 'week') {
    const offset = start.getDay() === 0 ? 6 : start.getDay() - 1;
    start.setDate(start.getDate() - offset);
  } else if (period === 'month') {
    start.setDate(1);
  } else if (period === 'year') {
    start.setMonth(0, 1);
  }
  const end = new Date(start);
  if (period === 'day') end.setDate(end.getDate() + 1);
  if (period === 'week') end.setDate(end.getDate() + 7);
  if (period === 'month') end.setMonth(end.getMonth() + 1);
  if (period === 'year') end.setFullYear(end.getFullYear() + 1);
  return { since: start, until: end };
}
function moveCalendarMonth(delta) {
  calendarDate.setMonth(calendarDate.getMonth() + delta);
  renderReportCalendar();
}
function renderReportCalendar() {
  const monthStart = new Date(calendarDate.getFullYear(), calendarDate.getMonth(), 1);
  const gridStart = new Date(monthStart);
  gridStart.setDate(gridStart.getDate() - (gridStart.getDay() === 0 ? 6 : gridStart.getDay() - 1));
  document.querySelector('#calendarTitle').textContent = monthStart.toLocaleDateString([], {month:'long', year:'numeric'});
  document.querySelector('#yearReportButton').textContent = String(monthStart.getFullYear());
  document.querySelector('#monthReportButton').textContent = monthStart.toLocaleDateString([], {month:'long'});
  document.querySelector('#selectedWeekButton').textContent = `Week ${isoWeekNumber(selectedReportDate)}`;
  const labels = ['Week', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
  let html = labels.map(label => `<div class="calendar-label">${label}</div>`).join('');
  for (let row = 0; row < 6; row += 1) {
    const weekDate = new Date(gridStart);
    weekDate.setDate(gridStart.getDate() + row * 7);
    html += `<button type="button" class="week-button" onclick="selectCalendarWeek(${weekDate.getFullYear()}, ${weekDate.getMonth()}, ${weekDate.getDate()})">W${isoWeekNumber(weekDate)}</button>`;
    for (let col = 0; col < 7; col += 1) {
      const day = new Date(weekDate);
      day.setDate(weekDate.getDate() + col);
      const outside = day.getMonth() !== monthStart.getMonth();
      const selected = sameDate(day, selectedReportDate);
      html += `<button type="button" class="day-button ${outside ? 'outside' : ''} ${selected ? 'selected' : ''}" onclick="selectCalendarDay(${day.getFullYear()}, ${day.getMonth()}, ${day.getDate()})">${day.getDate()}</button>`;
    }
  }
  document.querySelector('#calendarGrid').innerHTML = html;
}
function selectCalendarDay(year, month, day) {
  selectedReportDate = new Date(year, month, day);
  calendarDate = new Date(year, month, 1);
  renderReportCalendar();
  runCalendarReport('day', selectedReportDate);
}
function selectCalendarWeek(year, month, day) {
  selectedReportDate = new Date(year, month, day);
  calendarDate = new Date(year, month, 1);
  renderReportCalendar();
  runCalendarReport('week', selectedReportDate);
}
async function setFocusTaskWindow(period, windowRange) {
  const shell = document.querySelector('#focusTaskWindow');
  const hint = document.querySelector('#focusTaskWindowHint');
  const list = document.querySelector('#focusSessionList');
  shell.classList.toggle('disabled', period !== 'day');
  if (period !== 'day') {
    hint.textContent = 'Available only when a single date is selected.';
    list.innerHTML = '<p class="muted">Select a date to see focus tasks created that day.</p>';
    return;
  }
  hint.textContent = `Focus tasks created on ${windowRange.since.toLocaleDateString([], {dateStyle:'medium'})}.`;
  const since = Math.floor(windowRange.since.getTime() / 1000);
  const until = Math.floor(windowRange.until.getTime() / 1000);
  const sessions = await fetch(`/api/focus-sessions?since=${since}&until=${until}`).then(r => r.json());
  list.innerHTML = sessions.map(session => `
    <div class="focus-session-row">
      <strong>${escapeHtml(session.task || 'Focus session')}</strong>
      <div class="muted">${new Date(session.startedAt * 1000).toLocaleTimeString([], {hour:'numeric', minute:'2-digit'})} · ${session.durationMinutes || 0}m</div>
      <div>${escapeHtml(session.target || 'No focus apps/sites recorded')}</div>
    </div>
  `).join('') || '<p class="muted">No focus tasks were created for this date.</p>';
}
function generateCalendarReport(period) {
  if (period === 'year') {
    runCalendarReport('year', new Date(calendarDate.getFullYear(), 0, 1));
  } else if (period === 'month') {
    runCalendarReport('month', new Date(calendarDate.getFullYear(), calendarDate.getMonth(), 1));
  } else if (period === 'week') {
    runCalendarReport('week', selectedReportDate);
  } else if (period === 'day') {
    runCalendarReport('day', selectedReportDate);
  }
}
function sameDate(left, right) {
  return left.getFullYear() === right.getFullYear() && left.getMonth() === right.getMonth() && left.getDate() === right.getDate();
}
function isoWeekNumber(dateValue) {
  const date = new Date(Date.UTC(dateValue.getFullYear(), dateValue.getMonth(), dateValue.getDate()));
  const day = date.getUTCDay() || 7;
  date.setUTCDate(date.getUTCDate() + 4 - day);
  const yearStart = new Date(Date.UTC(date.getUTCFullYear(), 0, 1));
  return Math.ceil((((date - yearStart) / 86400000) + 1) / 7);
}
function renderFocusReport(report) {
  const periodName = report.period ? report.period[0].toUpperCase() + report.period.slice(1) : 'Report';
  const reportTitle = `Focus report for ${periodName.toLowerCase()}`;
  if (!report.targets.length) {
    return `<div><h2>${reportTitle}</h2><p class="muted">Add one or more focus apps or websites first, then run the report.</p></div>`;
  }
  const total = report.focusSeconds + report.outsideSeconds + report.idleSeconds;
  const maxTarget = Math.max(1, ...report.targetBreakdown.map(item => item.totalSeconds || item.seconds + (item.idleSeconds || 0)));
  const maxHour = Math.max(1, ...report.hourly.map(item => item.productiveSeconds + item.distractingSeconds + (item.idleSeconds || 0)));
  const focusAngle = `${Math.max(0, Math.min(100, report.focusPercent))}%`;
  const targetBars = report.targetBreakdown.map(item => `
    <div class="target-row">
      <div class="target-head">
        <div class="target-name">${sourceMarkup(item.target, `focus-${escapeAttr(item.target)}`)}</div>
        <div class="target-total">${formatDuration(item.totalSeconds || item.seconds + (item.idleSeconds || 0))}</div>
      </div>
      <div class="target-stack" aria-label="Active and idle time">
        <div class="target-active" style="width:${Math.max(0, item.seconds * 100 / maxTarget)}%"></div>
        <div class="target-idle" style="width:${Math.max(0, (item.idleSeconds || 0) * 100 / maxTarget)}%"></div>
      </div>
      <div class="target-meta">
        <span class="meta-pill">total ${formatDuration(item.totalSeconds || item.seconds + (item.idleSeconds || 0))}</span>
        <span class="meta-pill">focus active ${formatDuration(item.seconds)}</span>
        <span class="meta-pill">idle ${formatDuration(item.idleSeconds || 0)}</span>
      </div>
    </div>`).join('');
  const distractionRows = report.topDistractions.map((item, index) => `
    <div class="bar-row">
      <div><strong>${escapeHtml(item.app)}</strong><br>${sourceMarkup(item.source || 'local', `distraction-${index}`)}</div>
      <div class="bar-track"><div class="bar-fill bad" style="width:${Math.max(2, item.seconds * 100 / Math.max(1, report.outsideSeconds))}%"></div></div>
      <div class="muted">${formatDuration(item.seconds)}</div>
    </div>`).join('') || '<p class="muted">No outside-focus activity in this report window.</p>';
  const recentHours = report.hourly.slice(-12);
  const hours = recentHours.map(item => {
    const productiveHeight = Math.max(2, item.productiveSeconds * 100 / maxHour);
    const distractingHeight = Math.max(2, item.distractingSeconds * 100 / maxHour);
    const idleHeight = Math.max(2, (item.idleSeconds || 0) * 100 / maxHour);
    const start = new Date(item.hour * 1000);
    const end = new Date((item.hour + 3600) * 1000);
    const range = `${start.toLocaleTimeString([], {hour:'numeric'})} to ${end.toLocaleTimeString([], {hour:'numeric'})}`;
    return `<div>
      <button type="button" class="hour-click" onclick="showHourDetails(${item.hour}, this)" aria-label="Show details for ${range}">
      <div class="hour-bar">
        <div class="hour-segment hour-good" data-tip="Productive: ${formatDuration(item.productiveSeconds)} (${range})" aria-label="Productive: ${formatDuration(item.productiveSeconds)} (${range})" style="height:${productiveHeight}%"></div>
        <div class="hour-segment" data-tip="Idle: ${formatDuration(item.idleSeconds || 0)} (${range})" aria-label="Idle: ${formatDuration(item.idleSeconds || 0)} (${range})" style="background:var(--warn);height:${idleHeight}%"></div>
        <div class="hour-segment hour-bad" data-tip="Distracted: ${formatDuration(item.distractingSeconds)} (${range})" aria-label="Distracted: ${formatDuration(item.distractingSeconds)} (${range})" style="height:${distractingHeight}%"></div>
      </div>
      </button>
      <div class="muted" style="font-size:11px;text-align:center">${new Date(item.hour * 1000).toLocaleTimeString([], {hour:'numeric'})}</div>
    </div>`;
  }).join('') || '<p class="muted">No hourly data yet.</p>';
  const bestTarget = report.targetBreakdown.find(item => item.seconds > 0);
  const mainDistraction = report.topDistractions[0];
  const insights = [
    report.focusPercent >= 70 ? `Strong alignment: ${report.focusPercent}% of tracked time matched your focus list.` : `Focus drift is high: ${report.focusPercent}% of tracked time matched your focus list.`,
    bestTarget ? `Most time was spent on ${bestTarget.target}: ${formatDuration(bestTarget.seconds)}.` : 'No tracked time matched the current focus list yet.',
    report.idleSeconds ? `Idle time was ${formatDuration(report.idleSeconds)}, including idle periods inside focus apps or websites.` : 'No idle time was detected in this report window.',
    mainDistraction ? `Largest outside-focus item: ${mainDistraction.app} for ${formatDuration(mainDistraction.seconds)}.` : 'No outside-focus distractions were detected.',
    total ? `${periodName} tracked time is ${formatDuration(total)}.` : 'The report will get richer after more tracked activity.'
  ].map(text => `<p>${escapeHtml(text)}</p>`).join('');
  return `
    <div class="bar"><h2>${reportTitle}</h2><span class="muted">Since ${new Date(report.windowStart * 1000).toLocaleString([], {dateStyle:'short', timeStyle:'short'})} - generated ${new Date(report.generatedAt * 1000).toLocaleString([], {dateStyle:'short', timeStyle:'short'})}</span></div>
    <div class="report-grid">
      <div class="report-card"><span class="muted">Total time</span><strong>${formatDuration(total)}</strong></div>
      <div class="report-card"><span class="muted">Matched focus list</span><strong>${formatDuration(report.focusSeconds)}</strong></div>
      <div class="report-card"><span class="muted">Outside focus</span><strong>${formatDuration(report.outsideSeconds)}</strong></div>
      <div class="report-card"><span class="muted">Idle</span><strong>${formatDuration(report.idleSeconds)}</strong></div>
    </div>
    <div class="report-card"><h3>Time on focus apps and websites</h3><div class="target-list">${targetBars || '<p class="muted">No target activity yet.</p>'}</div></div>
    <div class="report-card"><h3>Productive vs distracted by hour</h3><div class="hour-bars">${hours}</div><div id="hourDetail" class="hour-detail"></div></div>
    <div class="report-two">
      <div class="report-card">
        <h3>Focus split</h3>
        <div class="split-chart" style="--focus-angle:${focusAngle}"><span>${report.focusPercent}% focused</span></div>
      </div>
      <div class="report-card"><h3>Analysis</h3><div class="insights">${insights}</div></div>
    </div>
    <div class="report-card"><h3>Top outside-focus activity</h3>${distractionRows}</div>`;
}
function showHourDetails(hour, button) {
  const panel = document.querySelector('#hourDetail');
  if (!panel) return;
  const end = hour + 3600;
  const hourData = currentFocusReport?.hourly?.find(item => item.hour === hour);
  document.querySelectorAll('.hour-click').forEach(item => item.classList.remove('selected'));
  if (button) button.classList.add('selected');
  const productive = hourData?.productiveSeconds || 0;
  const distracted = hourData?.distractingSeconds || 0;
  const idle = hourData?.idleSeconds || 0;
  const total = productive + distracted + idle;
  const rows = (hourData?.items || [])
    .map((item, index) => `
      <div class="activity-row">
        <div class="activity-main">
          <div class="activity-title"><strong>${escapeHtml(item.app)}</strong><span class="tag ${item.category}">${item.category}</span></div>
          <div>${escapeHtml(item.title)}</div>
          <div class="muted">${sourceMarkup(item.source || 'local', `hour-${hour}-${index}`)}</div>
        </div>
        <div class="activity-bar">
          <strong>${formatDuration(item.seconds)}</strong>
          <div class="activity-bar-track"><div class="activity-bar-fill ${item.category === 'productive' ? 'detail-good' : item.category === 'idle' ? 'detail-idle' : 'detail-bad'}" style="width:${Math.max(2, item.seconds * 100 / Math.max(1, total))}%"></div></div>
        </div>
      </div>
    `).join('');
  const startLabel = new Date(hour * 1000).toLocaleTimeString([], {hour:'numeric'});
  const endLabel = new Date(end * 1000).toLocaleTimeString([], {hour:'numeric'});
  panel.innerHTML = `
    <div class="hour-detail-head">
      <div class="hour-detail-title"><h3>${startLabel} to ${endLabel}</h3><div class="muted">Click another hour to compare the breakdown.</div></div>
      <div class="hour-summary">
        <span class="meta-pill">total <strong>${formatDuration(total)}</strong></span>
        <span class="meta-pill">productive <strong>${formatDuration(productive)}</strong></span>
        <span class="meta-pill">distracted <strong>${formatDuration(distracted)}</strong></span>
        <span class="meta-pill">idle <strong>${formatDuration(idle)}</strong></span>
      </div>
    </div>
    <div class="detail-stack" aria-label="Hour mix">
      <span class="detail-good" style="width:${Math.max(0, productive * 100 / Math.max(1, total))}%"></span>
      <span class="detail-idle" style="width:${Math.max(0, idle * 100 / Math.max(1, total))}%"></span>
      <span class="detail-bad" style="width:${Math.max(0, distracted * 100 / Math.max(1, total))}%"></span>
    </div>
    <div class="activity-mix">${rows || '<p class="muted">No detailed activity found for this hour.</p>'}</div>`;
}
async function refresh() {
  const [timeline, report, state, history] = await Promise.all([
    fetch('/api/timeline').then(r => r.json()),
    fetch('/api/report').then(r => r.json()),
    fetch('/api/state').then(r => r.json()),
    fetch('/api/report/history').then(r => r.json())
  ]);
  const totalSeconds = reportTotalSeconds(report);
  document.querySelector('#metrics').innerHTML = `
    <div class="metric"><span class="muted">Total time</span><strong>${formatDuration(totalSeconds)}</strong></div>
    <div class="metric"><span class="muted">Productive</span><strong>${formatDuration(report.productiveMinutes * 60)}</strong></div>
    <div class="metric"><span class="muted">Distracted</span><strong>${formatDuration(report.distractingMinutes * 60)}</strong></div>
    <div class="metric"><span class="muted">Idle</span><strong>${formatDuration((report.idleMinutes || 0) * 60)}</strong></div>`;
  document.querySelector('#timeline').innerHTML = timeline.slice(-80).reverse().map((item, index) => {
    const longAttention = item.durationSeconds > 15 * 60 && (item.category === 'idle' || item.category === 'distracting');
    const longClass = longAttention ? ` long-attention ${item.category === 'idle' ? 'long-idle' : 'long-distracting'}` : '';
    const longNote = longAttention ? `<span class="long-note">${item.category === 'idle' ? 'Long idle' : 'Long distraction'}</span>` : '';
    return `
    <div class="item${longClass}">
      <div class="muted">${fmtTime(item.start)}<br>${formatDuration(item.durationSeconds)}${longNote ? `<br>${longNote}` : ''}</div>
      <div><strong>${escapeHtml(item.app)}</strong><div>${escapeHtml(item.title)}</div><div class="muted">${sourceMarkup(item.source || 'local', `timeline-${index}`)}</div></div>
      <div class="tag ${item.category}">${item.category}</div>
    </div>`;
  }).join('') || '<div class="muted">No activity yet.</div>';
  document.querySelector('#apps').innerHTML = report.topApps.map((app, index) => `<p><strong>${escapeHtml(app.app)}</strong><br>${sourceMarkup(app.source || 'local', index)}<br><span class="muted">${formatDuration(app.seconds || app.minutes * 60)}</span></p>`).join('') || '<div class="muted">No apps yet.</div>';
  document.querySelector('#historyList').innerHTML = history.map(item => {
    const r = item.report;
    return `<div class="item">
      <div class="muted">${new Date(item.archivedAt * 1000).toLocaleString([], {dateStyle:'short', timeStyle:'short'})}</div>
      <div class="history-grid">
        <div><h3>Total time</h3><p>${formatDuration(reportTotalSeconds(r))}</p></div>
        <div><h3>Productive</h3><p>${formatDuration(r.productiveMinutes * 60)}</p></div>
        <div><h3>Distracted</h3><p>${formatDuration(r.distractingMinutes * 60)}</p></div>
        <div><h3>Idle</h3><p>${formatDuration((r.idleMinutes || 0) * 60)}</p></div>
      </div>
      <div class="muted">${(r.topApps || []).slice(0, 2).map(app => escapeHtml(`${app.app}${app.source ? ' - ' + app.source : ''}`)).join(', ')}</div>
    </div>`;
  }).join('') || '<div class="muted">No previous reports yet.</div>';
  updateFocusButtons(state.focus);
  seedFocusInputsFromActiveSession(state.focus);
  updateFocusSummary(state.focus);
}
function updateFocusSummary(focus) {
  const chip = document.querySelector('#focusState');
  const details = document.querySelector('#focusDetails');
  const quickStatus = document.querySelector('#quickStatus');
  const quickDelay = document.querySelector('#quickDelay');
  const quickAction = document.querySelector('#quickAction');
  if (!focus) {
    chip.textContent = 'Focus off';
    chip.className = 'status-chip';
    details.innerHTML = `<div class="detail-grid">
      <div class="detail-card"><span>Focus apps/sites</span><strong>None active</strong></div>
      <div class="detail-card"><span>Warning</span><strong>Off</strong></div>
      <div class="detail-card"><span>Action</span><strong>Start focus to enable alerts</strong></div>
    </div>`;
    quickStatus.textContent = 'Off';
    quickDelay.textContent = '1m';
    quickAction.textContent = 'Alert';
    focusEditorManuallyOpened = false;
    setFocusEditorOpen(true);
    return;
  }
  const paused = Boolean(focus.paused);
  chip.textContent = paused ? 'Focus paused' : 'Focus active';
  chip.className = `status-chip ${paused ? 'paused' : 'running'}`;
  const action = focus.alertAction === 'switch' && focus.redirectApp ? `move to ${focus.redirectApp}` : 'show alert';
  const targets = String(focus.target || '').split(/[,\n]/).map(value => value.trim()).filter(Boolean);
  const targetChips = targets.map(value => `<span class="target-chip">${escapeHtml(shortenSource(value))}</span>`).join('') || '<span class="target-chip">No target set</span>';
  details.innerHTML = `
    <div class="target-chips">${targetChips}</div>
    <div class="detail-grid">
      <div class="detail-card"><span>Full focus list</span><strong>${escapeHtml(focus.target || 'No target set')}</strong></div>
      <div class="detail-card"><span>Warning delay</span><strong>${formatDuration(focus.alertDelaySeconds || 60)} outside focus</strong></div>
      <div class="detail-card"><span>Notification action</span><strong>${escapeHtml(action)}</strong></div>
    </div>`;
  quickStatus.textContent = paused ? 'Paused' : 'Active';
  quickDelay.textContent = formatDuration(focus.alertDelaySeconds || 60);
  quickAction.textContent = focus.alertAction === 'switch' && focus.redirectApp ? `Move` : 'Alert';
  if (!focusEditorManuallyOpened) setFocusEditorOpen(false);
}
function seedFocusInputsFromActiveSession(focus) {
  if (!focus) return;
  const targetInput = document.querySelector('#target');
  const minutesInput = document.querySelector('#minutes');
  const alertInput = document.querySelector('#alertMinutes');
  const actionInput = document.querySelector('#alertAction');
  const redirectInput = document.querySelector('#redirectApp');
  if (focus.target && targetInput.value !== focus.target) setFocusTargets(focus.target);
  if (focus.durationMinutes) minutesInput.value = focus.durationMinutes;
  if (focus.alertDelaySeconds) alertInput.value = Math.max(1, Math.round(focus.alertDelaySeconds / 60));
  if (focus.alertAction) actionInput.value = focus.alertAction;
  redirectInput.value = focus.redirectApp || '';
  saveFocusDraft();
}
function updateFocusButtons(focus) {
  const startButton = document.querySelector('#startFocus');
  const pauseButton = document.querySelector('#pauseFocus');
  const stopButton = document.querySelector('#stopFocus');
  const running = Boolean(focus && !focus.paused);
  const paused = Boolean(focus && focus.paused);
  startButton.className = `focus-btn ${running ? 'focus-running' : 'focus-idle'}`;
  startButton.textContent = paused ? 'Restart focus' : running ? 'Focus' : 'Start focus';
  pauseButton.disabled = !focus;
  pauseButton.className = `focus-btn ${paused ? 'focus-paused' : running ? 'focus-running' : ''}`;
  pauseButton.textContent = paused ? 'Resume' : 'Pause';
  stopButton.disabled = !focus;
  stopButton.className = `focus-btn ${focus ? 'focus-stop-active' : ''}`;
}
function sourceMarkup(source, index) {
  const shortSource = shortenSource(source);
  if (shortSource === source) return `<span>${escapeHtml(source)}</span>`;
  return `<button class="source-toggle" data-full="${escapeHtml(source)}" data-short="${escapeHtml(shortSource)}" onclick="toggleSource(event)">${escapeHtml(shortSource)}</button>`;
}
function toggleSource(event) {
  const button = event.currentTarget;
  const showingFull = button.dataset.fullShown === 'true';
  button.textContent = showingFull ? button.dataset.short : button.dataset.full;
  button.dataset.fullShown = showingFull ? 'false' : 'true';
}
function shortenSource(source) {
  if (!/^[a-z][a-z0-9+.-]*:/i.test(source)) return source;
  try {
    const url = new URL(source);
    const parts = url.pathname.split('/').filter(Boolean);
    const path = parts.length ? `/${parts[0]}/` : '/';
    if (url.host) return `${url.protocol}//${url.host}${path}`;
    if (url.protocol === 'chrome:' && url.pathname) return `${url.protocol}//${url.pathname.split('/').filter(Boolean)[0] || ''}/`;
    return `${url.protocol}${path}`;
  } catch {
    const match = source.match(/^([a-z][a-z0-9+.-]*:\/\/[^/?#]+)(?:[/?#]|$)/i);
    return match ? `${match[1]}/` : source;
  }
}
function formatDuration(seconds) {
  if (!seconds) return '0s';
  if (seconds < 60) return `${seconds}s`;
  if (seconds > 3600) {
    const hours = Math.floor(seconds / 3600);
    const mins = Math.round((seconds % 3600) / 60);
    return mins ? `${hours}h ${mins}m` : `${hours}h`;
  }
  const mins = Math.floor(seconds / 60);
  const rest = seconds % 60;
  return rest ? `${mins}m ${rest}s` : `${mins}m`;
}
function reportTotalSeconds(report) {
  return ((report.productiveMinutes || 0) + (report.distractingMinutes || 0) + (report.idleMinutes || 0)) * 60;
}
function escapeHtml(value) {
  return String(value).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#039;'}[c]));
}
function escapeAttr(value) {
  return String(value).replace(/[^a-z0-9_-]/gi, '-');
}
restoreFocusDraft();
renderReportCalendar();
setFocusTaskWindow('day', calendarPeriodWindow('day', selectedReportDate));
refresh();
setInterval(refresh, 10000);
</script>
</body>
</html>"#
        .into()
}

fn data_dir() -> io::Result<PathBuf> {
    if let Ok(value) = env::var("LOCAL_FOCUS_DATA") {
        return Ok(PathBuf::from(value));
    }
    let home = env::var("HOME")
        .or_else(|_| env::var("USERPROFILE"))
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;

    #[cfg(target_os = "windows")]
    {
        Ok(PathBuf::from(home).join("AppData").join("Local").join(APP_NAME))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join(APP_NAME))
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        Ok(PathBuf::from(home).join(".local").join("share").join(APP_NAME))
    }
}

fn ensure_config(data_dir: &PathBuf) -> io::Result<()> {
    let path = data_dir.join("config.txt");
    if path.exists() {
        return Ok(());
    }
    fs::write(
        path,
        "productive=code,terminal,editor,docs,figma,notion,calendar,github,jira,linear\n\
distracting=youtube,netflix,reddit,instagram,tiktok,x.com,twitter,facebook,game,steam\n\
blocked=\n",
    )
}

fn load_config(data_dir: &PathBuf) -> io::Result<Config> {
    let mut config = Config::default();
    let path = data_dir.join("config.txt");
    let content = fs::read_to_string(path)?;
    for line in content.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let values = value
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        match key.trim() {
            "productive" => config.productive_keywords = values,
            "distracting" => config.distracting_keywords = values,
            "blocked" => config.blocked_keywords = values,
            _ => {}
        }
    }
    Ok(config)
}

fn save_config(data_dir: &PathBuf, config: &Config) -> io::Result<()> {
    fs::write(
        data_dir.join("config.txt"),
        format!(
            "productive={}\ndistracting={}\nblocked={}\n",
            config.productive_keywords.join(","),
            config.distracting_keywords.join(","),
            config.blocked_keywords.join(",")
        ),
    )
}

fn save_focus(data_dir: &PathBuf, focus: &FocusSession) -> io::Result<()> {
    let paused_at = focus
        .paused_at
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".into());
    let pomodoro_alerted_at = focus
        .pomodoro_alerted_at
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".into());
    fs::write(
        data_dir.join("focus.json"),
        format!(
            "{{\"task\":\"{}\",\"target\":\"{}\",\"startedAt\":{},\"durationMinutes\":{},\"breakMinutes\":{},\"pausedAt\":{},\"pausedTotalSeconds\":{},\"pomodoroAlertedAt\":{},\"alertDelaySeconds\":{},\"alertAction\":\"{}\",\"redirectApp\":\"{}\"}}",
            json_escape(&focus.task),
            json_escape(&focus.target),
            focus.started_at,
            focus.duration_minutes,
            focus.break_minutes,
            paused_at,
            focus.paused_total_seconds,
            pomodoro_alerted_at,
            focus.alert_delay_seconds,
            json_escape(&focus.alert_action),
            json_escape(&focus.redirect_app)
        ),
    )
}

fn load_focus(data_dir: &PathBuf) -> Option<FocusSession> {
    let value = fs::read_to_string(data_dir.join("focus.json")).ok()?;
    Some(FocusSession {
        task: json_string(&value, "task")?,
        target: json_string(&value, "target").unwrap_or_default(),
        started_at: json_number(&value, "startedAt")?,
        duration_minutes: json_number(&value, "durationMinutes")? as u64,
        break_minutes: json_number(&value, "breakMinutes")? as u64,
        paused_at: json_number(&value, "pausedAt"),
        paused_total_seconds: json_number(&value, "pausedTotalSeconds").unwrap_or(0),
        pomodoro_alerted_at: json_number(&value, "pomodoroAlertedAt"),
        alert_delay_seconds: json_number(&value, "alertDelaySeconds")
            .map(|value| value.max(1) as u64)
            .unwrap_or(DEFAULT_ALERT_DELAY_SECONDS),
        alert_action: json_string(&value, "alertAction").unwrap_or_else(|| "alert".into()),
        redirect_app: json_string(&value, "redirectApp").unwrap_or_default(),
    })
}

fn clear_focus(data_dir: &PathBuf) -> io::Result<()> {
    let path = data_dir.join("focus.json");
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn notify(title: &str, message: &str) {
    #[cfg(target_os = "macos")]
    let _ = Command::new("osascript")
        .arg("-e")
        .arg(format!(
            "display notification \"{}\" with title \"{}\"",
            apple_escape(message),
            apple_escape(title)
        ))
        .output();

    #[cfg(target_os = "windows")]
    {
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; \
             Add-Type -AssemblyName System.Drawing; \
             $n = New-Object System.Windows.Forms.NotifyIcon; \
             $n.Icon = [System.Drawing.SystemIcons]::Information; \
             $n.BalloonTipTitle = '{}'; \
             $n.BalloonTipText = '{}'; \
             $n.Visible = $true; \
             $n.ShowBalloonTip(5000); \
             Start-Sleep -Seconds 6; \
             $n.Dispose()",
            ps_escape(title),
            ps_escape(message)
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output();
    }

    #[cfg(target_os = "linux")]
    let _ = Command::new("notify-send").arg(title).arg(message).output();
}

fn os_alert(title: &str, message: &str) {
    #[cfg(target_os = "macos")]
    {
        let alert_title = format!("FOCUS WARNING - {}", title.to_uppercase());
        let script = format!(
            "display dialog \"{}\" with title \"{}\" buttons {{\"BACK TO FOCUS\"}} default button \"BACK TO FOCUS\" with icon caution giving up after 30",
            apple_escape(message),
            apple_escape(&alert_title)
        );
        spawn_macos_focus_alert(script);
    }

    #[cfg(target_os = "windows")]
    {
        let alert_title = format!("FOCUS WARNING - {}", title.to_uppercase());
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; \
             [System.Windows.Forms.MessageBox]::Show('{}', '{}', 'OK', 'Warning')",
            ps_escape(message),
            ps_escape(&alert_title)
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .spawn();
    }

    #[cfg(target_os = "linux")]
    {
        let alert_title = format!("FOCUS WARNING - {}", title.to_uppercase());
        let script = format!(
            "if command -v zenity >/dev/null 2>&1; then zenity --warning --width=560 --height=180 --title='{}' --text='{}'; else notify-send -u critical -a 'Local Focus' '{}' '{}'; fi",
            shell_escape(&alert_title),
            shell_escape(message),
            shell_escape(&alert_title),
            shell_escape(message)
        );
        let _ = Command::new("sh")
            .arg("-c")
            .arg(script)
            .spawn();
    }
}

fn os_alert_then_activate(title: &str, message: &str, app_name: &str) {
    let title = title.to_string();
    let message = message.to_string();
    let app_name = app_name.trim().to_string();

    #[cfg(target_os = "macos")]
    {
        let alert_title = format!("FOCUS WARNING - {}", title.to_uppercase());
        let script = format!(
            "set targetApp to \"{}\"\n\
             display dialog \"{}\" with title \"{}\" buttons {{\"OK\"}} default button \"OK\" with icon caution\n\
             do shell script \"open -a \" & quoted form of targetApp\n\
             delay 0.4\n\
             try\n\
             \ttell application targetApp to activate\n\
             end try\n\
             try\n\
             \ttell application \"System Events\" to set frontmost of first process whose name is targetApp to true\n\
             end try",
            apple_escape(&app_name),
            apple_escape(&message),
            apple_escape(&alert_title)
        );
        spawn_macos_focus_alert(script);
        return;
    }

    #[cfg(not(target_os = "macos"))]
    thread::spawn(move || {
        let acknowledged = os_alert_blocking(&title, &message);
        if acknowledged {
            let _ = activate_app(&app_name);
        }
    });
}

#[cfg(target_os = "macos")]
fn active_alert_pid() -> &'static Mutex<Option<u32>> {
    static ACTIVE_ALERT_PID: OnceLock<Mutex<Option<u32>>> = OnceLock::new();
    ACTIVE_ALERT_PID.get_or_init(|| Mutex::new(None))
}

#[cfg(target_os = "macos")]
fn close_existing_focus_alert() {
    let pid = active_alert_pid().lock().ok().and_then(|mut active| active.take());
    if let Some(pid) = pid {
        let _ = Command::new("kill").arg("-TERM").arg(pid.to_string()).status();
    }
}

#[cfg(target_os = "macos")]
fn spawn_macos_focus_alert(script: String) {
    close_existing_focus_alert();
    let Ok(mut child) = Command::new("osascript").arg("-e").arg(script).spawn() else {
        return;
    };
    let pid = child.id();
    if let Ok(mut active) = active_alert_pid().lock() {
        *active = Some(pid);
    }

    thread::spawn(move || {
        let _ = child.wait();
        if let Ok(mut active) = active_alert_pid().lock() {
            if matches!(*active, Some(active_pid) if active_pid == pid) {
                *active = None;
            }
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn os_alert_blocking(title: &str, message: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        let alert_title = format!("FOCUS WARNING - {}", title.to_uppercase());
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; \
             [System.Windows.Forms.MessageBox]::Show('{}', '{}', 'OK', 'Warning')",
            ps_escape(message),
            ps_escape(&alert_title)
        );
        return Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .status()
            .is_ok_and(|status| status.success());
    }

    #[cfg(target_os = "linux")]
    {
        let alert_title = format!("FOCUS WARNING - {}", title.to_uppercase());
        let script = format!(
            "if command -v zenity >/dev/null 2>&1; then zenity --warning --width=560 --height=180 --title='{}' --text='{}'; else notify-send -u critical -a 'Local Focus' '{}' '{}'; fi",
            shell_escape(&alert_title),
            shell_escape(message),
            shell_escape(&alert_title),
            shell_escape(message)
        );
        return Command::new("sh")
            .arg("-c")
            .arg(script)
            .status()
            .is_ok_and(|status| status.success());
    }
}

#[cfg(not(target_os = "macos"))]
fn activate_app(app_name: &str) -> io::Result<()> {
    let app_name = app_name.trim();
    if app_name.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "missing app name"));
    }

    #[cfg(target_os = "macos")]
    {
        let open_status = Command::new("open").args(["-a", app_name]).status()?;
        if open_status.success() {
            let frontmost_script = format!(
                "tell application \"System Events\" to set frontmost of first process whose name is \"{}\" to true",
                apple_escape(app_name)
            );
            let _ = Command::new("osascript")
                .arg("-e")
                .arg(frontmost_script)
                .status();
            return Ok(());
        }

        let script = format!("tell application \"{}\" to activate", apple_escape(app_name));
        let status = Command::new("osascript").arg("-e").arg(script).status()?;
        if status.success() {
            return Ok(());
        }
    }

    #[cfg(target_os = "windows")]
    {
        let script = format!(
            "$name = '{}'; \
             $shell = New-Object -ComObject WScript.Shell; \
             if (-not $shell.AppActivate($name)) {{ exit 1 }}",
            ps_escape(app_name)
        );
        let status = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .status()?;
        if status.success() {
            return Ok(());
        }
    }

    #[cfg(target_os = "linux")]
    {
        let status = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "if command -v wmctrl >/dev/null 2>&1; then wmctrl -a '{}'; else exit 1; fi",
                shell_escape(app_name)
            ))
            .status()?;
        if status.success() {
            return Ok(());
        }
    }

    Err(io::Error::new(
        io::ErrorKind::Other,
        "could not activate app",
    ))
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            map.insert(percent_decode(key), percent_decode(value));
        }
    }
    map
}

fn percent_decode(value: &str) -> String {
    let mut result = String::new();
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        match byte {
            b'+' => result.push(' '),
            b'%' => {
                let hi = chars.next().unwrap_or(b'0');
                let lo = chars.next().unwrap_or(b'0');
                if let Ok(hex) = u8::from_str_radix(&format!("{}{}", hi as char, lo as char), 16) {
                    result.push(hex as char);
                }
            }
            _ => result.push(byte as char),
        }
    }
    result
}

fn json_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| match c {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect(),
            '\n' => "\\n".chars().collect(),
            '\r' => "\\r".chars().collect(),
            '\t' => "\\t".chars().collect(),
            _ => vec![c],
        })
        .collect()
}

fn json_string(value: &str, key: &str) -> Option<String> {
    let marker = format!("\"{key}\":\"");
    let start = value.find(&marker)? + marker.len();
    let mut result = String::new();
    let mut escaped = false;
    for c in value[start..].chars() {
        if escaped {
            result.push(match c {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return Some(result);
        } else {
            result.push(c);
        }
    }
    None
}

fn json_number(value: &str, key: &str) -> Option<i64> {
    let marker = format!("\"{key}\":");
    let start = value.find(&marker)? + marker.len();
    let number = value[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect::<String>();
    number.parse().ok()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn clean(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "Unknown".into()
    } else {
        trimmed.into()
    }
}

#[cfg(target_os = "macos")]
fn apple_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "windows")]
fn ps_escape(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(target_os = "linux")]
fn shell_escape(value: &str) -> String {
    value.replace('\'', "'\"'\"'")
}

fn print_help() {
    println!(
        "Local Focus\n\nCommands:\n  local-focus serve                 Run tracker and private web UI\n  local-focus track                 Run tracker without UI\n  local-focus focus TASK MINUTES [TARGET]\n  local-focus report                Print JSON productivity report\n  local-focus data-dir              Show local data directory"
    );
}
