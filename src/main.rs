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
// Jump guard: catching a switching spiral while it is happening, rather than
// only showing it on a chart afterwards. ADHD self-report is unreliable — the
// research is clear that people do not notice they are doing this — so the
// intervention is to name it out loud the moment it starts.
const JUMP_GUARD_WINDOW_SECONDS: i64 = 5 * 60;
const JUMP_GUARD_SWITCHES: usize = 12;
const JUMP_GUARD_COOLDOWN_SECONDS: i64 = 10 * 60;
const DEVICE_NOTIFY_COOLDOWN_SECONDS: i64 = 60;
const DEFAULT_ALERT_DELAY_SECONDS: u64 = 60;
// The "move to app" action has its own, usually longer, timer than the alert.
const DEFAULT_ACTION_DELAY_SECONDS: u64 = 120;
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
    action_delay_seconds: u64,
    alert_action: String,
    alert_message: String,
    redirect_app: String,
    high_focus_mode: bool,
    // Nudge when a switching spiral starts. On by default.
    jump_guard: bool,
    // A locked session cannot be paused or stopped, and its block rules cannot
    // be edited, until its timer runs out. This is the commitment, and it is
    // the only thing that makes a block hold against the person who set it.
    locked: bool,
}

#[derive(Default)]
struct AppState {
    config: Config,
    focus: Option<FocusSession>,
    last_distraction_at: i64,
    last_focus_mismatch_at: i64,
    focus_mismatch_started_at: Option<i64>,
    // When the "move to app" action last fired, so it repeats on its own
    // interval (like the alert) rather than only once per streak.
    last_focus_action_at: i64,
    last_blocked_at: i64,
    last_blocked_key: String,
    last_device_notify_at: i64,
    last_device_notify_key: String,
    // Live switch detection for the jump guard. The switch report reads the
    // samples file after the fact; this is the same signal, in the moment.
    last_sample_key: String,
    recent_switch_times: Vec<i64>,
    last_jump_guard_at: i64,
    // Master switch. When true, the whole app is stopped: no tracking, blocking,
    // alerts, device notifications, or journal reminders until it is resumed.
    stopped: bool,
    // Last time each "browser:" receiver device was seen (registered or
    // polled `/api/device/events`). Not persisted — a fresh browser tab
    // always re-registers with a new endpoint, so entries that stop being
    // seen are dead and get pruned; see `prune_stale_browser_devices`.
    browser_last_seen: HashMap<String, i64>,
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
        // The dashboard is one big raw string in this file, so a typo in its
        // JS compiles fine and only breaks at runtime. This prints the page
        // without binding a port so scripts/test.sh can syntax-check the
        // script it contains.
        Some("dump-dashboard") => {
            println!("{}", index_html());
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
        last_focus_action_at: 0,
        last_blocked_at: 0,
        last_blocked_key: String::new(),
        last_device_notify_at: 0,
        last_device_notify_key: String::new(),
        stopped: false,
        browser_last_seen: HashMap::new(),
        last_sample_key: String::new(),
        recent_switch_times: Vec::new(),
        last_jump_guard_at: 0,
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
        println!("Device receiver URL: {url}/device");
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
        last_focus_action_at: 0,
        last_blocked_at: 0,
        last_blocked_key: String::new(),
        last_device_notify_at: 0,
        last_device_notify_key: String::new(),
        stopped: false,
        browser_last_seen: HashMap::new(),
        last_sample_key: String::new(),
        recent_switch_times: Vec::new(),
        last_jump_guard_at: 0,
    }));
    tracking_loop(data_dir, state)
}

fn tracking_loop(data_dir: PathBuf, state: Arc<Mutex<AppState>>) -> io::Result<()> {
    loop {
        prune_disconnected_browser_devices(&data_dir, &state)?;

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

        run_jump_guard(&data_dir, &state, &sample)?;
        let blocked = enforce_blocked_access(&data_dir, &state, &config, &sample)?;
        notify_devices_for_attention_event(&data_dir, &state, &config, &sample)?;
        append_sample(&data_dir, &sample)?;
        detect_distraction(&data_dir, &state, &sample)?;
        if blocked {
            guard_blocked_activity(&data_dir, &state, &config)?;
        } else {
            thread::sleep(Duration::from_secs(SAMPLE_SECONDS));
        }
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
                        "{} — time is up. Tracking keeps running until you pause the session or turn Local Focus off. Take a {} minute break when you are ready.",
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
        action_delay_seconds: DEFAULT_ACTION_DELAY_SECONDS,
        alert_action: "alert".into(),
        alert_message: DEFAULT_ALERT_MESSAGE_TEMPLATE.into(),
        redirect_app: String::new(),
        high_focus_mode: false,
        locked: false,
        jump_guard: true,
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

/// Whether a focus warning should move the user to the redirect app (the
/// "Move to app" warning action) rather than just showing an alert. Both the
/// focus-mismatch and distraction paths use this so the action behaves the same.
fn focus_alert_switches_app(alert_action: &str, redirect_app: &str) -> bool {
    alert_action == "switch" && !redirect_app.trim().is_empty()
}

/// Whether the "move to app" action should fire now. The action has its own
/// timer (separate from the alert) and repeats on its own interval: it must be
/// enabled, the user must have been off-focus at least as long as the action
/// delay, and at least one action interval must have passed since the last move.
fn should_move_to_app(
    off_focus_seconds: i64,
    action_delay: i64,
    since_last_action: i64,
    switch_enabled: bool,
) -> bool {
    switch_enabled && off_focus_seconds >= action_delay && since_last_action >= action_delay
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

    // Activity reported by a phone/companion should alert that device, not pop a
    // dialog on this Mac. Local foreground activity still alerts here.
    let sample_is_remote = sample.source.starts_with("mobile:");

    let distracting = sample.category == "distracting";
    let enough_time = sample.timestamp - guard.last_distraction_at >= DISTRACTION_SECONDS;
    let focus_mismatch = guard
        .focus
        .as_ref()
        .filter(|focus| !focus_targets(focus).is_empty())
        .is_some_and(|focus| !matches_focus_target(focus, sample));

    if focused && focus_mismatch {
        let (alert_delay, action_delay) = guard
            .focus
            .as_ref()
            .map(|focus| {
                (
                    focus.alert_delay_seconds.max(1) as i64,
                    focus.action_delay_seconds.max(1) as i64,
                )
            })
            .unwrap_or((
                DEFAULT_ALERT_DELAY_SECONDS as i64,
                DEFAULT_ACTION_DELAY_SECONDS as i64,
            ));
        let mismatch_started_at = match guard.focus_mismatch_started_at {
            Some(started_at) => started_at,
            None => {
                guard.focus_mismatch_started_at = Some(sample.timestamp);
                sample.timestamp
            }
        };
        let off_focus_seconds = sample.timestamp - mismatch_started_at;
        let alert_cooldown_passed = sample.timestamp - guard.last_focus_mismatch_at >= alert_delay;

        // 1) Alert: a warning message after the warn time, repeating every warn
        //    time while the user stays off-focus. Independent of the action.
        if off_focus_seconds >= alert_delay && alert_cooldown_passed {
            let focus = guard.focus.as_ref().expect("focus checked above");
            let message = focus_alert_message(focus, sample);
            let devices = selected_network_devices(&guard.config.network_devices);
            if !devices.is_empty() {
                send_device_notifications(&devices, "focus_target_mismatch", &message, sample);
                append_device_notification(
                    data_dir,
                    "focus_target_mismatch",
                    &message,
                    sample,
                    &devices,
                )?;
            }
            if !sample_is_remote {
                os_alert("Focus warning", &message);
            }
            guard.last_focus_mismatch_at = sample.timestamp;
            append_event(data_dir, "focus_target_mismatch", &message)?;
        }

        // 2) Action: its own timer. Move the user to the redirect app once
        //    off-focus past the action time, then repeat every action interval.
        let action_cooldown = sample.timestamp - guard.last_focus_action_at;
        let (switch_enabled, redirect_app) = guard
            .focus
            .as_ref()
            .map(|focus| {
                (
                    focus_alert_switches_app(&focus.alert_action, &focus.redirect_app),
                    focus.redirect_app.clone(),
                )
            })
            .unwrap_or((false, String::new()));
        if !sample_is_remote
            && should_move_to_app(off_focus_seconds, action_delay, action_cooldown, switch_enabled)
        {
            os_alert_then_activate(
                "Time to refocus",
                &format!("Moving you to {redirect_app} to get back on task."),
                &redirect_app,
            );
            guard.last_focus_action_at = sample.timestamp;
            append_event(data_dir, "focus_action_moved", &redirect_app)?;
        }
    } else {
        // Back on a focus target: reset the off-focus streak.
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
        if !sample_is_remote {
            os_alert("Distraction warning", &message);
        }
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

/// Returns whether something was actually blocked, so the caller can tighten
/// its cadence while the user is pushing against a rule.
fn enforce_blocked_access(
    data_dir: &Path,
    state: &Arc<Mutex<AppState>>,
    config: &Config,
    sample: &ActivitySample,
) -> io::Result<bool> {
    if activity_is_block_exempt(state, sample) {
        return Ok(false);
    }

    if enforce_high_focus_block(data_dir, state, sample)? {
        return Ok(true);
    }

    // Distraction rules belong to a focus session. Outside one, Local Focus
    // still tracks and reports but must not close tabs or quit apps.
    let focus = lock_state(state).focus.clone();
    if !block_rules_are_active(focus.as_ref()) {
        return Ok(false);
    }

    let Some((rule, rule_kind)) = blocked_keyword_match(config, sample) else {
        return Ok(false);
    };
    let blocked_key = format!(
        "{}|{}|{}",
        normalize_match_text(&rule.target),
        normalize_match_text(&sample.app),
        normalize_match_text(&sample.source)
    );

    // The cooldown used to gate the block itself, which handed back a free
    // window on the same target: get blocked, reopen it, and nothing touched
    // you for another BLOCK_COOLDOWN_SECONDS. Enforcement now runs every time
    // a blocked thing is in front of you; only the notification and the log
    // entry are rate-limited, which is all the cooldown was ever needed for.
    let repeat_within_cooldown = {
        let mut guard = lock_state(state);
        let repeat = sample.timestamp - guard.last_blocked_at < BLOCK_COOLDOWN_SECONDS
            && guard.last_blocked_key == blocked_key;
        guard.last_blocked_at = sample.timestamp;
        guard.last_blocked_key = blocked_key;
        repeat
    };

    match rule.mode {
        BlockMode::Full => block_activity_access(sample, &rule.target, rule_kind),
        BlockMode::Password => {
            // Only raise the password prompt once per cooldown; stacking
            // dialogs every second would be unusable.
            if !repeat_within_cooldown {
                let message = format!(
                    "Blocked access to '{}' because it matches your distraction rule '{}'.",
                    blocked_activity_label(sample),
                    rule.target
                );
                password_block_activity_access(sample, &rule, &message);
            }
        }
    }

    if repeat_within_cooldown {
        return Ok(true);
    }

    let message = format!(
        "Blocked access to '{}' because it matches your distraction rule '{}'.",
        blocked_activity_label(sample),
        rule.target
    );
    notify("Blocked by Local Focus", &message);
    append_event(data_dir, "blocked_access", &message)?;
    Ok(true)
}

/// After a block fires, re-check every second for the length of one normal
/// sample instead of waiting the full interval — reopening the tab used to buy
/// several seconds of access. Deliberately does not record samples: the reports
/// treat one sample as SAMPLE_SECONDS of time, so recording here would inflate
/// them. This replaces the normal sleep rather than adding to it, so the
/// sampling cadence is unchanged.
fn guard_blocked_activity(
    data_dir: &Path,
    state: &Arc<Mutex<AppState>>,
    config: &Config,
) -> io::Result<()> {
    for _ in 0..SAMPLE_SECONDS {
        thread::sleep(Duration::from_secs(1));
        let focus = {
            let guard = lock_state(state);
            if guard.stopped {
                return Ok(());
            }
            guard.focus.clone()
        };
        let raw = foreground_activity();
        let category = classify(config, &raw.0, &raw.1);
        let mut sample = ActivitySample {
            timestamp: now(),
            app: raw.0,
            title: raw.1,
            source: raw.2,
            category,
        };
        apply_focus_productivity_gate(&focus, &mut sample);
        enforce_blocked_access(data_dir, state, config, &sample)?;
    }
    Ok(())
}

/// Whether a switching spiral is underway and worth interrupting: enough
/// switches inside the rolling window, and long enough since the last nudge
/// that this is not nagging. Kept pure so the thresholds are testable.
fn jump_guard_should_fire(switches_in_window: usize, seconds_since_last_nudge: i64) -> bool {
    switches_in_window >= JUMP_GUARD_SWITCHES
        && seconds_since_last_nudge >= JUMP_GUARD_COOLDOWN_SECONDS
}

/// Records a switch if this sample moved to a different app or page, drops
/// anything that has aged out of the window, and reports how many remain.
fn track_switch_for_jump_guard(guard: &mut AppState, sample: &ActivitySample) -> usize {
    let key = format!("{}|{}", sample.app, sample.title);
    let switched = !guard.last_sample_key.is_empty() && guard.last_sample_key != key;
    guard.last_sample_key = key;
    if switched {
        guard.recent_switch_times.push(sample.timestamp);
    }
    let cutoff = sample.timestamp - JUMP_GUARD_WINDOW_SECONDS;
    guard.recent_switch_times.retain(|at| *at > cutoff);
    guard.recent_switch_times.len()
}

/// The intervention. Rize pops a window you cannot dismiss for ten seconds;
/// this names the pattern instead, because the thing an ADHD brain is missing
/// here is the noticing, not another wall. Only runs inside a session, so it
/// cannot nag someone who never asked Local Focus to help.
fn run_jump_guard(
    data_dir: &Path,
    state: &Arc<Mutex<AppState>>,
    sample: &ActivitySample,
) -> io::Result<()> {
    let (switches, focus) = {
        let mut guard = lock_state(state);
        let switches = track_switch_for_jump_guard(&mut guard, sample);
        (switches, guard.focus.clone())
    };

    let Some(focus) = focus.filter(|focus| focus.paused_at.is_none()) else {
        return Ok(());
    };
    if !focus.jump_guard {
        return Ok(());
    }

    let should_fire = {
        let guard = lock_state(state);
        jump_guard_should_fire(switches, sample.timestamp - guard.last_jump_guard_at)
    };
    if !should_fire {
        return Ok(());
    }
    lock_state(state).last_jump_guard_at = sample.timestamp;

    let minutes = JUMP_GUARD_WINDOW_SECONDS / 60;
    let message = format!(
        "You have jumped between apps {switches} times in {minutes} minutes. Pick one thing and stay with it for a few minutes."
    );
    notify("Lots of jumping", &message);
    append_event(data_dir, "jump_guard", &message)
}

/// Whether a session's commitment lock is still holding. Derived from the
/// clock rather than a flag, so it always releases on its own when the timer
/// runs out — there is no state anyone can get permanently stuck in. A paused
/// session cannot exist while locked (pausing is refused), so elapsed time is
/// simply now minus the start.
fn focus_lock_is_active(focus: &FocusSession, at: i64) -> bool {
    focus.locked && focus_elapsed_seconds(focus, at) < (focus.duration_minutes * 60) as i64
}

/// The same question for the optional session the request handlers hold.
fn session_is_locked(focus: Option<&FocusSession>, at: i64) -> bool {
    focus.is_some_and(|focus| focus_lock_is_active(focus, at))
}

/// Whether the distraction rules should be enforced right now. They only apply
/// while a focus session is actually running: with no session, or one that is
/// paused, the block list must not close tabs or quit apps. This mirrors
/// `high_focus_should_block`, which has always required a running session.
fn block_rules_are_active(focus: Option<&FocusSession>) -> bool {
    focus.is_some_and(|focus| focus.paused_at.is_none())
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
                "Distracted activity detected {}: {} - {}",
                if sample.source.starts_with("mobile:") {
                    "on your phone"
                } else {
                    "on this machine"
                },
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
        // A quick start with no parameters at all (the menu bar extra) reuses
        // the last session's setup instead of silently dropping the user's
        // focus list. The dashboard always sends its form, so it is unaffected.
        let last = if params.contains_key("task") || params.contains_key("target") {
            None
        } else {
            load_last_focus_settings(&data_dir)
        };
        let task = params.get("task").cloned().unwrap_or_else(|| {
            last.as_ref()
                .map(|settings| settings.task.clone())
                .unwrap_or_else(|| "Focus session".into())
        });
        let minutes = params
            .get("minutes")
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| {
                last.as_ref()
                    .map(|settings| settings.duration_minutes)
                    .unwrap_or(25)
            });
        let target = params
            .get("target")
            .map(|s| normalize_focus_target_text(s))
            .unwrap_or_else(|| {
                last.as_ref()
                    .map(|settings| settings.target.clone())
                    .unwrap_or_default()
            });
        let alert_delay_seconds = params
            .get("alertSeconds")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| {
                last.as_ref()
                    .map(|settings| settings.alert_delay_seconds)
                    .unwrap_or(DEFAULT_ALERT_DELAY_SECONDS)
            })
            .clamp(10, 60 * 60);
        let action_delay_seconds = params
            .get("actionSeconds")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| {
                last.as_ref()
                    .map(|settings| settings.action_delay_seconds)
                    .unwrap_or(DEFAULT_ACTION_DELAY_SECONDS)
            })
            .clamp(10, 60 * 60);
        let alert_action = params
            .get("alertAction")
            .filter(|action| action.as_str() == "switch")
            .cloned()
            .unwrap_or_else(|| {
                last.as_ref()
                    .map(|settings| settings.alert_action.clone())
                    .unwrap_or_else(|| "alert".into())
            });
        let alert_message = params
            .get("alertMessage")
            .map(|message| clean_alert_message_template(message))
            .unwrap_or_else(|| {
                last.as_ref()
                    .map(|settings| settings.alert_message.clone())
                    .unwrap_or_else(|| DEFAULT_ALERT_MESSAGE_TEMPLATE.into())
            });
        // On unless explicitly switched off.
        let jump_guard = params
            .get("jumpGuard")
            .map(|value| !matches!(value.as_str(), "0" | "false" | "off"))
            .unwrap_or(true);
        // Opt-in commitment: once set, this session cannot be paused or
        // stopped, and its block rules cannot be edited, until the timer ends.
        let locked = params
            .get("lock")
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "on"));
        let redirect_app = params
            .get("redirectApp")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| {
                last.as_ref()
                    .map(|settings| settings.redirect_app.clone())
                    .unwrap_or_default()
            });
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
            action_delay_seconds,
            alert_action,
            alert_message,
            redirect_app,
            high_focus_mode: false,
            locked,
            jump_guard,
        };
        save_focus(&data_dir, &session)?;
        save_last_focus_settings(&data_dir, &session)?;
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
    } else if path.starts_with("/api/focus/update") {
        // Edit an active session in place, preserving its running timer and
        // pause state (unlike /start, which begins a fresh session).
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let updated = {
            let mut guard = lock_state(&state);
            if let Some(mut focus) = guard.focus.clone() {
                if let Some(task) = params.get("task").map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    focus.task = task.to_string();
                }
                if let Some(target) = params.get("target") {
                    focus.target = normalize_focus_target_text(target);
                }
                if let Some(minutes) = params.get("minutes").and_then(|v| v.parse().ok()) {
                    focus.duration_minutes = minutes;
                }
                if let Some(seconds) = params.get("alertSeconds").and_then(|v| v.parse::<u64>().ok())
                {
                    focus.alert_delay_seconds = seconds.clamp(10, 60 * 60);
                }
                if let Some(seconds) = params.get("actionSeconds").and_then(|v| v.parse::<u64>().ok())
                {
                    focus.action_delay_seconds = seconds.clamp(10, 60 * 60);
                }
                if let Some(action) = params.get("alertAction") {
                    focus.alert_action = if action == "switch" {
                        "switch".into()
                    } else {
                        "alert".into()
                    };
                }
                if let Some(message) = params.get("alertMessage") {
                    focus.alert_message = clean_alert_message_template(message);
                }
                if let Some(redirect) = params.get("redirectApp") {
                    focus.redirect_app = redirect.trim().to_string();
                }
                guard.focus = Some(focus.clone());
                Some(focus)
            } else {
                None
            }
        };
        if let Some(focus) = updated {
            save_focus(&data_dir, &focus)?;
            append_event(&data_dir, "focus_updated", &focus.task)?;
            notify("Focus updated", &focus.task);
            write_response(&mut stream, "application/json", "{\"ok\":true}")?;
        } else {
            write_response(
                &mut stream,
                "application/json",
                "{\"ok\":false,\"noSession\":true}",
            )?;
        }
    } else if path.starts_with("/api/focus/pause") {
        if session_is_locked(lock_state(&state).focus.as_ref(), now()) {
            write_response(
                &mut stream,
                "application/json",
                "{\"ok\":false,\"locked\":true,\"error\":\"This session is locked until its timer ends.\"}",
            )?;
            return Ok(());
        }
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
        if session_is_locked(lock_state(&state).focus.as_ref(), now()) {
            write_response(
                &mut stream,
                "application/json",
                "{\"ok\":false,\"locked\":true,\"error\":\"This session is locked until its timer ends.\"}",
            )?;
            return Ok(());
        }
        // Stop is the master off switch: end the focus session and halt all
        // tracking, blocking, alerts, device notifications, and reminders until
        // the app is resumed (Resume button, a new focus session, or relaunch).
        clear_focus(&data_dir)?;
        {
            let mut guard = lock_state(&state);
            guard.focus = None;
            guard.stopped = true;
        }
        // "off", not "paused": pausing is what a single session does, and
        // reusing that word here is what made the two controls read alike.
        notify(
            "Local Focus turned off",
            "Tracking, blocking, warnings, and reminders stay off until you turn it back on.",
        );
        write_response(&mut stream, "application/json", "{\"ok\":true,\"stopped\":true}")?;
    } else if path.starts_with("/api/app/resume") {
        {
            let mut guard = lock_state(&state);
            guard.stopped = false;
        }
        notify(
            "Local Focus turned on",
            "Tracking, blocking, warnings, and reminders are on again.",
        );
        write_response(&mut stream, "application/json", "{\"ok\":true,\"stopped\":false}")?;
    } else if path.starts_with("/api/block/add") {
        if session_is_locked(lock_state(&state).focus.as_ref(), now()) {
            write_response(
                &mut stream,
                "application/json",
                "{\"ok\":false,\"locked\":true,\"error\":\"Block rules are locked until this session ends.\"}",
            )?;
            return Ok(());
        }
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        // Comma/newline-separated input becomes one block rule per keyword.
        let keywords = split_block_keywords(params.get("keyword").map(String::as_str).unwrap_or(""));
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

        if !keywords.is_empty() {
            let mut config = load_config(&data_dir).unwrap_or_default();
            // Drop the rule being edited and any existing rule that one of the new
            // keywords would duplicate, then add a record for each keyword.
            config.blocked_keywords.retain(|item| {
                let target = parse_block_rule_record(item).target;
                !keywords.contains(&target) && (original.is_empty() || target != original)
            });
            for keyword in &keywords {
                config
                    .blocked_keywords
                    .push(format_block_rule_record(keyword, mode, &password));
            }
            save_config(&data_dir, &config)?;
            lock_state(&state).config = config;
            append_event(&data_dir, "blocked_keyword_added", &keywords.join(", "))?;
        }

        write_response(&mut stream, "application/json", "{\"ok\":true}")?;
    } else if path.starts_with("/api/block/remove") {
        if session_is_locked(lock_state(&state).focus.as_ref(), now()) {
            write_response(
                &mut stream,
                "application/json",
                "{\"ok\":false,\"locked\":true,\"error\":\"Block rules are locked until this session ends.\"}",
            )?;
            return Ok(());
        }
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
        config.network_devices = prune_stale_browser_devices(
            &config.network_devices,
            &lock_state(&state).browser_last_seen,
            now(),
        );
        config.network_devices.push(device.clone());
        save_config(&data_dir, &config)?;
        {
            let mut guard = lock_state(&state);
            guard.config = config;
            guard.browser_last_seen.insert(endpoint.clone(), now());
        }
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
        if device.starts_with("browser:") {
            lock_state(&state)
                .browser_last_seen
                .insert(device.to_string(), now());
        }
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
    } else if route == "/api/mac/notifications" {
        // Loopback-only by omission from remote_path_allowed: this is the app's
        // own window process talking to its own server, never the LAN.
        //
        // Only `?host=1` claims the heartbeat and drains the queue, and the
        // native host sends it only once macOS has actually granted it
        // notification permission. Anything else is a read-only peek. Without
        // that split, any local reader would mark a host "live" and notify()
        // would queue banners that nobody ever posts — silently swallowing
        // them instead of falling back to osascript.
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let is_host = params
            .get("host")
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "on"));
        let pending = {
            match mac_notifications().lock() {
                Ok(mut queue) => {
                    if is_host {
                        queue.host_seen_at = now();
                        std::mem::take(&mut queue.queued)
                    } else {
                        queue.queued.clone()
                    }
                }
                Err(_) => Vec::new(),
            }
        };
        let items = pending
            .iter()
            .map(|(timestamp, title, message)| {
                format!(
                    "{{\"timestamp\":{},\"title\":\"{}\",\"message\":\"{}\"}}",
                    timestamp,
                    json_escape(title),
                    json_escape(message)
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        write_response(
            &mut stream,
            "application/json",
            &format!("{{\"ok\":true,\"notifications\":[{}]}}", items),
        )?;
    } else if route == "/api/report/switches" {
        write_response(
            &mut stream,
            "application/json",
            &switch_report_json(&data_dir)?,
        )?;
    } else if route == "/api/state" {
        let (focus, devices, blocks, stopped, recent_jumps) = {
            let guard = lock_state(&state);
            let cutoff = now() - JUMP_GUARD_WINDOW_SECONDS;
            (
                guard.focus.clone(),
                guard.config.network_devices.clone(),
                guard.config.blocked_keywords.clone(),
                guard.stopped,
                guard.recent_switch_times.iter().filter(|at| **at > cutoff).count(),
            )
        };
        write_response(
            &mut stream,
            "application/json",
            &state_json(&data_dir, focus, &devices, &blocks, stopped, recent_jumps),
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
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
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
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Disposition: attachment; filename=\"{}\"\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        filename.replace('"', ""),
        body.len()
    )?;
    stream.write_all(body)
}

fn write_not_found(stream: &mut TcpStream, message: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        message.len(),
        message
    )
}

fn write_forbidden(stream: &mut TcpStream, message: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        message.len(),
        message
    )
}

/// Endpoints reachable from other machines on the LAN: the device-companion
/// surface (receiver pages, downloads, mobile/native APIs) plus the
/// endpoints the phone companion needs — view state/reports, drive focus
/// sessions, manage the journal and block list, and read report history.
/// The most sensitive surface stays loopback-only: the full dashboard, the raw
/// activity timeline, report reset, and the master stop/resume.
fn remote_path_allowed(route: &str) -> bool {
    let prefixes = [
        "/api/mobile/register",
        "/api/mobile/activity",
        "/api/device/register",
        "/api/device/events",
        "/api/native/notify",
        "/connect",
        "/download/",
        // Phone companion: control focus sessions and read focus reports.
        "/api/focus/",
        "/api/focus-report",
        "/api/focus-sessions",
        // Phone companion parity: daily journal and block-list management.
        "/api/journal/",
        "/api/block/",
    ];
    // `/device`, `/device-sw.js`, and `/device-manifest.json` all share this prefix.
    route.starts_with("/device")
        || route == "/api/state"
        || route == "/api/report"
        || route == "/api/report/switches"
        // Past archived reports, but not report reset (loopback-only).
        || route == "/api/report/history"
        || prefixes.iter().any(|prefix| route.starts_with(prefix))
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

/// How often attention actually shifts, independent of `report_json`'s
/// duration-based view. The research behind this: habitual interruption
/// (how often you check something) degrades sustained attention on its own,
/// separately from how many total minutes were spent distracted — a low
/// distracted-minutes total can still hide a shredded, constantly-switching
/// attention pattern. Counts a "switch" as any change in the foreground
/// app or window title between consecutive samples, over the same rolling
/// report window `report_json` uses.
fn switch_report_json(data_dir: &Path) -> io::Result<String> {
    let samples = load_samples(data_dir)?;
    let since = report_window_start(data_dir)?.max(now() - 24 * 60 * 60);
    let recent: Vec<_> = samples
        .into_iter()
        .filter(|s| s.timestamp >= since)
        .collect();

    let mut total_switches: u64 = 0;
    let mut distracting_switches: u64 = 0;
    let mut switch_targets: BTreeMap<(String, String), usize> = BTreeMap::new();
    // Switches bucketed into clock hours, so the dashboard can draw when the
    // jumping happened rather than just how much of it there was. Keyed by the
    // unix timestamp of the hour's start; the browser turns that into a local
    // hour label, which keeps timezone handling out of here.
    let mut by_hour: BTreeMap<i64, usize> = BTreeMap::new();
    // Longest unbroken stretch on one thing, counting only non-idle samples so
    // walking away from the machine cannot masquerade as deep focus.
    let mut longest_calm_seconds: u64 = 0;
    let mut current_run_seconds: u64 = 0;
    let mut previous: Option<&ActivitySample> = None;
    for sample in &recent {
        let switched = previous
            .is_some_and(|prev| prev.app != sample.app || prev.title != sample.title);
        if switched {
            total_switches += 1;
            if sample.category == "distracting" {
                distracting_switches += 1;
            }
            *switch_targets
                .entry((sample.app.clone(), sample.source.clone()))
                .or_default() += 1;
            *by_hour
                .entry(sample.timestamp - sample.timestamp.rem_euclid(3600))
                .or_default() += 1;
            longest_calm_seconds = longest_calm_seconds.max(current_run_seconds);
            current_run_seconds = 0;
        }
        if sample.category != "idle" {
            current_run_seconds += SAMPLE_SECONDS;
        }
        previous = Some(sample);
    }
    longest_calm_seconds = longest_calm_seconds.max(current_run_seconds);

    let window_minutes = (recent.len() as u64 * SAMPLE_SECONDS / 60).max(1);
    let switches_per_hour = total_switches as f64 / (window_minutes as f64 / 60.0);
    // "About once every N minutes" reads far more plainly than a rate.
    let minutes_between_switches = if total_switches > 0 {
        window_minutes as f64 / total_switches as f64
    } else {
        0.0
    };

    let hours_json = by_hour
        .into_iter()
        .map(|(start, count)| format!("{{\"start\":{},\"switches\":{}}}", start, count))
        .collect::<Vec<_>>()
        .join(",");

    let mut top: Vec<_> = switch_targets.into_iter().collect();
    top.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    let top_json = top
        .into_iter()
        .take(10)
        .map(|((app, source), count)| {
            format!(
                "{{\"app\":\"{}\",\"source\":\"{}\",\"switches\":{}}}",
                json_escape(&app),
                json_escape(&source),
                count
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    Ok(format!(
        "{{\"totalSwitches\":{},\"distractingSwitches\":{},\"switchesPerHour\":{:.1},\"minutesBetweenSwitches\":{:.1},\"longestCalmSeconds\":{},\"windowMinutes\":{},\"byHour\":[{}],\"topSwitchTargets\":[{}]}}",
        total_switches,
        distracting_switches,
        switches_per_hour,
        minutes_between_switches,
        longest_calm_seconds,
        window_minutes,
        hours_json,
        top_json
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
    recent_jumps: usize,
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
                "{{\"stopped\":{},\"focus\":{{\"task\":\"{}\",\"target\":\"{}\",\"startedAt\":{},\"durationMinutes\":{},\"alertDelaySeconds\":{},\"actionDelaySeconds\":{},\"alertAction\":\"{}\",\"alertMessage\":\"{}\",\"redirectApp\":\"{}\",\"highFocusMode\":{},\"paused\":{},\"remainingSeconds\":{},\"locked\":{},\"lockActive\":{},\"jumpGuard\":{},\"recentJumps\":{}}},\"devices\":[{}],\"blockedRules\":[{}],\"journal\":{},\"deviceConnectUrl\":\"{}\",\"deviceInstallUrl\":\"{}\",\"androidAppUrl\":\"{}\",\"macAppUrl\":\"{}\"}}",
                stopped,
                json_escape(&focus.task),
                json_escape(&focus.target),
                focus.started_at,
                focus.duration_minutes,
                focus.alert_delay_seconds,
                focus.action_delay_seconds,
                json_escape(&focus.alert_action),
                json_escape(&clean_alert_message_template(&focus.alert_message)),
                json_escape(&focus.redirect_app),
                focus.high_focus_mode,
                focus.paused_at.is_some(),
                remaining,
                focus.locked,
                focus_lock_is_active(&focus, now()),
                focus.jump_guard,
                recent_jumps,
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
:root { color-scheme: light dark; --bg:#f5f2ff; --ink:#1c1330; --muted:#6c6486; --line:#eae3fb; --good:#10b981; --warn:#f59e0b; --bad:#f43f5e; --panel:#ffffff; --panel-soft:#f1ebff; --accent:#7c3aed; --accent-2:#ec4899; --accent-3:#22d3ee; --accent-grad:linear-gradient(135deg, #7c3aed 0%, #ec4899 100%); --shadow:0 18px 44px rgba(124,58,237,.18); }
@media (prefers-color-scheme: dark) { :root { --bg:#120e22; --ink:#f4f1ff; --muted:#aaa2cc; --line:#2c2746; --panel:#1d1838; --panel-soft:#171331; --accent:#a78bfa; --accent-2:#f472b6; --accent-3:#67e8f9; --accent-grad:linear-gradient(135deg, #a78bfa 0%, #f472b6 100%); --shadow:0 18px 44px rgba(0,0,0,.45); } }
/* Switchable templates (set via the header picker, persisted in localStorage). */
html[data-theme="cyber"] { color-scheme:dark; --bg:#07060f; --ink:#ece9ff; --muted:#9a92c4; --line:#2a2348; --good:#34d399; --warn:#fbbf24; --bad:#fb7185; --panel:#130f24; --panel-soft:#1b1533; --accent:#b66bff; --accent-2:#22d3ee; --accent-3:#f472b6; --accent-grad:linear-gradient(135deg, #b66bff 0%, #22d3ee 100%); --shadow:0 0 0 1px rgba(182,107,255,.20), 0 18px 50px rgba(124,58,237,.40); }
html[data-theme="clay"] { color-scheme:light; --bg:#ede9f6; --ink:#322b4a; --muted:#7a7397; --line:#e3ddf3; --good:#10b981; --warn:#f59e0b; --bad:#fb7185; --panel:#fbf9ff; --panel-soft:#f1ecfb; --accent:#8b7cf6; --accent-2:#f59ebc; --accent-3:#a78bfa; --accent-grad:linear-gradient(135deg, #8b7cf6 0%, #f59ebc 100%); --shadow:0 14px 34px rgba(139,124,246,.24); }
html[data-theme="minimal"] { color-scheme:light; --bg:#ffffff; --ink:#0a0a0a; --muted:#71717a; --line:#e4e4e7; --good:#16a34a; --warn:#b45309; --bad:#dc2626; --panel:#ffffff; --panel-soft:#f4f4f5; --accent:#111111; --accent-2:#111111; --accent-3:#111111; --accent-grad:linear-gradient(135deg, #111111, #111111); --shadow:0 1px 2px rgba(0,0,0,.06), 0 10px 28px rgba(0,0,0,.05); }
html[data-theme="minimal"] button { border-radius:10px; box-shadow:none; }
html[data-theme="minimal"] .control-shell, html[data-theme="minimal"] .focus-shell { border-radius:14px; }
html[data-theme="professional"] { color-scheme:light; --bg:#f8fafc; --ink:#0f172a; --muted:#64748b; --line:#e2e8f0; --good:#15803d; --warn:#b45309; --bad:#b91c1c; --panel:#ffffff; --panel-soft:#f1f5f9; --accent:#1e40af; --accent-2:#2563eb; --accent-3:#1d4ed8; --accent-grad:linear-gradient(135deg, #1e40af 0%, #2563eb 100%); --shadow:0 12px 32px rgba(15,23,42,.10); }
html[data-theme="professional"] .control-shell, html[data-theme="professional"] .focus-shell { border-radius:16px; }
* { box-sizing: border-box; }
body { margin:0; font:14px/1.4 system-ui, -apple-system, Segoe UI, sans-serif; background:var(--bg); color:var(--ink); }
header { display:flex; align-items:center; justify-content:space-between; gap:16px; padding:18px 24px; border-bottom:1px solid var(--line); background:color-mix(in srgb, var(--panel) 82%, transparent); backdrop-filter:blur(12px); position:sticky; top:0; z-index:20; transition:padding .18s ease, box-shadow .18s ease; }
header.compact { padding:8px 24px; box-shadow:var(--shadow); }
header.compact h1 { font-size:17px; }
header.compact .header-sub { display:none; }
.header-actions { display:flex; flex-wrap:wrap; align-items:center; gap:8px; justify-content:flex-end; }
.header-actions button { padding:7px 11px; }
h1 { margin:0; font-size:23px; font-weight:850; letter-spacing:-.02em; background:var(--accent-grad); -webkit-background-clip:text; background-clip:text; color:transparent; transition:font-size .18s ease; }
main { max-width:1180px; margin:0 auto; padding:24px; display:grid; gap:18px; }
.bar { display:flex; flex-wrap:wrap; gap:10px; align-items:center; }
.view-nav { display:flex; gap:6px; flex-wrap:wrap; background:var(--panel); border:1px solid var(--line); border-radius:14px; padding:6px; box-shadow:var(--shadow); }
.view-tab { border:0; background:transparent; color:var(--muted); font-weight:800; font-size:13px; padding:9px 16px; border-radius:10px; cursor:pointer; }
.view-tab:hover { color:var(--ink); background:var(--panel-soft); }
.view-tab.is-active { background:var(--accent-grad); color:#fff; }
.view { display:grid; gap:18px; }
.view[hidden] { display:none; }
input, select, textarea, button { border:1px solid var(--line); border-radius:12px; padding:10px 13px; background:var(--panel); color:var(--ink); }
input:focus, select:focus, textarea:focus { outline:none; border-color:var(--accent); box-shadow:0 0 0 3px color-mix(in srgb, var(--accent) 18%, transparent); }
textarea { min-height:88px; resize:vertical; font:inherit; }
button { cursor:pointer; font-weight:800; color:#fff; background:var(--accent-grad); border:1px solid transparent; border-radius:14px; box-shadow:0 8px 18px color-mix(in srgb, var(--accent) 32%, transparent); transition:filter .15s ease, transform .08s ease; }
button:hover:not(:disabled) { filter:brightness(1.06); }
button:active:not(:disabled) { transform:translateY(1px); }
button:disabled { cursor:not-allowed; opacity:.55; box-shadow:none; }
.focus-shell { background:linear-gradient(180deg, color-mix(in srgb, var(--panel) 92%, var(--panel-soft)), var(--panel)); border:1px solid var(--line); border-radius:22px; padding:20px; display:grid; gap:16px; box-shadow:var(--shadow); }
.focus-shell-head { display:flex; align-items:center; justify-content:space-between; gap:14px; }
.focus-title { display:flex; align-items:center; gap:12px; }
.focus-mark { width:44px; height:44px; border-radius:14px; background:var(--accent-grad); color:white; display:grid; place-items:center; font-weight:850; letter-spacing:.04em; box-shadow:0 8px 20px color-mix(in srgb, var(--accent) 35%, transparent); }
.focus-shell h2 { margin:0; font-size:18px; }
.control-shell { background:var(--panel); border:1px solid var(--line); border-radius:22px; padding:18px; display:grid; gap:14px; box-shadow:var(--shadow); }
.control-shell h2 { margin:0; font-size:16px; }
.report-calendar { display:grid; gap:12px; }
.calendar-head { display:grid; grid-template-columns:auto 1fr auto; gap:10px; align-items:center; }
.calendar-title { text-align:center; font-weight:800; }
.calendar-actions { display:grid; grid-template-columns:repeat(3, minmax(0, 1fr)); gap:10px; }
/* Grid/selector buttons stay neutral so only the active one pops. */
.calendar-actions button, .week-button, .day-button { min-height:40px; background:var(--panel); color:var(--ink); border:1px solid var(--line); box-shadow:none; font-weight:700; }
.calendar-actions button.active-report, .week-button.active-report, .calendar-actions button.active-year { background:var(--accent-grad); border-color:transparent; color:white; box-shadow:0 6px 16px color-mix(in srgb, var(--accent) 28%, transparent); }
.calendar-grid { display:grid; grid-template-columns:64px repeat(7, minmax(0, 1fr)); gap:6px; align-items:stretch; }
.calendar-label { color:var(--muted); font-size:12px; font-weight:750; text-align:center; padding:4px; }
.week-button, .day-button { width:100%; padding:8px 6px; }
.day-button.outside { color:var(--muted); opacity:.65; }
.day-button.selected { background:var(--accent-grad); border-color:transparent; color:white; box-shadow:0 6px 16px color-mix(in srgb, var(--accent) 28%, transparent); }
.focus-task-window { border:1px solid var(--line); border-radius:10px; padding:12px; background:var(--panel-soft); display:grid; gap:8px; }
.focus-task-window.disabled { opacity:.55; }
.focus-session-list { display:grid; gap:8px; }
.focus-session-row { border:1px solid var(--line); border-radius:8px; padding:9px; background:var(--panel); }
.check-field { display:grid; gap:7px; }
.switch-headline { font-size:22px; font-weight:800; line-height:1.35; margin:0; }
.switch-headline .switch-count { color:var(--accent); }
.switch-chart-wrap { border:1px solid var(--line); border-radius:10px; background:var(--panel-soft); padding:14px; display:grid; gap:8px; }
.switch-chart { display:flex; align-items:flex-end; gap:6px; min-height:132px; overflow-x:auto; }
.switch-bar { flex:1 1 0; min-width:26px; display:grid; gap:6px; justify-items:center; align-content:end; }
.switch-bar-track { width:100%; height:96px; display:flex; align-items:flex-end; }
.switch-bar-fill { width:100%; border-radius:8px 8px 4px 4px; background:var(--accent-grad); min-height:4px; transition:height .35s ease; }
.switch-bar.is-calm .switch-bar-fill { background:var(--good); }
.switch-bar.is-busy .switch-bar-fill { background:var(--bad); }
.switch-bar.is-quiet .switch-bar-track { border-bottom:2px dashed color-mix(in srgb, var(--line) 80%, transparent); }
.switch-bar.is-quiet .switch-bar-label { opacity:.55; }
.switch-bar-value { font-size:12px; font-weight:800; }
.switch-bar-label { font-size:11px; color:var(--muted); white-space:nowrap; }
.switch-chart-empty { color:var(--muted); align-self:center; margin:auto; }
.switch-chart-caption { font-size:12px; }
.switch-facts { display:grid; grid-template-columns:repeat(auto-fit, minmax(200px, 1fr)); gap:10px; }
.switch-fact { border:1px solid var(--line); border-radius:10px; padding:10px 12px; background:var(--panel); }
.switch-fact span { display:block; font-size:11px; font-weight:800; }
.switch-fact strong { display:block; margin-top:2px; font-size:20px; }
.switch-targets { display:grid; gap:8px; margin-top:10px; }
.switch-target-row { display:grid; grid-template-columns:minmax(90px, 160px) 1fr auto; gap:10px; align-items:center; font-size:13px; }
.switch-target-track { height:14px; border-radius:999px; background:color-mix(in srgb, var(--line) 55%, transparent); overflow:hidden; }
.switch-target-fill { height:100%; border-radius:999px; background:var(--accent-grad); min-width:3px; }
.switch-target-count { font-weight:800; }
.block-table-wrap { border:1px solid var(--line); border-radius:10px; background:var(--panel-soft); overflow-x:auto; }
.block-table { width:100%; border-collapse:collapse; font-size:13px; }
.block-table th { text-align:left; font-size:11px; font-weight:800; color:var(--muted); text-transform:uppercase; letter-spacing:.05em; padding:10px 12px; border-bottom:1px solid var(--line); white-space:nowrap; }
.block-table td { padding:8px 12px; border-bottom:1px solid color-mix(in srgb, var(--line) 60%, transparent); vertical-align:middle; }
.block-table tr:last-child td { border-bottom:0; }
.block-table input[type="text"], .block-table input[type="password"] { width:100%; min-width:150px; }
.block-col-check { width:110px; text-align:center; }
.block-table td.block-col-check { text-align:center; }
.block-table input[type="checkbox"] { width:18px; height:18px; accent-color:var(--accent); cursor:pointer; }
/* The password column only earns its space once a rule actually uses one. */
.block-col-password { display:none; }
.block-table.show-password .block-col-password { display:table-cell; }
.block-remove { border:0; background:transparent; color:var(--muted); font-weight:800; cursor:pointer; padding:6px 10px; border-radius:8px; }
.block-remove:hover { color:var(--bad); background:color-mix(in srgb, var(--bad) 12%, transparent); }
.block-row-pending td { background:color-mix(in srgb, var(--warn) 10%, transparent); }
.block-empty td { color:var(--muted); text-align:center; padding:18px 12px; }
.visually-hidden { position:absolute; width:1px; height:1px; margin:-1px; padding:0; overflow:hidden; clip:rect(0 0 0 0); white-space:nowrap; border:0; }
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
.connect-code-panel { border:1px solid var(--line); border-radius:10px; padding:16px; background:var(--panel-soft); display:grid; gap:10px; justify-items:start; }
.connect-code-label { font-size:13px; text-transform:uppercase; letter-spacing:.08em; }
.connect-code-value { font-size:34px; font-weight:850; letter-spacing:.12em; color:var(--ink); user-select:all; font-family:ui-monospace, SFMono-Regular, Menlo, monospace; }
.connect-code-actions { display:flex; gap:8px; flex-wrap:wrap; }
.connect-advanced { border:1px solid var(--line); border-radius:10px; padding:8px 12px; background:var(--panel); }
.connect-advanced summary { cursor:pointer; font-weight:700; }
.connect-advanced > *:not(summary) { margin-top:10px; }
.connect-downloads { display:grid; gap:6px; }
.connect-downloads a { color:var(--accent); font-weight:800; overflow-wrap:anywhere; }
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
.focus-timer { display:flex; align-items:center; gap:14px; border:1px solid var(--line); border-radius:10px; padding:12px 14px; background:var(--panel); }
.timer-ring { width:84px; height:84px; flex:none; transform:rotate(-90deg); }
.timer-ring circle { fill:none; stroke-width:8; stroke-linecap:round; }
.timer-ring-track { stroke:var(--line); }
.timer-ring-progress { stroke:var(--accent); transition:stroke-dashoffset .5s linear; }
.focus-timer.is-paused .timer-ring-progress { stroke:var(--warn); }
.focus-timer.is-done .timer-ring-progress { stroke:var(--good); }
.focus-timer.is-idle .timer-ring-progress { stroke:var(--line); }
.timer-readout { display:grid; gap:2px; min-width:0; }
.timer-readout strong { font-size:34px; line-height:1.05; letter-spacing:-.02em; font-variant-numeric:tabular-nums; }
.timer-readout span { font-size:12px; }
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
.journal-reminder-chip button { border:0; background:transparent; color:var(--bad); padding:0 2px; min-height:0; box-shadow:none; }
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
.source-toggle { display:inline; max-width:100%; padding:0; border:0; background:transparent; color:var(--ink); font:inherit; font-weight:500; text-align:left; overflow-wrap:anywhere; box-shadow:none; }
.source-toggle:hover { text-decoration:underline; }
/* Focus controls keep their own semantic colors instead of the default accent fill. */
.focus-btn { background:var(--panel); border:1px solid var(--line); color:var(--ink); box-shadow:none; transition: background .15s ease, border-color .15s ease, color .15s ease, filter .15s ease; }
.focus-idle { background:var(--accent-grad); border-color:transparent; color:#fff; box-shadow:0 8px 20px color-mix(in srgb, var(--accent) 30%, transparent); }
.focus-running { background:var(--good); border-color:transparent; color:white; box-shadow:0 8px 20px color-mix(in srgb, var(--good) 30%, transparent); }
.focus-paused { background:var(--warn); border-color:transparent; color:white; box-shadow:0 8px 20px color-mix(in srgb, var(--warn) 30%, transparent); }
.focus-stop-active { background:var(--panel); border-color:var(--bad); color:var(--bad); box-shadow:none; }
.focus-btn:hover:not(:disabled) { filter:brightness(1.05); }
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
@media (max-width:760px) { header, .two, .grid, .item, .explain-grid, .history-grid, .report-grid, .report-two, .bar-row, .focus-form, .detail-grid, .block-fields, .activity-row, .calendar-actions, .journal-settings, .journal-row, .journal-task-form { grid-template-columns:1fr; display:grid; } header { align-items:start; padding:12px 16px; gap:8px; } .header-sub { display:none; } #themeSelect { padding:6px 9px; } .header-actions { justify-content:flex-start; } .hour-bars, .period-bars { grid-template-columns:repeat(6, minmax(12px, 1fr)); } .focus-shell-head { align-items:start; display:grid; } .quick-metrics { grid-template-columns:1fr; } .calendar-grid { grid-template-columns:48px repeat(7, minmax(28px, 1fr)); gap:4px; } .block-type-options { grid-template-columns:1fr; } .block-password-field { grid-column:auto; } }
</style>
</head>
<body>
<header>
  <div><h1>Local Focus</h1><div class="muted header-sub">Private activity timeline, focus sessions, and reports. All data stays on this device.</div></div>
  <div class="header-actions">
    <select id="themeSelect" aria-label="Theme" title="Theme" onchange="setTheme(this.value)">
      <option value="vibrant">✨ Vibrant</option>
      <option value="cyber">🌌 Cyber</option>
      <option value="clay">🫧 Clay</option>
      <option value="minimal">◾ Minimal</option>
      <option value="professional">💼 Professional</option>
    </select>
    <div id="focusState" class="status-chip"></div>
    <button id="explainToggle" onclick="toggleExplain()" aria-expanded="false">Explain</button>
  </div>
</header>
<main>
  <div id="stopBanner" style="display:none; align-items:center; justify-content:space-between; gap:14px; flex-wrap:wrap; border:1px solid var(--bad); background:color-mix(in srgb, var(--bad) 12%, var(--panel)); color:var(--ink); border-radius:12px; padding:14px 18px;">
    <strong>Local Focus is off — tracking, blocking, warnings, and reminders stay off until you turn it back on.</strong>
    <button onclick="resumeApp()" style="background:var(--good); border-color:var(--good); color:#fff; white-space:nowrap;">Turn on Local Focus</button>
  </div>
  <nav class="view-nav" aria-label="Sections">
    <button type="button" class="view-tab is-active" data-view="focus" onclick="showView('focus')">Focus</button>
    <button type="button" class="view-tab" data-view="rules" onclick="showView('rules')">Blocking</button>
    <button type="button" class="view-tab" data-view="journal" onclick="showView('journal')">Journal</button>
    <button type="button" class="view-tab" data-view="reports" onclick="showView('reports')">Reports</button>
    <button type="button" class="view-tab" data-view="devices" onclick="showView('devices')">Devices</button>
  </nav>
  <div class="view" id="view-focus" role="tabpanel" aria-label="Focus">
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
        <div class="field"><label for="minutes">Session length (minutes)</label><input id="minutes" type="number" min="1" max="180" value="25" aria-label="Session length in minutes"></div>
        <div class="field"><label for="alertMinutes">Warn me after (minutes off task)</label><input id="alertMinutes" type="number" min="1" max="60" value="1" aria-label="Warn me after this many minutes off task" title="Warn me once I have been off task this long, then repeat every interval"></div>
        <div class="field"><label for="alertAction">If I stay off task</label><select id="alertAction" aria-label="What to do if I stay off task" title="What happens once the off-task timer runs out">
          <option value="alert">Just warn me</option>
          <option value="switch">Switch me to an app</option>
        </select></div>
        <div class="field"><label for="actionMinutes">Switch me back every (minutes)</label><input id="actionMinutes" type="number" min="1" max="60" value="2" aria-label="Switch me back every this many minutes" title="Switch me back once I have been off task this long, then repeat on its own timer"></div>
        <div class="field"><label for="redirectApp">App to switch me to</label><input id="redirectApp" placeholder="Pages" aria-label="App to switch me to"></div>
        <div class="field field-wide alert-message-field">
          <label for="alertMessage">Warning message</label>
          <textarea id="alertMessage" aria-label="Warning message">You have been outside your focus apps/sites for over {delay}. Allowed: '{targets}'. Current activity: {app}</textarea>
          <div class="muted">Use {delay}, {targets}, {app}, {title}, or {url}.</div>
        </div>
        <div class="field field-wide">
          <button id="saveFocusEdits" type="button" onclick="saveFocusEdits()" style="display:none;">Save changes</button>
        </div>
      </div>
    </div>
  </section>
  <aside class="focus-side">
    <h3>Current focus session</h3>
    <div id="focusTimer" class="focus-timer is-idle">
      <svg class="timer-ring" viewBox="0 0 120 120" aria-hidden="true">
        <circle class="timer-ring-track" cx="60" cy="60" r="54"></circle>
        <circle class="timer-ring-progress" id="timerRingProgress" cx="60" cy="60" r="54"></circle>
      </svg>
      <div class="timer-readout" role="timer" aria-live="off">
        <strong id="timerValue">--:--</strong>
        <span id="timerCaption" class="muted">No session running</span>
      </div>
    </div>
    <div class="quick-metrics">
      <div class="quick-metric"><span>Task</span><strong id="quickTask">None</strong></div>
      <div class="quick-metric"><span>Status</span><strong id="quickStatus">No session</strong></div>
      <div class="quick-metric"><span>Warn me after</span><strong id="quickDelay">1m</strong></div>
      <div class="quick-metric"><span>Off-task action</span><strong id="quickAction">Just warn me</strong></div>
    </div>
    <section class="grid focus-summary-grid" id="metrics" aria-label="Current focus summary"></section>
    <div class="high-focus-control">
      <div class="high-focus-row">
        <label class="high-focus-check" for="lockSession">
          <input id="lockSession" type="checkbox">
          Lock this session
        </label>
        <span id="lockSessionHint" class="muted">Blocks hold until the timer ends.</span>
      </div>
      <div class="high-focus-row">
        <label class="high-focus-check" for="jumpGuard">
          <input id="jumpGuard" type="checkbox" checked>
          Tell me when I am jumping a lot
        </label>
        <span id="jumpGuardHint" class="muted">A nudge when you switch apps over and over.</span>
      </div>
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
      <button id="pauseFocus" class="focus-btn" onclick="pauseFocus()" disabled>Pause session</button>
      <button id="stopFocus" class="focus-btn" onclick="stopFocus()" disabled>Turn off Local Focus</button>
    </div>
  </aside>
  </div>
  <div class="view" id="view-rules" role="tabpanel" aria-label="Blocking">
  <section id="distractionCard" class="control-shell distraction-card" aria-label="Distraction rules">
    <div>
      <h2>Blocked apps and websites</h2>
      <div class="muted">These apply while a focus session is running — start focus and websites close their active tab, apps are quit. A password block asks for the password instead, so you have to stop and decide.</div>
    </div>
    <div class="block-table-wrap">
      <table id="blockTable" class="block-table">
        <thead>
          <tr>
            <th scope="col">Site or app</th>
            <th scope="col" class="block-col-check">Full block</th>
            <th scope="col" class="block-col-check">Password block</th>
            <th scope="col" class="block-col-password">Password</th>
            <th scope="col"><span class="visually-hidden">Remove</span></th>
          </tr>
        </thead>
        <tbody id="blockRows"></tbody>
      </table>
    </div>
    <div class="focus-actions">
      <button type="button" onclick="addBlockRow()">Add row</button>
    </div>
  </section>
  </div>
  <div class="view" id="view-journal" role="tabpanel" aria-label="Journal">
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
  </div>
  <div class="view" id="view-reports" role="tabpanel" aria-label="Reports">
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
  <section id="switchReportCard" class="control-shell" aria-label="How often you jumped">
    <div>
      <h2>How often you jumped</h2>
      <div class="muted">Every time you swapped to a different app or page. Lots of small jumps break focus even on a day when your total distracted time looks fine.</div>
    </div>
    <p id="switchHeadline" class="switch-headline">No jumps yet today.</p>
    <div class="switch-chart-wrap">
      <div id="switchChart" class="switch-chart" role="img" aria-label="Jumps per hour"></div>
      <div class="switch-chart-caption muted">When you jumped, hour by hour</div>
    </div>
    <div class="switch-facts">
      <div class="switch-fact"><span class="muted">Longest stretch on one thing</span><strong id="switchLongest">--</strong></div>
      <div class="switch-fact"><span class="muted">Jumps onto something distracting</span><strong id="switchDistracting">--</strong></div>
    </div>
    <div>
      <strong>What you jumped to most</strong>
      <div id="switchTargets" class="switch-targets muted">No jumps recorded yet.</div>
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
      <div><h3>Jumps</h3><p>Every time the foreground app or page changes, over the same window as the report above. Independent of duration — a low distracted-minutes total can still hide constant jumping. An empty hour in the chart means no jumps at all.</p></div>
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
  </div>
  <div class="view" id="view-devices" role="tabpanel" aria-label="Devices">
  <section id="devicesCard" class="control-shell" aria-label="Connect to device">
    <div>
      <h2>Connect to device</h2>
      <div class="muted">Open the Local Focus companion app on your phone, tablet, or another computer, then enter the connection code below. Local Focus does not scan the network for devices.</div>
    </div>
    <div class="connect-code-panel">
      <div class="connect-code-label muted">Connection code</div>
      <code id="connectCode" class="connect-code-value" title="Connection code">--------</code>
      <div class="connect-code-actions">
        <button type="button" onclick="copyConnectCode()">Copy code</button>
      </div>
      <div id="connectCodeHint" class="muted">Type this code into the companion app's <strong>Connection code</strong> field to connect. It changes if Local Focus restarts on a different network address.</div>
    </div>
    <details class="connect-advanced">
      <summary>Manual link &amp; app downloads</summary>
      <div class="device-pill"><strong>Direct link</strong><br><span id="deviceConnectUrl" class="muted">Loading...</span></div>
      <div class="connect-downloads">
        <a id="androidDownloadLink" href="/download/local-focus-mobile.apk" target="_blank" rel="noreferrer">Download Android app (APK)</a>
        <a id="macDownloadLink" href="/download/local-focus-macos.dmg" target="_blank" rel="noreferrer">Download Mac app (DMG)</a>
      </div>
    </details>
    <div>
      <strong>Connected devices</strong>
      <div id="deviceList" class="device-list"></div>
    </div>
  </section>
  </div>
</main>
<script>
// Look-and-feel templates: swap the CSS variable palette via a data-theme
// attribute and remember the choice on this device.
function setTheme(value) {
  const theme = ['vibrant', 'cyber', 'clay', 'minimal', 'professional'].includes(value) ? value : 'vibrant';
  document.documentElement.dataset.theme = theme;
  try { localStorage.setItem('lfTheme', theme); } catch (e) {}
  const select = document.querySelector('#themeSelect');
  if (select) select.value = theme;
}
(function initTheme() {
  let saved = 'vibrant';
  try { saved = localStorage.getItem('lfTheme') || 'vibrant'; } catch (e) {}
  setTheme(saved);
})();
// One screen, one job: the dashboard used to stack focus setup, blocking,
// journal, reports, and pairing on a single scroll, so "am I focused right
// now?" competed with everything else. Each group is its own view now, and
// the choice is remembered per device.
const DASHBOARD_VIEWS = ['focus', 'rules', 'journal', 'reports', 'devices'];
function showView(name) {
  const target = DASHBOARD_VIEWS.includes(name) ? name : 'focus';
  for (const view of DASHBOARD_VIEWS) {
    const panel = document.querySelector(`#view-${view}`);
    if (panel) panel.hidden = view !== target;
  }
  for (const tab of document.querySelectorAll('.view-tab')) {
    const active = tab.dataset.view === target;
    tab.classList.toggle('is-active', active);
    tab.setAttribute('aria-current', active ? 'page' : 'false');
  }
  try { localStorage.setItem('lfView', target); } catch (e) {}
}
(function initView() {
  let saved = 'focus';
  try { saved = localStorage.getItem('lfView') || 'focus'; } catch (e) {}
  showView(saved);
})();
// Shrink the sticky header once the page scrolls past the focus setup.
(function initHeaderShrink() {
  const header = document.querySelector('header');
  if (!header) return;
  const onScroll = () => header.classList.toggle('compact', window.scrollY > 56);
  window.addEventListener('scroll', onScroll, { passive: true });
  onScroll();
})();
const focusDraftKey = 'local-focus-draft';
let focusEditorManuallyOpened = false;
// Tracks which session's values we've already loaded into the editor, so the
// 10s refresh doesn't overwrite edits the user is making to the active session.
let lastSeededFocusStart = null;
let focusTargets = [];
let currentFocusReport = null;
let calendarDate = new Date();
let selectedReportDate = new Date();
let activeReportPeriod = 'day';
let activeReportYear = selectedReportDate.getFullYear();
let activeReportMonth = selectedReportDate.getMonth();
let activeReportWeek = 0;
let blockedRules = [];
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
  const actionSeconds = encodeURIComponent(Math.max(1, Number(document.querySelector('#actionMinutes').value || '2')) * 60);
  const alertAction = encodeURIComponent(document.querySelector('#alertAction').value || 'alert');
  const alertMessage = encodeURIComponent(document.querySelector('#alertMessage').value || DEFAULT_ALERT_MESSAGE_TEMPLATE);
  const redirectApp = encodeURIComponent(document.querySelector('#redirectApp').value || '');
  const lockInput = document.querySelector('#lockSession');
  const lock = lockInput && lockInput.checked ? '1' : '0';
  const jumpGuardInput = document.querySelector('#jumpGuard');
  const jumpGuard = !jumpGuardInput || jumpGuardInput.checked ? '1' : '0';
  if (lock === '1' && !confirm(`Lock this session for ${document.querySelector('#minutes').value || '25'} minutes?\n\nYou will not be able to pause it, stop it, or change your block rules until the timer ends.`)) return;
  await fetch(`/api/focus/start?task=${task}&target=${target}&minutes=${mins}&alertSeconds=${alertSeconds}&actionSeconds=${actionSeconds}&alertAction=${alertAction}&alertMessage=${alertMessage}&redirectApp=${redirectApp}&lock=${lock}&jumpGuard=${jumpGuard}`);
  refresh();
}
async function saveFocusEdits() {
  saveFocusDraft();
  const task = encodeURIComponent(document.querySelector('#task').value || 'Deep work');
  const target = encodeURIComponent(document.querySelector('#target').value || '');
  const mins = encodeURIComponent(document.querySelector('#minutes').value || '25');
  const alertSeconds = encodeURIComponent(Math.max(1, Number(document.querySelector('#alertMinutes').value || '1')) * 60);
  const actionSeconds = encodeURIComponent(Math.max(1, Number(document.querySelector('#actionMinutes').value || '2')) * 60);
  const alertAction = encodeURIComponent(document.querySelector('#alertAction').value || 'alert');
  const alertMessage = encodeURIComponent(document.querySelector('#alertMessage').value || DEFAULT_ALERT_MESSAGE_TEMPLATE);
  const redirectApp = encodeURIComponent(document.querySelector('#redirectApp').value || '');
  const button = document.querySelector('#saveFocusEdits');
  if (button) { button.disabled = true; button.textContent = 'Saving...'; }
  await fetch(`/api/focus/update?task=${task}&target=${target}&minutes=${mins}&alertSeconds=${alertSeconds}&actionSeconds=${actionSeconds}&alertAction=${alertAction}&alertMessage=${alertMessage}&redirectApp=${redirectApp}`);
  if (button) { button.disabled = false; button.textContent = 'Save changes'; }
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
// "How often you jumped" is meant to be readable at a glance, so it leads with
// a plain sentence and a bar per hour instead of a row of rate figures. Bars
// are coloured by how jumpy that hour was, against this hour-by-hour rule of
// thumb: a handful of switches an hour is normal, dozens is fragmented.
const CALM_HOUR_SWITCHES = 6;
const BUSY_HOUR_SWITCHES = 20;
const MAX_CHART_HOURS = 24;
/// Turns the sparse hours the server sends into a continuous run of clock
/// hours, so a gap in the chart means "quiet hour" rather than "missing bar".
function fillQuietHours(hours) {
  if (!hours.length) return [];
  const HOUR = 3600;
  const first = hours[0].start;
  const last = hours[hours.length - 1].start;
  const counts = new Map(hours.map(hour => [hour.start, hour.switches || 0]));
  const span = Math.floor((last - first) / HOUR) + 1;
  // A very long gap (a machine idle for days) would make the chart unreadable,
  // so fall back to just the hours that had activity.
  if (span > MAX_CHART_HOURS) return hours;
  const series = [];
  for (let start = first; start <= last; start += HOUR) {
    series.push({ start, switches: counts.get(start) || 0 });
  }
  return series;
}
function renderSwitchReport(switches) {
  const total = switches.totalSwitches || 0;
  const gap = switches.minutesBetweenSwitches || 0;
  const headline = document.querySelector('#switchHeadline');
  if (headline) {
    setHtml(headline, total === 0
      ? 'No jumps recorded yet.'
      : `You jumped <span class="switch-count">${total}</span> time${total === 1 ? '' : 's'}${gap >= 1 ? ` — about once every ${Math.round(gap)} minute${Math.round(gap) === 1 ? '' : 's'}.` : '.'}`);
  }

  // The server only sends hours that had a jump. Drawing those side by side
  // would imply they were consecutive, so fill the quiet hours back in — an
  // empty bar is the honest picture of an hour you were away or heads-down.
  const series = fillQuietHours(switches.byHour || []);
  const peak = series.reduce((most, hour) => Math.max(most, hour.switches || 0), 0);
  setHtml('#switchChart', series.length === 0
    ? '<div class="switch-chart-empty">Nothing to show yet. Bars appear here as you use your Mac.</div>'
    : series.map(hour => {
        const count = hour.switches || 0;
        // Small counts still get a visible stub; zero stays visibly empty.
        const height = count === 0 ? 0 : peak > 0 ? Math.max(12, Math.round((count / peak) * 96)) : 12;
        const tone = count === 0 ? 'is-quiet' : count <= CALM_HOUR_SWITCHES ? 'is-calm' : count >= BUSY_HOUR_SWITCHES ? 'is-busy' : '';
        const label = new Date((hour.start || 0) * 1000).toLocaleTimeString([], { hour: 'numeric' });
        return `<div class="switch-bar ${tone}" title="${escapeTextAttr(`${count} jump${count === 1 ? '' : 's'} at ${label}`)}">
          <span class="switch-bar-value">${count === 0 ? '' : count}</span>
          <div class="switch-bar-track">${count === 0 ? '' : `<div class="switch-bar-fill" style="height:${height}px"></div>`}</div>
          <span class="switch-bar-label">${escapeHtml(label)}</span>
        </div>`;
      }).join(''));

  const longest = document.querySelector('#switchLongest');
  if (longest) longest.textContent = switches.longestCalmSeconds ? formatDuration(switches.longestCalmSeconds) : '--';
  const distracting = document.querySelector('#switchDistracting');
  if (distracting) distracting.textContent = total === 0 ? '--' : String(switches.distractingSwitches || 0);

  const targets = switches.topSwitchTargets || [];
  const busiest = targets.reduce((most, target) => Math.max(most, target.switches || 0), 0);
  setHtml('#switchTargets', targets.map(target => {
    const count = target.switches || 0;
    const width = busiest > 0 ? Math.max(3, Math.round((count / busiest) * 100)) : 3;
    return `<div class="switch-target-row">
      <span>${escapeHtml(target.app)}</span>
      <div class="switch-target-track"><div class="switch-target-fill" style="width:${width}%"></div></div>
      <span class="switch-target-count">${count}</span>
    </div>`;
  }).join('') || '<div class="muted">No jumps recorded yet.</div>');
}
function normalizedBlockValue(value) {
  return String(value || '').trim().toLowerCase();
}
// One blank row at a time, held client-side until it has enough to save.
let draftBlockRow = null;
function addBlockRow() {
  if (draftBlockRow) {
    const existing = document.querySelector('#blockRows .block-row-draft .block-target');
    if (existing) existing.focus();
    return;
  }
  draftBlockRow = { target: '', mode: 'full', password: '' };
  renderBlockTable(true);
  const input = document.querySelector('#blockRows .block-row-draft .block-target');
  if (input) input.focus();
}
function discardBlockDraft() {
  draftBlockRow = null;
  renderBlockTable(true);
}
/// A password rule with no password yet cannot be saved, so the row stays
/// pending (highlighted, password focused) instead of being written half-formed.
function blockRowIsSavable(row) {
  if (!normalizedBlockValue(row.target)) return false;
  if (row.mode === 'password') return Boolean(row.password) || row.hasPassword;
  return true;
}
async function saveBlockRow(row, original) {
  const params = new URLSearchParams();
  params.set('keyword', row.target);
  params.set('mode', row.mode);
  params.set('password', row.password || '');
  params.set('original', original || '');
  await fetch(`/api/block/add?${params.toString()}`);
  draftBlockRow = null;
  await refresh();
  // refresh() leaves the table alone while it has focus, which is right for the
  // background poll but would strand the row we just saved (and its now-stale
  // draft). This edit is ours, so redraw it.
  renderBlockTable(true);
}
/// Reads a row's controls, saves when it is complete, and otherwise leaves it
/// pending. `original` is the target as the server knows it, so renaming a rule
/// replaces it rather than creating a second one.
async function commitBlockRow(element) {
  const tr = element.closest('tr');
  if (!tr) return;
  const original = tr.dataset.target || '';
  const row = {
    target: (tr.querySelector('.block-target') || {}).value || '',
    mode: tr.querySelector('.block-mode-password') && tr.querySelector('.block-mode-password').checked ? 'password' : 'full',
    password: (tr.querySelector('.block-password') || {}).value || '',
    hasPassword: tr.dataset.hasPassword === 'true' && tr.dataset.mode === 'password'
  };
  if (draftBlockRow && tr.classList.contains('block-row-draft')) {
    draftBlockRow = row;
  }
  if (!blockRowIsSavable(row)) {
    tr.classList.add('block-row-pending');
    if (normalizedBlockValue(row.target) && row.mode === 'password') {
      const password = tr.querySelector('.block-password');
      if (password) password.focus();
    }
    return;
  }
  tr.classList.remove('block-row-pending');
  await saveBlockRow(row, original);
}
/// Checkboxes act as one choice: turning either on turns the other off.
function setBlockRowMode(element, mode) {
  const tr = element.closest('tr');
  if (!tr) return;
  const full = tr.querySelector('.block-mode-full');
  const password = tr.querySelector('.block-mode-password');
  if (full) full.checked = mode === 'full';
  if (password) password.checked = mode === 'password';
  syncBlockPasswordColumn();
  syncBlockRowPasswordCell(tr);
  commitBlockRow(element);
}
/// Only rows that use a password get a password box; the rest show a dash, so
/// an empty field never implies a rule has a password it does not. Swapped in
/// place rather than by re-rendering, which would revert the checkbox to
/// whatever the server still thinks the mode is.
function syncBlockRowPasswordCell(tr) {
  const cell = tr.querySelector('.block-col-password');
  if (!cell) return;
  const wantsPassword = Boolean(tr.querySelector('.block-mode-password') && tr.querySelector('.block-mode-password').checked);
  const existing = cell.querySelector('.block-password');
  if (wantsPassword && !existing) {
    const label = tr.querySelector('.block-target');
    const placeholder = tr.dataset.hasPassword === 'true' ? 'Saved. Type to replace' : 'Password to continue';
    cell.innerHTML = `<input class="block-password" type="password" value="" placeholder="${escapeTextAttr(placeholder)}" aria-label="Password for ${escapeTextAttr((label && label.value) || 'new rule')}" onchange="commitBlockRow(this)">`;
    const input = cell.querySelector('.block-password');
    if (input) input.focus();
  } else if (!wantsPassword && existing) {
    cell.innerHTML = '<span class="muted">&mdash;</span>';
  }
}
/// The password column appears as soon as any row uses one.
function syncBlockPasswordColumn() {
  const table = document.querySelector('#blockTable');
  if (!table) return;
  const anyPassword = [...table.querySelectorAll('.block-mode-password')].some(input => input.checked);
  table.classList.toggle('show-password', anyPassword);
}
function blockRowMarkup(rule, isDraft) {
  const target = rule.target || '';
  const password = rule.mode === 'password';
  const placeholder = password && rule.hasPassword
    ? 'Saved. Type to replace'
    : 'Password to continue';
  return `<tr data-target="${escapeTextAttr(isDraft ? '' : target)}" data-mode="${escapeTextAttr(rule.mode || 'full')}" data-has-password="${rule.hasPassword ? 'true' : 'false'}" class="${isDraft ? 'block-row-draft' : ''}">
    <td><input class="block-target" type="text" value="${escapeTextAttr(target)}" placeholder="youtube.com, Messages" aria-label="Blocked site or app" onchange="commitBlockRow(this)"></td>
    <td class="block-col-check"><input class="block-mode-full" type="checkbox" ${password ? '' : 'checked'} aria-label="Full block for ${escapeTextAttr(target || 'new rule')}" onchange="setBlockRowMode(this, 'full')"></td>
    <td class="block-col-check"><input class="block-mode-password" type="checkbox" ${password ? 'checked' : ''} aria-label="Password block for ${escapeTextAttr(target || 'new rule')}" onchange="setBlockRowMode(this, 'password')"></td>
    <td class="block-col-password">${password ? `<input class="block-password" type="password" value="" placeholder="${escapeTextAttr(placeholder)}" aria-label="Password for ${escapeTextAttr(target || 'new rule')}" onchange="commitBlockRow(this)">` : '<span class="muted">&mdash;</span>'}</td>
    <td><button type="button" class="block-remove" aria-label="Remove ${escapeTextAttr(target || 'new rule')}" onclick="${isDraft ? 'discardBlockDraft()' : `removeBlock('${escapeTextAttr(target)}')`}">Remove</button></td>
  </tr>`;
}
/// Rebuilds the table from `blockedRules` plus any draft row. Skipped while the
/// user is typing inside it, so the 10s poll cannot pull the row out from under
/// them; `force` is for edits we made ourselves.
function renderBlockTable(force) {
  const body = document.querySelector('#blockRows');
  if (!body) return;
  const table = document.querySelector('#blockTable');
  if (!force && table && table.contains(document.activeElement)) return;
  const rows = blockedRules.map(rule => blockRowMarkup(rule, false));
  if (draftBlockRow) rows.push(blockRowMarkup(draftBlockRow, true));
  const markup = rows.join('') || '<tr class="block-empty"><td colspan="5">Nothing blocked yet. Add a row to block a site or app.</td></tr>';
  if (force) {
    body.innerHTML = markup;
    renderedHtml.set(body, markup);
  } else {
    setHtml(body, markup);
  }
  syncBlockPasswordColumn();
}
async function removeBlock(target) {
  await fetch(`/api/block/remove?keyword=${encodeURIComponent(target)}`);
  await refresh();
  renderBlockTable(true);
}
function connectedDeviceRowMarkup(device) {
  const endpoint = device.endpoint || '';
  const note = endpoint.startsWith('browser:')
    ? 'Receiver browser connected.'
    : 'Companion app connected.';
  const kind = device.kind || 'device';
  return `<div class="device-pill"><strong>${escapeHtml(device.name || 'Device')}</strong><br><span class="muted">${escapeHtml(deviceKindLabel(kind))}<br>${escapeHtml(note)}</span></div>`;
}
function deviceKindLabel(kind) {
  const labels = {phone:'Phone', tv:'TV', tablet:'Tablet', laptop:'Laptop', desktop:'Desktop', router:'Router', device:'Device'};
  return labels[kind] || 'Device';
}
const CONNECT_CODE_ALPHABET = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
// Encodes an IPv4 host (the port is always 4799) into an 8-character connection
// code: four address octets plus a checksum byte, rendered as Crockford base32.
// The companion app reverses this to rebuild http://<ip>:4799 and connect.
function encodeConnectCode(host) {
  const match = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(String(host || '').trim());
  if (!match) return '';
  const octets = [Number(match[1]), Number(match[2]), Number(match[3]), Number(match[4])];
  if (octets.some(value => value > 255)) return '';
  const checksum = (octets[0] + octets[1] + octets[2] + octets[3]) & 0xff;
  const bits = [...octets, checksum].map(byte => byte.toString(2).padStart(8, '0')).join('');
  let code = '';
  for (let i = 0; i < 40; i += 5) code += CONNECT_CODE_ALPHABET[parseInt(bits.slice(i, i + 5), 2)];
  return `${code.slice(0, 4)}-${code.slice(4)}`;
}
function hostFromUrl(value) {
  try { return new URL(value).hostname; } catch { return ''; }
}
async function copyConnectCode() {
  const code = (document.querySelector('#connectCode')?.textContent || '').trim();
  const hint = document.querySelector('#connectCodeHint');
  if (!/^[0-9A-Z]{4}-[0-9A-Z]{4}$/.test(code)) return;
  try {
    await navigator.clipboard.writeText(code);
    if (hint) hint.textContent = `Copied ${code}. Enter it in the companion app's Connection code field to connect.`;
  } catch {
    const range = document.createRange();
    range.selectNodeContents(document.querySelector('#connectCode'));
    const selection = window.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
  }
}
function saveFocusDraft() {
  localStorage.setItem(focusDraftKey, JSON.stringify({
    target: document.querySelector('#target').value,
    task: document.querySelector('#task').value,
    minutes: document.querySelector('#minutes').value,
    alertMinutes: document.querySelector('#alertMinutes').value,
    actionMinutes: document.querySelector('#actionMinutes').value,
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
    if (draft.actionMinutes) document.querySelector('#actionMinutes').value = draft.actionMinutes;
    if (draft.alertAction) document.querySelector('#alertAction').value = draft.alertAction;
    if (draft.alertMessage) document.querySelector('#alertMessage').value = draft.alertMessage;
    if (draft.redirectApp) document.querySelector('#redirectApp').value = draft.redirectApp;
  } catch {}
  ['#task', '#minutes', '#alertMinutes', '#actionMinutes', '#alertAction', '#alertMessage', '#redirectApp'].forEach(selector => {
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
  } catch (error) {
    panel.innerHTML = `<div class="report-head"><p class="muted">Could not generate report.</p><button class="report-close" onclick="closeFocusReport()" aria-label="Close report">X</button></div>`;
    panel.classList.add('open');
  }
}
function closeFocusReport() {
  const panel = document.querySelector('#focusReportPanel');
  panel.classList.remove('open');
  panel.innerHTML = '';
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
  const [timeline, report, state, history, switches] = await Promise.all([
    fetch('/api/timeline').then(r => r.json()),
    fetch('/api/report').then(r => r.json()),
    fetch('/api/state').then(r => r.json()),
    fetch('/api/report/history').then(r => r.json()),
    fetch('/api/report/switches').then(r => r.json())
  ]);
  activeFocusSession = state.focus || null;
  const stopBanner = document.querySelector('#stopBanner');
  if (stopBanner) stopBanner.style.display = state.stopped ? 'flex' : 'none';
  const totalSeconds = reportTotalSeconds(report);
  setHtml('#metrics', `
    <div class="metric"><span class="muted">Total time</span><strong>${formatDuration(totalSeconds)}</strong></div>
    <div class="metric"><span class="muted">Productive</span><strong>${formatDuration(report.productiveMinutes * 60)}</strong></div>
    <div class="metric"><span class="muted">Distracted</span><strong>${formatDuration(report.distractingMinutes * 60)}</strong></div>
    <div class="metric"><span class="muted">Idle</span><strong>${formatDuration((report.idleMinutes || 0) * 60)}</strong></div>`);
  renderSwitchReport(switches);
  setHtml('#timeline', timeline.slice(-80).reverse().map((item, index) => {
    const longAttention = item.durationSeconds > 15 * 60 && (item.category === 'idle' || item.category === 'distracting');
    const longClass = longAttention ? ` long-attention ${item.category === 'idle' ? 'long-idle' : 'long-distracting'}` : '';
    const longNote = longAttention ? `<span class="long-note">${item.category === 'idle' ? 'Long idle' : 'Long distraction'}</span>` : '';
    return `
    <div class="item${longClass}">
      <div class="muted">${fmtTime(item.start)}<br>${formatDuration(item.durationSeconds)}${longNote ? `<br>${longNote}` : ''}</div>
      <div><strong>${escapeHtml(item.app)}</strong><div>${escapeHtml(item.title)}</div><div class="muted">${sourceMarkup(item.source || 'local', `timeline-${index}`)}</div></div>
      <div class="tag ${item.category}">${item.category}</div>
    </div>`;
  }).join('') || '<div class="muted">No activity yet.</div>');
  setHtml('#apps', report.topApps.map((app, index) => `<p><strong>${escapeHtml(app.app)}</strong><br>${sourceMarkup(app.source || 'local', index)}<br><span class="muted">${formatDuration(app.seconds || app.minutes * 60)}</span></p>`).join('') || '<div class="muted">No apps yet.</div>');
  blockedRules = (state.blockedRules || []).map(rule => ({...rule, target: normalizedBlockValue(rule.target || '')}));
  renderBlockTable(false);
  const connectUrl = state.deviceConnectUrl || 'http://127.0.0.1:4799/device';
  document.querySelector('#deviceConnectUrl').textContent = connectUrl;
  const connectHost = hostFromUrl(connectUrl);
  const connectCode = encodeConnectCode(connectHost);
  const connectCodeEl = document.querySelector('#connectCode');
  if (connectCodeEl) connectCodeEl.textContent = connectCode || 'Unavailable';
  const androidLink = document.querySelector('#androidDownloadLink');
  if (androidLink) androidLink.href = state.androidAppUrl || `${location.origin}/download/local-focus-mobile.apk`;
  const macLink = document.querySelector('#macDownloadLink');
  if (macLink) macLink.href = state.macAppUrl || `${location.origin}/download/local-focus-macos.dmg`;
  const connectedDevices = (state.devices || []).filter(device => String(device.endpoint || '').startsWith('browser:') || String(device.endpoint || '').startsWith('mobile:'));
  setHtml('#deviceList', connectedDevices.map(connectedDeviceRowMarkup).join('') || '<div class="muted">No devices connected yet.</div>');
  setHtml('#historyList', history.map(item => {
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
  }).join('') || '<div class="muted">No previous reports yet.</div>');
  updateFocusButtons(state.focus, state.stopped);
  updateFocusTimer(state.focus, state.stopped);
  seedFocusInputsFromActiveSession(state.focus);
  updateFocusSummary(state.focus);
  if (state.stopped) {
    const chip = document.querySelector('#focusState');
    chip.textContent = 'Local Focus off';
    chip.className = 'status-chip paused';
  }
  updateJournalControlState(state.journal);
}
// The dashboard re-polls every 10s and used to rewrite every list wholesale
// each time, which threw away scroll position, text selection, and hover
// state even when nothing had actually changed. Rendering to a string is
// cheap; touching the DOM is not — so only write when the markup differs.
const renderedHtml = new WeakMap();
function setHtml(target, html) {
  const node = typeof target === 'string' ? document.querySelector(target) : target;
  if (!node) return;
  if (renderedHtml.get(node) === html) return;
  renderedHtml.set(node, html);
  node.innerHTML = html;
}
// Live session countdown. The server already sends remainingSeconds on every
// /api/state poll; it is authoritative, and the local 1s tick below just fills
// the gap between polls so the number moves the way a timer should. Time
// blindness is the whole reason this is the biggest element in the panel.
const TIMER_RING_CIRCUMFERENCE = 2 * Math.PI * 54;
let focusTimerState = { remaining: 0, duration: 1500, active: false, paused: false, stopped: false };
function updateFocusTimer(focus, stopped) {
  focusTimerState = {
    remaining: Math.max(0, Number(focus && focus.remainingSeconds) || 0),
    duration: Math.max(1, (Number(focus && focus.durationMinutes) || 25) * 60),
    active: Boolean(focus) && !stopped,
    paused: Boolean(focus && focus.paused),
    stopped: Boolean(stopped)
  };
  renderFocusTimer();
}
function renderFocusTimer() {
  const shell = document.querySelector('#focusTimer');
  const value = document.querySelector('#timerValue');
  const caption = document.querySelector('#timerCaption');
  const ring = document.querySelector('#timerRingProgress');
  if (!shell || !value || !caption || !ring) return;
  const { remaining, duration, active, paused, stopped } = focusTimerState;
  // The session keeps tracking after the timer runs out, so "done" is its own
  // state rather than an end state.
  const done = active && remaining <= 0;
  shell.className = `focus-timer ${!active ? 'is-idle' : done ? 'is-done' : paused ? 'is-paused' : ''}`;
  value.textContent = active ? formatClock(remaining) : '--:--';
  caption.textContent = stopped ? 'Local Focus is off'
    : !active ? 'No session running'
    : paused ? 'Paused'
    : done ? 'Time is up, still tracking'
    : 'Left in this session';
  const fraction = done ? 1 : active ? Math.min(1, remaining / duration) : 0;
  ring.style.strokeDasharray = TIMER_RING_CIRCUMFERENCE;
  ring.style.strokeDashoffset = TIMER_RING_CIRCUMFERENCE * (1 - fraction);
}
function formatClock(totalSeconds) {
  const seconds = Math.max(0, Math.round(totalSeconds));
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const pad = part => String(part).padStart(2, '0');
  return hours > 0 ? `${hours}:${pad(minutes)}:${pad(seconds % 60)}` : `${pad(minutes)}:${pad(seconds % 60)}`;
}
setInterval(() => {
  if (!focusTimerState.active || focusTimerState.paused || focusTimerState.remaining <= 0) return;
  focusTimerState.remaining -= 1;
  renderFocusTimer();
}, 1000);
function updateFocusSummary(focus) {
  const chip = document.querySelector('#focusState');
  const details = document.querySelector('#focusDetails');
  const quickTask = document.querySelector('#quickTask');
  const quickStatus = document.querySelector('#quickStatus');
  const quickDelay = document.querySelector('#quickDelay');
  const quickAction = document.querySelector('#quickAction');
  updateHighFocusControls(focus);
  if (!focus) {
    chip.textContent = 'No session';
    chip.className = 'status-chip';
    setHtml(details, `<div class="detail-grid">
      <div class="detail-card"><span>Focus apps/sites</span><strong>None active</strong></div>
      <div class="detail-card"><span>Warn me after</span><strong>Off</strong></div>
      <div class="detail-card"><span>Off-task action</span><strong>Start a session to turn on warnings</strong></div>
    </div>`);
    quickTask.textContent = 'None';
    quickStatus.textContent = 'No session';
    quickDelay.textContent = '1m';
    quickAction.textContent = 'Just warn me';
    focusEditorManuallyOpened = false;
    setFocusEditorOpen(true);
    return;
  }
  const paused = Boolean(focus.paused);
  chip.textContent = paused ? 'Paused' : 'Focusing';
  chip.className = `status-chip ${paused ? 'paused' : 'running'}`;
  const action = focus.alertAction === 'switch' && focus.redirectApp ? `switch to ${focus.redirectApp}` : 'just warn me';
  const alertMessage = focus.alertMessage || DEFAULT_ALERT_MESSAGE_TEMPLATE;
  const targets = String(focus.target || '').split(/[,\n]/).map(value => value.trim()).filter(Boolean);
  const targetChips = targets.map(value => `<span class="target-chip">${escapeHtml(shortenSource(value))}</span>`).join('') || '<span class="target-chip">No target set</span>';
  setHtml(details, `
    <div class="target-chips">${targetChips}</div>
    <div class="detail-grid">
      <div class="detail-card"><span>Full focus list</span><strong>${escapeHtml(focus.target || 'No target set')}</strong></div>
      <div class="detail-card"><span>Warn me after</span><strong>${formatDuration(focus.alertDelaySeconds || 60)} off task</strong></div>
      <div class="detail-card"><span>Off-task action</span><strong>${escapeHtml(action)}</strong></div>
      <div class="detail-card"><span>Warning message</span><strong>${escapeHtml(alertMessage)}</strong></div>
    </div>`);
  quickTask.textContent = focus.task || 'Focus session';
  quickStatus.textContent = paused ? 'Paused' : 'Focusing';
  quickDelay.textContent = formatDuration(focus.alertDelaySeconds || 60);
  quickAction.textContent = focus.alertAction === 'switch' && focus.redirectApp ? 'Switch me' : 'Just warn me';
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
        ? 'Resume the session to change High Focus mode.'
        : 'Block every active app or website outside the focus list.';
}
function seedFocusInputsFromActiveSession(focus) {
  if (!focus) { lastSeededFocusStart = null; return; }
  // Only load the session's values into the form once (when it starts), so the
  // periodic refresh never reverts edits the user is making.
  if (focus.startedAt === lastSeededFocusStart) return;
  lastSeededFocusStart = focus.startedAt;
  const taskInput = document.querySelector('#task');
  const targetInput = document.querySelector('#target');
  const minutesInput = document.querySelector('#minutes');
  const alertInput = document.querySelector('#alertMinutes');
  const actionTimeInput = document.querySelector('#actionMinutes');
  const actionInput = document.querySelector('#alertAction');
  const messageInput = document.querySelector('#alertMessage');
  const redirectInput = document.querySelector('#redirectApp');
  if (focus.task) taskInput.value = focus.task;
  if (focus.target && targetInput.value !== focus.target) setFocusTargets(focus.target);
  if (focus.durationMinutes) minutesInput.value = focus.durationMinutes;
  if (focus.alertDelaySeconds) alertInput.value = Math.max(1, Math.round(focus.alertDelaySeconds / 60));
  if (focus.actionDelaySeconds && actionTimeInput) actionTimeInput.value = Math.max(1, Math.round(focus.actionDelaySeconds / 60));
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
  startButton.textContent = stopped ? 'Start focus' : paused ? 'Start new session' : running ? 'Focusing' : 'Start focus';
  // A locked session refuses these server-side; disabling them here just
  // stops the buttons lying about what they will do.
  const lockHolds = Boolean(focus && focus.lockActive);
  pauseButton.disabled = !focus || Boolean(stopped) || lockHolds;
  // Pause uses the same treatment as the off switch so it reads differently
  // from the green "Focusing" button.
  pauseButton.className = `focus-btn ${paused ? 'focus-paused' : running ? 'focus-stop-active' : ''}`;
  // "session" is explicit here so this never reads as the master switch below.
  pauseButton.textContent = paused ? 'Resume session' : 'Pause session';
  // This is the master off switch, not an end-this-session button: it is
  // available whenever the app is running, even without an active focus
  // session, and disabled once already off. It pairs with the banner's
  // "Resume Local Focus".
  stopButton.disabled = Boolean(stopped) || lockHolds;
  stopButton.className = `focus-btn ${stopped ? '' : 'focus-stop-active'}`;
  stopButton.title = lockHolds
    ? 'Locked until this session\'s timer ends'
    : 'Turn off all tracking, blocking, warnings, and reminders until you resume';
  if (lockHolds) pauseButton.title = 'Locked until this session\'s timer ends';
  // The lock is chosen before a session starts, so the control only applies
  // while one is not running.
  const lockInput = document.querySelector('#lockSession');
  const lockHint = document.querySelector('#lockSessionHint');
  if (lockInput) {
    lockInput.disabled = Boolean(focus) || Boolean(stopped);
    if (focus) lockInput.checked = Boolean(focus.locked);
  }
  const jumpInput = document.querySelector('#jumpGuard');
  if (jumpInput) {
    jumpInput.disabled = Boolean(focus) || Boolean(stopped);
    if (focus) jumpInput.checked = Boolean(focus.jumpGuard);
  }
  const jumpHint = document.querySelector('#jumpGuardHint');
  if (jumpHint) {
    const recent = focus ? (focus.recentJumps || 0) : 0;
    jumpHint.textContent = focus && recent >= 3
      ? `${recent} jumps in the last 5 minutes.`
      : 'A nudge when you switch apps over and over.';
    jumpHint.className = focus && recent >= 12 ? 'switch-count' : 'muted';
  }
  if (lockHint) {
    lockHint.textContent = lockHolds
      ? 'Locked. Pause, stop, and block edits are unavailable until the timer ends.'
      : focus
        ? 'Set before starting a session.'
        : 'Blocks hold until the timer ends.';
  }
  // Show the editor's Save button only while a session is active to edit.
  const saveEditsButton = document.querySelector('#saveFocusEdits');
  if (saveEditsButton) saveEditsButton.style.display = focus && !stopped ? '' : 'none';
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
    <p class="muted">This page connects only the device that opens this exact link. Local Focus does not scan for nearby devices.</p>
    <p><code>{lan_url}</code></p>
  </section>
  <section>
    <h2>Android phone or tablet</h2>
    <p class="muted">Download the installable APK. After installing, open Local Focus, connect, and allow Usage Access for app tracking.</p>
    <div class="actions"><a class="button" href="{android_url}">Download Android app</a></div>
  </section>
  <section>
    <h2>iPhone or iPad receiver</h2>
    <p class="muted">This page cannot install a native iPhone app. Apple requires Xcode, TestFlight, App Store, or a signed enterprise/ad-hoc package for iOS app installation. Use this receiver link now to receive Local Focus alerts.</p>
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
    <p class="muted">Connect this phone, TV, tablet, or laptop with the connection code or direct link to receive Local Focus alerts. Local Focus does not scan for nearby devices.</p>
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

/// Split a block input into individual, trimmed, lowercased keywords. Commas and
/// newlines separate entries, so "youtube, reddit, games" becomes three blocks.
/// Duplicates and blanks are dropped.
fn split_block_keywords(raw: &str) -> Vec<String> {
    let mut keywords: Vec<String> = Vec::new();
    for part in raw.split([',', '\n']) {
        let keyword = part.trim().to_lowercase();
        if !keyword.is_empty() && !keywords.contains(&keyword) {
            keywords.push(keyword);
        }
    }
    keywords
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
            "{{\"task\":\"{}\",\"target\":\"{}\",\"startedAt\":{},\"durationMinutes\":{},\"breakMinutes\":{},\"pausedAt\":{},\"pausedTotalSeconds\":{},\"pomodoroAlertedAt\":{},\"alertDelaySeconds\":{},\"actionDelaySeconds\":{},\"alertAction\":\"{}\",\"alertMessage\":\"{}\",\"redirectApp\":\"{}\",\"highFocusMode\":{},\"locked\":{},\"jumpGuard\":{}}}",
            json_escape(&focus.task),
            json_escape(&focus.target),
            focus.started_at,
            focus.duration_minutes,
            focus.break_minutes,
            paused_at,
            focus.paused_total_seconds,
            pomodoro_alerted_at,
            focus.alert_delay_seconds,
            focus.action_delay_seconds,
            json_escape(&focus.alert_action),
            json_escape(&clean_alert_message_template(&focus.alert_message)),
            json_escape(&focus.redirect_app),
            focus.high_focus_mode,
            focus.locked,
            focus.jump_guard
        ),
    )
}

/// The settings from the last session started, kept after the session itself
/// is cleared. Quick-start entry points (the menu bar extra, `/api/focus/start`
/// with no parameters) reuse these so starting a session without opening the
/// dashboard doesn't silently drop the focus list the user set up.
fn save_last_focus_settings(data_dir: &Path, focus: &FocusSession) -> io::Result<()> {
    fs::write(
        data_dir.join("last_focus.json"),
        format!(
            "{{\"task\":\"{}\",\"target\":\"{}\",\"durationMinutes\":{},\"alertDelaySeconds\":{},\"actionDelaySeconds\":{},\"alertAction\":\"{}\",\"alertMessage\":\"{}\",\"redirectApp\":\"{}\"}}",
            json_escape(&focus.task),
            json_escape(&focus.target),
            focus.duration_minutes,
            focus.alert_delay_seconds,
            focus.action_delay_seconds,
            json_escape(&focus.alert_action),
            json_escape(&clean_alert_message_template(&focus.alert_message)),
            json_escape(&focus.redirect_app),
        ),
    )
}

struct LastFocusSettings {
    task: String,
    target: String,
    duration_minutes: u64,
    alert_delay_seconds: u64,
    action_delay_seconds: u64,
    alert_action: String,
    alert_message: String,
    redirect_app: String,
}

fn load_last_focus_settings(data_dir: &Path) -> Option<LastFocusSettings> {
    let value = fs::read_to_string(data_dir.join("last_focus.json")).ok()?;
    Some(LastFocusSettings {
        task: json_string(&value, "task")?,
        target: json_string(&value, "target").unwrap_or_default(),
        duration_minutes: json_number(&value, "durationMinutes").unwrap_or(25) as u64,
        alert_delay_seconds: json_number(&value, "alertDelaySeconds")
            .unwrap_or(DEFAULT_ALERT_DELAY_SECONDS as i64) as u64,
        action_delay_seconds: json_number(&value, "actionDelaySeconds")
            .unwrap_or(DEFAULT_ACTION_DELAY_SECONDS as i64) as u64,
        alert_action: json_string(&value, "alertAction").unwrap_or_else(|| "alert".into()),
        alert_message: json_string(&value, "alertMessage")
            .unwrap_or_else(|| DEFAULT_ALERT_MESSAGE_TEMPLATE.into()),
        redirect_app: json_string(&value, "redirectApp").unwrap_or_default(),
    })
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
        action_delay_seconds: json_number(&value, "actionDelaySeconds")
            .map(|value| value.max(1) as u64)
            .unwrap_or(DEFAULT_ACTION_DELAY_SECONDS),
        alert_action: json_string(&value, "alertAction").unwrap_or_else(|| "alert".into()),
        alert_message: json_string(&value, "alertMessage")
            .map(|message| clean_alert_message_template(&message))
            .unwrap_or_else(|| DEFAULT_ALERT_MESSAGE_TEMPLATE.into()),
        redirect_app: json_string(&value, "redirectApp").unwrap_or_default(),
        high_focus_mode: json_bool(&value, "highFocusMode").unwrap_or(false),
        locked: json_bool(&value, "locked").unwrap_or(false),
        jump_guard: json_bool(&value, "jumpGuard").unwrap_or(true),
    })
}

fn clear_focus(data_dir: &Path) -> io::Result<()> {
    let path = data_dir.join("focus.json");
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Notifications waiting for the native macOS host to collect, plus when that
/// host last checked in. `osascript` can post a banner, but it posts it *as
/// osascript* — wrong name, wrong icon, and nothing the user can manage under
/// Local Focus in System Settings. So when the native host is running we hand
/// it the notification to post as itself, and fall back to `osascript` only
/// when nothing is there to collect (CLI `serve`, or notifications denied).
struct MacNotifications {
    host_seen_at: i64,
    queued: Vec<(i64, String, String)>,
}

static MAC_NOTIFICATIONS: OnceLock<Mutex<MacNotifications>> = OnceLock::new();
const MAC_HOST_TIMEOUT_SECONDS: i64 = 15;
const MAX_QUEUED_MAC_NOTIFICATIONS: usize = 50;

fn mac_notifications() -> &'static Mutex<MacNotifications> {
    MAC_NOTIFICATIONS.get_or_init(|| {
        Mutex::new(MacNotifications {
            host_seen_at: 0,
            queued: Vec::new(),
        })
    })
}

/// True when the native host has checked in recently enough to be trusted to
/// deliver the banner itself.
fn native_host_is_live(host_seen_at: i64, now: i64) -> bool {
    host_seen_at > 0 && now - host_seen_at <= MAC_HOST_TIMEOUT_SECONDS
}

fn notify(title: &str, message: &str) {
    #[cfg(target_os = "macos")]
    {
        let now_ts = now();
        let handled_by_host = {
            match mac_notifications().lock() {
                Ok(mut queue) => {
                    let live = native_host_is_live(queue.host_seen_at, now_ts);
                    if live {
                        queue
                            .queued
                            .push((now_ts, title.to_string(), message.to_string()));
                        // Bound the queue so a host that goes away mid-run
                        // cannot grow it without limit.
                        let overflow = queue
                            .queued
                            .len()
                            .saturating_sub(MAX_QUEUED_MAC_NOTIFICATIONS);
                        queue.queued.drain(..overflow);
                    }
                    live
                }
                Err(_) => false,
            }
        };

        if !handled_by_host {
            let _ = Command::new("osascript")
                .arg("-e")
                .arg(format!(
                    "display notification \"{}\" with title \"{}\"",
                    apple_escape(message),
                    apple_escape(title)
                ))
                .output();
        }
    }

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

// A browser tab always registers a brand-new `browser:<timestamp>` endpoint
// (see `/api/device/register`) and never reconnects to an old one, so once an
// entry stops being seen (registered or polled) it can never become live
// again. Drop any such entry the connect page hasn't touched recently, so
// closed tabs don't pile up in the persisted device list forever. Real
// companion devices (`mobile:...`) are untouched — they reconnect to a
// stable endpoint and are deduped by `/api/mobile/register` instead.
const BROWSER_DEVICE_TTL_SECONDS: i64 = 60;

fn prune_stale_browser_devices(
    records: &[String],
    last_seen: &HashMap<String, i64>,
    now: i64,
) -> Vec<String> {
    records
        .iter()
        .filter(|record| {
            let device = parse_network_device_record(record);
            if !device.endpoint.starts_with("browser:") {
                return true;
            }
            last_seen
                .get(&device.endpoint)
                .is_some_and(|seen| now - seen <= BROWSER_DEVICE_TTL_SECONDS)
        })
        .cloned()
        .collect()
}

// Runs on every tracking tick so a closed connect-page tab drops out of the
// persisted device list on its own, instead of sitting there forever.
fn prune_disconnected_browser_devices(
    data_dir: &Path,
    state: &Arc<Mutex<AppState>>,
) -> io::Result<()> {
    let now_ts = now();
    let config_snapshot = {
        let mut guard = lock_state(state);
        let pruned =
            prune_stale_browser_devices(&guard.config.network_devices, &guard.browser_last_seen, now_ts);
        if pruned.len() == guard.config.network_devices.len() {
            return Ok(());
        }
        guard.config.network_devices = pruned;
        guard
            .browser_last_seen
            .retain(|_, seen| now_ts - *seen <= BROWSER_DEVICE_TTL_SECONDS);
        guard.config.clone()
    };
    save_config(data_dir, &config_snapshot)
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
            action_delay_seconds: DEFAULT_ACTION_DELAY_SECONDS,
            alert_action: "alert".into(),
            alert_message: DEFAULT_ALERT_MESSAGE_TEMPLATE.into(),
            redirect_app: String::new(),
            high_focus_mode: true,
            locked: false,
            jump_guard: true,
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
    fn block_keywords_split_on_commas_and_dedupe() {
        assert_eq!(
            split_block_keywords("youtube, reddit, games"),
            vec!["youtube", "reddit", "games"]
        );
        // Trims, lowercases, and drops duplicates and blanks.
        assert_eq!(
            split_block_keywords("  YouTube , youtube ,, Reddit\nreddit "),
            vec!["youtube", "reddit"]
        );
        assert_eq!(split_block_keywords("solo"), vec!["solo"]);
        assert!(split_block_keywords("  ,  ,  ").is_empty());
    }

    #[test]
    fn move_to_app_action_switches_only_with_a_redirect_app() {
        // Regression guard for the "Move to app" warning action: it must switch
        // to the redirect app when one is set...
        assert!(focus_alert_switches_app("switch", "Pages"));
        // ...but fall back to a plain alert when the action is "alert"...
        assert!(!focus_alert_switches_app("alert", "Pages"));
        // ...or when there is no redirect app to move to.
        assert!(!focus_alert_switches_app("switch", ""));
        assert!(!focus_alert_switches_app("switch", "   "));
    }

    #[test]
    fn move_action_repeats_on_its_own_timer() {
        // Action timer (300s) is independent of the alert timer.
        // Below the action delay -> no move yet.
        assert!(!should_move_to_app(120, 300, 9999, true));
        // Reached the action delay and the repeat cooldown has passed -> move.
        assert!(should_move_to_app(300, 300, 300, true));
        // Off focus long enough, but it just moved (cooldown not elapsed) -> wait.
        assert!(!should_move_to_app(600, 300, 100, true));
        // Cooldown elapsed again -> repeat the move.
        assert!(should_move_to_app(600, 300, 300, true));
        // Action disabled (alert-only) -> never moves.
        assert!(!should_move_to_app(600, 300, 9999, false));
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
    fn enforce_blocked_access_does_nothing_without_a_running_session() {
        // Drives the real enforcement path, not just the predicate: with no
        // session, a sample that matches a block rule must not be acted on and
        // must not log a block. (Only the no-session case is exercised here —
        // the blocking branch really does quit apps.)
        let dir = temp_test_dir("block-gate");
        fs::create_dir_all(&dir).expect("create temp dir");
        let state = Arc::new(Mutex::new(AppState::default()));
        let config = Config {
            blocked_keywords: vec![format_block_rule_record("news.example", BlockMode::Full, "")],
            ..Default::default()
        };
        let sample = ActivitySample {
            timestamp: now(),
            app: "Safari".into(),
            title: "News".into(),
            source: "https://news.example/story".into(),
            category: "distracting".into(),
        };
        // Sanity: the rule really does match this sample.
        assert!(blocked_keyword_match(&config, &sample).is_some());

        enforce_blocked_access(&dir, &state, &config, &sample).expect("enforce");

        let events = fs::read_to_string(dir.join("events.jsonl")).unwrap_or_default();
        assert!(
            !events.contains("blocked_access"),
            "block fired with no focus session: {events}"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn jump_guard_fires_only_on_a_spiral_and_not_repeatedly() {
        // Below the threshold: no nudge, however long it has been.
        assert!(!jump_guard_should_fire(JUMP_GUARD_SWITCHES - 1, 60 * 60));
        // At the threshold, with the cooldown served.
        assert!(jump_guard_should_fire(
            JUMP_GUARD_SWITCHES,
            JUMP_GUARD_COOLDOWN_SECONDS
        ));
        // Spiralling, but nudged a moment ago: stay quiet rather than nag.
        assert!(!jump_guard_should_fire(
            JUMP_GUARD_SWITCHES * 3,
            JUMP_GUARD_COOLDOWN_SECONDS - 1
        ));
    }

    #[test]
    fn jump_guard_counts_only_real_switches_inside_the_window() {
        let mut state = AppState::default();
        let base = now();
        let at = |app: &str, timestamp: i64| ActivitySample {
            timestamp,
            app: app.into(),
            title: app.into(),
            source: "local".into(),
            category: "distracting".into(),
        };

        // The first sample establishes a baseline; it is not a switch.
        assert_eq!(track_switch_for_jump_guard(&mut state, &at("Claude", base)), 0);
        // Same app again: still not a switch.
        assert_eq!(
            track_switch_for_jump_guard(&mut state, &at("Claude", base + 5)),
            0
        );
        // Moving to something else counts.
        assert_eq!(
            track_switch_for_jump_guard(&mut state, &at("Chrome", base + 10)),
            1
        );
        assert_eq!(
            track_switch_for_jump_guard(&mut state, &at("Slack", base + 15)),
            2
        );
        // A switch well outside the window ages the earlier ones out.
        let later = base + JUMP_GUARD_WINDOW_SECONDS + 60;
        assert_eq!(track_switch_for_jump_guard(&mut state, &at("Mail", later)), 1);
    }

    #[test]
    fn a_locked_session_releases_itself_when_the_timer_ends() {
        let mut session = focus("Pages");
        session.duration_minutes = 25;
        session.started_at = now();

        // Not locked: nothing is ever held.
        assert!(!focus_lock_is_active(&session, now()));
        assert!(!session_is_locked(Some(&session), now()));

        session.locked = true;
        assert!(focus_lock_is_active(&session, session.started_at));
        // Still inside the 25 minutes.
        assert!(focus_lock_is_active(&session, session.started_at + 24 * 60));
        // The moment the timer runs out the lock lets go on its own, so there
        // is no way to end up permanently locked.
        assert!(!focus_lock_is_active(&session, session.started_at + 25 * 60));
        assert!(!focus_lock_is_active(&session, session.started_at + 60 * 60));

        // No session at all is never locked.
        assert!(!session_is_locked(None, now()));
    }

    #[test]
    fn integration_a_locked_session_refuses_stop_pause_and_block_edits() {
        let (port, dir) = start_test_server("integration-lock");

        let (status, _) = test_get(
            port,
            "/api/focus/start?task=Locked&minutes=25&target=Pages&lock=1",
        );
        assert_eq!(status, 200);
        let (_, body) = test_get(port, "/api/state");
        assert_eq!(json_bool(&body, "lockActive"), Some(true));

        // Every escape hatch is refused while the timer runs.
        for route in [
            "/api/focus/pause",
            "/api/focus/stop",
            "/api/block/add?keyword=escape.example&mode=full",
            "/api/block/remove?keyword=escape.example",
        ] {
            let (status, body) = test_get(port, route);
            assert_eq!(status, 200, "{route}");
            assert_eq!(json_bool(&body, "ok"), Some(false), "{route} was allowed");
            assert_eq!(json_bool(&body, "locked"), Some(true), "{route}");
        }

        // The session really is still running and unpaused after all that.
        let (_, body) = test_get(port, "/api/state");
        assert_eq!(json_bool(&body, "paused"), Some(false));
        assert_eq!(json_bool(&body, "stopped"), Some(false));
        assert_eq!(json_string(&body, "task").as_deref(), Some("Locked"));
        // And no block rule slipped through.
        assert!(!body.contains("escape.example"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn integration_an_unlocked_session_can_still_be_stopped() {
        // The guards must not leak into ordinary sessions.
        let (port, dir) = start_test_server("integration-unlocked");
        let (status, _) = test_get(port, "/api/focus/start?task=Open&minutes=25&target=Pages");
        assert_eq!(status, 200);
        let (_, body) = test_get(port, "/api/state");
        assert_eq!(json_bool(&body, "lockActive"), Some(false));

        let (_, body) = test_get(port, "/api/focus/pause");
        assert_eq!(json_bool(&body, "ok"), Some(true));
        let (_, body) = test_get(port, "/api/focus/stop");
        assert_eq!(json_bool(&body, "stopped"), Some(true));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn block_rules_only_apply_during_a_running_session() {
        // No session: the block list must not close tabs or quit apps.
        assert!(!block_rules_are_active(None));

        let running = focus("Pages");
        assert!(block_rules_are_active(Some(&running)));

        // Paused counts as not running, matching high focus mode.
        let mut paused = focus("Pages");
        paused.paused_at = Some(now());
        assert!(!block_rules_are_active(Some(&paused)));
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
    fn prune_stale_browser_devices_drops_unseen_browser_entries() {
        let records = vec![
            "Device|phone|browser:1000|selected".to_string(),
            "Device|phone|browser:2000|selected".to_string(),
            "Phone|phone|mobile:1782160452|selected".to_string(),
        ];
        let mut last_seen: HashMap<String, i64> = HashMap::new();
        // browser:1000 hasn't been seen recently; browser:2000 was just seen.
        last_seen.insert("browser:1000".to_string(), 0);
        last_seen.insert("browser:2000".to_string(), 100);

        let kept = prune_stale_browser_devices(&records, &last_seen, 100);

        let endpoints: Vec<String> = kept
            .iter()
            .map(|record| parse_network_device_record(record).endpoint)
            .collect();
        assert_eq!(endpoints, vec!["browser:2000", "mobile:1782160452"]);
    }

    #[test]
    fn prune_stale_browser_devices_drops_never_seen_entries() {
        // A "browser:" device with no last_seen entry at all (e.g. left over
        // from before a restart) is dead and must be pruned immediately.
        let records = vec!["Device|phone|browser:1000|selected".to_string()];
        let kept = prune_stale_browser_devices(&records, &HashMap::new(), 100);
        assert!(kept.is_empty());
    }

    #[test]
    fn switch_report_counts_app_and_title_changes() {
        let dir = temp_test_dir("switch-report");
        fs::create_dir_all(&dir).expect("create temp dir");

        let base = now();
        let samples = [
            ("Claude", "Claude", "local", "productive", base),
            ("Claude", "Claude", "local", "productive", base + 5),
            ("Google Chrome", "YouTube", "local", "distracting", base + 10),
            ("Google Chrome", "YouTube", "local", "distracting", base + 15),
            ("Claude", "Claude", "local", "productive", base + 20),
        ];
        for (app, title, source, category, timestamp) in samples {
            append_sample(
                &dir,
                &ActivitySample {
                    timestamp,
                    app: app.into(),
                    title: title.into(),
                    source: source.into(),
                    category: category.into(),
                },
            )
            .expect("append sample");
        }

        let report = switch_report_json(&dir).expect("switch report");
        // Claude -> Chrome -> Claude: two switches, one of them into a
        // distracting app. No switch is counted between the two identical
        // consecutive Claude/Chrome samples.
        assert_eq!(json_number(&report, "totalSwitches"), Some(2));
        assert_eq!(json_number(&report, "distractingSwitches"), Some(1));
        assert!(report.contains("\"topSwitchTargets\""));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn switch_report_buckets_jumps_by_hour_and_measures_the_longest_stretch() {
        let dir = temp_test_dir("switch-report-hours");
        fs::create_dir_all(&dir).expect("create temp dir");

        // Anchor on an exact hour boundary so the buckets are predictable.
        let hour = (now() / 3600) * 3600;
        let samples = [
            // Two samples on one app, then a jump: a 10s stretch.
            ("Claude", "Claude", "productive", hour + 10),
            ("Claude", "Claude", "productive", hour + 15),
            ("Chrome", "YouTube", "distracting", hour + 20),
            // Idle time must not count toward the longest stretch.
            ("Chrome", "YouTube", "idle", hour + 25),
            ("Chrome", "YouTube", "idle", hour + 30),
            // A jump in the next hour lands in its own bucket.
            ("Pages", "Draft", "productive", hour + 3600 + 10),
        ];
        for (app, title, category, timestamp) in samples {
            append_sample(
                &dir,
                &ActivitySample {
                    timestamp,
                    app: app.into(),
                    title: title.into(),
                    source: "local".into(),
                    category: category.into(),
                },
            )
            .expect("append sample");
        }

        let report = switch_report_json(&dir).expect("switch report");

        // Claude -> Chrome, then Chrome -> Pages. The two extra Chrome samples
        // only change category, not app or title, so they are not jumps.
        assert_eq!(json_number(&report, "totalSwitches"), Some(2));
        // Claude ran for two non-idle samples before jumping; the Chrome run
        // that follows is mostly idle, so it must not beat it.
        assert_eq!(
            json_number(&report, "longestCalmSeconds"),
            Some(2 * SAMPLE_SECONDS as i64)
        );
        // One jump in each hour, each in its own bucket.
        assert!(report.contains(&format!("{{\"start\":{},\"switches\":1}}", hour)));
        assert!(report.contains(&format!("{{\"start\":{},\"switches\":1}}", hour + 3600)));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn switch_report_handles_no_samples_without_panicking() {
        let dir = temp_test_dir("switch-report-empty");
        fs::create_dir_all(&dir).expect("create temp dir");

        let report = switch_report_json(&dir).expect("switch report");

        assert_eq!(json_number(&report, "totalSwitches"), Some(0));
        assert_eq!(json_number(&report, "distractingSwitches"), Some(0));
        assert!(report.contains("\"topSwitchTargets\":[]"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn switch_report_excludes_switches_before_the_window_start() {
        let dir = temp_test_dir("switch-report-window");
        fs::create_dir_all(&dir).expect("create temp dir");

        let base = now();
        // Two switches well before the report window starts...
        append_sample(
            &dir,
            &ActivitySample {
                timestamp: base - 3600,
                app: "Claude".into(),
                title: "Claude".into(),
                source: "local".into(),
                category: "productive".into(),
            },
        )
        .expect("append old sample");
        append_sample(
            &dir,
            &ActivitySample {
                timestamp: base - 3595,
                app: "Google Chrome".into(),
                title: "YouTube".into(),
                source: "local".into(),
                category: "distracting".into(),
            },
        )
        .expect("append old sample");
        // ...then one switch after the window starts.
        fs::write(dir.join("report_start.txt"), (base - 10).to_string())
            .expect("write report_start.txt");
        append_sample(
            &dir,
            &ActivitySample {
                timestamp: base,
                app: "Claude".into(),
                title: "Claude".into(),
                source: "local".into(),
                category: "productive".into(),
            },
        )
        .expect("append new sample");
        append_sample(
            &dir,
            &ActivitySample {
                timestamp: base + 5,
                app: "WhatsApp".into(),
                title: "WhatsApp".into(),
                source: "local".into(),
                category: "distracting".into(),
            },
        )
        .expect("append new sample");

        let report = switch_report_json(&dir).expect("switch report");

        // Only the switch after report_start.txt should count.
        assert_eq!(json_number(&report, "totalSwitches"), Some(1));
        assert_eq!(json_number(&report, "distractingSwitches"), Some(1));
        assert!(report.contains("WhatsApp"));
        assert!(!report.contains("YouTube"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn integration_blocking_is_an_editable_table() {
        let (port, dir) = start_test_server("integration-block-table");
        let (status, body) = test_get(port, "/");
        assert_eq!(status, 200);

        // The four columns, in order, plus the row-adding control.
        let site = body.find("Site or app").expect("site column");
        let full = body.find("Full block").expect("full block column");
        let password_block = body.find("Password block").expect("password block column");
        let password = body
            .find("class=\"block-col-password\"")
            .expect("password column");
        assert!(site < full && full < password_block && password_block < password);
        assert!(body.contains("id=\"blockRows\""));
        assert!(body.contains("addBlockRow()"));

        // The password column is hidden until a rule actually uses one.
        assert!(body.contains(".block-col-password { display:none; }"));
        assert!(body.contains(".block-table.show-password .block-col-password"));

        // The old single-rule chip form is gone.
        assert!(!body.contains("id=\"blockKeyword\""));
        assert!(!body.contains("id=\"blockSubmit\""));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn integration_blocking_card_stays_inside_the_blocking_view() {
        // Generating a report used to relocate the blocking card in the DOM
        // (a leftover from the single-page layout), which moved it out of
        // Blocking and into Reports. Nothing may move it between views.
        let (port, dir) = start_test_server("integration-card-home");
        let (status, body) = test_get(port, "/");
        assert_eq!(status, 200);

        let rules_view = body.find("id=\"view-rules\"").expect("rules view");
        let journal_view = body.find("id=\"view-journal\"").expect("journal view");
        let card = body.find("id=\"distractionCard\"").expect("distraction card");
        assert!(
            card > rules_view && card < journal_view,
            "the blocking card must be markup-nested in the Blocking view"
        );
        assert!(!body.contains("moveDistractionCard"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn integration_quick_start_reuses_the_last_session_setup() {
        // Starting from the menu bar sends no parameters. It must not wipe the
        // focus list the user configured in the dashboard.
        let (port, dir) = start_test_server("integration-quick-start");

        let (status, _) = test_get(
            port,
            "/api/focus/start?task=Thesis&minutes=45&target=Pages&alertSeconds=120",
        );
        assert_eq!(status, 200);
        // End the session, which clears focus.json.
        let (status, _) = test_get(port, "/api/focus/stop");
        assert_eq!(status, 200);

        // Bare quick start: same task, targets, and length as before.
        let (status, _) = test_get(port, "/api/focus/start");
        assert_eq!(status, 200);
        let (_, body) = test_get(port, "/api/state");
        assert_eq!(json_string(&body, "task").as_deref(), Some("Thesis"));
        assert_eq!(json_string(&body, "target").as_deref(), Some("Pages"));
        assert_eq!(json_number(&body, "durationMinutes"), Some(45));
        assert_eq!(json_number(&body, "alertDelaySeconds"), Some(120));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn native_host_must_be_recent_to_take_over_notifications() {
        let now_ts = 1_000_000;
        // Never checked in: the server has to post the banner itself.
        assert!(!native_host_is_live(0, now_ts));
        // Checked in just now, and still inside the window.
        assert!(native_host_is_live(now_ts, now_ts));
        assert!(native_host_is_live(
            now_ts - MAC_HOST_TIMEOUT_SECONDS,
            now_ts
        ));
        // Gone quiet (quit, or notifications denied) — fall back rather than
        // queue banners nobody will post.
        assert!(!native_host_is_live(
            now_ts - MAC_HOST_TIMEOUT_SECONDS - 1,
            now_ts
        ));
    }

    #[test]
    fn integration_peeking_at_mac_notifications_does_not_claim_the_host() {
        // A plain read must not mark a host live, or notify() would queue
        // banners for a host that cannot post them.
        let (port, dir) = start_test_server("integration-mac-notify");
        let (status, body) = test_get(port, "/api/mac/notifications");
        assert_eq!(status, 200);
        assert!(body.contains("\"notifications\""));
        {
            let mut queue = mac_notifications().lock().expect("queue");
            assert!(!native_host_is_live(queue.host_seen_at, now()));
            queue.host_seen_at = 0;
        }
        // The host's own call does claim it.
        let (status, _) = test_get(port, "/api/mac/notifications?host=1");
        assert_eq!(status, 200);
        {
            let mut queue = mac_notifications().lock().expect("queue");
            assert!(native_host_is_live(queue.host_seen_at, now()));
            queue.host_seen_at = 0;
        }
        let _ = fs::remove_dir_all(dir);
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
    fn remote_requests_are_limited_to_device_and_companion_endpoints() {
        // Device-companion surface.
        assert!(remote_path_allowed("/device"));
        assert!(remote_path_allowed("/device-sw.js"));
        assert!(remote_path_allowed("/connect"));
        assert!(remote_path_allowed("/api/mobile/activity"));
        assert!(remote_path_allowed("/download/local-focus-mobile.apk"));
        // Phone companion: state, reports, and focus control.
        assert!(remote_path_allowed("/api/state"));
        assert!(remote_path_allowed("/api/report"));
        assert!(remote_path_allowed("/api/focus/start"));
        assert!(remote_path_allowed("/api/focus/stop"));
        assert!(remote_path_allowed("/api/focus-report"));
        assert!(remote_path_allowed("/api/focus-sessions"));
        // Companion parity: journal, block list, and report history are reachable
        // so the phone can use them.
        assert!(remote_path_allowed("/api/journal/entry"));
        assert!(remote_path_allowed("/api/journal/save"));
        assert!(remote_path_allowed("/api/block/add"));
        assert!(remote_path_allowed("/api/report/history"));
        // Sensitive surface stays loopback-only.
        assert!(!remote_path_allowed("/"));
        assert!(!remote_path_allowed("/api/timeline"));
        assert!(!remote_path_allowed("/api/report/reset"));
        assert!(!remote_path_allowed("/api/app/resume"));
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

    // --- Integration tests: spin up the real HTTP server (`handle_http`, the
    // same function `run_app` hands every accepted connection) on an ephemeral
    // port against a throwaway data dir, then hit routes over a real TCP
    // socket exactly like the dashboard's `fetch()` calls do. This exercises
    // routing, query parsing, CSRF/mutation checks, and on-disk persistence
    // together, unlike the unit tests above which call internal functions
    // directly. The background tracking/focus/journal loops are intentionally
    // not started, since they poll real OS activity and aren't needed to
    // exercise the HTTP surface.

    fn start_test_server(name: &str) -> (u16, PathBuf) {
        let data_dir = temp_test_dir(name);
        fs::create_dir_all(&data_dir).expect("create test data dir");
        ensure_config(&data_dir).expect("ensure config");

        let config = load_config(&data_dir).unwrap_or_default();
        let state = Arc::new(Mutex::new(AppState {
            config,
            focus: load_focus(&data_dir),
            ..Default::default()
        }));

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let port = listener.local_addr().expect("listener addr").port();
        let returned_dir = data_dir.clone();

        thread::spawn(move || {
            for stream in listener.incoming() {
                if let Ok(stream) = stream {
                    let request_dir = data_dir.clone();
                    let request_state = Arc::clone(&state);
                    thread::spawn(move || {
                        let _ = handle_http(stream, request_dir, request_state);
                    });
                }
            }
        });

        (port, returned_dir)
    }

    /// GET `path` from the test server on `port` and return (status, body).
    fn test_get(port: u16, path: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to test server");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        )
        .expect("write request");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .expect("read response");
        let response = String::from_utf8_lossy(&response).into_owned();
        let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or(0);
        (status, body.to_string())
    }

    #[test]
    fn integration_focus_session_lifecycle_over_http() {
        let (port, dir) = start_test_server("integration-focus");

        let (status, body) = test_get(port, "/api/state");
        assert_eq!(status, 200);
        assert_eq!(json_bool(&body, "stopped"), Some(false));

        let (status, _) = test_get(
            port,
            "/api/focus/start?task=Deep+work&minutes=25&target=Terminal",
        );
        assert_eq!(status, 200);

        let (_, body) = test_get(port, "/api/state");
        assert_eq!(json_string(&body, "task").as_deref(), Some("Deep work"));
        assert_eq!(json_string(&body, "target").as_deref(), Some("Terminal"));
        assert_eq!(json_bool(&body, "paused"), Some(false));

        let (_, body) = test_get(port, "/api/focus/pause");
        assert_eq!(json_bool(&body, "ok"), Some(true));
        let (_, body) = test_get(port, "/api/state");
        assert_eq!(json_bool(&body, "paused"), Some(true));

        let (_, body) = test_get(port, "/api/focus/pause");
        assert_eq!(json_bool(&body, "ok"), Some(true));
        let (_, body) = test_get(port, "/api/state");
        assert_eq!(json_bool(&body, "paused"), Some(false));

        let (_, body) = test_get(port, "/api/focus/stop");
        assert_eq!(json_bool(&body, "stopped"), Some(true));
        let (_, body) = test_get(port, "/api/state");
        assert_eq!(json_bool(&body, "stopped"), Some(true));

        let (_, body) = test_get(port, "/api/app/resume");
        assert_eq!(json_bool(&body, "stopped"), Some(false));
        let (_, body) = test_get(port, "/api/state");
        assert_eq!(json_bool(&body, "stopped"), Some(false));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn integration_block_rule_add_and_remove_over_http() {
        let (port, dir) = start_test_server("integration-block");

        let (_, before) = test_get(port, "/api/state");
        assert!(!before.contains("qa-blocked.example"));

        let (status, _) = test_get(
            port,
            "/api/block/add?keyword=qa-blocked.example&mode=full",
        );
        assert_eq!(status, 200);

        let (_, after_add) = test_get(port, "/api/state");
        assert!(after_add.contains("qa-blocked.example"));

        let (status, _) = test_get(port, "/api/block/remove?keyword=qa-blocked.example");
        assert_eq!(status, 200);

        let (_, after_remove) = test_get(port, "/api/state");
        assert!(!after_remove.contains("qa-blocked.example"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn integration_journal_save_and_load_over_http() {
        let (port, dir) = start_test_server("integration-journal");

        let (status, body) = test_get(
            port,
            "/api/journal/save?date=2026-01-15&text=Shipped+the+QA+suite",
        );
        assert_eq!(status, 200);
        assert_eq!(
            json_string(&body, "text").as_deref(),
            Some("Shipped the QA suite")
        );

        let (status, body) = test_get(port, "/api/journal/entry?date=2026-01-15");
        assert_eq!(status, 200);
        assert_eq!(json_string(&body, "date").as_deref(), Some("2026-01-15"));
        assert_eq!(
            json_string(&body, "text").as_deref(),
            Some("Shipped the QA suite")
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn integration_device_register_appears_then_expires_over_http() {
        let (port, dir) = start_test_server("integration-device");

        let (status, _) = test_get(port, "/api/device/register?name=QaBrowser&kind=phone");
        assert_eq!(status, 200);

        let (_, body) = test_get(port, "/api/state");
        assert!(body.contains("QaBrowser"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn integration_report_switches_endpoint_returns_json() {
        let (port, dir) = start_test_server("integration-switches");
        let (status, body) = test_get(port, "/api/report/switches");
        assert_eq!(status, 200);
        assert!(body.contains("totalSwitches"));
        assert!(body.contains("distractingSwitches"));
        assert!(body.contains("topSwitchTargets"));
        // Fields the chart is drawn from.
        assert!(body.contains("byHour"));
        assert!(body.contains("longestCalmSeconds"));
        assert!(body.contains("minutesBetweenSwitches"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn integration_dashboard_ships_the_session_countdown() {
        // The server has always sent remainingSeconds; for a long time the
        // dashboard never rendered it. Guard the countdown markup so the most
        // important number in a focus app cannot silently disappear again.
        let (port, dir) = start_test_server("integration-countdown");
        let (status, body) = test_get(port, "/");
        assert_eq!(status, 200);
        assert!(body.contains("id=\"timerValue\""));
        assert!(body.contains("id=\"timerRingProgress\""));
        assert!(body.contains("remainingSeconds"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn integration_dashboard_never_labels_two_controls_the_same_stop() {
        // "Stop" used to mean both "end this session" and "halt the whole app".
        // The session control says "Pause session"; the master switch says
        // "Turn off Local Focus" and pairs with "Turn on Local Focus".
        let (port, dir) = start_test_server("integration-vocabulary");
        let (status, body) = test_get(port, "/");
        assert_eq!(status, 200);
        assert!(body.contains("Pause session"));
        assert!(body.contains("Turn off Local Focus"));
        assert!(body.contains("Turn on Local Focus"));
        assert!(!body.contains(">Stop</button>"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn integration_unknown_route_falls_back_to_dashboard() {
        // Any unmatched path serves the dashboard shell (no client-side
        // router, no separate 404 page), matching `handle_http`'s final
        // `else` branch.
        let (port, dir) = start_test_server("integration-fallback");
        let (status, body) = test_get(port, "/api/does-not-exist");
        assert_eq!(status, 200);
        assert!(body.contains("Local Focus"));
        let _ = fs::remove_dir_all(dir);
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
