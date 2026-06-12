use qrcode::render::svg;
use qrcode::QrCode;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const APP_NAME: &str = "local-focus";
const SAMPLE_SECONDS: u64 = 5;
const DISTRACTION_SECONDS: i64 = 90;
const BLOCK_COOLDOWN_SECONDS: i64 = 10;
const DEVICE_NOTIFY_COOLDOWN_SECONDS: i64 = 60;
const DEFAULT_ALERT_DELAY_SECONDS: u64 = 60;
const DEFAULT_ALERT_MESSAGE_TEMPLATE: &str = "You have been outside your focus apps/sites for over {delay}. Allowed: '{targets}'. Current activity: {app}";
const IDLE_SECONDS: u64 = 60;
const MAX_FOCUS_TARGETS: usize = 15;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const SOCKET_TIMEOUT_SECONDS: u64 = 15;
const SAMPLE_RETENTION_SECONDS: i64 = 30 * 24 * 60 * 60;

#[derive(Clone, Debug)]
struct Config {
    productive_keywords: Vec<String>,
    distracting_keywords: Vec<String>,
    blocked_keywords: Vec<String>,
    network_devices: Vec<String>,
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
    alert_message: String,
    redirect_app: String,
    high_focus_mode: bool,
}

#[derive(Default)]
struct AppState {
    config: Config,
    focus: Option<FocusSession>,
    last_distraction_at: i64,
    last_focus_mismatch_at: i64,
    focus_mismatch_started_at: Option<i64>,
    last_blocked_at: i64,
    last_blocked_key: String,
    last_device_notify_at: i64,
    last_device_notify_key: String,
    // Master switch. When true, the whole app is stopped: no tracking, blocking,
    // alerts, device notifications, or journal reminders until it is resumed.
    stopped: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockRuleKind {
    App,
    Website,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockMode {
    Full,
    Password,
}

#[derive(Clone, Debug)]
struct BlockRule {
    target: String,
    mode: BlockMode,
    password: String,
}

#[derive(Clone, Debug)]
struct NetworkDevice {
    name: String,
    kind: String,
    endpoint: String,
    selected: bool,
}

#[derive(Clone, Debug)]
struct JournalSettings {
    enabled: bool,
    reminder_mode: String,
}

#[derive(Clone, Debug)]
struct JournalReminderDue {
    date: String,
    label: String,
    message: String,
    marker_key: String,
}

#[derive(Clone, Debug)]
struct JournalTaskReminder {
    id: String,
    task: String,
    time: String,
}

#[derive(Clone, Debug)]
struct LocalClock {
    today: String,
    yesterday: String,
    hour: u32,
    minute: u32,
}

impl Default for JournalSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reminder_mode: "evening".into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UrlMatchParts {
    host: String,
    path: String,
    port: Option<String>,
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
            network_devices: Vec::new(),
        }
    }
}

/// Lock the shared state, recovering the inner value if a previous holder
/// panicked. This avoids silently swapping in empty/default state (which would
/// disable tracking or config) just because some unrelated thread paniced.
fn lock_state(state: &Mutex<AppState>) -> std::sync::MutexGuard<'_, AppState> {
    state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let data_dir = data_dir()?;
    fs::create_dir_all(&data_dir)?;
    ensure_config(&data_dir)?;

    match args.get(1).map(String::as_str) {
        Some("track") => run_tracker(data_dir),
        Some("focus") => {
            let task = args
                .get(2)
                .cloned()
                .unwrap_or_else(|| "Focus session".into());
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
        last_blocked_at: 0,
        last_blocked_key: String::new(),
        last_device_notify_at: 0,
        last_device_notify_key: String::new(),
        stopped: false,
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

    {
        let journal_dir = data_dir.clone();
        let journal_state = Arc::clone(&state);
        thread::spawn(move || {
            if let Err(error) = journal_reminder_loop(journal_dir, journal_state) {
                eprintln!("journal reminder stopped: {error}");
            }
        });
    }

    let listener = TcpListener::bind("0.0.0.0:4799")?;
    println!("Local Focus is running at http://127.0.0.1:4799");
    if let Some(url) = local_network_url() {
        println!("Device QR receiver URL: {url}/device");
    }
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
        last_blocked_at: 0,
        last_blocked_key: String::new(),
        last_device_notify_at: 0,
        last_device_notify_key: String::new(),
        stopped: false,
    }));
    tracking_loop(data_dir, state)
}

fn tracking_loop(data_dir: PathBuf, state: Arc<Mutex<AppState>>) -> io::Result<()> {
    loop {
        // Master switch: when stopped, do nothing — no sampling, no blocking,
        // no alerts — until the app is resumed.
        let (config, focus) = {
            let guard = lock_state(&state);
            if guard.stopped {
                drop(guard);
                thread::sleep(Duration::from_secs(SAMPLE_SECONDS));
                continue;
            }
            (guard.config.clone(), guard.focus.clone())
        };
        let raw = foreground_activity();
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

        enforce_blocked_access(&data_dir, &state, &config, &sample)?;
        notify_devices_for_attention_event(&data_dir, &state, &config, &sample)?;
        append_sample(&data_dir, &sample)?;
        detect_distraction(&data_dir, &state, &sample)?;
        thread::sleep(Duration::from_secs(SAMPLE_SECONDS));
    }
}

fn focus_loop(data_dir: PathBuf, state: Arc<Mutex<AppState>>) -> io::Result<()> {
    loop {
        thread::sleep(Duration::from_secs(10));
        let focus = {
            let guard = lock_state(&state);
            if guard.stopped {
                continue;
            }
            guard.focus.clone()
        };
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
                lock_state(&state).focus = Some(completed);
            }
        }
    }
}

fn daily_report_loop(data_dir: PathBuf, state: Arc<Mutex<AppState>>) -> io::Result<()> {
    loop {
        if !lock_state(&state).stopped {
            maybe_log_previous_day_report(&data_dir, &state)?;
        }
        prune_old_records(&data_dir)?;
        thread::sleep(Duration::from_secs(5 * 60));
    }
}

/// Keep the high-frequency activity and notification logs bounded so the files
/// (and the per-request parse cost) do not grow without limit. Daily report
/// archives retain long-term history beyond the retention window.
fn prune_old_records(data_dir: &Path) -> io::Result<()> {
    let cutoff = now() - SAMPLE_RETENTION_SECONDS;
    prune_jsonl_by_timestamp(&data_dir.join("activity.jsonl"), cutoff)?;
    prune_jsonl_by_timestamp(&data_dir.join("device_notifications.jsonl"), cutoff)?;
    Ok(())
}

fn prune_jsonl_by_timestamp(path: &Path, cutoff: i64) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let reader = BufReader::new(File::open(path)?);
    let mut kept = Vec::new();
    let mut dropped = false;
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        match json_number(&line, "timestamp") {
            Some(timestamp) if timestamp < cutoff => dropped = true,
            _ => kept.push(line),
        }
    }
    if !dropped {
        return Ok(());
    }
    let mut content = kept.join("\n");
    content.push('\n');
    let tmp = path.with_extension("jsonl.tmp");
    fs::write(&tmp, content)?;
    fs::rename(tmp, path)
}

fn journal_reminder_loop(data_dir: PathBuf, state: Arc<Mutex<AppState>>) -> io::Result<()> {
    loop {
        if !lock_state(&state).stopped {
            maybe_send_journal_reminder(&data_dir)?;
            maybe_send_journal_task_reminders(&data_dir)?;
        }
        thread::sleep(Duration::from_secs(30));
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
        alert_message: DEFAULT_ALERT_MESSAGE_TEMPLATE.into(),
        redirect_app: String::new(),
        high_focus_mode: false,
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
        &format!(
            "{} minutes: {}{}",
            duration_minutes, session.task, target_note
        ),
    );
    println!("Started focus session: {}", session.task);
    Ok(())
}

fn detect_distraction(
    data_dir: &Path,
    state: &Arc<Mutex<AppState>>,
    sample: &ActivitySample,
) -> io::Result<()> {
    let mut guard = lock_state(state);

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
            let message = focus_alert_message(focus, sample);
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
        let message = format!(
            "You are in focus mode for {task}. Current activity: {}",
            sample.app
        );
        os_alert("Distraction warning", &message);
        guard.last_distraction_at = sample.timestamp;
        append_event(data_dir, "distraction_alert", &message)?;
    }

    Ok(())
}

fn focus_alert_message(focus: &FocusSession, sample: &ActivitySample) -> String {
    let template = clean_alert_message_template(&focus.alert_message);
    template
        .replace("{delay}", &human_duration(focus.alert_delay_seconds.max(1)))
        .replace("{targets}", &focus.target)
        .replace("{app}", &sample.app)
        .replace("{title}", &sample.title)
        .replace("{url}", &sample.source)
}

fn clean_alert_message_template(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        DEFAULT_ALERT_MESSAGE_TEMPLATE.into()
    } else {
        trimmed.into()
    }
}

fn enforce_blocked_access(
    data_dir: &Path,
    state: &Arc<Mutex<AppState>>,
    config: &Config,
    sample: &ActivitySample,
) -> io::Result<()> {
    if activity_is_block_exempt(state, sample) {
        return Ok(());
    }

    if enforce_high_focus_block(data_dir, state, sample)? {
        return Ok(());
    }

    let Some((rule, rule_kind)) = blocked_keyword_match(config, sample) else {
        return Ok(());
    };
    let blocked_key = format!(
        "{}|{}|{}",
        normalize_match_text(&rule.target),
        normalize_match_text(&sample.app),
        normalize_match_text(&sample.source)
    );

    {
        let mut guard = lock_state(state);
        let within_cooldown = sample.timestamp - guard.last_blocked_at < BLOCK_COOLDOWN_SECONDS;
        if within_cooldown && guard.last_blocked_key == blocked_key {
            return Ok(());
        }
        guard.last_blocked_at = sample.timestamp;
        guard.last_blocked_key = blocked_key;
    }

    let message = format!(
        "Blocked access to '{}' because it matches your distraction rule '{}'.",
        blocked_activity_label(sample),
        rule.target
    );
    notify("Blocked by Local Focus", &message);
    match rule.mode {
        BlockMode::Full => block_activity_access(sample, &rule.target, rule_kind),
        BlockMode::Password => password_block_activity_access(sample, &rule, &message),
    }
    append_event(data_dir, "blocked_access", &message)
}

fn activity_is_block_exempt(state: &Arc<Mutex<AppState>>, sample: &ActivitySample) -> bool {
    if is_local_focus_control_activity(sample) || is_system_connection_activity(sample) {
        return true;
    }

    lock_state(state)
        .focus
        .clone()
        .filter(|focus| focus.paused_at.is_none())
        .filter(|focus| !focus_targets(focus).is_empty())
        .is_some_and(|focus| matches_focus_target(&focus, sample))
}

fn enforce_high_focus_block(
    data_dir: &Path,
    state: &Arc<Mutex<AppState>>,
    sample: &ActivitySample,
) -> io::Result<bool> {
    let focus = lock_state(state).focus.clone();
    let Some(focus) = focus else {
        return Ok(false);
    };
    if !high_focus_should_block(&focus, sample) {
        return Ok(false);
    }

    let rule_kind = high_focus_block_rule_kind(sample);
    let block_key = format!(
        "high-focus|{}|{}|{}",
        normalize_match_text(&sample.app),
        normalize_match_text(&sample.source),
        normalize_match_text(&sample.title)
    );
    {
        let mut guard = lock_state(state);
        let within_cooldown = sample.timestamp - guard.last_blocked_at < BLOCK_COOLDOWN_SECONDS;
        if within_cooldown && guard.last_blocked_key == block_key {
            return Ok(true);
        }
        guard.last_blocked_at = sample.timestamp;
        guard.last_blocked_key = block_key;
    }

    let message = format!(
        "High Focus blocked '{}' because it is outside your focus apps/sites '{}'.",
        blocked_activity_label(sample),
        focus.target
    );
    notify("High Focus block", &message);
    block_high_focus_activity_access(sample, rule_kind);
    append_event(data_dir, "high_focus_blocked_access", &message)?;
    Ok(true)
}

fn high_focus_should_block(focus: &FocusSession, sample: &ActivitySample) -> bool {
    focus.high_focus_mode
        && focus.paused_at.is_none()
        && !focus_targets(focus).is_empty()
        && !matches_focus_target(focus, sample)
        && !is_local_focus_control_activity(sample)
        && !is_system_connection_activity(sample)
}

fn high_focus_block_rule_kind(sample: &ActivitySample) -> BlockRuleKind {
    if is_browser_app(&sample.app)
        || (sample.source != "local" && website_rule_domain(&sample.source).is_some())
    {
        BlockRuleKind::Website
    } else {
        BlockRuleKind::App
    }
}

fn is_local_focus_control_activity(sample: &ActivitySample) -> bool {
    let haystack = normalize_match_text(&format!(
        "{} {} {}",
        sample.app, sample.title, sample.source
    ));
    haystack.contains("local-focus")
        || haystack.contains("local focus")
        || haystack.contains("127.0.0.1:4799")
        || haystack.contains("localhost:4799")
        || sample_url_parts(sample)
            .iter()
            .any(|part| part.port.as_deref() == Some("4799"))
        || local_network_url()
            .map(|url| haystack.contains(&normalize_match_text(&url)))
            .unwrap_or(false)
}

fn is_system_connection_activity(sample: &ActivitySample) -> bool {
    let haystack = normalize_match_text(&format!(
        "{} {} {}",
        sample.app, sample.title, sample.source
    ))
    .replace('-', " ");
    haystack.contains("wi fi")
        || haystack.contains("wifi")
        || haystack.contains("network settings")
        || haystack.contains("network connection")
}

fn blocked_keyword_match(
    config: &Config,
    sample: &ActivitySample,
) -> Option<(BlockRule, BlockRuleKind)> {
    config
        .blocked_keywords
        .iter()
        .map(|record| parse_block_rule_record(record))
        .find_map(|rule| blocked_rule_match(sample, &rule.target).map(|kind| (rule, kind)))
}

fn blocked_rule_match(sample: &ActivitySample, keyword: &str) -> Option<BlockRuleKind> {
    if website_rule_matches(sample, keyword) {
        return Some(BlockRuleKind::Website);
    }
    if app_rule_matches(sample, keyword) {
        return Some(BlockRuleKind::App);
    }
    None
}

fn website_rule_matches(sample: &ActivitySample, keyword: &str) -> bool {
    let Some(rule_domain) = website_rule_domain(keyword) else {
        return false;
    };
    let source = sample.source.trim();
    if let Some(sample_domain) = website_rule_domain(source) {
        return sample_domain == rule_domain || sample_domain.ends_with(&format!(".{rule_domain}"));
    }
    let haystack = normalize_match_text(&format!("{} {}", sample.title, sample.source));
    haystack.contains(&rule_domain)
}

fn app_rule_matches(sample: &ActivitySample, keyword: &str) -> bool {
    if website_rule_domain(keyword).is_some() {
        return false;
    }
    let normalized = normalize_match_text(keyword);
    !normalized.is_empty()
        && normalize_match_text(&format!("{} {}", sample.app, sample.title)).contains(&normalized)
}

fn website_rule_domain(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(domain) = domain_from_url(trimmed) {
        return Some(domain);
    }
    let host = trimmed
        .trim_end_matches('/')
        .trim_start_matches("www.")
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches(':')
        .to_lowercase();
    if host.contains('.') && !host.contains(' ') {
        Some(host)
    } else {
        None
    }
}

fn blocked_activity_label(sample: &ActivitySample) -> String {
    if sample.source != "local" && !sample.source.trim().is_empty() {
        return sample.source.clone();
    }
    if !sample.title.trim().is_empty() {
        return format!("{} - {}", sample.app, sample.title);
    }
    sample.app.clone()
}

fn notify_devices_for_attention_event(
    data_dir: &Path,
    state: &Arc<Mutex<AppState>>,
    config: &Config,
    sample: &ActivitySample,
) -> io::Result<()> {
    if !matches!(sample.category.as_str(), "idle" | "distracting") {
        return Ok(());
    }

    let (devices, event_key, message) = if sample.category == "idle" {
        let focus = lock_state(state).focus.clone();
        let Some(focus) = focus.filter(|focus| focus.paused_at.is_none()) else {
            return Ok(());
        };
        let mobile_reported_idle = sample.source.starts_with("mobile:");
        let idle_seconds = if mobile_reported_idle {
            focus.alert_delay_seconds.max(1)
        } else {
            system_idle_seconds().unwrap_or(0)
        };
        let warn_seconds = focus.alert_delay_seconds.max(1);
        if idle_seconds < warn_seconds {
            return Ok(());
        }
        let devices = idle_warning_devices(&config.network_devices);
        if devices.is_empty() {
            return Ok(());
        }
        (
            devices,
            format!("idle_after_warn|{}", idle_seconds / warn_seconds),
            format!(
                "Idle warning: {} has been idle for {} during '{}'.",
                if mobile_reported_idle {
                    blocked_activity_label(sample)
                } else {
                    "this laptop".into()
                },
                human_duration(idle_seconds),
                focus.task
            ),
        )
    } else {
        let devices = selected_network_devices(&config.network_devices);
        if devices.is_empty() {
            return Ok(());
        }
        (
            devices,
            format!(
                "{}|{}|{}",
                sample.category,
                normalize_match_text(&sample.app),
                normalize_match_text(&sample.source)
            ),
            format!(
                "Distracted activity detected on this machine: {} - {}",
                sample.app,
                blocked_activity_label(sample)
            ),
        )
    };

    {
        let mut guard = lock_state(state);
        let within_cooldown =
            sample.timestamp - guard.last_device_notify_at < DEVICE_NOTIFY_COOLDOWN_SECONDS;
        if within_cooldown && guard.last_device_notify_key == event_key {
            return Ok(());
        }
        guard.last_device_notify_at = sample.timestamp;
        guard.last_device_notify_key = event_key;
    }

    send_device_notifications(&devices, &sample.category, &message, sample);
    append_device_notification(data_dir, &sample.category, &message, sample, &devices)?;
    append_event(data_dir, "device_notification", &message)
}

fn matches_focus_target(focus: &FocusSession, sample: &ActivitySample) -> bool {
    let targets = focus_targets(focus);
    if targets.is_empty() {
        return true;
    }

    targets
        .iter()
        .any(|target| sample_matches_target_text(sample, target))
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

fn normalize_focus_target_text(value: &str) -> String {
    let mut targets = Vec::new();
    for target in value
        .split([',', '\n'])
        .map(str::trim)
        .filter(|target| !target.is_empty())
    {
        if targets.len() >= MAX_FOCUS_TARGETS {
            break;
        }
        if !targets
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(target))
        {
            targets.push(target.to_string());
        }
    }
    targets.join(", ")
}

fn human_duration(seconds: u64) -> String {
    if seconds == 60 {
        "1 minute".into()
    } else if seconds.is_multiple_of(60) {
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
    url_match_parts_from_text(value).map(|parts| parts.host)
}

fn sample_url_parts(sample: &ActivitySample) -> Vec<UrlMatchParts> {
    let mut parts = Vec::new();
    push_url_parts_from_text(&mut parts, &sample.source);
    push_url_parts_from_text(&mut parts, &sample.title);
    push_url_parts_from_text(&mut parts, &sample.app);
    parts
}

fn push_url_parts_from_text(parts: &mut Vec<UrlMatchParts>, value: &str) {
    if let Some(part) = url_match_parts_from_text(value) {
        push_unique_url_part(parts, part);
    }

    for token in value.split_whitespace() {
        if let Some(part) = url_match_parts_from_text(token) {
            push_unique_url_part(parts, part);
        }
    }
}

fn push_unique_url_part(parts: &mut Vec<UrlMatchParts>, part: UrlMatchParts) {
    if !parts.iter().any(|existing| existing == &part) {
        parts.push(part);
    }
}

fn url_match_parts_from_text(value: &str) -> Option<UrlMatchParts> {
    let trimmed = trim_url_candidate(value);
    if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    let without_scheme = if lower.starts_with("https://") {
        &trimmed[8..]
    } else if lower.starts_with("http://") {
        &trimmed[7..]
    } else {
        trimmed
    };
    let without_query = without_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let (host_port, raw_path) = without_query.split_once('/').unwrap_or((without_query, ""));
    let (host, port) = split_host_port(host_port);
    if !looks_like_host(&host) {
        return None;
    }

    let path = if raw_path.is_empty() {
        "/".into()
    } else {
        format!("/{}", raw_path.trim_matches('/')).to_ascii_lowercase()
    };

    Some(UrlMatchParts { host, path, port })
}

fn trim_url_candidate(value: &str) -> &str {
    value.trim().trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';'
        )
    })
}

fn split_host_port(value: &str) -> (String, Option<String>) {
    let host_port = value.trim().trim_start_matches("www.");
    if let Some((host, port)) = host_port.rsplit_once(':') {
        if !host.contains(':') && port.chars().all(|c| c.is_ascii_digit()) {
            return (host.to_ascii_lowercase(), Some(port.to_string()));
        }
    }
    (host_port.to_ascii_lowercase(), None)
}

fn looks_like_host(host: &str) -> bool {
    host == "localhost" || host.parse::<IpAddr>().is_ok() || host.contains('.')
}

fn url_parts_match(target: &UrlMatchParts, sample: &UrlMatchParts) -> bool {
    let host_matches =
        sample.host == target.host || sample.host.ends_with(&format!(".{}", target.host));
    if !host_matches {
        return false;
    }

    let target_path = target.path.trim_end_matches('/');
    if target_path.is_empty() {
        return true;
    }
    let sample_path = sample.path.trim_end_matches('/');
    sample_path == target_path || sample_path.starts_with(&format!("{target_path}/"))
}

fn foreground_activity() -> (String, String, String) {
    platform_foreground_activity()
        .unwrap_or_else(|| ("Unknown".into(), "Unknown activity".into(), "local".into()))
}

fn local_network_url() -> Option<String> {
    local_network_ip().map(|ip| format!("http://{ip}:4799"))
}

fn local_network_ip() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_loopback() {
        None
    } else {
        Some(ip.to_string())
    }
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

    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()?;
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

    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()?;
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
    let window_id = String::from_utf8_lossy(&window_id.stdout)
        .trim()
        .to_string();
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
    if config
        .blocked_keywords
        .iter()
        .any(|k| haystack.contains(&normalize_match_text(&parse_block_rule_record(k).target)))
    {
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

fn append_sample(data_dir: &Path, sample: &ActivitySample) -> io::Result<()> {
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

fn append_event(data_dir: &Path, kind: &str, message: &str) -> io::Result<()> {
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

fn append_device_notification(
    data_dir: &Path,
    event: &str,
    message: &str,
    sample: &ActivitySample,
    devices: &[NetworkDevice],
) -> io::Result<()> {
    let timestamp = now();
    let device_targets = devices
        .iter()
        .map(|device| device.endpoint.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("device_notifications.jsonl"))?;
    writeln!(
        file,
        "{{\"timestamp\":{},\"event\":\"{}\",\"message\":\"{}\",\"app\":\"{}\",\"title\":\"{}\",\"source\":\"{}\",\"category\":\"{}\",\"devices\":\"{}\"}}",
        timestamp,
        json_escape(event),
        json_escape(message),
        json_escape(&sample.app),
        json_escape(&sample.title),
        json_escape(&sample.source),
        json_escape(&sample.category),
        json_escape(&device_targets)
    )
}

fn append_focus_session(data_dir: &Path, focus: &FocusSession) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("focus_sessions.jsonl"))?;
    writeln!(
        file,
        "{{\"task\":\"{}\",\"target\":\"{}\",\"startedAt\":{},\"durationMinutes\":{},\"alertDelaySeconds\":{},\"alertAction\":\"{}\",\"alertMessage\":\"{}\",\"redirectApp\":\"{}\",\"highFocusMode\":{}}}",
        json_escape(&focus.task),
        json_escape(&focus.target),
        focus.started_at,
        focus.duration_minutes,
        focus.alert_delay_seconds,
        json_escape(&focus.alert_action),
        json_escape(&clean_alert_message_template(&focus.alert_message)),
        json_escape(&focus.redirect_app),
        focus.high_focus_mode
    )
}

fn focus_sessions_json(
    data_dir: &Path,
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
        if since.is_none_or(|value| focus.started_at >= value)
            && until.is_none_or(|value| focus.started_at < value)
            && !rows
                .iter()
                .any(|line| json_number(line, "startedAt") == Some(focus.started_at))
        {
            rows.push(format!(
                "{{\"task\":\"{}\",\"target\":\"{}\",\"startedAt\":{},\"durationMinutes\":{},\"alertDelaySeconds\":{},\"alertAction\":\"{}\",\"alertMessage\":\"{}\",\"redirectApp\":\"{}\",\"highFocusMode\":{}}}",
                json_escape(&focus.task),
                json_escape(&focus.target),
                focus.started_at,
                focus.duration_minutes,
                focus.alert_delay_seconds,
                json_escape(&focus.alert_action),
                json_escape(&clean_alert_message_template(&focus.alert_message)),
                json_escape(&focus.redirect_app),
                focus.high_focus_mode
            ));
        }
    }

    rows.sort_by_key(|line| json_number(line, "startedAt").unwrap_or(0));
    Ok(format!("[{}]", rows.join(",")))
}

fn load_samples(data_dir: &Path) -> io::Result<Vec<ActivitySample>> {
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

fn read_http_request(stream: &mut TcpStream) -> io::Result<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0; 8192];

    // Keep reading until the full header block has arrived. A single read() is
    // not guaranteed to deliver the whole header, and the cap prevents a slow or
    // malicious client from making us buffer without bound.
    let header_end = loop {
        if let Some(header_end) = find_header_end(&buffer) {
            break header_end;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers too large",
            ));
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            // Connection closed before the headers completed.
            return Ok(String::from_utf8_lossy(&buffer).into_owned());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request body too large",
        ));
    }

    let target_len = header_end + 4 + content_length;
    while buffer.len() < target_len {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn handle_http(
    mut stream: TcpStream,
    data_dir: PathBuf,
    state: Arc<Mutex<AppState>>,
) -> io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(SOCKET_TIMEOUT_SECONDS)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(SOCKET_TIMEOUT_SECONDS)));
    let is_loopback = stream
        .peer_addr()
        .map(|addr| addr.ip().is_loopback())
        .unwrap_or(false);

    let request = read_http_request(&mut stream)?;
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let route = path.split(['?', '#']).next().unwrap_or(path);

    // The private dashboard and all control/data endpoints are localhost-only.
    // Only the device-companion surface is reachable from other LAN machines.
    if !is_loopback && !remote_path_allowed(route) {
        return write_forbidden(&mut stream, "This endpoint is only available on this device.");
    }

    // Block cross-site (CSRF) calls to state-changing endpoints. Browsers tag
    // cross-origin requests with Sec-Fetch-Site / Origin; native companions send
    // neither and are therefore unaffected.
    if is_mutation_path(route) && request_is_cross_site(&request) {
        return write_forbidden(&mut stream, "Cross-site requests are not allowed.");
    }

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
            .map(|s| normalize_focus_target_text(s))
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
        let alert_message = params
            .get("alertMessage")
            .map(|message| clean_alert_message_template(message))
            .unwrap_or_else(|| DEFAULT_ALERT_MESSAGE_TEMPLATE.into());
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
            alert_message,
            redirect_app,
            high_focus_mode: false,
        };
        save_focus(&data_dir, &session)?;
        append_focus_session(&data_dir, &session)?;
        {
            let mut guard = lock_state(&state);
            guard.focus = Some(session.clone());
            // Starting a focus session also resumes the app if it was stopped.
            guard.stopped = false;
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
            let mut guard = lock_state(&state);
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
    } else if path.starts_with("/api/focus/targets") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let target = params
            .get("target")
            .map(|s| normalize_focus_target_text(s))
            .unwrap_or_default();
        let updated = {
            let mut guard = lock_state(&state);
            if let Some(mut focus) = guard.focus.clone() {
                focus.target = target.clone();
                guard.focus = Some(focus.clone());
                Some(focus)
            } else {
                None
            }
        };
        if let Some(focus) = updated {
            save_focus(&data_dir, &focus)?;
            append_event(&data_dir, "focus_targets_updated", &target)?;
        }
        write_response(&mut stream, "application/json", "{\"ok\":true}")?;
    } else if path.starts_with("/api/focus/high-focus") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let enabled = params
            .get("enabled")
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "on"));
        let updated = {
            let mut guard = lock_state(&state);
            if let Some(mut focus) = guard.focus.clone() {
                focus.high_focus_mode = enabled;
                guard.focus = Some(focus.clone());
                Some(focus)
            } else {
                None
            }
        };
        if let Some(focus) = updated {
            save_focus(&data_dir, &focus)?;
            notify(
                "High Focus mode",
                if enabled {
                    "Outside-focus apps and websites will be fully blocked."
                } else {
                    "Outside-focus apps and websites will only be tracked and warned."
                },
            );
        }
        write_response(&mut stream, "application/json", "{\"ok\":true}")?;
    } else if path.starts_with("/api/focus/stop") {
        // Stop is the master off switch: end the focus session and halt all
        // tracking, blocking, alerts, device notifications, and reminders until
        // the app is resumed (Resume button, a new focus session, or relaunch).
        clear_focus(&data_dir)?;
        {
            let mut guard = lock_state(&state);
            guard.focus = None;
            guard.stopped = true;
        }
        notify(
            "Local Focus stopped",
            "Tracking, blocking, alerts, and reminders are paused until you resume.",
        );
        write_response(&mut stream, "application/json", "{\"ok\":true,\"stopped\":true}")?;
    } else if path.starts_with("/api/app/resume") {
        {
            let mut guard = lock_state(&state);
            guard.stopped = false;
        }
        notify(
            "Local Focus resumed",
            "Tracking, blocking, alerts, and reminders are active again.",
        );
        write_response(&mut stream, "application/json", "{\"ok\":true,\"stopped\":false}")?;
    } else if path.starts_with("/api/block/add") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let keyword = params
            .get("keyword")
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        let mode = params
            .get("mode")
            .map(|value| parse_block_mode(value))
            .unwrap_or(BlockMode::Full);
        let password = params
            .get("password")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let original = params
            .get("original")
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_default();

        if !keyword.is_empty() {
            let record = format_block_rule_record(&keyword, mode, &password);
            let mut config = load_config(&data_dir).unwrap_or_default();
            config.blocked_keywords.retain(|item| {
                let target = parse_block_rule_record(item).target;
                target != keyword && (original.is_empty() || target != original)
            });
            config.blocked_keywords.push(record.clone());
            save_config(&data_dir, &config)?;
            lock_state(&state).config = config;
            append_event(&data_dir, "blocked_keyword_added", &keyword)?;
        }

        write_response(&mut stream, "application/json", "{\"ok\":true}")?;
    } else if path.starts_with("/api/block/remove") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let keyword = params
            .get("keyword")
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        if !keyword.is_empty() {
            let mut config = load_config(&data_dir).unwrap_or_default();
            config
                .blocked_keywords
                .retain(|item| parse_block_rule_record(item).target != keyword);
            save_config(&data_dir, &config)?;
            lock_state(&state).config = config;
            append_event(&data_dir, "blocked_keyword_removed", &keyword)?;
        }
        write_response(&mut stream, "application/json", "{\"ok\":true}")?;
    } else if path.starts_with("/api/device/register") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let name = params
            .get("name")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Device".into());
        let kind = params
            .get("kind")
            .map(|s| normalize_device_kind(s))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "device".into());
        let endpoint = format!("browser:{}", now());
        let device = format_device_record_selected(&name, &kind, &endpoint, true);
        let mut config = load_config(&data_dir).unwrap_or_default();
        config.network_devices.push(device.clone());
        save_config(&data_dir, &config)?;
        lock_state(&state).config = config;
        append_event(&data_dir, "browser_device_connected", &device)?;
        write_response(
            &mut stream,
            "application/json",
            &format!(
                "{{\"ok\":true,\"device\":\"{}\",\"endpoint\":\"{}\",\"since\":{}}}",
                json_escape(&device),
                json_escape(&endpoint),
                now()
            ),
        )?;
    } else if path.starts_with("/api/mobile/register") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let body = request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("");
        let name = request_value(&params, body, "name")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Phone".into());
        let kind = request_value(&params, body, "kind")
            .map(|s| normalize_device_kind(&s))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "phone".into());
        let endpoint = request_value(&params, body, "endpoint")
            .map(|s| normalize_device_endpoint(&s))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("mobile:{}", now()));
        let device = format_device_record_selected(&name, &kind, &endpoint, true);
        let mut config = load_config(&data_dir).unwrap_or_default();
        config
            .network_devices
            .retain(|item| parse_network_device_record(item).endpoint != endpoint);
        config.network_devices.push(device.clone());
        save_config(&data_dir, &config)?;
        lock_state(&state).config = config;
        append_event(&data_dir, "mobile_device_registered", &device)?;
        write_response(
            &mut stream,
            "application/json",
            &format!(
                "{{\"ok\":true,\"device\":\"{}\",\"endpoint\":\"{}\",\"eventsUrl\":\"/api/device/events?device={}\"}}",
                json_escape(&device),
                json_escape(&endpoint),
                json_escape(&url_encode(&endpoint))
            ),
        )?;
    } else if path.starts_with("/api/mobile/activity") {
        if lock_state(&state).stopped {
            write_response(
                &mut stream,
                "application/json",
                "{\"ok\":false,\"stopped\":true}",
            )?;
            return Ok(());
        }
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let body = request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("");
        let device = request_value(&params, body, "device").unwrap_or_else(|| "Phone".into());
        let app =
            request_value(&params, body, "app").unwrap_or_else(|| "Unknown mobile app".into());
        let title = request_value(&params, body, "title").unwrap_or_default();
        let source = request_value(&params, body, "source")
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| format!("mobile:{}", device));
        let timestamp = request_value(&params, body, "timestamp")
            .and_then(|value| value.parse::<i64>().ok())
            .or_else(|| json_number(body, "timestamp"))
            .unwrap_or_else(now);
        let (config, focus) = {
            let guard = lock_state(&state);
            (guard.config.clone(), guard.focus.clone())
        };
        let category = request_value(&params, body, "category")
            .filter(|value| matches!(value.as_str(), "productive" | "distracting" | "idle"))
            .unwrap_or_else(|| classify(&config, &app, &format!("{title} {source}")));
        let mut sample = ActivitySample {
            timestamp,
            app,
            title: if title.trim().is_empty() {
                format!("{} activity", device)
            } else {
                format!("{} - {}", device, title)
            },
            source,
            category,
        };
        apply_focus_productivity_gate(&focus, &mut sample);
        append_sample(&data_dir, &sample)?;
        detect_distraction(&data_dir, &state, &sample)?;
        notify_devices_for_attention_event(&data_dir, &state, &config, &sample)?;
        write_response(
            &mut stream,
            "application/json",
            &format!(
                "{{\"ok\":true,\"category\":\"{}\",\"timestamp\":{}}}",
                json_escape(&sample.category),
                sample.timestamp
            ),
        )?;
    } else if path.starts_with("/api/device/events") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let since = params
            .get("since")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        let device = params.get("device").map(String::as_str).unwrap_or("");
        write_response(
            &mut stream,
            "application/json",
            &device_notifications_json(&data_dir, since, device)?,
        )?;
    } else if path.starts_with("/api/native/notify") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let body = request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("");
        let message = params
            .get("message")
            .cloned()
            .or_else(|| json_string(body, "message"))
            .unwrap_or_else(|| "Focus alert".into());
        let title = params
            .get("title")
            .cloned()
            .or_else(|| json_string(body, "event"))
            .unwrap_or_else(|| "Local Focus".into());
        notify(&title, &message);
        write_response(&mut stream, "application/json", "{\"ok\":true}")?;
    } else if path.starts_with("/api/qr.svg") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let Some(value) = params.get("value").filter(|value| !value.trim().is_empty()) else {
            write_not_found(&mut stream, "Missing QR value.")?;
            return Ok(());
        };
        let label = params
            .get("label")
            .map(String::as_str)
            .unwrap_or("Local Focus connection QR");
        write_response(
            &mut stream,
            "image/svg+xml; charset=utf-8",
            &qr_svg(value, label)?,
        )?;
    } else if path.starts_with("/api/journal/settings") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let enabled = params
            .get("enabled")
            .map(|value| matches!(value.as_str(), "1" | "true" | "on" | "yes"))
            .unwrap_or(true);
        let reminder_mode = params
            .get("reminderMode")
            .map(|value| normalize_journal_reminder_mode(value))
            .unwrap_or_else(|| "evening".into());
        let settings = JournalSettings {
            enabled,
            reminder_mode,
        };
        save_journal_settings(&data_dir, &settings)?;
        append_event(
            &data_dir,
            "journal_settings_updated",
            if settings.enabled {
                "Daily journaling reminders enabled."
            } else {
                "Daily journaling reminders disabled."
            },
        )?;
        write_response(
            &mut stream,
            "application/json",
            &journal_settings_json(&settings),
        )?;
    } else if path.starts_with("/api/journal/reminders/add") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let body = request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("");
        let task = request_value(&params, body, "task").unwrap_or_default();
        let time = request_value(&params, body, "time").unwrap_or_default();
        if let Some(reminder) = add_journal_task_reminder(&data_dir, &task, &time)? {
            append_event(
                &data_dir,
                "journal_task_reminder_added",
                &format!("{} - {}", reminder.time, reminder.task),
            )?;
        }
        write_response(
            &mut stream,
            "application/json",
            &journal_task_reminders_json(&data_dir)?,
        )?;
    } else if path.starts_with("/api/journal/reminders/remove") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        if let Some(id) = params.get("id").filter(|value| !value.trim().is_empty()) {
            if remove_journal_task_reminder(&data_dir, id)? {
                append_event(&data_dir, "journal_task_reminder_removed", id)?;
            }
        }
        write_response(
            &mut stream,
            "application/json",
            &journal_task_reminders_json(&data_dir)?,
        )?;
    } else if path.starts_with("/api/journal/reminders") {
        write_response(
            &mut stream,
            "application/json",
            &journal_task_reminders_json(&data_dir)?,
        )?;
    } else if path.starts_with("/api/journal/entry") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let date = params
            .get("date")
            .and_then(|value| clean_journal_date(value))
            .or_else(local_today)
            .unwrap_or_default();
        write_response(
            &mut stream,
            "application/json",
            &journal_entry_json(&data_dir, &date)?,
        )?;
    } else if path.starts_with("/api/journal/save") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let body = request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("");
        let date = request_value(&params, body, "date")
            .and_then(|value| clean_journal_date(&value))
            .or_else(local_today)
            .unwrap_or_default();
        let text = json_string(body, "text")
            .or_else(|| params.get("text").cloned())
            .unwrap_or_default();
        save_journal_entry(&data_dir, &date, &text)?;
        append_event(
            &data_dir,
            "journal_saved",
            &format!("Journal saved for {date}."),
        )?;
        write_response(
            &mut stream,
            "application/json",
            &journal_entry_json(&data_dir, &date)?,
        )?;
    } else if route == "/api/timeline" {
        write_response(&mut stream, "application/json", &timeline_json(&data_dir)?)?;
    } else if route == "/api/report/reset" {
        reset_report(&data_dir)?;
        write_response(&mut stream, "application/json", "{\"ok\":true}")?;
    } else if route == "/api/report/history" {
        write_response(
            &mut stream,
            "application/json",
            &report_history_json(&data_dir)?,
        )?;
    } else if path.starts_with("/api/focus-sessions") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let since = params
            .get("since")
            .and_then(|value| value.parse::<i64>().ok());
        let until = params
            .get("until")
            .and_then(|value| value.parse::<i64>().ok());
        let focus = lock_state(&state).focus.clone();
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
        let since = params
            .get("since")
            .and_then(|value| value.parse::<i64>().ok());
        let until = params
            .get("until")
            .and_then(|value| value.parse::<i64>().ok());
        let period = params
            .get("period")
            .map(|value| value.as_str())
            .unwrap_or("window");
        write_response(
            &mut stream,
            "application/json",
            &focus_report_json(&data_dir, &target, since, until, period)?,
        )?;
    } else if route == "/api/report" {
        write_response(&mut stream, "application/json", &report_json(&data_dir)?)?;
    } else if route == "/api/state" {
        let (focus, devices, blocks, stopped) = {
            let guard = lock_state(&state);
            (
                guard.focus.clone(),
                guard.config.network_devices.clone(),
                guard.config.blocked_keywords.clone(),
                guard.stopped,
            )
        };
        write_response(
            &mut stream,
            "application/json",
            &state_json(&data_dir, focus, &devices, &blocks, stopped),
        )?;
    } else if route == "/connect" {
        write_response(
            &mut stream,
            "text/html; charset=utf-8",
            &connect_device_html(),
        )?;
    } else if path.starts_with("/download/local-focus-mobile.apk") {
        write_artifact_response(
            &mut stream,
            "application/vnd.android.package-archive",
            "local-focus-mobile.apk",
            &["mobile/local_focus_mobile/build/app/outputs/flutter-apk/app-debug.apk"],
        )?;
    } else if path.starts_with("/download/local-focus-macos.dmg") {
        write_artifact_response(
            &mut stream,
            "application/x-apple-diskimage",
            "LocalFocus.dmg",
            &["target/macos/LocalFocus.dmg"],
        )?;
    } else if route == "/device-sw.js" {
        write_response(
            &mut stream,
            "application/javascript; charset=utf-8",
            &device_service_worker_js(),
        )?;
    } else if route == "/device-manifest.json" {
        write_response(
            &mut stream,
            "application/manifest+json",
            &device_manifest_json(),
        )?;
    } else if route == "/device" {
        write_response(&mut stream, "text/html; charset=utf-8", &device_html())?;
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

fn write_binary_response(
    stream: &mut TcpStream,
    content_type: &str,
    filename: &str,
    body: &[u8],
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Disposition: attachment; filename=\"{}\"\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n",
        filename.replace('"', ""),
        body.len()
    )?;
    stream.write_all(body)
}

fn write_not_found(stream: &mut TcpStream, message: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n{}",
        message.len(),
        message
    )
}

fn write_forbidden(stream: &mut TcpStream, message: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n{}",
        message.len(),
        message
    )
}

/// Endpoints reachable from other machines on the LAN: only the device-companion
/// surface (receiver pages, QR images, downloads, and the mobile/native APIs).
/// Everything else — dashboard, timeline, reports, journal, focus/block control —
/// stays restricted to loopback.
fn remote_path_allowed(route: &str) -> bool {
    const ALLOWED_PREFIXES: [&str; 8] = [
        "/api/mobile/register",
        "/api/mobile/activity",
        "/api/device/register",
        "/api/device/events",
        "/api/native/notify",
        "/api/qr.svg",
        "/connect",
        "/download/",
    ];
    // `/device`, `/device-sw.js`, and `/device-manifest.json` all share this prefix.
    route.starts_with("/device") || ALLOWED_PREFIXES.iter().any(|prefix| route.starts_with(prefix))
}

/// State-changing endpoints that must reject cross-site browser requests.
fn is_mutation_path(route: &str) -> bool {
    const MUTATION_PREFIXES: [&str; 12] = [
        "/api/focus/",
        "/api/app/",
        "/api/block/",
        "/api/device/register",
        "/api/mobile/register",
        "/api/mobile/activity",
        "/api/native/notify",
        "/api/report/reset",
        "/api/journal/settings",
        "/api/journal/reminders/add",
        "/api/journal/reminders/remove",
        "/api/journal/save",
    ];
    MUTATION_PREFIXES
        .iter()
        .any(|prefix| route.starts_with(prefix))
}

/// Detect a cross-origin (CSRF) request using the browser-supplied
/// `Sec-Fetch-Site` header, falling back to `Origin` vs `Host`. Native clients
/// send neither header and are treated as same-site.
fn request_is_cross_site(request: &str) -> bool {
    let mut sec_fetch_site: Option<String> = None;
    let mut origin: Option<String> = None;
    let mut host: Option<String> = None;
    for line in request.lines() {
        if line.is_empty() {
            break; // end of headers
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("sec-fetch-site") {
            sec_fetch_site = Some(value.to_ascii_lowercase());
        } else if name.eq_ignore_ascii_case("origin") {
            origin = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("host") {
            host = Some(value.to_string());
        }
    }

    if let Some(site) = sec_fetch_site {
        return !matches!(site.as_str(), "same-origin" | "same-site" | "none");
    }

    match (origin, host) {
        (Some(origin), Some(host)) if !origin.is_empty() && origin != "null" => {
            let origin_host = origin
                .split_once("://")
                .map(|(_, rest)| rest)
                .unwrap_or(origin.as_str());
            origin_host != host
        }
        _ => false,
    }
}

fn write_artifact_response(
    stream: &mut TcpStream,
    content_type: &str,
    filename: &str,
    relative_paths: &[&str],
) -> io::Result<()> {
    if let Some(path) = find_artifact_path(relative_paths) {
        let body = fs::read(path)?;
        write_binary_response(stream, content_type, filename, &body)
    } else {
        write_not_found(
            stream,
            "Local Focus installer artifact has not been built yet.",
        )
    }
}

fn qr_svg(value: &str, label: &str) -> io::Result<String> {
    let code = QrCode::new(value.as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let mut image = code
        .render::<svg::Color>()
        .min_dimensions(410, 410)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build();
    image = image.replacen(
        "<svg",
        &format!(
            "<svg role=\"img\" aria-label=\"{}\"",
            html_attr_escape(label)
        ),
        1,
    );
    Ok(image)
}

fn find_artifact_path(relative_paths: &[&str]) -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(current_dir) = env::current_dir() {
        roots.push(current_dir);
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_path_buf());
            if let Some(parent) = dir.parent() {
                roots.push(parent.to_path_buf());
            }
        }
    }

    roots
        .into_iter()
        .flat_map(|root| {
            relative_paths
                .iter()
                .map(move |relative| root.join(relative))
        })
        .find(|path| path.exists())
}

fn timeline_json(data_dir: &Path) -> io::Result<String> {
    let samples = load_samples(data_dir)?;
    let mut segments = Vec::new();
    let mut current: Option<ActivitySample> = None;
    let mut current_start = 0;
    let mut last_timestamp = 0;

    for sample in samples
        .into_iter()
        .rev()
        .take(1500)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
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

fn report_json(data_dir: &Path) -> io::Result<String> {
    let samples = load_samples(data_dir)?;
    let since = report_window_start(data_dir)?.max(now() - 24 * 60 * 60);
    let recent: Vec<_> = samples
        .into_iter()
        .filter(|s| s.timestamp >= since)
        .collect();
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
    apps.sort_by_key(|entry| std::cmp::Reverse(entry.1));
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
    data_dir: &Path,
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
        .filter(|s| {
            s.timestamp >= since && until_override.is_none_or(|until| s.timestamp < until)
        })
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
    let mut hourly_details: HourlyDetails = BTreeMap::new();

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
        } else if sample.category != "idle" {
            outside_seconds += seconds;
            let (app, source) = report_activity_key(sample);
            *distraction_counts.entry((app, source)).or_default() += seconds;
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
    distractions.sort_by_key(|entry| std::cmp::Reverse(entry.1));
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
            details.sort_by_key(|entry| std::cmp::Reverse(entry.1));
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
    let focus_percent = (focused_seconds * 100)
        .checked_div(total_seconds)
        .unwrap_or(0)
        .min(100);
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

/// Per-hour activity rollup keyed by (app, title, source, category) -> seconds.
type HourlyDetails = BTreeMap<i64, BTreeMap<(String, String, String, String), u64>>;

fn target_list_from_text(target_text: &str) -> Vec<String> {
    target_text
        .split([',', '\n'])
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn sample_matches_target_text(sample: &ActivitySample, target: &str) -> bool {
    let target = target.trim();
    if target.is_empty() {
        return false;
    }

    if let Some(target_parts) = url_match_parts_from_text(target) {
        let sample_parts = sample_url_parts(sample);
        if sample_parts
            .iter()
            .any(|sample_parts| url_parts_match(&target_parts, sample_parts))
        {
            return true;
        }
    }

    let haystack = normalize_match_text(&format!(
        "{} {} {}",
        sample.app, sample.title, sample.source
    ));
    let normalized = normalize_match_text(target);
    if !normalized.is_empty() && haystack.contains(&normalized) {
        return true;
    }

    let Some(domain) = website_rule_domain(target) else {
        return false;
    };
    let domain = normalize_match_text(&domain);
    !domain.is_empty() && haystack.contains(&domain)
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

fn reset_report(data_dir: &Path) -> io::Result<()> {
    let archived_at = now();
    let report = report_json(data_dir)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("report_history.jsonl"))?;
    writeln!(
        file,
        "{{\"archivedAt\":{},\"report\":{}}}",
        archived_at, report
    )?;
    fs::write(data_dir.join("report_start.txt"), archived_at.to_string())
}

fn report_history_json(data_dir: &Path) -> io::Result<String> {
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

fn device_notifications_json(data_dir: &Path, since: i64, device: &str) -> io::Result<String> {
    let path = data_dir.join("device_notifications.jsonl");
    if !path.exists() {
        return Ok("[]".into());
    }

    let rows = BufReader::new(File::open(path)?)
        .lines()
        .map_while(Result::ok)
        .filter(|line| json_number(line, "timestamp").is_some_and(|timestamp| timestamp > since))
        .filter(|line| {
            let target_devices = json_string(line, "devices").unwrap_or_default();
            if device.is_empty() {
                true
            } else {
                !target_devices.is_empty()
                    && target_devices.split(';').any(|target| target == device)
            }
        })
        .collect::<Vec<_>>();
    Ok(format!("[{}]", rows.join(",")))
}

fn load_journal_settings(data_dir: &Path) -> io::Result<JournalSettings> {
    let path = data_dir.join("journal_settings.json");
    if !path.exists() {
        return Ok(JournalSettings::default());
    }

    let value = fs::read_to_string(path)?;
    Ok(JournalSettings {
        enabled: json_bool(&value, "enabled").unwrap_or(true),
        reminder_mode: json_string(&value, "reminderMode")
            .map(|value| normalize_journal_reminder_mode(&value))
            .unwrap_or_else(|| "evening".into()),
    })
}

fn save_journal_settings(data_dir: &Path, settings: &JournalSettings) -> io::Result<()> {
    fs::write(
        data_dir.join("journal_settings.json"),
        format!(
            "{{\"enabled\":{},\"reminderMode\":\"{}\",\"updatedAt\":{}}}",
            settings.enabled,
            json_escape(&normalize_journal_reminder_mode(&settings.reminder_mode)),
            now()
        ),
    )
}

fn normalize_journal_reminder_mode(value: &str) -> String {
    match value.trim().to_lowercase().as_str() {
        "next_morning" | "morning" => "next_morning".into(),
        _ => "evening".into(),
    }
}

fn journal_settings_json(settings: &JournalSettings) -> String {
    format!(
        "{{\"enabled\":{},\"reminderMode\":\"{}\"}}",
        settings.enabled,
        json_escape(&normalize_journal_reminder_mode(&settings.reminder_mode))
    )
}

fn journal_entry_json(data_dir: &Path, date: &str) -> io::Result<String> {
    let date = clean_journal_date(date).unwrap_or_else(|| local_today().unwrap_or_default());
    let (text, updated_at) = journal_entry_for_date(data_dir, &date)?.unwrap_or_default();
    Ok(format!(
        "{{\"date\":\"{}\",\"text\":\"{}\",\"updatedAt\":{}}}",
        json_escape(&date),
        json_escape(&text),
        updated_at
    ))
}

fn save_journal_entry(data_dir: &Path, date: &str, text: &str) -> io::Result<()> {
    let date = clean_journal_date(date)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid journal date"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("journal_entries.jsonl"))?;
    writeln!(
        file,
        "{{\"date\":\"{}\",\"updatedAt\":{},\"text\":\"{}\"}}",
        json_escape(&date),
        now(),
        json_escape(text)
    )
}

fn journal_entry_for_date(data_dir: &Path, date: &str) -> io::Result<Option<(String, i64)>> {
    let path = data_dir.join("journal_entries.jsonl");
    if !path.exists() {
        return Ok(None);
    }

    let mut latest = None;
    for line in BufReader::new(File::open(path)?)
        .lines()
        .map_while(Result::ok)
    {
        if json_string(&line, "date").as_deref() != Some(date) {
            continue;
        }
        let updated_at = json_number(&line, "updatedAt").unwrap_or(0);
        let text = json_string(&line, "text").unwrap_or_default();
        if latest
            .as_ref()
            .is_none_or(|(_, previous_at)| updated_at >= *previous_at)
        {
            latest = Some((text, updated_at));
        }
    }
    Ok(latest)
}

fn journal_entry_exists(data_dir: &Path, date: &str) -> bool {
    journal_entry_for_date(data_dir, date)
        .ok()
        .flatten()
        .is_some_and(|(text, _)| !text.trim().is_empty())
}

fn clean_journal_date(value: &str) -> Option<String> {
    let value = value.trim();
    if value.len() != 10 {
        return None;
    }
    for (index, c) in value.chars().enumerate() {
        if index == 4 || index == 7 {
            if c != '-' {
                return None;
            }
        } else if !c.is_ascii_digit() {
            return None;
        }
    }
    Some(value.to_string())
}

fn load_journal_task_reminders(data_dir: &Path) -> io::Result<Vec<JournalTaskReminder>> {
    let path = data_dir.join("journal_task_reminders.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }

    let mut reminders = Vec::new();
    for line in BufReader::new(File::open(path)?)
        .lines()
        .map_while(Result::ok)
    {
        let Some(id) = json_string(&line, "id").filter(|value| !value.trim().is_empty()) else {
            continue;
        };
        let Some(time) = json_string(&line, "time").and_then(|value| clean_reminder_time(&value))
        else {
            continue;
        };
        let Some(task) = json_string(&line, "task").filter(|value| !value.trim().is_empty()) else {
            continue;
        };
        reminders.push(JournalTaskReminder { id, task, time });
    }
    reminders.sort_by(|left, right| left.time.cmp(&right.time).then(left.task.cmp(&right.task)));
    Ok(reminders)
}

fn save_journal_task_reminders(
    data_dir: &Path,
    reminders: &[JournalTaskReminder],
) -> io::Result<()> {
    let mut content = String::new();
    for reminder in reminders {
        content.push_str(&format!(
            "{{\"id\":\"{}\",\"time\":\"{}\",\"task\":\"{}\"}}\n",
            json_escape(&reminder.id),
            json_escape(&reminder.time),
            json_escape(&reminder.task)
        ));
    }
    fs::write(data_dir.join("journal_task_reminders.jsonl"), content)
}

fn journal_task_reminders_json(data_dir: &Path) -> io::Result<String> {
    let rows = load_journal_task_reminders(data_dir)?
        .into_iter()
        .map(|reminder| {
            format!(
                "{{\"id\":\"{}\",\"time\":\"{}\",\"task\":\"{}\"}}",
                json_escape(&reminder.id),
                json_escape(&reminder.time),
                json_escape(&reminder.task)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!("[{}]", rows))
}

fn add_journal_task_reminder(
    data_dir: &Path,
    task: &str,
    time: &str,
) -> io::Result<Option<JournalTaskReminder>> {
    let task = clean_journal_reminder_task(task);
    let Some(time) = clean_reminder_time(time) else {
        return Ok(None);
    };
    if task.is_empty() {
        return Ok(None);
    }

    let mut reminders = load_journal_task_reminders(data_dir)?;
    if reminders
        .iter()
        .any(|reminder| reminder.time == time && reminder.task.eq_ignore_ascii_case(&task))
    {
        return Ok(reminders
            .into_iter()
            .find(|reminder| reminder.time == time && reminder.task.eq_ignore_ascii_case(&task)));
    }

    let reminder = JournalTaskReminder {
        id: format!("{}-{}", now(), reminders.len() + 1),
        task,
        time,
    };
    reminders.push(reminder.clone());
    save_journal_task_reminders(data_dir, &reminders)?;
    Ok(Some(reminder))
}

fn remove_journal_task_reminder(data_dir: &Path, id: &str) -> io::Result<bool> {
    let mut reminders = load_journal_task_reminders(data_dir)?;
    let before = reminders.len();
    reminders.retain(|reminder| reminder.id != id);
    if reminders.len() != before {
        save_journal_task_reminders(data_dir, &reminders)?;
        return Ok(true);
    }
    Ok(false)
}

fn clean_journal_reminder_task(value: &str) -> String {
    value
        .trim()
        .chars()
        .take(160)
        .collect::<String>()
        .replace(['\n', '\r', '\t'], " ")
}

fn clean_reminder_time(value: &str) -> Option<String> {
    let value = value.trim();
    let (hour, minute) = value.split_once(':')?;
    if hour.len() != 2 || minute.len() != 2 {
        return None;
    }
    let hour = hour.parse::<u32>().ok()?;
    let minute = minute.parse::<u32>().ok()?;
    if hour < 24 && minute < 60 {
        Some(format!("{hour:02}:{minute:02}"))
    } else {
        None
    }
}

fn maybe_send_journal_task_reminders(data_dir: &Path) -> io::Result<()> {
    let Some(clock) = local_clock() else {
        return Ok(());
    };
    let current_time = format!("{:02}:{:02}", clock.hour, clock.minute);
    let reminders = load_journal_task_reminders(data_dir)?;
    if reminders.is_empty() {
        return Ok(());
    }

    let marker_path = data_dir.join("journal_task_reminder_marker.txt");
    let sent = fs::read_to_string(&marker_path).unwrap_or_default();
    let mut new_markers = Vec::new();
    for reminder in reminders
        .into_iter()
        .filter(|reminder| reminder.time == current_time)
    {
        let marker = format!("{}|{}|{}", clock.today, reminder.id, reminder.time);
        if sent.lines().any(|line| line.trim() == marker) {
            continue;
        }
        let message = format!("{} - {}", reminder.time, reminder.task);
        notify("Journal task reminder", &message);
        append_event(data_dir, "journal_task_reminder", &message)?;
        new_markers.push(marker);
    }

    if !new_markers.is_empty() {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(marker_path)?;
        for marker in new_markers {
            writeln!(file, "{marker}")?;
        }
    }
    Ok(())
}

fn maybe_send_journal_reminder(data_dir: &Path) -> io::Result<()> {
    let settings = load_journal_settings(data_dir).unwrap_or_default();
    let Some(due) = journal_reminder_due(data_dir, &settings) else {
        return Ok(());
    };
    let marker_path = data_dir.join("journal_reminder_marker.txt");
    let last_marker = fs::read_to_string(&marker_path).unwrap_or_default();
    if last_marker.trim() == due.marker_key {
        return Ok(());
    }

    notify("Journal reminder", &due.message);
    append_event(data_dir, "journal_reminder", &due.message)?;
    fs::write(marker_path, due.marker_key)
}

fn journal_reminder_due(
    data_dir: &Path,
    settings: &JournalSettings,
) -> Option<JournalReminderDue> {
    if !settings.enabled {
        return None;
    }
    let clock = local_clock()?;
    let mode = normalize_journal_reminder_mode(&settings.reminder_mode);
    let (date, label, message) = if mode == "next_morning" {
        if !(7..11).contains(&clock.hour) {
            return None;
        }
        (
            clock.yesterday.clone(),
            "Yesterday".to_string(),
            "Take a few minutes to journal about yesterday.".to_string(),
        )
    } else {
        if !(20..22).contains(&clock.hour) {
            return None;
        }
        (
            clock.today.clone(),
            "Today".to_string(),
            "Take a few minutes to journal about today.".to_string(),
        )
    };
    if journal_entry_exists(data_dir, &date) {
        return None;
    }
    Some(JournalReminderDue {
        marker_key: format!("{mode}:{date}"),
        date,
        label,
        message,
    })
}

fn local_today() -> Option<String> {
    local_clock().map(|clock| clock.today)
}

fn local_clock() -> Option<LocalClock> {
    #[cfg(target_os = "macos")]
    {
        let today = command_text("date", &["+%Y-%m-%d"])?;
        let yesterday = command_text("date", &["-v-1d", "+%Y-%m-%d"])?;
        let hour = command_text("date", &["+%H"])?.parse().ok()?;
        let minute = command_text("date", &["+%M"])?.parse().ok()?;
        Some(LocalClock {
            today,
            yesterday,
            hour,
            minute,
        })
    }

    #[cfg(target_os = "linux")]
    {
        let today = command_text("date", &["+%Y-%m-%d"])?;
        let yesterday = command_text("date", &["-d", "yesterday", "+%Y-%m-%d"])?;
        let hour = command_text("date", &["+%H"])?.parse().ok()?;
        let minute = command_text("date", &["+%M"])?.parse().ok()?;
        return Some(LocalClock {
            today,
            yesterday,
            hour,
            minute,
        });
    }

    #[cfg(target_os = "windows")]
    {
        let script = "$now=Get-Date; $y=$now.AddDays(-1); \"$($now.ToString('yyyy-MM-dd'))|$($y.ToString('yyyy-MM-dd'))|$($now.ToString('HH'))|$($now.ToString('mm'))\"";
        let value = command_text("powershell", &["-NoProfile", "-Command", script])?;
        let mut parts = value.split('|');
        return Some(LocalClock {
            today: parts.next()?.to_string(),
            yesterday: parts.next()?.to_string(),
            hour: parts.next()?.parse().ok()?,
            minute: parts.next()?.parse().ok()?,
        });
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

fn maybe_log_previous_day_report(
    data_dir: &Path,
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
            &[
                "-j",
                "-f",
                "%Y-%m-%d %H:%M:%S",
                &format!("{today} 00:00:00"),
                "+%s",
            ],
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
        Some((yesterday, yesterday_start, today_start))
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

    // Other platforms (e.g. Android) compute the day window in the native layer.
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

fn command_text(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn report_window_start(data_dir: &Path) -> io::Result<i64> {
    let path = data_dir.join("report_start.txt");
    if !path.exists() {
        return Ok(0);
    }

    let value = fs::read_to_string(path)?;
    Ok(value.trim().parse().unwrap_or(0))
}

fn state_json(
    data_dir: &Path,
    focus: Option<FocusSession>,
    devices: &[String],
    blocks: &[String],
    stopped: bool,
) -> String {
    let lan_url = local_network_url().unwrap_or_else(|| "http://127.0.0.1:4799".into());
    let device_connect_url = format!("{lan_url}/device");
    let device_install_url = format!("{lan_url}/connect");
    let android_app_url = format!("{lan_url}/download/local-focus-mobile.apk");
    let mac_app_url = format!("{lan_url}/download/local-focus-macos.dmg");
    let journal_settings = load_journal_settings(data_dir).unwrap_or_default();
    let journal_due = journal_reminder_due(data_dir, &journal_settings);
    let journal_json = match journal_due {
        Some(due) => format!(
            "{{\"settings\":{},\"due\":true,\"dueDate\":\"{}\",\"dueLabel\":\"{}\",\"dueMessage\":\"{}\"}}",
            journal_settings_json(&journal_settings),
            json_escape(&due.date),
            json_escape(&due.label),
            json_escape(&due.message)
        ),
        None => format!(
            "{{\"settings\":{},\"due\":false,\"dueDate\":\"\",\"dueLabel\":\"\",\"dueMessage\":\"\"}}",
            journal_settings_json(&journal_settings)
        ),
    };
    let devices_json = devices
        .iter()
        .map(|device| {
            
            parse_network_device_record(device)
        })
        .filter(is_qr_connected_device)
        .map(|device| {
            format!(
                "{{\"name\":\"{}\",\"kind\":\"{}\",\"endpoint\":\"{}\",\"selected\":{}}}",
                json_escape(&device.name),
                json_escape(&device.kind),
                json_escape(&device.endpoint),
                device.selected
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let blocks_json = blocks
        .iter()
        .map(|record| {
            let rule = parse_block_rule_record(record);
            format!(
                "{{\"target\":\"{}\",\"mode\":\"{}\",\"hasPassword\":{}}}",
                json_escape(&rule.target),
                json_escape(block_mode_name(rule.mode)),
                !rule.password.is_empty()
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    match focus {
        Some(focus) => {
            let elapsed = focus_elapsed_seconds(&focus, now());
            let remaining = ((focus.duration_minutes * 60) as i64 - elapsed).max(0);
            format!(
                "{{\"stopped\":{},\"focus\":{{\"task\":\"{}\",\"target\":\"{}\",\"startedAt\":{},\"durationMinutes\":{},\"alertDelaySeconds\":{},\"alertAction\":\"{}\",\"alertMessage\":\"{}\",\"redirectApp\":\"{}\",\"highFocusMode\":{},\"paused\":{},\"remainingSeconds\":{}}},\"devices\":[{}],\"blockedRules\":[{}],\"journal\":{},\"deviceConnectUrl\":\"{}\",\"deviceInstallUrl\":\"{}\",\"androidAppUrl\":\"{}\",\"macAppUrl\":\"{}\"}}",
                stopped,
                json_escape(&focus.task),
                json_escape(&focus.target),
                focus.started_at,
                focus.duration_minutes,
                focus.alert_delay_seconds,
                json_escape(&focus.alert_action),
                json_escape(&clean_alert_message_template(&focus.alert_message)),
                json_escape(&focus.redirect_app),
                focus.high_focus_mode,
                focus.paused_at.is_some(),
                remaining,
                devices_json,
                blocks_json,
                journal_json,
                json_escape(&device_connect_url),
                json_escape(&device_install_url),
                json_escape(&android_app_url),
                json_escape(&mac_app_url)
            )
        }
        None => format!(
            "{{\"stopped\":{},\"focus\":null,\"devices\":[{}],\"blockedRules\":[{}],\"journal\":{},\"deviceConnectUrl\":\"{}\",\"deviceInstallUrl\":\"{}\",\"androidAppUrl\":\"{}\",\"macAppUrl\":\"{}\"}}",
            stopped,
            devices_json,
            blocks_json,
            journal_json,
            json_escape(&device_connect_url),
            json_escape(&device_install_url),
            json_escape(&android_app_url),
            json_escape(&mac_app_url)
        ),
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
.calendar-actions button.active-report, .week-button.active-report { background:var(--good); border-color:var(--good); color:white; }
.calendar-actions button.active-year { background:var(--accent); border-color:var(--accent); color:white; }
.calendar-grid { display:grid; grid-template-columns:64px repeat(7, minmax(0, 1fr)); gap:6px; align-items:stretch; }
.calendar-label { color:var(--muted); font-size:12px; font-weight:750; text-align:center; padding:4px; }
.week-button, .day-button { width:100%; padding:8px 6px; }
.day-button.outside { color:var(--muted); opacity:.65; }
.day-button.selected { background:var(--good); border-color:var(--good); color:white; }
.focus-task-window { border:1px solid var(--line); border-radius:10px; padding:12px; background:var(--panel-soft); display:grid; gap:8px; }
.focus-task-window.disabled { opacity:.55; }
.focus-session-list { display:grid; gap:8px; }
.focus-session-row { border:1px solid var(--line); border-radius:8px; padding:9px; background:var(--panel); }
.block-fields { border:1px solid var(--line); border-radius:10px; padding:12px; background:var(--panel-soft); display:grid; grid-template-columns:minmax(0, 1fr); gap:12px; align-items:start; }
.block-fields button { min-height:42px; min-width:140px; white-space:nowrap; }
.check-field { display:grid; gap:7px; }
.block-type-options { display:grid; grid-template-columns:repeat(2, minmax(140px, 1fr)); gap:8px; max-width:520px; }
.block-password-field { max-width:520px; }
.block-submit { justify-self:start; align-self:start; }
.inline-check { min-height:42px; border:1px solid var(--line); border-radius:8px; padding:8px 10px; display:flex; align-items:center; gap:9px; background:var(--panel); font-weight:800; cursor:pointer; }
.inline-check.selected, .inline-check:has(input:checked) { border-color:color-mix(in srgb, var(--accent) 55%, var(--line)); background:color-mix(in srgb, var(--accent) 10%, var(--panel)); color:var(--ink); }
.inline-check input { width:18px; height:18px; accent-color:var(--accent); }
.password-hidden { display:none !important; }
.device-list { display:grid; gap:8px; margin-top:10px; }
.device-pill { border:1px solid var(--line); border-radius:8px; padding:8px 10px; background:var(--panel); overflow-wrap:anywhere; }
.device-row { display:grid; grid-template-columns:auto 1fr; gap:10px; align-items:start; }
.device-row input { width:18px; height:18px; min-width:18px; margin-top:2px; accent-color:var(--accent); }
.device-connect-actions { display:flex; flex-wrap:wrap; gap:10px; }
.device-connect-actions button:first-child { background:var(--good); border-color:var(--good); color:white; }
.device-qr-panel { border:1px solid var(--line); border-radius:10px; padding:14px; background:var(--panel-soft); display:grid; gap:12px; }
.device-qr-panel.hidden { display:none; }
.qr-type-grid { display:grid; grid-template-columns:repeat(5, minmax(0, 1fr)); gap:8px; }
.qr-type-grid button { min-height:42px; padding:8px; }
.qr-type-grid button.active { background:var(--accent); border-color:var(--accent); color:white; }
.device-qr-body { display:grid; grid-template-columns:auto minmax(0, 1fr); gap:14px; align-items:center; }
.device-qr-code { width:432px; min-height:432px; border:1px solid var(--line); border-radius:10px; padding:10px; background:#fff; display:grid; place-items:center; max-width:100%; }
.device-qr-code svg, .device-qr-code img { width:410px; max-width:100%; height:auto; display:block; shape-rendering:crispEdges; image-rendering:pixelated; }
.device-qr-meta { display:grid; gap:8px; min-width:0; }
.device-qr-meta a { overflow-wrap:anywhere; color:var(--accent); font-weight:800; }
.device-qr-url { border:1px solid var(--line); border-radius:8px; background:var(--panel); padding:10px; display:grid; gap:6px; }
.device-qr-url code { display:block; font-size:16px; font-weight:850; color:var(--ink); overflow-wrap:anywhere; user-select:all; }
.device-qr-url button { justify-self:start; padding:7px 10px; }
.blocked-list { display:flex; flex-wrap:wrap; gap:8px; margin-top:10px; }
.blocked-chip { display:inline-flex; align-items:center; gap:8px; border:1px solid color-mix(in srgb, var(--bad) 38%, var(--line)); border-radius:999px; padding:6px 10px; background:color-mix(in srgb, var(--bad) 7%, transparent); color:var(--ink); font-weight:700; max-width:100%; overflow-wrap:anywhere; }
.blocked-chip.editing { border-color:color-mix(in srgb, var(--accent) 65%, var(--line)); background:color-mix(in srgb, var(--accent) 12%, transparent); }
.blocked-chip small { color:var(--muted); font-weight:800; }
.blocked-chip button { width:auto; min-width:0; border:0; background:transparent; color:var(--bad); padding:0 2px; font-weight:900; }
.blocked-chip .edit-chip { color:var(--accent); }
.focus-layout { display:grid; gap:16px; align-items:start; }
.focus-layout.editor-collapsed { grid-template-columns:minmax(0, 520px); }
.focus-layout.editor-collapsed .focus-form { display:none; }
.focus-form { display:grid; grid-template-columns:repeat(2, minmax(0, 1fr)); gap:12px; align-items:end; }
.focus-form .field-wide { grid-column:1 / -1; }
.alert-message-field textarea { min-height:78px; }
.target-builder { display:grid; gap:8px; }
.target-entry { display:grid; grid-template-columns:minmax(0, 1fr) auto; gap:8px; }
.target-entry button { min-width:96px; }
.target-list-editor { display:flex; flex-wrap:wrap; gap:8px; min-height:38px; padding:8px; border:1px solid var(--line); border-radius:8px; background:var(--panel-soft); }
.target-list-editor.empty::before { content:"Add up to 15 focus apps or websites."; color:var(--muted); }
.target-remove { display:inline-flex; align-items:center; gap:6px; max-width:100%; border:1px solid color-mix(in srgb, var(--accent) 35%, var(--line)); border-radius:999px; padding:5px 9px; background:var(--panel); color:var(--ink); font:inherit; font-weight:650; overflow-wrap:anywhere; }
.target-remove span { color:var(--muted); font-weight:850; }
.focus-actions { display:flex; flex-wrap:wrap; gap:10px; align-items:center; justify-content:flex-end; }
.focus-side { border:1px solid var(--line); border-radius:10px; padding:14px; background:var(--panel-soft); display:grid; gap:12px; }
.focus-side h3 { margin:0; font-size:13px; color:var(--muted); text-transform:uppercase; letter-spacing:.06em; }
.quick-metrics { display:grid; grid-template-columns:repeat(3, minmax(0, 1fr)); gap:8px; }
.quick-metric { border:1px solid var(--line); border-radius:8px; padding:10px; background:var(--panel); }
.quick-metric span { color:var(--muted); display:block; font-size:11px; font-weight:700; }
.quick-metric strong { display:block; margin-top:2px; font-size:16px; }
.high-focus-control { border:1px solid var(--line); border-radius:8px; padding:10px; background:var(--panel); display:grid; gap:8px; }
.high-focus-row { display:flex; flex-wrap:wrap; gap:10px; align-items:center; justify-content:space-between; }
.high-focus-check { display:flex; align-items:center; gap:8px; font-weight:800; }
.high-focus-check input { width:18px; height:18px; accent-color:var(--bad); }
.high-focus-check input:disabled { opacity:.55; }
.high-focus-explain { display:none; color:var(--muted); font-size:12px; }
.high-focus-explain.open { display:block; }
.journal-card { gap:16px; }
.journal-head { display:flex; flex-wrap:wrap; gap:12px; align-items:flex-start; justify-content:space-between; }
.journal-toggle { display:flex; align-items:center; gap:8px; font-weight:850; }
.journal-toggle input { width:18px; height:18px; accent-color:var(--good); }
.journal-settings { display:grid; grid-template-columns:minmax(180px, 260px) minmax(0, 1fr); gap:12px; align-items:end; }
.journal-reminder { border:1px solid var(--line); border-radius:8px; padding:10px; background:var(--panel-soft); min-height:42px; }
.journal-reminder.due { border-color:color-mix(in srgb, var(--warn) 55%, var(--line)); background:color-mix(in srgb, var(--warn) 10%, var(--panel)); }
.journal-reminder button { margin-top:8px; padding:7px 10px; }
.journal-editor { display:grid; gap:10px; }
.journal-row { display:grid; grid-template-columns:minmax(160px, 220px) auto 1fr; gap:10px; align-items:end; }
.journal-row button { min-height:42px; }
#journalText { min-height:150px; }
.journal-task-reminders { border:1px solid var(--line); border-radius:10px; padding:12px; background:var(--panel-soft); display:grid; gap:12px; }
.journal-task-form { display:grid; grid-template-columns:minmax(0, 1fr) 120px auto; gap:10px; align-items:end; }
.journal-task-form button { min-height:42px; }
.journal-reminder-list { display:flex; flex-wrap:wrap; gap:8px; }
.journal-reminder-chip { display:inline-flex; align-items:center; gap:8px; border:1px solid color-mix(in srgb, var(--good) 38%, var(--line)); border-radius:999px; padding:6px 10px; background:var(--panel); max-width:100%; }
.journal-reminder-chip strong { white-space:nowrap; }
.journal-reminder-chip span { overflow-wrap:anywhere; }
.journal-reminder-chip button { border:0; background:transparent; color:var(--bad); padding:0 2px; min-height:0; }
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
.focus-summary-grid { gap:8px; }
.metric, .timeline, .apps, .explain, .history, .report { background:var(--panel); border:1px solid var(--line); border-radius:10px; padding:16px; }
.metric strong { display:block; font-size:28px; }
.muted { color:var(--muted); }
.explain { display:none; }
.explain.open { display:block; }
.history { display:none; }
.history.open { display:block; }
.report { display:none; }
.report.open { display:grid; gap:16px; }
.report-inline { background:transparent; border:0; border-radius:0; padding:0; }
.report-inline.open { border-top:1px solid var(--line); padding-top:16px; }
.report-head { display:flex; align-items:flex-start; justify-content:space-between; gap:12px; }
.report-close { min-width:40px; padding:7px 10px; }
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
.hour-bars, .period-bars { display:grid; grid-template-columns:repeat(auto-fit, minmax(30px, 1fr)); gap:10px; align-items:end; min-height:150px; }
.hour-bar { display:grid; align-items:end; height:120px; gap:2px; }
.hour-segment { position:relative; border-radius:4px 4px 0 0; min-height:2px; cursor:default; }
.hour-click, .period-click { border:0; background:transparent; padding:0; color:inherit; width:100%; cursor:pointer; }
.hour-click.selected .hour-bar, .period-click.selected .hour-bar { outline:2px solid color-mix(in srgb, var(--accent) 75%, var(--line)); outline-offset:3px; border-radius:6px; }
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
.hour-segment:hover::after { content:attr(data-tip); position:absolute; left:50%; bottom:calc(100% + 8px); transform:translateX(-50%); z-index:10; width:max-content; max-width:240px; padding:6px 8px; border:1px solid var(--line); border-radius:6px; background:var(--panel); color:var(--ink); box-shadow:0 8px 24px color-mix(in srgb, var(--ink) 18%, transparent); font-size:12px; font-weight:650; white-space:normal; }
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
@media (max-width:900px) { .focus-shell-head { align-items:start; display:grid; } .top-actions { justify-content:flex-start; } }
@media (max-width:760px) { header, .two, .grid, .item, .explain-grid, .history-grid, .report-grid, .report-two, .bar-row, .focus-form, .detail-grid, .block-fields, .activity-row, .calendar-actions, .device-qr-body, .journal-settings, .journal-row, .journal-task-form { grid-template-columns:1fr; display:grid; } header { align-items:start; } .header-actions { justify-items:start; } .hour-bars, .period-bars { grid-template-columns:repeat(6, minmax(12px, 1fr)); } .focus-shell-head { align-items:start; display:grid; } .quick-metrics { grid-template-columns:1fr; } .calendar-grid { grid-template-columns:48px repeat(7, minmax(28px, 1fr)); gap:4px; } .block-type-options, .qr-type-grid { grid-template-columns:1fr; } .block-password-field { grid-column:auto; } .device-qr-code { width:100%; max-width:432px; justify-self:center; } }
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
  <div id="stopBanner" style="display:none; align-items:center; justify-content:space-between; gap:14px; flex-wrap:wrap; border:1px solid var(--bad); background:color-mix(in srgb, var(--bad) 12%, var(--panel)); color:var(--ink); border-radius:12px; padding:14px 18px;">
    <strong>Local Focus is stopped — tracking, blocking, alerts, and reminders are paused until you resume.</strong>
    <button onclick="resumeApp()" style="background:var(--good); border-color:var(--good); color:#fff; white-space:nowrap;">Resume Local Focus</button>
  </div>
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
        <div class="field field-wide"><label for="task">Focus task</label><input id="task" value="Deep work" placeholder="Deep work" aria-label="Focus task"></div>
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
        <div class="field field-wide alert-message-field">
          <label for="alertMessage">Alert message</label>
          <textarea id="alertMessage" aria-label="Alert message">You have been outside your focus apps/sites for over {delay}. Allowed: '{targets}'. Current activity: {app}</textarea>
          <div class="muted">Use {delay}, {targets}, {app}, {title}, or {url}.</div>
        </div>
      </div>
    </div>
  </section>
  <aside class="focus-side">
    <h3>Current focus session</h3>
    <div class="quick-metrics">
      <div class="quick-metric"><span>Task</span><strong id="quickTask">None</strong></div>
      <div class="quick-metric"><span>Status</span><strong id="quickStatus">Off</strong></div>
      <div class="quick-metric"><span>Warn after</span><strong id="quickDelay">1m</strong></div>
      <div class="quick-metric"><span>Action</span><strong id="quickAction">Alert</strong></div>
    </div>
    <section class="grid focus-summary-grid" id="metrics" aria-label="Current focus summary"></section>
    <div class="high-focus-control">
      <div class="high-focus-row">
        <label class="high-focus-check" for="highFocusMode">
          <input id="highFocusMode" type="checkbox" onchange="toggleHighFocusMode()" disabled>
          High focus mode
        </label>
        <button id="highFocusExplainToggle" type="button" onclick="toggleHighFocusExplanation()" aria-expanded="false">Explain</button>
      </div>
      <div id="highFocusExplanation" class="high-focus-explain">When High Focus is checked, Local Focus fully blocks active apps or websites outside the current focus list. Your Local Focus dashboard stays allowed so you can turn this off.</div>
    </div>
    <div class="focus-actions">
      <button id="startFocus" class="focus-btn focus-idle" onclick="startFocus()">Start focus</button>
      <button id="pauseFocus" class="focus-btn" onclick="pauseFocus()" disabled>Pause</button>
      <button id="stopFocus" class="focus-btn" onclick="stopFocus()" disabled>Stop</button>
      <button onclick="resetReport()">Refresh</button>
    </div>
  </aside>
  <section id="journalCard" class="control-shell journal-card" aria-label="Daily journal">
    <div class="journal-head">
      <div>
        <h2>Daily journal</h2>
        <div class="muted">Optional private notes for each day. Entries stay on this device.</div>
      </div>
      <label class="journal-toggle" for="journalEnabled">
        <input id="journalEnabled" type="checkbox" checked onchange="saveJournalSettings()">
        Journal each day
      </label>
    </div>
    <div class="journal-settings">
      <div class="field">
        <label for="journalReminderMode">Reminder</label>
        <select id="journalReminderMode" onchange="saveJournalSettings()">
          <option value="evening">Evening, 8-10 PM</option>
          <option value="next_morning">Next morning, about yesterday</option>
        </select>
      </div>
      <div id="journalReminderState" class="journal-reminder muted">Journaling is on by default. Save an entry to clear that day's reminder.</div>
    </div>
    <div class="journal-editor">
      <div class="journal-row">
        <div class="field"><label for="journalDate">Journal date</label><input id="journalDate" type="date" onchange="loadJournalEntry()"></div>
        <button type="button" onclick="openJournalDate(todayYmd())">Today</button>
        <div id="journalStatus" class="muted">Ready.</div>
      </div>
      <textarea id="journalText" placeholder="What mattered today? What pulled focus? What should tomorrow remember?" aria-label="Daily journal entry" oninput="markJournalUnsaved()"></textarea>
      <div class="focus-actions">
        <button type="button" onclick="saveJournalEntry()">Save journal</button>
      </div>
      <div class="journal-task-reminders">
        <div>
          <strong>Reminders</strong>
          <div class="muted">Add a task and a 24-hour time. Local Focus will alert you at that time.</div>
        </div>
        <div class="journal-task-form">
          <div class="field"><label for="journalReminderTask">Task</label><input id="journalReminderTask" placeholder="Reflect on writing progress" aria-label="Reminder task"></div>
          <div class="field"><label for="journalReminderTime">Time (24 hr)</label><input id="journalReminderTime" inputmode="numeric" pattern="[0-2][0-9]:[0-5][0-9]" placeholder="18:30" aria-label="Reminder time in 24 hour HH:MM format"></div>
          <button type="button" onclick="addJournalTaskReminder()">Add reminder</button>
        </div>
        <div id="journalReminderTaskStatus" class="muted">No reminder added yet.</div>
        <div id="journalReminderList" class="journal-reminder-list" aria-live="polite"></div>
      </div>
    </div>
  </section>
  <section id="reportsCard" class="control-shell" aria-label="Reports">
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
    <section class="report report-inline" id="focusReportPanel" aria-live="polite"></section>
  </section>
  <section id="distractionCard" class="control-shell distraction-card" aria-label="Distraction rules">
    <div>
      <h2>Add distraction rule</h2>
      <div class="muted">Block matching apps or sites. Websites close their active tab; apps are quit.</div>
    </div>
    <div>
      <strong>Blocked apps and websites</strong>
      <div id="blockedList" class="blocked-list"></div>
    </div>
    <div class="block-fields">
      <div class="field"><label for="blockKeyword">Block keyword, app, or site</label><input id="blockKeyword" placeholder="youtube, reddit, games" aria-label="Block keyword" oninput="syncBlockEditState()"></div>
      <div class="field check-field">
        <label>Block type</label>
        <div class="block-type-options" role="group" aria-label="Block type">
          <label id="fullBlockOption" class="inline-check selected" for="fullBlock"><input id="fullBlock" type="checkbox" checked onchange="setBlockMode('full')" aria-label="Use full block"> Full block</label>
          <label id="passwordBlockOption" class="inline-check" for="passwordBlock"><input id="passwordBlock" type="checkbox" onchange="setBlockMode('password')" aria-label="Use password block"> Password block</label>
        </div>
        <div id="blockModeHint" class="muted">Full block is active.</div>
      </div>
      <div id="blockPasswordField" class="field block-password-field password-hidden"><label for="blockPassword">Password</label><input id="blockPassword" type="password" placeholder="Enter password to continue when blocked" aria-label="Block password"></div>
      <button id="blockSubmit" class="block-submit" onclick="addBlock()">Add block</button>
    </div>
  </section>
  <section id="devicesCard" class="control-shell" aria-label="Connect to device">
    <div>
      <h2>Connect to device</h2>
      <div class="muted">All device setup starts from a QR code or copied link. Local Focus does not scan the network for devices.</div>
    </div>
    <div class="device-pill"><strong>QR link</strong><br><span id="deviceConnectUrl" class="muted">Loading...</span></div>
    <div class="device-connect-actions">
      <button type="button" onclick="openDeviceQrPanel('install')">Show QR code</button>
    </div>
    <div id="deviceQrPanel" class="device-qr-panel hidden" aria-live="polite">
      <div>
        <strong>Download or connect with QR</strong>
        <div class="muted">Choose the device type, then scan the QR code from that device. Only devices that open this QR link can connect.</div>
      </div>
      <div class="qr-type-grid" role="group" aria-label="QR destination">
        <button id="qrInstallButton" type="button" onclick="renderDeviceQr('install')">Any device</button>
        <button id="qrAndroidButton" type="button" onclick="renderDeviceQr('android')">Android app</button>
        <button id="qrIphoneButton" type="button" onclick="renderDeviceQr('iphone')">iPhone/iPad</button>
        <button id="qrLaptopButton" type="button" onclick="renderDeviceQr('laptop')">Mac laptop app</button>
        <button id="qrReceiverButton" type="button" onclick="renderDeviceQr('receiver')">Receiver link</button>
      </div>
      <div class="device-qr-body">
        <div id="deviceQrCode" class="device-qr-code"></div>
        <div class="device-qr-meta">
          <strong id="deviceQrTitle">Local Focus</strong>
          <p id="deviceQrHint" class="muted"></p>
          <div class="device-qr-url">
            <span class="muted">If iPhone Camera does not show the QR URL, copy or type this exact link:</span>
            <code id="deviceQrPlainUrl"></code>
            <button type="button" onclick="copyDeviceQrUrl()">Copy URL</button>
          </div>
          <a id="deviceQrLink" href="" target="_blank" rel="noreferrer"></a>
        </div>
      </div>
    </div>
    <div>
      <strong>QR-connected devices</strong>
      <div id="deviceList" class="device-list"></div>
    </div>
  </section>
  <section class="explain" id="explainPanel">
    <h2>Report meaning</h2>
    <div class="explain-grid">
      <div><h3>Total time</h3><p>All tracked time in the current report window: productive, distracted, and idle.</p></div>
      <div><h3>Productive</h3><p>During a targeted focus session, only activity matching one of your focus apps or sites counts here. Outside targeted focus, productive keywords are used.</p></div>
      <div><h3>Distracted</h3><p>Any activity that is not productive. During targeted focus, every app or site outside your focus list is tracked here.</p></div>
      <div><h3>Idle</h3><p>If there is no keyboard or mouse input for 60 seconds, time is tracked as idle even when the focused app or website matches your focus list.</p></div>
      <div><h3>Blocked</h3><p>Blocked apps or sites are actively closed when detected, and the blocked time is tracked as distracted.</p></div>
    </div>
  </section>
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
let activeReportPeriod = 'day';
let activeReportYear = selectedReportDate.getFullYear();
let activeReportMonth = selectedReportDate.getMonth();
let activeReportWeek = 0;
let blockedRules = [];
let editingBlockTarget = '';
let deviceQrUrls = {};
let activeDeviceQrKind = 'install';
let activeFocusSession = null;
let journalEntryDirty = false;
let activeJournalDate = '';
let journalTaskReminders = [];
const MAX_FOCUS_TARGETS = 15;
const DEFAULT_ALERT_MESSAGE_TEMPLATE = `You have been outside your focus apps/sites for over {delay}. Allowed: '{targets}'. Current activity: {app}`;
const fmtTime = seconds => new Date(seconds * 1000).toLocaleTimeString([], {hour:'2-digit', minute:'2-digit'});
const minutes = seconds => Math.max(1, Math.round(seconds / 60));
async function startFocus() {
  saveFocusDraft();
  const task = encodeURIComponent(document.querySelector('#task').value || 'Deep work');
  const target = encodeURIComponent(document.querySelector('#target').value || '');
  const mins = encodeURIComponent(document.querySelector('#minutes').value || '25');
  const alertSeconds = encodeURIComponent(Math.max(1, Number(document.querySelector('#alertMinutes').value || '1')) * 60);
  const alertAction = encodeURIComponent(document.querySelector('#alertAction').value || 'alert');
  const alertMessage = encodeURIComponent(document.querySelector('#alertMessage').value || DEFAULT_ALERT_MESSAGE_TEMPLATE);
  const redirectApp = encodeURIComponent(document.querySelector('#redirectApp').value || '');
  await fetch(`/api/focus/start?task=${task}&target=${target}&minutes=${mins}&alertSeconds=${alertSeconds}&alertAction=${alertAction}&alertMessage=${alertMessage}&redirectApp=${redirectApp}`);
  refresh();
}
async function stopFocus() { await fetch('/api/focus/stop'); refresh(); }
async function resumeApp() { await fetch('/api/app/resume'); refresh(); }
async function pauseFocus() { await fetch('/api/focus/pause'); refresh(); }
async function toggleHighFocusMode() {
  const checkbox = document.querySelector('#highFocusMode');
  checkbox.disabled = true;
  await fetch(`/api/focus/high-focus?enabled=${checkbox.checked ? '1' : '0'}`);
  refresh();
}
function toggleHighFocusExplanation() {
  const panel = document.querySelector('#highFocusExplanation');
  const button = document.querySelector('#highFocusExplainToggle');
  const open = panel.classList.toggle('open');
  button.setAttribute('aria-expanded', String(open));
  button.textContent = open ? 'Hide explanation' : 'Explain';
}
async function resetReport() {
  await fetch('/api/report/reset');
  closeFocusReport();
  refresh();
}
function todayYmd(date = new Date()) {
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, '0');
  const day = String(date.getDate()).padStart(2, '0');
  return `${year}-${month}-${day}`;
}
function openJournalDate(date) {
  const input = document.querySelector('#journalDate');
  input.value = date || todayYmd();
  loadJournalEntry();
}
async function loadJournalEntry() {
  const input = document.querySelector('#journalDate');
  const status = document.querySelector('#journalStatus');
  const date = input.value || todayYmd();
  input.value = date;
  status.textContent = 'Loading journal...';
  try {
    const entry = await fetch(`/api/journal/entry?date=${encodeURIComponent(date)}`).then(r => r.json());
    activeJournalDate = entry.date || date;
    input.value = activeJournalDate;
    document.querySelector('#journalText').value = entry.text || '';
    journalEntryDirty = false;
    status.textContent = entry.updatedAt ? `Saved for ${activeJournalDate}.` : `No journal saved for ${activeJournalDate}.`;
  } catch {
    status.textContent = 'Could not load journal.';
  }
}
async function saveJournalEntry() {
  const date = document.querySelector('#journalDate').value || todayYmd();
  const text = document.querySelector('#journalText').value || '';
  const status = document.querySelector('#journalStatus');
  status.textContent = 'Saving journal...';
  try {
    const entry = await fetch('/api/journal/save', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({date, text})
    }).then(r => r.json());
    activeJournalDate = entry.date || date;
    document.querySelector('#journalDate').value = activeJournalDate;
    journalEntryDirty = false;
    status.textContent = `Saved for ${activeJournalDate}.`;
    refresh();
  } catch {
    status.textContent = 'Could not save journal.';
  }
}
async function saveJournalSettings() {
  const enabled = document.querySelector('#journalEnabled').checked;
  const reminderMode = document.querySelector('#journalReminderMode').value || 'evening';
  await fetch(`/api/journal/settings?enabled=${enabled ? '1' : '0'}&reminderMode=${encodeURIComponent(reminderMode)}`);
  updateJournalControlState({settings: {enabled, reminderMode}, due: false});
  refresh();
}
function markJournalUnsaved() {
  journalEntryDirty = true;
  const date = document.querySelector('#journalDate').value || todayYmd();
  document.querySelector('#journalStatus').textContent = `Unsaved changes for ${date}.`;
}
function updateJournalControlState(journal) {
  const settings = journal?.settings || {enabled: true, reminderMode: 'evening'};
  const enabled = settings.enabled !== false;
  const enabledInput = document.querySelector('#journalEnabled');
  const reminderInput = document.querySelector('#journalReminderMode');
  const reminderState = document.querySelector('#journalReminderState');
  enabledInput.checked = enabled;
  reminderInput.value = settings.reminderMode || 'evening';
  reminderInput.disabled = !enabled;
  if (!enabled) {
    reminderState.className = 'journal-reminder muted';
    reminderState.textContent = 'Journaling reminders are off. You can still write manually.';
    return;
  }
  if (journal?.due && journal.dueDate) {
    reminderState.className = 'journal-reminder due';
    reminderState.innerHTML = `<strong>${escapeHtml(journal.dueLabel || 'Journal')}</strong><br>${escapeHtml(journal.dueMessage || 'Take a few minutes to journal.')}<br><button type="button" onclick="openJournalDate('${escapeTextAttr(journal.dueDate)}')">Open ${escapeHtml(journal.dueLabel || 'journal')}</button>`;
    return;
  }
  reminderState.className = 'journal-reminder muted';
  reminderState.textContent = settings.reminderMode === 'next_morning'
    ? 'Reminder is set for the beginning of the next day, about the previous day.'
    : 'Reminder is set for the evening, between 8 PM and 10 PM.';
}
function normalizeReminderTime(value) {
  const match = String(value || '').trim().match(/^([0-2][0-9]):([0-5][0-9])$/);
  if (!match) return '';
  const hour = Number(match[1]);
  const minute = Number(match[2]);
  if (hour > 23 || minute > 59) return '';
  return `${String(hour).padStart(2, '0')}:${String(minute).padStart(2, '0')}`;
}
async function loadJournalTaskReminders() {
  try {
    journalTaskReminders = await fetch('/api/journal/reminders').then(r => r.json());
  } catch {
    journalTaskReminders = [];
  }
  renderJournalTaskReminders();
}
async function addJournalTaskReminder() {
  const taskInput = document.querySelector('#journalReminderTask');
  const timeInput = document.querySelector('#journalReminderTime');
  const status = document.querySelector('#journalReminderTaskStatus');
  const task = taskInput.value.trim();
  const time = normalizeReminderTime(timeInput.value);
  if (!task) {
    status.textContent = 'Enter a reminder task.';
    taskInput.focus();
    return;
  }
  if (!time) {
    status.textContent = 'Enter time as HH:MM in 24-hour format.';
    timeInput.focus();
    return;
  }
  journalTaskReminders = await fetch('/api/journal/reminders/add', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({task, time})
  }).then(r => r.json());
  taskInput.value = '';
  timeInput.value = '';
  status.textContent = `Reminder added for ${time}.`;
  renderJournalTaskReminders();
}
async function removeJournalTaskReminder(id) {
  journalTaskReminders = await fetch(`/api/journal/reminders/remove?id=${encodeURIComponent(id)}`).then(r => r.json());
  document.querySelector('#journalReminderTaskStatus').textContent = 'Reminder removed.';
  renderJournalTaskReminders();
}
function removeJournalTaskReminderFromButton(button) {
  removeJournalTaskReminder(button.dataset.id || '');
}
function renderJournalTaskReminders() {
  const list = document.querySelector('#journalReminderList');
  const status = document.querySelector('#journalReminderTaskStatus');
  if (!list) return;
  list.innerHTML = (journalTaskReminders || []).map(reminder => `
    <span class="journal-reminder-chip">
      <strong>${escapeHtml(reminder.time || '')}</strong>
      <span>${escapeHtml(reminder.task || '')}</span>
      <button type="button" data-id="${escapeTextAttr(reminder.id || '')}" onclick="removeJournalTaskReminderFromButton(this)" aria-label="Remove reminder ${escapeTextAttr(reminder.task || '')}">x</button>
    </span>
  `).join('');
  if (!journalTaskReminders.length) {
    list.innerHTML = '<div class="muted">No task reminders yet.</div>';
    if (status) status.textContent = 'No reminder added yet.';
  }
}
async function addBlock() {
  const input = document.querySelector('#blockKeyword');
  const fullModeInput = document.querySelector('#fullBlock');
  const passwordModeInput = document.querySelector('#passwordBlock');
  const passwordInput = document.querySelector('#blockPassword');
  const keyword = encodeURIComponent(input.value || '');
  const passwordMode = Boolean(passwordModeInput && passwordModeInput.checked);
  const mode = encodeURIComponent(passwordMode ? 'password' : 'full');
  const password = encodeURIComponent(passwordInput.value || '');
  const original = encodeURIComponent(editingBlockTarget || '');
  if (!keyword) return;
  if (passwordMode && !passwordInput.value) {
    passwordInput.focus();
    return;
  }
  await fetch(`/api/block/add?keyword=${keyword}&mode=${mode}&password=${password}&original=${original}`);
  editingBlockTarget = '';
  input.value = '';
  passwordInput.value = '';
  if (fullModeInput) fullModeInput.checked = true;
  if (passwordModeInput) passwordModeInput.checked = false;
  syncBlockMode();
  syncBlockEditState();
  refresh();
}
function selectBlockRule(target) {
  const rule = blockedRules.find(item => item.target === target);
  if (!rule) return;
  editingBlockTarget = rule.target || '';
  const input = document.querySelector('#blockKeyword');
  const passwordInput = document.querySelector('#blockPassword');
  input.value = rule.target || '';
  passwordInput.value = '';
  passwordInput.placeholder = rule.mode === 'password' && rule.hasPassword ? 'Enter password to update this block' : 'Enter password to continue when blocked';
  setBlockMode(rule.mode === 'password' ? 'password' : 'full');
  syncBlockEditState();
  input.focus();
}
function editBlockFromButton(button) {
  selectBlockRule(button.dataset.target || '');
}
function normalizedBlockValue(value) {
  return String(value || '').trim().toLowerCase();
}
function syncBlockEditState() {
  const input = document.querySelector('#blockKeyword');
  const button = document.querySelector('#blockSubmit');
  if (!input || !button) return;
  const current = normalizedBlockValue(input.value);
  const selectedExists = editingBlockTarget && blockedRules.some(rule => rule.target === editingBlockTarget);
  const typedRule = blockedRules.find(rule => rule.target === current);
  if (!selectedExists && typedRule) editingBlockTarget = typedRule.target;
  if (selectedExists && current !== editingBlockTarget) editingBlockTarget = '';
  button.textContent = editingBlockTarget ? 'Edit block' : 'Add block';
  document.querySelectorAll('.blocked-chip').forEach(chip => {
    chip.classList.toggle('editing', chip.dataset.target === editingBlockTarget);
  });
}
function setBlockMode(mode) {
  const fullModeInput = document.querySelector('#fullBlock');
  const passwordModeInput = document.querySelector('#passwordBlock');
  if (mode === 'password') {
    if (fullModeInput) fullModeInput.checked = false;
    if (passwordModeInput) passwordModeInput.checked = true;
  } else {
    if (fullModeInput) fullModeInput.checked = true;
    if (passwordModeInput) passwordModeInput.checked = false;
  }
  syncBlockMode();
}
function syncBlockMode() {
  const fullModeInput = document.querySelector('#fullBlock');
  const passwordModeInput = document.querySelector('#passwordBlock');
  const fullOption = document.querySelector('#fullBlockOption');
  const passwordOption = document.querySelector('#passwordBlockOption');
  const passwordField = document.querySelector('#blockPasswordField');
  const passwordInput = document.querySelector('#blockPassword');
  const modeHint = document.querySelector('#blockModeHint');
  let passwordMode = Boolean(passwordModeInput && passwordModeInput.checked);
  if (!passwordMode && fullModeInput && !fullModeInput.checked) {
    fullModeInput.checked = true;
  }
  if (passwordMode && fullModeInput) fullModeInput.checked = false;
  if (fullOption) fullOption.classList.toggle('selected', !passwordMode);
  if (passwordOption) passwordOption.classList.toggle('selected', passwordMode);
  if (passwordField) passwordField.classList.toggle('password-hidden', !passwordMode);
  if (modeHint) modeHint.textContent = passwordMode ? 'Password block is active.' : 'Full block is active.';
  if (passwordInput) {
    passwordInput.required = passwordMode;
    if (!passwordMode) passwordInput.value = '';
  }
}
async function removeBlock(target) {
  await fetch(`/api/block/remove?keyword=${encodeURIComponent(target)}`);
  if (editingBlockTarget === target) {
    editingBlockTarget = '';
    document.querySelector('#blockKeyword').value = '';
    document.querySelector('#blockPassword').value = '';
    setBlockMode('full');
  }
  refresh();
}
function removeBlockFromButton(button) {
  removeBlock(button.dataset.target || '');
}
function qrDeviceRowMarkup(device) {
  const endpoint = device.endpoint || '';
  const note = endpoint.startsWith('browser:')
    ? 'Receiver browser connected from QR.'
    : 'Mobile app connected from QR.';
  const kind = device.kind || 'device';
  return `<div class="device-pill"><strong>${escapeHtml(device.name || 'Device')}</strong><br><span class="muted">${escapeHtml(deviceKindLabel(kind))}<br>${escapeHtml(note)}</span></div>`;
}
function deviceKindLabel(kind) {
  const labels = {phone:'Phone', tv:'TV', tablet:'Tablet', laptop:'Laptop', desktop:'Desktop', router:'Router', device:'Device'};
  return labels[kind] || 'Device';
}
function openDeviceQrPanel(kind = 'install') {
  document.querySelector('#deviceQrPanel').classList.remove('hidden');
  renderDeviceQr(kind);
}
function renderDeviceQr(kind = 'install') {
  activeDeviceQrKind = kind;
  const option = deviceQrOption(kind);
  ['install', 'android', 'iphone', 'laptop', 'receiver'].forEach(name => {
    const button = document.querySelector(`#qr${name[0].toUpperCase()}${name.slice(1)}Button`);
    if (button) button.classList.toggle('active', name === kind);
  });
  document.querySelector('#deviceQrTitle').textContent = option.title;
  document.querySelector('#deviceQrHint').textContent = option.hint;
  const link = document.querySelector('#deviceQrLink');
  link.href = option.url;
  link.textContent = `Open ${option.url}`;
  const plainUrl = document.querySelector('#deviceQrPlainUrl');
  if (plainUrl) plainUrl.textContent = option.url;
  const qrSrc = `/api/qr.svg?value=${encodeURIComponent(option.url)}&label=${encodeURIComponent(option.title)}`;
  document.querySelector('#deviceQrCode').innerHTML = `<img src="${qrSrc}" alt="${escapeTextAttr(option.title)} QR code" width="410" height="410">`;
}
async function copyDeviceQrUrl() {
  const value = document.querySelector('#deviceQrPlainUrl')?.textContent || '';
  if (!value) return;
  try {
    await navigator.clipboard.writeText(value);
    document.querySelector('#deviceQrHint').textContent = `Copied. Open this URL on the device: ${value}`;
  } catch {
    const range = document.createRange();
    range.selectNodeContents(document.querySelector('#deviceQrPlainUrl'));
    const selection = window.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
  }
}
function deviceQrOption(kind) {
  const installUrl = deviceQrUrls.install || `${location.origin}/connect`;
  const receiverUrl = deviceQrUrls.receiver || document.querySelector('#deviceConnectUrl')?.textContent || `${location.origin}/device`;
  const androidUrl = deviceQrUrls.android || `${location.origin}/download/local-focus-mobile.apk`;
  const macUrl = deviceQrUrls.mac || `${location.origin}/download/local-focus-macos.dmg`;
  const options = {
    install: {
      title: 'Choose install or receiver option',
      hint: 'Scan from any device. Android can download the APK, Mac can download the DMG, and iPhone/iPad can connect as a receiver. Native iPhone install requires Xcode, TestFlight, or App Store signing.',
      url: installUrl
    },
    android: {
      title: 'Android phone or tablet app',
      hint: 'Scan from an Android phone or tablet to download the Local Focus APK. After installing, open Tracking and allow Usage Access.',
      url: androidUrl
    },
    iphone: {
      title: 'iPhone or iPad receiver',
      hint: 'Scan from iPhone or iPad to connect as a receiver. This does not install a native iPhone app; native install requires Xcode, TestFlight, or App Store signing.',
      url: receiverUrl
    },
    laptop: {
      title: 'Mac laptop app',
      hint: 'Scan from a Mac to download the Local Focus DMG. Other laptops can use the receiver link.',
      url: macUrl
    },
    receiver: {
      title: 'Receiver connection',
      hint: 'Scan from any phone, tablet, TV, or laptop browser to receive Local Focus alerts from this machine.',
      url: receiverUrl
    }
  };
  return options[kind] || options.install;
}
function saveFocusDraft() {
  localStorage.setItem(focusDraftKey, JSON.stringify({
    target: document.querySelector('#target').value,
    task: document.querySelector('#task').value,
    minutes: document.querySelector('#minutes').value,
    alertMinutes: document.querySelector('#alertMinutes').value,
    alertAction: document.querySelector('#alertAction').value,
    alertMessage: document.querySelector('#alertMessage').value,
    redirectApp: document.querySelector('#redirectApp').value
  }));
}
function restoreFocusDraft() {
  try {
    const draft = JSON.parse(localStorage.getItem(focusDraftKey) || '{}');
    if (draft.target) setFocusTargets(draft.target);
    if (draft.task) document.querySelector('#task').value = draft.task;
    if (draft.minutes) document.querySelector('#minutes').value = draft.minutes;
    if (draft.alertMinutes) document.querySelector('#alertMinutes').value = draft.alertMinutes;
    if (draft.alertAction) document.querySelector('#alertAction').value = draft.alertAction;
    if (draft.alertMessage) document.querySelector('#alertMessage').value = draft.alertMessage;
    if (draft.redirectApp) document.querySelector('#redirectApp').value = draft.redirectApp;
  } catch {}
  ['#task', '#minutes', '#alertMinutes', '#alertAction', '#alertMessage', '#redirectApp'].forEach(selector => {
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
  focusTargets = String(value || '').split(/[,\n]/).map(item => item.trim()).filter(Boolean).slice(0, MAX_FOCUS_TARGETS);
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
async function saveActiveFocusTargets() {
  if (!activeFocusSession) return;
  const target = document.querySelector('#target').value || '';
  activeFocusSession = {...activeFocusSession, target};
  await fetch(`/api/focus/targets?target=${encodeURIComponent(target)}`);
  refresh();
}
function addFocusTarget() {
  const input = document.querySelector('#targetInput');
  const value = input.value.trim();
  if (!value || focusTargets.length >= MAX_FOCUS_TARGETS) return;
  for (const target of value.split(/[,\n]/).map(item => item.trim()).filter(Boolean)) {
    if (focusTargets.length >= MAX_FOCUS_TARGETS) break;
    if (!focusTargets.some(existing => existing.toLowerCase() === target.toLowerCase())) {
      focusTargets.push(target);
    }
  }
  input.value = '';
  syncFocusTargets();
  saveActiveFocusTargets();
}
function removeFocusTarget(index) {
  focusTargets.splice(index, 1);
  syncFocusTargets();
  saveActiveFocusTargets();
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
    moveDistractionCard(true);
  } catch (error) {
    panel.innerHTML = `<div class="report-head"><p class="muted">Could not generate report.</p><button class="report-close" onclick="closeFocusReport()" aria-label="Close report">X</button></div>`;
    panel.classList.add('open');
    moveDistractionCard(true);
  }
}
function closeFocusReport() {
  const panel = document.querySelector('#focusReportPanel');
  panel.classList.remove('open');
  panel.innerHTML = '';
  moveDistractionCard(false);
}
function moveDistractionCard(afterReport) {
  const card = document.querySelector('#distractionCard');
  const reportsCard = document.querySelector('#reportsCard');
  reportsCard.insertAdjacentElement('afterend', card);
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
  document.querySelector('#yearReportButton').classList.toggle('active-year', activeReportPeriod === 'year' && activeReportYear === monthStart.getFullYear());
  document.querySelector('#monthReportButton').classList.toggle('active-report', activeReportPeriod === 'month' && activeReportYear === monthStart.getFullYear() && activeReportMonth === monthStart.getMonth());
  document.querySelector('#selectedWeekButton').classList.toggle('active-report', activeReportPeriod === 'week' && activeReportWeek === isoWeekNumber(selectedReportDate));
  const labels = ['Week', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
  let html = labels.map(label => `<div class="calendar-label">${label}</div>`).join('');
  for (let row = 0; row < 6; row += 1) {
    const weekDate = new Date(gridStart);
    weekDate.setDate(gridStart.getDate() + row * 7);
    const weekActive = activeReportPeriod === 'week' && activeReportWeek === isoWeekNumber(weekDate);
    html += `<button type="button" class="week-button ${weekActive ? 'active-report' : ''}" onclick="selectCalendarWeek(${weekDate.getFullYear()}, ${weekDate.getMonth()}, ${weekDate.getDate()})">W${isoWeekNumber(weekDate)}</button>`;
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
  setActiveCalendarScope('day', selectedReportDate);
  renderReportCalendar();
  runCalendarReport('day', selectedReportDate);
}
function selectCalendarWeek(year, month, day) {
  selectedReportDate = new Date(year, month, day);
  calendarDate = new Date(year, month, 1);
  setActiveCalendarScope('week', selectedReportDate);
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
    const dateValue = new Date(calendarDate.getFullYear(), 0, 1);
    setActiveCalendarScope('year', dateValue);
    runCalendarReport('year', dateValue);
  } else if (period === 'month') {
    const dateValue = new Date(calendarDate.getFullYear(), calendarDate.getMonth(), 1);
    setActiveCalendarScope('month', dateValue);
    runCalendarReport('month', dateValue);
  } else if (period === 'week') {
    setActiveCalendarScope('week', selectedReportDate);
    runCalendarReport('week', selectedReportDate);
  } else if (period === 'day') {
    setActiveCalendarScope('day', selectedReportDate);
    runCalendarReport('day', selectedReportDate);
  }
  renderReportCalendar();
}
function setActiveCalendarScope(period, dateValue) {
  activeReportPeriod = period;
  activeReportYear = dateValue.getFullYear();
  activeReportMonth = dateValue.getMonth();
  activeReportWeek = isoWeekNumber(dateValue);
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
  const productivityChart = renderProductivityChart(report);
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
    <div class="report-head"><div><h2>${reportTitle}</h2><span class="muted">Since ${new Date(report.windowStart * 1000).toLocaleString([], {dateStyle:'short', timeStyle:'short'})} - generated ${new Date(report.generatedAt * 1000).toLocaleString([], {dateStyle:'short', timeStyle:'short'})}</span></div><button class="report-close" onclick="closeFocusReport()" aria-label="Close report">X</button></div>
    <div class="report-grid">
      <div class="report-card"><span class="muted">Total time</span><strong>${formatDuration(total)}</strong></div>
      <div class="report-card"><span class="muted">Matched focus list</span><strong>${formatDuration(report.focusSeconds)}</strong></div>
      <div class="report-card"><span class="muted">Outside focus</span><strong>${formatDuration(report.outsideSeconds)}</strong></div>
      <div class="report-card"><span class="muted">Idle</span><strong>${formatDuration(report.idleSeconds)}</strong></div>
    </div>
    <div class="report-card"><h3>Time on focus apps and websites</h3><div class="target-list">${targetBars || '<p class="muted">No target activity yet.</p>'}</div></div>
    <div class="report-card"><h3>${productivityChart.title}</h3><div class="muted">${productivityChart.hint}</div>${productivityChart.html}<div id="hourDetail" class="hour-detail"></div></div>
    <div class="report-two">
      <div class="report-card">
        <h3>Focus split</h3>
        <div class="split-chart" style="--focus-angle:${focusAngle}"><span>${report.focusPercent}% focused</span></div>
      </div>
      <div class="report-card"><h3>Analysis</h3><div class="insights">${insights}</div></div>
    </div>
    <div class="report-card"><h3>Top outside-focus activity</h3>${distractionRows}</div>`;
}
function renderProductivityChart(report) {
  const period = report.period || 'day';
  const buckets = productivityBuckets(report);
  const maxBucket = Math.max(1, ...buckets.map(item => item.productiveSeconds + item.distractingSeconds + (item.idleSeconds || 0)));
  const title = period === 'year'
    ? 'Productive vs distracted by month'
    : period === 'month' || period === 'week'
      ? 'Productive vs distracted by day'
      : 'Productive vs distracted by hour';
  const hint = period === 'year'
    ? 'Click a month bar to open that month report.'
    : period === 'month' || period === 'week'
      ? 'Click a day bar to open that day report.'
      : 'Click an hour bar to see what happened in that hour.';
  const html = buckets.map(item => {
    const total = item.productiveSeconds + item.distractingSeconds + (item.idleSeconds || 0);
    const productiveHeight = Math.max(total ? 2 : 0, item.productiveSeconds * 100 / maxBucket);
    const distractingHeight = Math.max(total ? 2 : 0, item.distractingSeconds * 100 / maxBucket);
    const idleHeight = Math.max(total ? 2 : 0, (item.idleSeconds || 0) * 100 / maxBucket);
    const click = item.kind === 'hour'
      ? `showHourDetails(${item.startSeconds}, this)`
      : `drillIntoReport('${item.nextPeriod}', ${item.startSeconds})`;
    const buttonClass = item.kind === 'hour' ? 'hour-click' : 'period-click';
    return `<div>
      <button type="button" class="${buttonClass}" onclick="${click}" aria-label="${escapeTextAttr(item.ariaLabel)}">
      <div class="hour-bar">
        <div class="hour-segment hour-good" data-tip="Productive: ${formatDuration(item.productiveSeconds)} (${escapeTextAttr(item.rangeLabel)})" aria-label="Productive: ${formatDuration(item.productiveSeconds)} (${escapeTextAttr(item.rangeLabel)})" style="height:${productiveHeight}%"></div>
        <div class="hour-segment" data-tip="Idle: ${formatDuration(item.idleSeconds || 0)} (${escapeTextAttr(item.rangeLabel)})" aria-label="Idle: ${formatDuration(item.idleSeconds || 0)} (${escapeTextAttr(item.rangeLabel)})" style="background:var(--warn);height:${idleHeight}%"></div>
        <div class="hour-segment hour-bad" data-tip="Distracted: ${formatDuration(item.distractingSeconds)} (${escapeTextAttr(item.rangeLabel)})" aria-label="Distracted: ${formatDuration(item.distractingSeconds)} (${escapeTextAttr(item.rangeLabel)})" style="height:${distractingHeight}%"></div>
      </div>
      </button>
      <div class="muted" style="font-size:11px;text-align:center">${escapeHtml(item.label)}</div>
    </div>`;
  }).join('');
  return { title, hint, html: html ? `<div class="period-bars">${html}</div>` : '<p class="muted">No productivity data yet.</p>' };
}
function productivityBuckets(report) {
  const period = report.period || 'day';
  const start = new Date((report.windowStart || 0) * 1000);
  const hourly = report.hourly || [];
  if (period === 'year') {
    return Array.from({length: 12}, (_, month) => {
      const bucketStart = new Date(start.getFullYear(), month, 1);
      const bucketEnd = new Date(start.getFullYear(), month + 1, 1);
      return aggregateProductivityBucket(hourly, bucketStart, bucketEnd, {
        kind: 'month',
        nextPeriod: 'month',
        label: bucketStart.toLocaleDateString([], {month:'short'}),
        rangeLabel: bucketStart.toLocaleDateString([], {month:'long', year:'numeric'}),
        ariaLabel: `Open ${bucketStart.toLocaleDateString([], {month:'long', year:'numeric'})} report`
      });
    });
  }
  if (period === 'month') {
    const monthStart = new Date(start.getFullYear(), start.getMonth(), 1);
    const nextMonth = new Date(start.getFullYear(), start.getMonth() + 1, 1);
    const days = Math.round((nextMonth - monthStart) / 86400000);
    return Array.from({length: days}, (_, index) => {
      const bucketStart = new Date(monthStart);
      bucketStart.setDate(monthStart.getDate() + index);
      const bucketEnd = new Date(bucketStart);
      bucketEnd.setDate(bucketStart.getDate() + 1);
      return aggregateProductivityBucket(hourly, bucketStart, bucketEnd, {
        kind: 'day',
        nextPeriod: 'day',
        label: String(bucketStart.getDate()),
        rangeLabel: bucketStart.toLocaleDateString([], {weekday:'short', month:'short', day:'numeric'}),
        ariaLabel: `Open ${bucketStart.toLocaleDateString([], {weekday:'long', month:'long', day:'numeric'})} report`
      });
    });
  }
  if (period === 'week') {
    return Array.from({length: 7}, (_, index) => {
      const bucketStart = new Date(start);
      bucketStart.setDate(start.getDate() + index);
      const bucketEnd = new Date(bucketStart);
      bucketEnd.setDate(bucketStart.getDate() + 1);
      return aggregateProductivityBucket(hourly, bucketStart, bucketEnd, {
        kind: 'day',
        nextPeriod: 'day',
        label: bucketStart.toLocaleDateString([], {weekday:'short'}),
        rangeLabel: bucketStart.toLocaleDateString([], {weekday:'short', month:'short', day:'numeric'}),
        ariaLabel: `Open ${bucketStart.toLocaleDateString([], {weekday:'long', month:'long', day:'numeric'})} report`
      });
    });
  }
  return Array.from({length: 24}, (_, index) => {
    const bucketStart = new Date(start);
    bucketStart.setHours(index, 0, 0, 0);
    const bucketEnd = new Date(bucketStart);
    bucketEnd.setHours(bucketStart.getHours() + 1);
    return aggregateProductivityBucket(hourly, bucketStart, bucketEnd, {
      kind: 'hour',
      nextPeriod: 'hour',
      label: bucketStart.toLocaleTimeString([], {hour:'numeric'}),
      rangeLabel: `${bucketStart.toLocaleTimeString([], {hour:'numeric'})} to ${bucketEnd.toLocaleTimeString([], {hour:'numeric'})}`,
      ariaLabel: `Show details for ${bucketStart.toLocaleTimeString([], {hour:'numeric'})}`
    });
  });
}
function aggregateProductivityBucket(hourly, bucketStart, bucketEnd, meta) {
  const startSeconds = Math.floor(bucketStart.getTime() / 1000);
  const endSeconds = Math.floor(bucketEnd.getTime() / 1000);
  const totals = hourly.reduce((acc, item) => {
    if (item.hour >= startSeconds && item.hour < endSeconds) {
      acc.productiveSeconds += item.productiveSeconds || 0;
      acc.distractingSeconds += item.distractingSeconds || 0;
      acc.idleSeconds += item.idleSeconds || 0;
    }
    return acc;
  }, {productiveSeconds: 0, distractingSeconds: 0, idleSeconds: 0});
  return {...meta, ...totals, startSeconds, endSeconds};
}
function drillIntoReport(period, startSeconds) {
  const dateValue = new Date(startSeconds * 1000);
  selectedReportDate = new Date(dateValue);
  calendarDate = new Date(dateValue.getFullYear(), dateValue.getMonth(), 1);
  setActiveCalendarScope(period, dateValue);
  renderReportCalendar();
  runCalendarReport(period, dateValue);
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
  activeFocusSession = state.focus || null;
  const stopBanner = document.querySelector('#stopBanner');
  if (stopBanner) stopBanner.style.display = state.stopped ? 'flex' : 'none';
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
  blockedRules = (state.blockedRules || []).map(rule => ({...rule, target: normalizedBlockValue(rule.target || '')}));
  document.querySelector('#blockedList').innerHTML = blockedRules.map(rule => `<span class="blocked-chip${rule.target === editingBlockTarget ? ' editing' : ''}" data-target="${escapeTextAttr(rule.target || '')}">${escapeHtml(shortenSource(rule.target || ''))} <small>${rule.mode === 'password' ? 'password' : 'full'}</small><button class="edit-chip" type="button" data-target="${escapeTextAttr(rule.target || '')}" onclick="editBlockFromButton(this)" aria-label="Edit ${escapeTextAttr(rule.target || '')}">edit</button><button type="button" data-target="${escapeTextAttr(rule.target || '')}" onclick="removeBlockFromButton(this)" aria-label="Remove ${escapeTextAttr(rule.target || '')}">x</button></span>`).join('') || '<div class="muted">No blocked apps or sites yet.</div>';
  syncBlockEditState();
  document.querySelector('#deviceConnectUrl').textContent = state.deviceConnectUrl || 'http://127.0.0.1:4799/device';
  deviceQrUrls = {
    install: state.deviceInstallUrl || `${location.origin}/connect`,
    receiver: state.deviceConnectUrl || `${location.origin}/device`,
    android: state.androidAppUrl || `${location.origin}/download/local-focus-mobile.apk`,
    mac: state.macAppUrl || `${location.origin}/download/local-focus-macos.dmg`
  };
  if (!document.querySelector('#deviceQrPanel').classList.contains('hidden')) renderDeviceQr(activeDeviceQrKind);
  const qrDevices = (state.devices || []).filter(device => String(device.endpoint || '').startsWith('browser:') || String(device.endpoint || '').startsWith('mobile:'));
  document.querySelector('#deviceList').innerHTML = qrDevices.map(qrDeviceRowMarkup).join('') || '<div class="muted">No QR-connected devices yet.</div>';
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
  updateFocusButtons(state.focus, state.stopped);
  seedFocusInputsFromActiveSession(state.focus);
  updateFocusSummary(state.focus);
  if (state.stopped) {
    const chip = document.querySelector('#focusState');
    chip.textContent = 'Stopped';
    chip.className = 'status-chip paused';
  }
  updateJournalControlState(state.journal);
}
function updateFocusSummary(focus) {
  const chip = document.querySelector('#focusState');
  const details = document.querySelector('#focusDetails');
  const quickTask = document.querySelector('#quickTask');
  const quickStatus = document.querySelector('#quickStatus');
  const quickDelay = document.querySelector('#quickDelay');
  const quickAction = document.querySelector('#quickAction');
  updateHighFocusControls(focus);
  if (!focus) {
    chip.textContent = 'Focus off';
    chip.className = 'status-chip';
    details.innerHTML = `<div class="detail-grid">
      <div class="detail-card"><span>Focus apps/sites</span><strong>None active</strong></div>
      <div class="detail-card"><span>Warning</span><strong>Off</strong></div>
      <div class="detail-card"><span>Action</span><strong>Start focus to enable alerts</strong></div>
    </div>`;
    quickTask.textContent = 'None';
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
  const alertMessage = focus.alertMessage || DEFAULT_ALERT_MESSAGE_TEMPLATE;
  const targets = String(focus.target || '').split(/[,\n]/).map(value => value.trim()).filter(Boolean);
  const targetChips = targets.map(value => `<span class="target-chip">${escapeHtml(shortenSource(value))}</span>`).join('') || '<span class="target-chip">No target set</span>';
  details.innerHTML = `
    <div class="target-chips">${targetChips}</div>
    <div class="detail-grid">
      <div class="detail-card"><span>Full focus list</span><strong>${escapeHtml(focus.target || 'No target set')}</strong></div>
      <div class="detail-card"><span>Warning delay</span><strong>${formatDuration(focus.alertDelaySeconds || 60)} outside focus</strong></div>
      <div class="detail-card"><span>Notification action</span><strong>${escapeHtml(action)}</strong></div>
      <div class="detail-card"><span>Alert message</span><strong>${escapeHtml(alertMessage)}</strong></div>
    </div>`;
  quickTask.textContent = focus.task || 'Focus session';
  quickStatus.textContent = paused ? 'Paused' : 'Active';
  quickDelay.textContent = formatDuration(focus.alertDelaySeconds || 60);
  quickAction.textContent = focus.alertAction === 'switch' && focus.redirectApp ? `Move` : 'Alert';
  if (!focusEditorManuallyOpened) setFocusEditorOpen(false);
}
function updateHighFocusControls(focus) {
  const checkbox = document.querySelector('#highFocusMode');
  if (!checkbox) return;
  const targets = String(focus?.target || '').split(/[,\n]/).map(value => value.trim()).filter(Boolean);
  checkbox.checked = Boolean(focus?.highFocusMode);
  checkbox.disabled = !focus || Boolean(focus.paused) || targets.length === 0;
  checkbox.title = !focus
    ? 'Start a focus session first.'
    : targets.length === 0
      ? 'Add focus apps or websites before enabling High Focus mode.'
      : checkbox.disabled
        ? 'Resume focus to change High Focus mode.'
        : 'Block every active app or website outside the focus list.';
}
function seedFocusInputsFromActiveSession(focus) {
  if (!focus) return;
  const taskInput = document.querySelector('#task');
  const targetInput = document.querySelector('#target');
  const minutesInput = document.querySelector('#minutes');
  const alertInput = document.querySelector('#alertMinutes');
  const actionInput = document.querySelector('#alertAction');
  const messageInput = document.querySelector('#alertMessage');
  const redirectInput = document.querySelector('#redirectApp');
  if (focus.task) taskInput.value = focus.task;
  if (focus.target && targetInput.value !== focus.target) setFocusTargets(focus.target);
  if (focus.durationMinutes) minutesInput.value = focus.durationMinutes;
  if (focus.alertDelaySeconds) alertInput.value = Math.max(1, Math.round(focus.alertDelaySeconds / 60));
  if (focus.alertAction) actionInput.value = focus.alertAction;
  messageInput.value = focus.alertMessage || DEFAULT_ALERT_MESSAGE_TEMPLATE;
  redirectInput.value = focus.redirectApp || '';
  saveFocusDraft();
}
function updateFocusButtons(focus, stopped) {
  const startButton = document.querySelector('#startFocus');
  const pauseButton = document.querySelector('#pauseFocus');
  const stopButton = document.querySelector('#stopFocus');
  const running = Boolean(focus && !focus.paused);
  const paused = Boolean(focus && focus.paused);
  startButton.className = `focus-btn ${running ? 'focus-running' : 'focus-idle'}`;
  startButton.textContent = stopped ? 'Start' : paused ? 'Restart focus' : running ? 'Focus' : 'Start focus';
  pauseButton.disabled = !focus || Boolean(stopped);
  pauseButton.className = `focus-btn ${paused ? 'focus-paused' : running ? 'focus-running' : ''}`;
  pauseButton.textContent = paused ? 'Resume' : 'Pause';
  // Stop is the master off switch: available whenever the app is running,
  // even without an active focus session, and disabled once already stopped.
  stopButton.disabled = Boolean(stopped);
  stopButton.className = `focus-btn ${stopped ? '' : 'focus-stop-active'}`;
  stopButton.title = 'Stop all tracking, blocking, alerts, and reminders until you resume';
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
function escapeTextAttr(value) {
  return String(value).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#039;'}[c]));
}
restoreFocusDraft();
syncBlockMode();
openJournalDate(todayYmd());
loadJournalTaskReminders();
activeReportWeek = isoWeekNumber(selectedReportDate);
renderReportCalendar();
setFocusTaskWindow('day', calendarPeriodWindow('day', selectedReportDate));
refresh();
setInterval(refresh, 10000);
</script>
</body>
</html>"#
        .into()
}

fn connect_device_html() -> String {
    let lan_url = local_network_url().unwrap_or_else(|| "http://127.0.0.1:4799".into());
    let android_url = format!("{lan_url}/download/local-focus-mobile.apk");
    let mac_url = format!("{lan_url}/download/local-focus-macos.dmg");
    let receiver_url = format!("{lan_url}/device");
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Connect Local Focus</title>
<style>
:root {{ color-scheme:light dark; --bg:#f6f7f2; --ink:#202124; --muted:#686b63; --line:#d9dbd2; --panel:#ffffff; --good:#24734d; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg:#121512; --ink:#f1f1e9; --muted:#aeb0a8; --line:#34362f; --panel:#22231f; }} }}
* {{ box-sizing:border-box; }}
body {{ margin:0; font:16px/1.45 system-ui, -apple-system, Segoe UI, sans-serif; background:var(--bg); color:var(--ink); }}
main {{ max-width:720px; margin:0 auto; padding:22px; display:grid; gap:14px; }}
section {{ background:var(--panel); border:1px solid var(--line); border-radius:12px; padding:16px; display:grid; gap:10px; }}
h1, h2, p {{ margin:0; }}
h1 {{ font-size:24px; }}
h2 {{ font-size:17px; }}
.muted {{ color:var(--muted); }}
.actions {{ display:grid; gap:10px; }}
a.button {{ display:block; text-align:center; text-decoration:none; border:1px solid var(--good); background:var(--good); color:white; border-radius:10px; padding:13px; font-weight:850; }}
a.secondary {{ border-color:var(--line); background:transparent; color:var(--ink); }}
code {{ overflow-wrap:anywhere; }}
</style>
</head>
<body>
<main>
  <section>
    <h1>Connect Local Focus</h1>
    <p class="muted">This QR page connects only the device that opens this exact link. Local Focus does not scan for nearby devices.</p>
    <p><code>{lan_url}</code></p>
  </section>
  <section>
    <h2>Android phone or tablet</h2>
    <p class="muted">Download the installable APK. After installing, open Local Focus, connect, and allow Usage Access for app tracking.</p>
    <div class="actions"><a class="button" href="{android_url}">Download Android app</a></div>
  </section>
  <section>
    <h2>iPhone or iPad receiver</h2>
    <p class="muted">This QR cannot install a native iPhone app. Apple requires Xcode, TestFlight, App Store, or a signed enterprise/ad-hoc package for iOS app installation. Use this receiver link now to receive Local Focus alerts.</p>
    <div class="actions"><a class="button secondary" href="{receiver_url}">Connect iPhone as receiver</a></div>
  </section>
  <section>
    <h2>Mac laptop</h2>
    <p class="muted">Download the Mac DMG from this laptop. Other computers can use the receiver link.</p>
    <div class="actions">
      <a class="button" href="{mac_url}">Download Mac app</a>
      <a class="button secondary" href="{receiver_url}">Open receiver link</a>
    </div>
  </section>
</main>
</body>
</html>"#
    )
}

fn device_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<link rel="manifest" href="/device-manifest.json">
<title>Local Focus Device</title>
<style>
:root { color-scheme:light dark; --bg:#f6f6f1; --ink:#202124; --muted:#686b63; --line:#d9dbd2; --good:#24734d; --bad:#a8323b; --panel:#ffffff; }
body { margin:0; font-family:ui-sans-serif, system-ui, sans-serif; background:var(--bg); color:var(--ink); }
main { max-width:620px; margin:0 auto; padding:24px; display:grid; gap:18px; }
section { background:var(--panel); border:1px solid var(--line); border-radius:12px; padding:18px; display:grid; gap:12px; }
h1, h2, p { margin:0; }
.muted { color:var(--muted); }
label { font-size:12px; font-weight:800; color:var(--muted); }
input, select, button { width:100%; box-sizing:border-box; border:1px solid var(--line); border-radius:9px; padding:12px; font:inherit; }
button { background:var(--good); color:white; font-weight:800; cursor:pointer; }
.row { display:grid; grid-template-columns:1fr 140px; gap:10px; }
.event { border-top:1px solid var(--line); padding-top:12px; }
.event strong { color:var(--bad); }
@media (max-width:560px) { .row { grid-template-columns:1fr; } }
</style>
</head>
<body>
<main>
  <section>
    <h1>Local Focus Device</h1>
    <p class="muted">Connect this phone, TV, tablet, or laptop from the QR link to receive Local Focus alerts. Local Focus does not scan for nearby devices.</p>
  </section>
  <section>
    <h2>Connect device</h2>
    <label for="name">Device name</label>
    <input id="name" placeholder="Mukesh phone">
    <div class="row">
      <div>
        <label for="kind">Device type</label>
        <select id="kind">
          <option value="phone">Phone</option>
          <option value="tv">TV</option>
          <option value="tablet">Tablet</option>
          <option value="laptop">Laptop</option>
          <option value="desktop">Desktop</option>
          <option value="device">Other</option>
        </select>
      </div>
      <button onclick="connectDevice()">Connect</button>
    </div>
    <p id="status" class="muted">Not connected yet.</p>
  </section>
  <section>
    <h2>Alerts</h2>
    <div id="events" class="muted">No alerts yet.</div>
  </section>
</main>
<script>
let since = Math.floor(Date.now() / 1000);
let connected = false;
let deviceEndpoint = '';
let serviceWorkerReady = null;
async function setupServiceWorker() {
  if (!('serviceWorker' in navigator)) return null;
  try {
    const registration = await navigator.serviceWorker.register('/device-sw.js');
    serviceWorkerReady = navigator.serviceWorker.ready;
    return registration;
  } catch (_) {
    return null;
  }
}
async function connectDevice() {
  const name = encodeURIComponent(document.querySelector('#name').value || 'Device');
  const kind = encodeURIComponent(document.querySelector('#kind').value || 'device');
  const registration = await setupServiceWorker();
  if ('Notification' in window && Notification.permission === 'default') {
    try { await Notification.requestPermission(); } catch (_) {}
  }
  const response = await fetch(`/api/device/register?name=${name}&kind=${kind}`).then(r => r.json());
  since = response.since || since;
  deviceEndpoint = response.endpoint || '';
  connected = true;
  const notificationState = registration && Notification.permission === 'granted' ? 'OS notifications enabled.' : 'Alerts will show on this page.';
  document.querySelector('#status').textContent = `Connected. ${notificationState}`;
}
async function pollEvents() {
  if (!connected) return;
  const events = await fetch(`/api/device/events?since=${since}&device=${encodeURIComponent(deviceEndpoint)}`).then(r => r.json()).catch(() => []);
  if (!events.length) return;
  since = Math.max(...events.map(event => event.timestamp || since), since);
  const list = document.querySelector('#events');
  list.className = '';
  list.innerHTML = events.reverse().map(event => `<div class="event"><strong>${escapeHtml(event.event || 'Alert')}</strong><p>${escapeHtml(event.message || '')}</p><p class="muted">${new Date((event.timestamp || 0) * 1000).toLocaleTimeString([], {hour:'2-digit', minute:'2-digit'})}</p></div>`).join('') + list.innerHTML;
  for (const event of events) {
    showDeviceNotification(event);
  }
}
async function showDeviceNotification(event) {
  if (!('Notification' in window) || Notification.permission !== 'granted') return;
  try {
    const registration = await (serviceWorkerReady || navigator.serviceWorker.ready);
    if (registration.active) {
      registration.active.postMessage({type:'focus-alert', title:'Local Focus', message:event.message || 'Focus alert'});
    } else {
      registration.showNotification('Local Focus', {body:event.message || 'Focus alert', tag:'local-focus-alert', renotify:true});
    }
  } catch (_) {}
}
function escapeHtml(value) {
  return String(value).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#039;'}[c]));
}
setInterval(pollEvents, 5000);
setupServiceWorker();
</script>
</body>
</html>"#
        .into()
}

fn device_service_worker_js() -> String {
    r#"self.addEventListener('install', event => {
  self.skipWaiting();
});
self.addEventListener('activate', event => {
  event.waitUntil(self.clients.claim());
});
self.addEventListener('message', event => {
  if (!event.data || event.data.type !== 'focus-alert') return;
  const title = event.data.title || 'Local Focus';
  const body = event.data.message || 'Focus alert';
  event.waitUntil(self.registration.showNotification(title, {
    body,
    tag: 'local-focus-alert',
    renotify: true,
    requireInteraction: true
  }));
});
self.addEventListener('notificationclick', event => {
  event.notification.close();
  event.waitUntil((async () => {
    const clients = await self.clients.matchAll({type:'window', includeUncontrolled:true});
    for (const client of clients) {
      if (client.url.includes('/device')) return client.focus();
    }
    return self.clients.openWindow('/device');
  })());
});
"#
    .into()
}

fn device_manifest_json() -> String {
    r##"{"name":"Local Focus Device","short_name":"Local Focus","start_url":"/device","display":"standalone","background_color":"#f6f6f1","theme_color":"#24734d"}"##.into()
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
        Ok(PathBuf::from(home)
            .join("AppData")
            .join("Local")
            .join(APP_NAME))
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
        Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join(APP_NAME))
    }
}

fn ensure_config(data_dir: &Path) -> io::Result<()> {
    let path = data_dir.join("config.txt");
    if path.exists() {
        return Ok(());
    }
    fs::write(
        path,
        "productive=code,terminal,editor,docs,figma,notion,calendar,github,jira,linear\n\
distracting=youtube,netflix,reddit,instagram,tiktok,x.com,twitter,facebook,game,steam\n\
blocked=\n\
devices=\n",
    )
}

fn load_config(data_dir: &Path) -> io::Result<Config> {
    let mut config = Config::default();
    let path = data_dir.join("config.txt");
    let content = fs::read_to_string(path)?;
    for line in content.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "productive" => config.productive_keywords = config_values(value, true),
            "distracting" => config.distracting_keywords = config_values(value, true),
            "blocked" => config.blocked_keywords = config_values(value, false),
            "devices" => config.network_devices = config_values(value, false),
            _ => {}
        }
    }
    Ok(config)
}

fn config_values(value: &str, lowercase: bool) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if lowercase {
                s.to_lowercase()
            } else {
                s.to_string()
            }
        })
        .collect()
}

fn parse_block_mode(value: &str) -> BlockMode {
    if value.trim().eq_ignore_ascii_case("password") {
        BlockMode::Password
    } else {
        BlockMode::Full
    }
}

fn block_mode_name(mode: BlockMode) -> &'static str {
    match mode {
        BlockMode::Full => "full",
        BlockMode::Password => "password",
    }
}

fn format_block_rule_record(target: &str, mode: BlockMode, password: &str) -> String {
    format!(
        "{}|{}|{}",
        target.trim().replace(['|', ','], " "),
        block_mode_name(mode),
        password.trim().replace(['|', ','], " ")
    )
}

fn parse_block_rule_record(record: &str) -> BlockRule {
    let mut parts = record.splitn(3, '|');
    let target = parts.next().unwrap_or("").trim().to_lowercase();
    let mode = parts
        .next()
        .map(parse_block_mode)
        .unwrap_or(BlockMode::Full);
    let password = parts.next().unwrap_or("").trim().to_string();
    BlockRule {
        target,
        mode,
        password,
    }
}

fn save_config(data_dir: &Path, config: &Config) -> io::Result<()> {
    fs::write(
        data_dir.join("config.txt"),
        format!(
            "productive={}\ndistracting={}\nblocked={}\ndevices={}\n",
            config.productive_keywords.join(","),
            config.distracting_keywords.join(","),
            config.blocked_keywords.join(","),
            config.network_devices.join(",")
        ),
    )
}

fn save_focus(data_dir: &Path, focus: &FocusSession) -> io::Result<()> {
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
            "{{\"task\":\"{}\",\"target\":\"{}\",\"startedAt\":{},\"durationMinutes\":{},\"breakMinutes\":{},\"pausedAt\":{},\"pausedTotalSeconds\":{},\"pomodoroAlertedAt\":{},\"alertDelaySeconds\":{},\"alertAction\":\"{}\",\"alertMessage\":\"{}\",\"redirectApp\":\"{}\",\"highFocusMode\":{}}}",
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
            json_escape(&clean_alert_message_template(&focus.alert_message)),
            json_escape(&focus.redirect_app),
            focus.high_focus_mode
        ),
    )
}

fn load_focus(data_dir: &Path) -> Option<FocusSession> {
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
        alert_message: json_string(&value, "alertMessage")
            .map(|message| clean_alert_message_template(&message))
            .unwrap_or_else(|| DEFAULT_ALERT_MESSAGE_TEMPLATE.into()),
        redirect_app: json_string(&value, "redirectApp").unwrap_or_default(),
        high_focus_mode: json_bool(&value, "highFocusMode").unwrap_or(false),
    })
}

fn clear_focus(data_dir: &Path) -> io::Result<()> {
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

fn send_device_notifications(
    devices: &[NetworkDevice],
    event: &str,
    message: &str,
    sample: &ActivitySample,
) {
    let devices = devices.to_vec();
    let event = event.to_string();
    let message = message.to_string();
    let sample = sample.clone();
    thread::spawn(move || {
        for device in devices {
            if !device.selected || device.endpoint.starts_with("browser:") {
                continue;
            }
            if let Some(endpoint) = native_notification_endpoint(&device.endpoint) {
                let _ = post_device_notification(&endpoint, &event, &message, &sample);
            }
        }
    });
}

fn native_notification_endpoint(endpoint: &str) -> Option<String> {
    if let Some(ip) = endpoint.strip_prefix("lan:") {
        return Some(format!("http://{ip}:4799/api/native/notify"));
    }
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return Some(endpoint.to_string());
    }
    None
}

fn post_device_notification(
    device: &str,
    event: &str,
    message: &str,
    sample: &ActivitySample,
) -> io::Result<()> {
    let Some((host, port, path)) = parse_device_endpoint(device) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid device endpoint",
        ));
    };
    let body = format!(
        "{{\"app\":\"{}\",\"title\":\"{}\",\"source\":\"{}\",\"category\":\"{}\",\"event\":\"{}\",\"message\":\"{}\",\"timestamp\":{}}}",
        json_escape(&sample.app),
        json_escape(&sample.title),
        json_escape(&sample.source),
        json_escape(&sample.category),
        json_escape(event),
        json_escape(message),
        sample.timestamp
    );
    let mut stream = TcpStream::connect((host.as_str(), port))?;
    let timeout = Some(Duration::from_secs(2));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        path,
        host,
        body.len(),
        body
    );
    stream.write_all(request.as_bytes())
}

fn parse_device_endpoint(device: &str) -> Option<(String, u16, String)> {
    let trimmed = device.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_scheme = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
        .unwrap_or(trimmed);
    let (authority, path_part) = without_scheme
        .split_once('/')
        .map(|(authority, path)| (authority, format!("/{path}")))
        .unwrap_or((without_scheme, "/".to_string()));
    let (host, port) = authority
        .rsplit_once(':')
        .and_then(|(host, port)| port.parse::<u16>().ok().map(|port| (host, port)))
        .unwrap_or((authority, 80));
    let host = host.trim().trim_matches(['[', ']']).to_string();
    if host.is_empty() {
        None
    } else {
        Some((host, port, path_part))
    }
}

fn normalize_device_endpoint(device: &str) -> String {
    let trimmed = device.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with("browser:")
        || trimmed.starts_with("lan:")
        || trimmed.starts_with("mobile:")
    {
        return trimmed.to_string();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

fn normalize_device_kind(kind: &str) -> String {
    match kind.trim().to_lowercase().as_str() {
        "phone" | "tv" | "tablet" | "laptop" | "desktop" => kind.trim().to_lowercase(),
        _ => "device".into(),
    }
}

fn format_device_record_selected(name: &str, kind: &str, endpoint: &str, selected: bool) -> String {
    format!(
        "{}|{}|{}|{}",
        name.trim().replace(['|', ','], " "),
        normalize_device_kind(kind),
        normalize_device_endpoint(endpoint),
        if selected { "selected" } else { "off" }
    )
}

fn parse_network_device_record(record: &str) -> NetworkDevice {
    let mut parts = record.splitn(4, '|');
    let first = parts.next().unwrap_or("").trim();
    let second = parts.next().map(str::trim);
    let third = parts.next().map(str::trim);
    let fourth = parts.next().map(str::trim);
    if let (Some(kind), Some(endpoint)) = (second, third) {
        return NetworkDevice {
            name: if first.is_empty() {
                "Device".into()
            } else {
                first.to_string()
            },
            kind: normalize_device_kind(kind),
            endpoint: normalize_device_endpoint(endpoint),
            selected: !matches!(fourth, Some("off" | "false" | "0")),
        };
    }

    NetworkDevice {
        name: "Device".into(),
        kind: "device".into(),
        endpoint: normalize_device_endpoint(record),
        selected: true,
    }
}

fn selected_network_devices(records: &[String]) -> Vec<NetworkDevice> {
    records
        .iter()
        .map(|record| parse_network_device_record(record))
        .filter(|device| device.selected && is_qr_connected_device(device))
        .collect()
}

fn idle_warning_devices(records: &[String]) -> Vec<NetworkDevice> {
    let mut devices = Vec::new();
    for device in records
        .iter()
        .map(|record| parse_network_device_record(record))
        .filter(|device| {
            device.selected && is_qr_connected_device(device) && is_phone_or_tv_device(device)
        })
    {
        push_unique_device(&mut devices, device.clone());
    }
    devices
}

fn is_qr_connected_device(device: &NetworkDevice) -> bool {
    device.endpoint.starts_with("browser:") || device.endpoint.starts_with("mobile:")
}

fn push_unique_device(devices: &mut Vec<NetworkDevice>, device: NetworkDevice) {
    if !devices
        .iter()
        .any(|existing| existing.endpoint == device.endpoint)
    {
        devices.push(device);
    }
}

fn is_phone_or_tv_device(device: &NetworkDevice) -> bool {
    matches!(device.kind.as_str(), "phone" | "tv")
        || device.name.to_lowercase().contains("iphone")
        || device.name.to_lowercase().contains("phone")
        || device.name.to_lowercase().contains("tv")
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
        let _ = Command::new("sh").arg("-c").arg(script).spawn();
    }
}

fn os_alert_then_activate(title: &str, message: &str, app_name: &str) {
    let title = title.to_string();
    let message = message.to_string();
    let app_name = app_name.trim().to_string();

    thread::spawn(move || {
        #[cfg(target_os = "macos")]
        {
            close_existing_focus_alert();
            if notify_then_activate_macos(&title, &message, &app_name).is_err() {
                notify(
                    &format!("FOCUS WARNING - {}", title.to_uppercase()),
                    &message,
                );
                thread::sleep(Duration::from_secs(2));
                let _ = activate_app(&app_name);
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            notify(
                &format!("FOCUS WARNING - {}", title.to_uppercase()),
                &message,
            );
            thread::sleep(Duration::from_secs(2));
            let _ = activate_app(&app_name);
        }
    });
}

#[cfg(target_os = "macos")]
fn notify_then_activate_macos(title: &str, message: &str, app_name: &str) -> io::Result<()> {
    let alert_title = format!("FOCUS WARNING - {}", title.to_uppercase());
    let script = format!(
        "set targetApp to \"{}\"\n\
         display notification \"{}\" with title \"{}\" sound name \"Glass\"\n\
         delay 2\n\
         do shell script \"open -a \" & quoted form of targetApp\n\
         delay 0.2\n\
         try\n\
         \ttell application targetApp to activate\n\
         end try\n\
         try\n\
         \ttell application \"System Events\" to set frontmost of first process whose name is targetApp to true\n\
         end try",
        apple_escape(app_name),
        apple_escape(message),
        apple_escape(&alert_title)
    );
    let status = Command::new("osascript").arg("-e").arg(script).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("could not notify before activating app"))
    }
}

fn block_activity_access(sample: &ActivitySample, keyword: &str, rule_kind: BlockRuleKind) {
    let sample = sample.clone();
    let keyword = keyword.trim().to_string();
    thread::spawn(move || match rule_kind {
        BlockRuleKind::Website => {
            if close_active_browser_tab(&sample.app).is_err() {
                let _ = quit_app(&sample.app);
            }
        }
        BlockRuleKind::App => {
            if should_quit_blocked_app(&sample, &keyword) {
                let _ = quit_app(&sample.app);
            }
        }
    });
}

fn block_high_focus_activity_access(sample: &ActivitySample, rule_kind: BlockRuleKind) {
    let sample = sample.clone();
    thread::spawn(move || match rule_kind {
        BlockRuleKind::Website => {
            if close_active_browser_tab(&sample.app).is_err() && !is_browser_app(&sample.app) {
                let _ = force_quit_app(&sample.app);
            }
        }
        BlockRuleKind::App => {
            let _ = force_quit_app(&sample.app);
        }
    });
}

fn password_block_activity_access(sample: &ActivitySample, rule: &BlockRule, message: &str) {
    let rule = rule.clone();
    let sample = sample.clone();
    let message = message.to_string();
    thread::spawn(move || {
        let allowed = prompt_for_block_password(&rule, &message);
        if !allowed {
            notify(
                "Password block",
                "Incorrect password. Access remains blocked.",
            );
            if let Some(kind) = blocked_rule_match(&sample, &rule.target) {
                block_activity_access(&sample, &rule.target, kind);
            }
        }
    });
}

fn prompt_for_block_password(rule: &BlockRule, message: &str) -> bool {
    if rule.password.is_empty() {
        notify("Password block", message);
        return true;
    }

    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display dialog \"{}\" default answer \"\" with title \"Local Focus Password Block\" buttons {{\"Continue\"}} default button \"Continue\" with hidden answer",
            apple_escape(message)
        );
        let output = Command::new("osascript").arg("-e").arg(script).output();
        if let Ok(output) = output {
            let text = String::from_utf8_lossy(&output.stdout);
            let answer = text
                .split("text returned:")
                .nth(1)
                .unwrap_or("")
                .trim()
                .to_string();
            return answer == rule.password;
        }
        false
    }

    #[cfg(target_os = "windows")]
    {
        let script = format!(
            "$p = Read-Host '{}' -AsSecureString; \
             $b=[Runtime.InteropServices.Marshal]::SecureStringToBSTR($p); \
             [Runtime.InteropServices.Marshal]::PtrToStringAuto($b)",
            ps_escape(message)
        );
        return Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()
            .ok()
            .is_some_and(|output| String::from_utf8_lossy(&output.stdout).trim() == rule.password);
    }

    #[cfg(target_os = "linux")]
    {
        let script = format!(
            "if command -v zenity >/dev/null 2>&1; then zenity --password --title='Local Focus Password Block' --text='{}'; else exit 1; fi",
            shell_escape(message)
        );
        return Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .ok()
            .is_some_and(|output| String::from_utf8_lossy(&output.stdout).trim() == rule.password);
    }

    // Other platforms (e.g. Android) enforce password blocks in the native layer.
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        false
    }
}

fn should_quit_blocked_app(sample: &ActivitySample, keyword: &str) -> bool {
    let normalized_keyword = normalize_match_text(keyword);
    if normalized_keyword.is_empty() || domain_from_url(keyword).is_some() || keyword.contains('.')
    {
        return false;
    }
    normalize_match_text(&sample.app).contains(&normalized_keyword)
}

fn is_browser_app(app_name: &str) -> bool {
    let app = app_name.trim().to_lowercase();
    app == "arc"
        || app == "chrome"
        || app == "chrome.exe"
        || app == "firefox"
        || app == "firefox.exe"
        || app == "safari"
        || app.contains("arc browser")
        || app.contains("brave")
        || app.contains("chromium")
        || app.contains("firefox")
        || app.contains("google chrome")
        || app.contains("google-chrome")
        || app.contains("librewolf")
        || app.contains("microsoft edge")
        || app.contains("msedge")
        || app.contains("opera")
        || app.contains("vivaldi")
        || app.contains("zen browser")
}

fn close_active_browser_tab(app_name: &str) -> io::Result<()> {
    let app_name = app_name.trim();
    if app_name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing browser app name",
        ));
    }

    #[cfg(target_os = "macos")]
    {
        let script = if app_name.eq_ignore_ascii_case("safari") {
            format!(
                "tell application \"{}\" to if (count of windows) > 0 then close current tab of front window",
                apple_escape(app_name)
            )
        } else {
            format!(
                "tell application \"{}\" to if (count of windows) > 0 then close active tab of front window",
                apple_escape(app_name)
            )
        };
        let status = Command::new("osascript").arg("-e").arg(script).status()?;
        if status.success() {
            return Ok(());
        }

        if is_browser_app(app_name) && close_active_tab_with_keyboard_macos(app_name).is_ok() {
            return Ok(());
        }
    }

    #[cfg(target_os = "windows")]
    {
        let status = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "$shell = New-Object -ComObject WScript.Shell; $shell.SendKeys('^w')",
            ])
            .status()?;
        if status.success() {
            return Ok(());
        }
    }

    #[cfg(target_os = "linux")]
    {
        let status = Command::new("sh")
            .arg("-c")
            .arg("if command -v xdotool >/dev/null 2>&1; then xdotool key Ctrl+w; else exit 1; fi")
            .status()?;
        if status.success() {
            return Ok(());
        }
    }

    Err(io::Error::other("could not close blocked browser tab"))
}

#[cfg(target_os = "macos")]
fn close_active_tab_with_keyboard_macos(app_name: &str) -> io::Result<()> {
    let script = format!(
        "tell application \"System Events\"\n\
         set frontmost of first process whose name is \"{}\" to true\n\
         keystroke \"w\" using command down\n\
         end tell",
        apple_escape(app_name)
    );
    let status = Command::new("osascript").arg("-e").arg(script).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            "could not close active browser tab with keyboard",
        ))
    }
}

fn quit_app(app_name: &str) -> io::Result<()> {
    let app_name = app_name.trim();
    if app_name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing app name",
        ));
    }

    #[cfg(target_os = "macos")]
    {
        let script = format!("tell application \"{}\" to quit", apple_escape(app_name));
        let status = Command::new("osascript").arg("-e").arg(script).status()?;
        if status.success() {
            return Ok(());
        }
    }

    #[cfg(target_os = "windows")]
    {
        let script = format!(
            "Get-Process -Name '{}' -ErrorAction SilentlyContinue | Stop-Process",
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
                "if command -v wmctrl >/dev/null 2>&1; then wmctrl -c '{}'; else pkill -x '{}'; fi",
                shell_escape(app_name),
                shell_escape(app_name)
            ))
            .status()?;
        if status.success() {
            return Ok(());
        }
    }

    Err(io::Error::other("could not quit blocked app"))
}

fn force_quit_app(app_name: &str) -> io::Result<()> {
    if quit_app(app_name).is_ok() {
        return Ok(());
    }

    let app_name = app_name.trim();
    if app_name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing app name",
        ));
    }

    #[cfg(target_os = "macos")]
    {
        let status = Command::new("pkill").args(["-x", app_name]).status()?;
        if status.success() {
            return Ok(());
        }
    }

    #[cfg(target_os = "windows")]
    {
        let script = format!(
            "Get-Process -Name '{}' -ErrorAction SilentlyContinue | Stop-Process -Force",
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
        let status = Command::new("pkill")
            .args(["-KILL", "-x", app_name])
            .status()?;
        if status.success() {
            return Ok(());
        }
    }

    Err(io::Error::other("could not force quit blocked app"))
}

#[cfg(target_os = "macos")]
fn active_alert_pid() -> &'static Mutex<Option<u32>> {
    static ACTIVE_ALERT_PID: OnceLock<Mutex<Option<u32>>> = OnceLock::new();
    ACTIVE_ALERT_PID.get_or_init(|| Mutex::new(None))
}

#[cfg(target_os = "macos")]
fn close_existing_focus_alert() {
    let pid = active_alert_pid()
        .lock()
        .ok()
        .and_then(|mut active| active.take());
    if let Some(pid) = pid {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
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

    // Other platforms (e.g. Android) raise alerts through the native layer.
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (title, message);
        false
    }
}

fn activate_app(app_name: &str) -> io::Result<()> {
    let app_name = app_name.trim();
    if app_name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing app name",
        ));
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

        let script = format!(
            "tell application \"{}\" to activate",
            apple_escape(app_name)
        );
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

    Err(io::Error::other(
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

fn request_value(params: &HashMap<String, String>, body: &str, key: &str) -> Option<String> {
    params
        .get(key)
        .cloned()
        .or_else(|| json_string(body, key))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn percent_decode(value: &str) -> String {
    // Decode at the byte level and interpret the result as UTF-8, so multi-byte
    // characters (accents, emoji, CJK) survive instead of being mangled by a
    // byte-to-char cast.
    let mut bytes = Vec::with_capacity(value.len());
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        match byte {
            b'+' => bytes.push(b' '),
            b'%' => {
                let hi = chars.next().unwrap_or(b'0');
                let lo = chars.next().unwrap_or(b'0');
                if let Ok(decoded) =
                    u8::from_str_radix(&format!("{}{}", hi as char, lo as char), 16)
                {
                    bytes.push(decoded);
                } else {
                    bytes.push(byte);
                    bytes.push(hi);
                    bytes.push(lo);
                }
            }
            other => bytes.push(other),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn url_encode(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char)
            }
            _ => result.push_str(&format!("%{byte:02X}")),
        }
    }
    result
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            // Remaining control characters must be \u-escaped to stay valid JSON.
            c if (c as u32) < 0x20 => escaped.push_str(&format!("\\u{:04x}", c as u32)),
            c => escaped.push(c),
        }
    }
    escaped
}

fn html_attr_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| match c {
            '&' => "&amp;".chars().collect::<Vec<_>>(),
            '<' => "&lt;".chars().collect(),
            '>' => "&gt;".chars().collect(),
            '"' => "&quot;".chars().collect(),
            '\'' => "&#039;".chars().collect(),
            _ => vec![c],
        })
        .collect()
}

/// Find the byte offset just past `"key":` for a key that sits at a value
/// boundary (preceded by `{`, `,`, or whitespace), so a key name appearing as a
/// substring of another key does not produce a false match.
fn json_value_start(value: &str, key: &str) -> Option<usize> {
    let marker = format!("\"{key}\":");
    let mut search_from = 0;
    while let Some(rel) = value[search_from..].find(&marker) {
        let pos = search_from + rel;
        let preceded_ok = pos == 0
            || matches!(
                value.as_bytes()[pos - 1],
                b'{' | b',' | b' ' | b'\t' | b'\n' | b'\r'
            );
        if preceded_ok {
            return Some(pos + marker.len());
        }
        search_from = pos + marker.len();
    }
    None
}

fn json_string(value: &str, key: &str) -> Option<String> {
    let start = json_value_start(value, key)?;
    let mut chars = value[start..].trim_start().chars();
    if chars.next()? != '"' {
        return None;
    }
    let mut result = String::new();
    let mut escaped = false;
    for c in chars {
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
    let start = json_value_start(value, key)?;
    let number = value[start..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect::<String>();
    number.parse().ok()
}

fn json_bool(value: &str, key: &str) -> Option<bool> {
    let start = json_value_start(value, key)?;
    let tail = value[start..].trim_start();
    if tail.starts_with("true") {
        Some(true)
    } else if tail.starts_with("false") {
        Some(false)
    } else {
        None
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(app: &str, title: &str, source: &str) -> ActivitySample {
        ActivitySample {
            timestamp: 1,
            app: app.into(),
            title: title.into(),
            source: source.into(),
            category: "distracting".into(),
        }
    }

    fn focus(target: &str) -> FocusSession {
        FocusSession {
            task: "Deep work".into(),
            target: target.into(),
            started_at: 1,
            duration_minutes: 25,
            break_minutes: 5,
            paused_at: None,
            paused_total_seconds: 0,
            pomodoro_alerted_at: None,
            alert_delay_seconds: DEFAULT_ALERT_DELAY_SECONDS,
            alert_action: "alert".into(),
            alert_message: DEFAULT_ALERT_MESSAGE_TEMPLATE.into(),
            redirect_app: String::new(),
            high_focus_mode: true,
        }
    }

    #[test]
    fn focus_alert_message_uses_custom_template_and_default_fallback() {
        let active = sample("Safari", "News", "https://www.nytimes.com/");
        let mut session = focus("Pages, https://claude.ai/");
        session.alert_delay_seconds = 180;
        session.alert_message =
            "Return to {targets}. Current: {app} at {url} after {delay}.".into();

        assert_eq!(
            focus_alert_message(&session, &active),
            "Return to Pages, https://claude.ai/. Current: Safari at https://www.nytimes.com/ after 3 minutes."
        );

        session.alert_message = "   ".into();
        assert_eq!(
            focus_alert_message(&session, &active),
            "You have been outside your focus apps/sites for over 3 minutes. Allowed: 'Pages, https://claude.ai/'. Current activity: Safari"
        );
    }

    #[test]
    fn focus_target_allows_claude_new_tab() {
        let session = focus("https://claude.ai/");
        let active = sample("Safari", "Claude", "https://claude.ai/new");

        assert!(matches_focus_target(&session, &active));
    }

    #[test]
    fn focus_target_allows_chatgpt_conversation() {
        let session = focus("https://chatgpt.com");
        let active = sample("Google Chrome", "ChatGPT", "https://chatgpt.com/c/abc123");

        assert!(matches_focus_target(&session, &active));
    }

    #[test]
    fn focus_target_allows_app_name() {
        let session = focus("Claude, Pages");
        let active = sample("Claude", "Claude", "local");

        assert!(matches_focus_target(&session, &active));
    }

    #[test]
    fn local_focus_connect_pages_are_exempt_from_blocking() {
        let active = sample(
            "Safari",
            "Local Focus Connect",
            "http://192.168.4.22:4799/connect",
        );

        assert!(is_local_focus_control_activity(&active));
    }

    #[test]
    fn wifi_connection_pages_are_exempt_from_blocking() {
        let active = sample("System Settings", "Wi-Fi connection", "local");

        assert!(is_system_connection_activity(&active));
    }

    #[test]
    fn active_focus_target_is_exempt_from_block_rules() {
        let state = Arc::new(Mutex::new(AppState {
            focus: Some(focus("https://claude.ai/")),
            ..Default::default()
        }));
        let active = sample("Safari", "Claude", "https://claude.ai/new");

        assert!(activity_is_block_exempt(&state, &active));
    }

    #[test]
    fn high_focus_blocks_outside_desktop_apps() {
        let session = focus("Pages, https://claude.ai/, https://chatgpt.com");
        let active = sample("VLC", "VLC media player", "local");

        assert!(high_focus_should_block(&session, &active));
    }

    #[test]
    fn high_focus_blocks_outside_desktop_apps_even_when_idle() {
        let session = focus("Pages, https://claude.ai/, https://chatgpt.com");
        let mut active = sample("TV", "Apple TV", "local");
        active.category = "idle".into();

        assert!(high_focus_should_block(&session, &active));
    }

    #[test]
    fn high_focus_empty_browser_tab_is_tab_level_block() {
        let session = focus("Pages, https://claude.ai/, https://chatgpt.com");
        let active = sample("Google Chrome", "New Tab", "chrome://newtab/");

        assert!(high_focus_should_block(&session, &active));
        assert_eq!(high_focus_block_rule_kind(&active), BlockRuleKind::Website);
    }

    #[test]
    fn high_focus_blank_safari_tab_is_tab_level_block() {
        let session = focus("Pages, https://claude.ai/, https://chatgpt.com");
        let active = sample("Safari", "Favorites", "about:blank");

        assert!(high_focus_should_block(&session, &active));
        assert_eq!(high_focus_block_rule_kind(&active), BlockRuleKind::Website);
    }

    #[test]
    fn high_focus_does_not_block_focus_desktop_apps() {
        let session = focus("Pages, https://claude.ai/, https://chatgpt.com");
        let active = sample("Pages", "Writing", "local");

        assert!(!high_focus_should_block(&session, &active));
    }

    #[test]
    fn focus_target_text_keeps_first_fifteen_unique_targets() {
        let targets = (1..=18)
            .map(|index| format!("App{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let normalized = normalize_focus_target_text(&format!("{targets}, App1, app2"));
        let values = target_list_from_text(&normalized);

        assert_eq!(values.len(), MAX_FOCUS_TARGETS);
        assert_eq!(values.first().map(String::as_str), Some("App1"));
        assert_eq!(values.last().map(String::as_str), Some("App15"));
    }

    #[test]
    fn journal_settings_default_to_enabled_evening() {
        let dir = temp_test_dir("journal-settings");

        let settings = load_journal_settings(&dir).expect("journal settings");

        assert!(settings.enabled);
        assert_eq!(settings.reminder_mode, "evening");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn journal_entry_keeps_latest_saved_text() {
        let dir = temp_test_dir("journal-entry");
        fs::create_dir_all(&dir).expect("create temp dir");

        save_journal_entry(&dir, "2026-06-05", "First").expect("save first");
        save_journal_entry(&dir, "2026-06-05", "Second\nwith detail").expect("save second");
        let entry = journal_entry_for_date(&dir, "2026-06-05")
            .expect("load entry")
            .expect("entry exists");

        assert_eq!(entry.0, "Second\nwith detail");
        assert!(journal_entry_exists(&dir, "2026-06-05"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn journal_date_accepts_iso_day_only() {
        assert_eq!(
            clean_journal_date("2026-06-05").as_deref(),
            Some("2026-06-05")
        );
        assert!(clean_journal_date("2026/06/05").is_none());
        assert!(clean_journal_date("June 5").is_none());
    }

    #[test]
    fn journal_reminder_time_accepts_24_hour_hhmm_only() {
        assert_eq!(clean_reminder_time("00:00").as_deref(), Some("00:00"));
        assert_eq!(clean_reminder_time("23:59").as_deref(), Some("23:59"));
        assert!(clean_reminder_time("24:00").is_none());
        assert!(clean_reminder_time("7:30").is_none());
        assert!(clean_reminder_time("07:60").is_none());
    }

    #[test]
    fn journal_task_reminders_can_be_added_and_removed() {
        let dir = temp_test_dir("journal-task-reminder");
        fs::create_dir_all(&dir).expect("create temp dir");

        let reminder = add_journal_task_reminder(&dir, "Plan tomorrow", "18:30")
            .expect("add reminder")
            .expect("valid reminder");
        let reminders = load_journal_task_reminders(&dir).expect("load reminders");

        assert_eq!(reminders.len(), 1);
        assert_eq!(reminders[0].time, "18:30");
        assert_eq!(reminders[0].task, "Plan tomorrow");
        assert!(remove_journal_task_reminder(&dir, &reminder.id).expect("remove reminder"));
        assert!(load_journal_task_reminders(&dir)
            .expect("reload reminders")
            .is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn percent_decode_preserves_utf8() {
        // "café 🚀" percent-encoded.
        assert_eq!(percent_decode("caf%C3%A9+%F0%9F%9A%80"), "café 🚀");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn json_escape_escapes_control_characters() {
        assert_eq!(json_escape("a\u{0007}b"), "a\\u0007b");
        assert_eq!(json_escape("tab\tnewline\n"), "tab\\tnewline\\n");
        assert_eq!(json_escape("quote\"slash\\"), "quote\\\"slash\\\\");
    }

    #[test]
    fn json_value_lookup_ignores_key_substrings_and_boundaries() {
        let line = "{\"task\":\"write\",\"startedAt\":42,\"highFocusMode\":true}";
        assert_eq!(json_string(line, "task").as_deref(), Some("write"));
        assert_eq!(json_number(line, "startedAt"), Some(42));
        assert_eq!(json_bool(line, "highFocusMode"), Some(true));
        // A key that only appears as a substring of another key must not match.
        let tricky = "{\"xtask\":\"nope\"}";
        assert_eq!(json_string(tricky, "task"), None);
    }

    #[test]
    fn remote_requests_are_limited_to_device_endpoints() {
        assert!(remote_path_allowed("/device"));
        assert!(remote_path_allowed("/device-sw.js"));
        assert!(remote_path_allowed("/connect"));
        assert!(remote_path_allowed("/api/mobile/activity"));
        assert!(remote_path_allowed("/download/local-focus-mobile.apk"));
        assert!(!remote_path_allowed("/"));
        assert!(!remote_path_allowed("/api/timeline"));
        assert!(!remote_path_allowed("/api/journal/entry"));
        assert!(!remote_path_allowed("/api/state"));
    }

    #[test]
    fn mutation_paths_are_flagged_for_csrf_checks() {
        assert!(is_mutation_path("/api/focus/start"));
        assert!(is_mutation_path("/api/block/add"));
        assert!(is_mutation_path("/api/journal/save"));
        assert!(!is_mutation_path("/api/focus-sessions"));
        assert!(!is_mutation_path("/api/timeline"));
        assert!(!is_mutation_path("/api/state"));
    }

    #[test]
    fn cross_site_detection_uses_fetch_metadata_and_origin() {
        let same_origin = "GET /api/focus/stop HTTP/1.1\r\nHost: 127.0.0.1:4799\r\nSec-Fetch-Site: same-origin\r\n\r\n";
        let cross_site = "GET /api/focus/stop HTTP/1.1\r\nHost: 127.0.0.1:4799\r\nSec-Fetch-Site: cross-site\r\n\r\n";
        let native = "POST /api/mobile/activity HTTP/1.1\r\nHost: 127.0.0.1:4799\r\n\r\n";
        let cross_origin =
            "GET /x HTTP/1.1\r\nHost: 127.0.0.1:4799\r\nOrigin: http://evil.test\r\n\r\n";
        assert!(!request_is_cross_site(same_origin));
        assert!(request_is_cross_site(cross_site));
        assert!(!request_is_cross_site(native));
        assert!(request_is_cross_site(cross_origin));
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "local-focus-{name}-{}-{}",
            std::process::id(),
            now()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
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
