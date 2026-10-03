//! Stall detection for rclone children, and the bounded record of each run.
//!
//! A hung rclone used to hold the scheduled-Push queue for as long as it stayed
//! alive: one child sat for 23 hours with a minute of CPU and no sockets, while
//! `child.wait()` blocked and every other project queued behind it. rclone's own
//! `--timeout` only bounds an idle I/O call after a transfer has begun, so a child
//! parked anywhere else never trips it. This module judges *progress* instead.
//!
//! Progress is a change in rclone's own stats counters (bytes, files, checks,
//! objects listed, deletes, renames, in-flight transfer bytes), read from the
//! `--use-json-log` heartbeat. A heartbeat that repeats the same counters is not
//! progress, nor is `elapsedTime`, nor a repeated error: otherwise a child that
//! only ticks would keep itself alive forever.
//!
//! Deliberately narrow. Only `sync` (Push, Pull, dry run) and the `size` probe are
//! watched. `bisync` and `check` are not: a fixed inactivity timer has no model of
//! a long check or hash, and stopping a bisync without rclone's graceful SIGINT
//! (which Windows cannot send) risks its saved state. A limit here is a
//! conservative heuristic, not a guarantee that no healthy run is ever stopped.

use serde_json::Value;
use shared_child::SharedChild;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Counters whose movement means rclone is working. `listed` is the one that
/// moves through a long `--fast-list` scan, `checks` through comparison, and
/// `bytes` through an upload; the in-flight sum below covers one very large file.
const PROGRESS_KEYS: [&str; 10] = [
    "bytes",
    "transfers",
    "checks",
    "listed",
    "deletes",
    "deletedDirs",
    "renames",
    "totalBytes",
    "totalTransfers",
    "totalChecks",
];

/// A run that emits structured stats but has not produced one yet is stuck at
/// startup, auth, or process creation — far sooner than a long listing is.
const NO_SIGNAL_JSON: Duration = Duration::from_secs(10 * 60);
/// A run with no stats at all (`rclone size`) can only be judged by any output.
const NO_SIGNAL_SILENT: Duration = Duration::from_secs(30 * 60);
/// Unchanged counters, once stats are flowing. Longer than the slowest
/// legitimate silence seen (a cold `--fast-list` of a very large remote, or
/// retry backoff against Drive's rate limit) and far shorter than a day.
const NO_PROGRESS: Duration = Duration::from_secs(30 * 60);
const TERM_GRACE: Duration = Duration::from_secs(15);

/// Bounds on a retained run record.
const RUN_LOG_KEEP: usize = 20;
const RUN_LOG_MAX_BYTES: usize = 16 * 1024 * 1024;

/// How long `lsjson` may take before it is stopped. It lists one directory level.
const LIST_DEADLINE: Duration = Duration::from_secs(5 * 60);

pub fn list_deadline() -> Duration {
    #[cfg(test)]
    if let Some(d) = TEST_LIST_DEADLINE.with(|c| c.get()) {
        return d;
    }
    LIST_DEADLINE
}

#[derive(Clone, Copy, Debug)]
pub struct StallLimits {
    pub no_signal: Duration,
    pub no_progress: Duration,
    /// Whether any stderr line counts as life before the first stats record. True
    /// for a run that never emits stats (`size`); false for a JSON run, where a
    /// warning every few minutes must not postpone the "no progress record" limit.
    pub output_counts: bool,
    pub grace: Duration,
    pub tick: Duration,
}

#[cfg(test)]
thread_local! {
    /// Lets a test shrink the limits `run_rclone` derives for itself, so the
    /// production wiring is exercised without waiting minutes.
    pub static TEST_LIMITS: std::cell::Cell<Option<StallLimits>> =
        const { std::cell::Cell::new(None) };
    pub static TEST_LIST_DEADLINE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

impl StallLimits {
    /// `None` for an operation the watchdog does not supervise.
    pub fn for_args(args: &[String]) -> Option<Self> {
        #[cfg(test)]
        if let Some(limits) = TEST_LIMITS.with(|c| c.get()) {
            return Some(limits);
        }
        if !matches!(args.first().map(String::as_str), Some("sync" | "size")) {
            return None;
        }
        let json = args.iter().any(|a| a == "--use-json-log");
        Some(Self {
            no_signal: if json { NO_SIGNAL_JSON } else { NO_SIGNAL_SILENT },
            no_progress: NO_PROGRESS,
            output_counts: !json,
            grace: TERM_GRACE,
            tick: Duration::from_secs(1),
        })
    }
}

struct Clock {
    started: Instant,
    fingerprint: Option<Vec<u64>>,
    last_change: Instant,
    last_output: Instant,
    saw_stats: bool,
}

/// What rclone has done so far in one run, plus the file that records it.
pub struct RunMonitor {
    clock: Mutex<Clock>,
    log: Mutex<Option<RunLog>>,
}

impl RunMonitor {
    pub fn new(project_id: &str, argv: &[String]) -> Arc<Self> {
        let now = Instant::now();
        let log = RunLog::open(project_id, argv);
        Arc::new(Self {
            clock: Mutex::new(Clock {
                started: now,
                fingerprint: None,
                last_change: now,
                last_output: now,
                saw_stats: false,
            }),
            log: Mutex::new(log),
        })
    }

    pub fn log_path(&self) -> Option<PathBuf> {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|l| l.path.clone())
    }

    /// Every raw stderr line passes through here exactly once.
    pub fn on_stderr_line(&self, raw: &str, now: Instant) {
        self.write_raw(raw);
        // Cheap filter first: only the stats heartbeat is worth parsing here.
        let fingerprint = if raw.contains("\"stats\":") {
            serde_json::from_str::<Value>(raw)
                .ok()
                .and_then(|v| v.get("stats").map(fingerprint_of))
        } else {
            None
        };
        let mut c = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        c.last_output = now;
        if let Some(fp) = fingerprint {
            if !c.saw_stats || c.fingerprint.as_ref() != Some(&fp) {
                c.last_change = now;
            }
            c.saw_stats = true;
            c.fingerprint = Some(fp);
        }
    }

    /// Why this run should be stopped, if it should.
    pub fn stall(&self, now: Instant, limits: &StallLimits) -> Option<String> {
        let c = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        if c.saw_stats {
            let idle = now.saturating_duration_since(c.last_change);
            (idle >= limits.no_progress).then(|| {
                format!(
                    "no rclone progress counter changed for {}",
                    human(idle)
                )
            })
        } else {
            let since = if limits.output_counts { c.last_output } else { c.started };
            let idle = now.saturating_duration_since(since);
            (idle >= limits.no_signal).then(|| {
                format!("rclone produced no progress record for {}", human(idle))
            })
        }
    }

    /// Forgive time the machine spent asleep, so a healthy child suspended with
    /// the Mac is not judged stalled the instant it wakes.
    pub fn reset_clock(&self, now: Instant) {
        let mut c = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        c.started = now;
        c.last_change = now;
        c.last_output = now;
    }

    pub fn note(&self, event: Value) {
        self.write_raw(&event.to_string());
    }

    fn write_raw(&self, line: &str) {
        if let Some(log) = self.log.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            log.write(line);
        }
    }
}

/// The counters that make up the progress fingerprint, in `PROGRESS_KEYS` order,
/// plus the bytes moved so far by every transfer still in flight.
fn fingerprint_of(stats: &Value) -> Vec<u64> {
    let mut fp: Vec<u64> = PROGRESS_KEYS
        .iter()
        .map(|k| stats.get(k).and_then(Value::as_f64).unwrap_or(0.0) as u64)
        .collect();
    let in_flight: f64 = stats
        .get("transferring")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("bytes").and_then(Value::as_f64))
                .sum()
        })
        .unwrap_or(0.0);
    fp.push(in_flight as u64);
    fp
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 120 {
        format!("{} min", s / 60)
    } else {
        format!("{} s", s)
    }
}

/// Raw stderr of one run, newest 20 per project, each capped. The UI log dies
/// with the app and holds only a rendered view; the cause of a hang is in the
/// last records before it, so they have to outlive the process.
struct RunLog {
    file: File,
    path: PathBuf,
    written: usize,
    cap: usize,
    truncated: bool,
}

impl RunLog {
    /// Best effort by design: failing to keep a record must never fail a sync.
    fn open(project_id: &str, argv: &[String]) -> Option<Self> {
        Self::open_in(&run_log_dir()?, project_id, argv, RUN_LOG_MAX_BYTES)
    }

    fn open_in(dir: &std::path::Path, project_id: &str, argv: &[String], cap: usize) -> Option<Self> {
        fs::create_dir_all(dir).ok()?;
        let safe: String = project_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
        let path = dir.join(format!("{safe}-{stamp}.ndjson"));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).ok()?;
        prune(dir, &safe);
        let mut log = Self { file, path, written: 0, cap, truncated: false };
        log.write(
            &serde_json::json!({
                "rcsync": "run-start",
                "project_id": project_id,
                "argv": argv,
                "rcsync_version": env!("CARGO_PKG_VERSION"),
            })
            .to_string(),
        );
        Some(log)
    }

    fn write(&mut self, line: &str) {
        if self.truncated {
            return;
        }
        if self.written + line.len() + 1 > self.cap {
            self.truncated = true;
            let _ = writeln!(self.file, "{{\"rcsync\":\"log-truncated\"}}");
            return;
        }
        if writeln!(self.file, "{line}").is_ok() {
            self.written += line.len() + 1;
        }
    }
}

fn run_log_dir() -> Option<PathBuf> {
    // Unit tests drive the real runner; they must not write into a user's live
    // record directory or prune their history.
    #[cfg(test)]
    {
        Some(std::env::temp_dir().join(format!("rcsync-test-{}", std::process::id())).join("run-logs"))
    }
    #[cfg(not(test))]
    {
        Some(dirs::config_dir()?.join("rcsync").join("run-logs"))
    }
}

/// Keep the newest `RUN_LOG_KEEP` records for one project. Names sort by their
/// UTC stamp, so lexical order is chronological.
fn prune(dir: &std::path::Path, safe_id: &str) {
    let prefix = format!("{safe_id}-");
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut mine: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".ndjson"))
        })
        .collect();
    mine.sort();
    let excess = mine.len().saturating_sub(RUN_LOG_KEEP);
    for old in mine.into_iter().take(excess) {
        let _ = fs::remove_file(old);
    }
}

/// Watch one child. Returns the stall reason if it had to be stopped; returns
/// `None` once the sender is dropped (the child exited on its own).
pub fn watch(
    child: Arc<SharedChild>,
    monitor: Arc<RunMonitor>,
    limits: StallLimits,
    done: Receiver<()>,
) -> Option<String> {
    let mut prev_wall = SystemTime::now();
    loop {
        match done.recv_timeout(limits.tick) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => return None,
        }
        let wall = SystemTime::now();
        if let Ok(gap) = wall.duration_since(prev_wall) {
            if gap > limits.tick + Duration::from_secs(30) {
                monitor.reset_clock(Instant::now());
            }
        }
        prev_wall = wall;
        if let Some(reason) = monitor.stall(Instant::now(), &limits) {
            // The child may have exited on its own in the moment since the last
            // check; a finished run is not a stalled one.
            if matches!(child.try_wait(), Ok(Some(_))) {
                return None;
            }
            monitor.note(serde_json::json!({ "rcsync": "stall", "reason": reason }));
            stop_child(&child, limits.grace);
            return Some(reason);
        }
    }
}

/// Ask rclone to stop and escalate if it does not: SIGTERM, then SIGKILL after
/// the grace. Windows has no equivalent signal and just terminates the process.
#[cfg(unix)]
pub fn stop_child(child: &SharedChild, grace: Duration) {
    use shared_child::unix::SharedChildExt;
    let _ = child.send_signal(libc::SIGTERM);
    if !exited_within(child, grace) {
        let _ = child.kill();
    }
}

#[cfg(not(unix))]
pub fn stop_child(child: &SharedChild, _grace: Duration) {
    let _ = child.kill();
}

#[cfg(unix)]
fn exited_within(child: &SharedChild, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `Command::output` with a deadline. A child that outlives it is stopped the
/// same way a stalled push is, and the caller gets an error rather than a hang.
pub fn output_within(cmd: &mut Command, deadline: Duration) -> Result<Output, String> {
    // No stdin, as `Command::output` gives none: a child that prompts (an
    // encrypted rclone config) must fail at once, not sit on the app's terminal
    // until the deadline.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = Arc::new(SharedChild::spawn(cmd).map_err(|e| e.to_string())?);
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = read(child.take_stdout().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = read(child.take_stderr().map(|p| Box::new(p) as Box<dyn Read + Send>));

    let (done_tx, done_rx) = mpsc::channel::<()>();
    let timer = std::thread::spawn({
        let child = child.clone();
        move || match done_rx.recv_timeout(deadline) {
            Err(RecvTimeoutError::Timeout) => {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return false;
                }
                stop_child(&child, TERM_GRACE);
                true
            }
            _ => false,
        }
    });
    let status = child.wait();
    drop(done_tx);
    let timed_out = timer.join().unwrap_or(false);
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if timed_out {
        return Err(format!(
            "did not finish within {} and was stopped",
            human(deadline)
        ));
    }
    Ok(Output {
        status: status.map_err(|e| e.to_string())?,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(no_signal_ms: u64, no_progress_ms: u64) -> StallLimits {
        StallLimits {
            no_signal: Duration::from_millis(no_signal_ms),
            no_progress: Duration::from_millis(no_progress_ms),
            output_counts: true,
            grace: Duration::from_millis(300),
            tick: Duration::from_millis(20),
        }
    }

    fn stats(fields: &str) -> String {
        format!(r#"{{"level":"notice","msg":"x","stats":{{{fields}}}}}"#)
    }

    fn monitor() -> Arc<RunMonitor> {
        RunMonitor::new("watchdog-unit", &[])
    }

    #[test]
    fn counters_moving_is_progress_and_a_repeated_heartbeat_is_not() {
        let m = monitor();
        let l = limits(1000, 100);
        let t0 = Instant::now();
        m.on_stderr_line(&stats(r#""listed":10,"elapsedTime":1.0"#), t0);
        // Same counters, only the clock field moved: a child that merely ticks.
        let t1 = t0 + Duration::from_millis(80);
        m.on_stderr_line(&stats(r#""listed":10,"elapsedTime":9.0"#), t1);
        assert!(
            m.stall(t0 + Duration::from_millis(150), &l).is_some(),
            "a heartbeat that repeats the same counters must not count as progress"
        );
        // `listed` moves: the long fast-list scan is alive.
        let t2 = t0 + Duration::from_millis(140);
        m.on_stderr_line(&stats(r#""listed":11,"elapsedTime":10.0"#), t2);
        assert!(
            m.stall(t0 + Duration::from_millis(150), &l).is_none(),
            "a changing `listed` count is progress"
        );
    }

    #[test]
    fn in_flight_bytes_keep_one_huge_upload_alive() {
        let m = monitor();
        let l = limits(1000, 100);
        let t0 = Instant::now();
        let frame = |b: u64| {
            stats(&format!(
                r#""transfers":0,"bytes":0,"transferring":[{{"name":"big","bytes":{b}}}]"#
            ))
        };
        m.on_stderr_line(&frame(10), t0);
        m.on_stderr_line(&frame(20), t0 + Duration::from_millis(90));
        assert!(
            m.stall(t0 + Duration::from_millis(150), &l).is_none(),
            "bytes moving inside an unfinished transfer are progress"
        );
    }

    #[test]
    fn a_run_that_never_reports_is_judged_on_the_no_signal_limit() {
        let m = monitor();
        let l = limits(100, 10_000);
        let t0 = Instant::now();
        m.on_stderr_line("plain text, not a stats record", t0);
        assert!(m.stall(t0 + Duration::from_millis(50), &l).is_none());
        assert!(
            m.stall(t0 + Duration::from_millis(150), &l).is_some(),
            "output without a stats record must not extend the deadline past `no_signal`"
        );
    }

    #[test]
    fn waking_from_sleep_forgives_the_gap() {
        let m = monitor();
        let l = limits(1000, 100);
        let t0 = Instant::now();
        m.on_stderr_line(&stats(r#""listed":1"#), t0);
        let later = t0 + Duration::from_millis(500);
        m.reset_clock(later);
        assert!(m.stall(later + Duration::from_millis(50), &l).is_none());
    }

    #[test]
    fn repeated_non_stats_output_does_not_postpone_the_json_startup_deadline() {
        // A wedged run that prints a warning every so often must still be stopped
        // at the start-based limit, not have it renewed by each warning.
        let m = monitor();
        let mut l = limits(100, 10_000);
        l.output_counts = false;
        let t0 = Instant::now();
        for ms in [30, 60, 90] {
            m.on_stderr_line("WARNING: still retrying", t0 + Duration::from_millis(ms));
        }
        assert!(
            m.stall(t0 + Duration::from_millis(150), &l).is_some(),
            "a JSON run's startup deadline must not be refreshed by non-stats stderr"
        );
        l.output_counts = true;
        assert!(
            m.stall(t0 + Duration::from_millis(150), &l).is_none(),
            "a run that never emits stats (rclone size) is judged on silence instead"
        );
    }

    #[test]
    fn production_limits_cover_only_the_operations_with_a_model() {
        let args = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        let sync = StallLimits::for_args(&args(&["sync", "a", "b", "--use-json-log"])).unwrap();
        assert_eq!(sync.no_signal, NO_SIGNAL_JSON);
        assert!(!sync.output_counts);
        let size = StallLimits::for_args(&args(&["size", "a", "--json"])).unwrap();
        assert_eq!(size.no_signal, NO_SIGNAL_SILENT);
        assert!(size.output_counts);
        for unwatched in [
            args(&["bisync", "a", "b", "--use-json-log"]),
            args(&["check", "a", "b", "--use-json-log"]),
            args(&["lsjson", "a"]),
        ] {
            assert!(
                StallLimits::for_args(&unwatched).is_none(),
                "{:?} has no progress model and must not be stopped by a fixed timer",
                unwatched[0]
            );
        }
    }

    #[test]
    fn the_run_record_byte_cap_is_enforced() {
        let dir = std::env::temp_dir().join(format!("rcsync-cap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut log = RunLog::open_in(&dir, "p_cap", &[], 400).unwrap();
        for i in 0..50 {
            log.write(&format!("line number {i} padding padding padding"));
        }
        let text = fs::read_to_string(&log.path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert!(text.len() <= 400 + 64, "the record grew past its cap: {} bytes", text.len());
        assert!(text.contains("log-truncated"), "a capped record must say it was cut");
        assert!(!text.contains("line number 49"), "nothing may be written after the cap");
    }

    #[test]
    fn run_records_keep_only_the_newest_per_project() {
        let dir = std::env::temp_dir().join(format!("rcsync-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for i in 0..(RUN_LOG_KEEP + 5) {
            fs::write(dir.join(format!("p_one-2026{i:04}.ndjson")), "x").unwrap();
        }
        fs::write(dir.join("p_two-20260001.ndjson"), "x").unwrap();
        prune(&dir, "p_one");
        let mine = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("p_one-"))
            .count();
        let other = dir.join("p_two-20260001.ndjson").exists();
        let oldest_gone = !dir.join("p_one-20260000.ndjson").exists();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(mine, RUN_LOG_KEEP, "retention must bound the records per project");
        assert!(oldest_gone, "the oldest record is the one dropped");
        assert!(other, "another project's records are never pruned");
    }

    #[cfg(unix)]
    #[test]
    fn output_within_stops_a_child_that_outlives_its_deadline() {
        let started = Instant::now();
        let err = output_within(
            Command::new("sleep").arg("30"),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(err.contains("was stopped"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the deadline must actually end the wait"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_within_gives_the_child_no_stdin_as_command_output_does() {
        // `cat` exits at once on a closed stdin and blocks on an inherited open
        // one (a terminal, a pipe), so under an open stdin this only passes if
        // the child is given none.
        let started = Instant::now();
        let out = output_within(&mut Command::new("cat"), Duration::from_secs(30)).unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the child must not be left reading the app's stdin"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_within_returns_a_finished_childs_output() {
        let out = output_within(
            Command::new("sh").args(["-c", "echo hi; echo oops >&2"]),
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "oops");
    }
}
