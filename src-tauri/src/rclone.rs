use crate::config::{self, expand_tilde, find_local_path, AppConfig, Project};
use shared_child::SharedChild;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// Error text returned when the user stops an operation. Exact string, matched
/// by the frontend so a cancel reads as a cancel and not as a sync failure.
pub const CANCELLED: &str = "CANCELLED";

/// Operations in flight, all keyed by project name — the same key the UI uses
/// for its running state, so a cancel request needs nothing but that name.
///
/// A project enters `active` the moment its command starts, *before* it queues
/// on the concurrency semaphore. That distinction is the whole point: during a
/// "push all" most projects are sitting in the queue with no process to kill,
/// and a registry that only tracked live children would let every one of them
/// start anyway after the user cancelled.
#[derive(Default)]
struct Ops {
    active: HashSet<String>,
    running: HashMap<String, Arc<SharedChild>>,
    cancelled: HashSet<String>,
}

fn ops() -> MutexGuard<'static, Ops> {
    static OPS: OnceLock<Mutex<Ops>> = OnceLock::new();
    // A panic while holding this lock must not wedge every future sync — the
    // state it guards is bookkeeping, not something we can corrupt halfway.
    OPS.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Marks a project as having an operation in flight, and clears every trace of
/// it on drop — including on early-return paths. Without the drop, a cancel
/// arriving just after an operation finished would leak onto its next run.
pub struct OpGuard(String);

impl Drop for OpGuard {
    fn drop(&mut self) {
        let mut ops = ops();
        ops.active.remove(&self.0);
        ops.running.remove(&self.0);
        ops.cancelled.remove(&self.0);
    }
}

/// Claim a project for an operation. Fails if one is already in flight for it,
/// which would mean two rclone processes syncing the same pair of directories.
pub fn start_op(name: &str) -> Result<OpGuard, String> {
    let mut ops = ops();
    if !ops.active.insert(name.to_string()) {
        return Err(format!("An operation is already running for '{}'", name));
    }
    Ok(OpGuard(name.to_string()))
}

/// Stop before doing any work if a cancel landed while this operation was
/// queued. Call after acquiring a concurrency permit.
pub fn check_cancelled(name: &str) -> Result<(), String> {
    if ops().cancelled.contains(name) {
        return Err(CANCELLED.to_string());
    }
    Ok(())
}

/// Ask the operation on `name` to stop, killing rclone if it has started.
/// Returns false when nothing was in flight for that project.
///
/// This is currently `SharedChild::kill` (SIGKILL / TerminateProcess), which is
/// abrupt. It is NOT because a graceful signal is unavailable — `SharedChildExt`
/// exposes a pid-race-safe `send_signal` on unix — so the choice rests only on
/// simplicity, and it is not equivalent to a graceful shutdown for `bisync`:
/// rclone treats SIGINT as its clean-stop signal, finishing in-flight work and
/// saving state, and its `--recover` / `--resilient` flags are meant to avoid the
/// forced `--resync` that an abrupt kill leaves behind. Revisit before relying on
/// cancel mid-bisync.
pub fn request_cancel(name: &str) -> bool {
    let mut ops = ops();
    if !ops.active.contains(name) {
        return false;
    }
    ops.cancelled.insert(name.to_string());
    // No child yet means the operation is still queued; `check_cancelled` will
    // stop it before it ever spawns.
    let child = ops.running.get(name).cloned();
    drop(ops);
    if let Some(child) = child {
        let _ = child.kill();
    }
    true
}

fn rclone_command(program: &str) -> Command {
    let cmd = Command::new(program);
    #[cfg(windows)]
    let cmd = {
        use std::os::windows::process::CommandExt;
        let mut c = cmd;
        c.creation_flags(0x08000000); // CREATE_NO_WINDOW — suppress console pop-up
        c
    };
    cmd
}

/// A check verdict. Typed so the frontend never has to infer sync state by
/// pattern-matching human-readable output.
#[derive(Clone, serde::Serialize)]
pub struct CheckOutcome {
    pub synced: bool,
    pub differences: u32,
    pub matches: u32,
    pub details: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoteDir {
    pub name: String,
    pub has_local: bool,
    pub local_path: Option<String>,
    pub in_config: bool,
}

fn build_exclude_args(cfg: &AppConfig) -> Vec<String> {
    cfg.excludes
        .iter()
        .flat_map(|e| vec!["--exclude".to_string(), e.clone()])
        .collect()
}

/// Per-project excludes as rclone `--exclude PATTERN` args. Applied in addition
/// to the global excludes, only for this project's operations.
fn project_exclude_args(project: &Project) -> Vec<String> {
    project
        .excludes
        .iter()
        .filter(|e| !e.trim().is_empty())
        .flat_map(|e| vec!["--exclude".to_string(), e.clone()])
        .collect()
}

/// `--transfers N`, or nothing at all when the setting is Automatic.
///
/// Deliberately narrow, and deliberately NOT applied inside `run_rclone`: that
/// function runs exactly the argv its caller builds, so keeping performance
/// policy at the two transfer call sites makes it mechanically visible that the
/// empty-source probe is untouched. Absence must mean absence — passing even
/// `--transfers 4` would override a user's `RCLONE_TRANSFERS` or rclone config,
/// since a command-line value outranks both.
fn build_transfer_args(cfg: &AppConfig) -> Result<Vec<String>, String> {
    config::validate_rclone_transfers(cfg.rclone_transfers)?;
    Ok(match cfg.rclone_transfers {
        None => Vec::new(),
        Some(n) => vec!["--transfers".to_string(), n.to_string()],
    })
}

/// Pull the file count out of `rclone size --json` output.
///
/// Separated from the subprocess call so the fail-closed paths are testable: any
/// output this cannot read must become an error, never a zero and never an
/// assumed-nonempty, because the caller decides whether to run a `sync` that
/// deletes on the strength of it.
fn parse_rclone_size_count(output: &str) -> Result<i64, String> {
    output
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .and_then(|v| v.get("count").and_then(serde_json::Value::as_i64))
        .ok_or_else(|| format!("Could not read a file count from rclone size output:\n{}", output))
}

fn check_local_path(project: &Project) -> Result<String, String> {
    let local = expand_tilde(&project.local_path);
    if !Path::new(&local).exists() {
        return Err(format!(
            "Local path does not exist: {}",
            local
        ));
    }
    Ok(local)
}

fn resolve_rclone(cfg: &AppConfig) -> String {
    let p = &cfg.rclone_path;
    if Path::new(p).is_absolute() {
        return p.clone();
    }

    #[cfg(target_os = "macos")]
    {
        for candidate in &[
            "/opt/homebrew/bin/rclone",
            "/usr/local/bin/rclone",
            "/usr/bin/rclone",
        ] {
            if Path::new(candidate).exists() {
                return candidate.to_string();
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Check common Windows install locations
        if let Ok(program_files) = std::env::var("ProgramFiles") {
            let candidate = format!("{}\\rclone\\rclone.exe", program_files);
            if Path::new(&candidate).exists() {
                return candidate;
            }
        }
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            let candidate = format!("{}\\rclone\\rclone.exe", local_app_data);
            if Path::new(&candidate).exists() {
                return candidate;
            }
        }
        // Also check scoop and chocolatey common paths via HOME
        if let Some(home) = dirs::home_dir() {
            let scoop = home.join("scoop\\shims\\rclone.exe");
            if scoop.exists() {
                return scoop.to_string_lossy().to_string();
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        for candidate in &[
            "/usr/local/bin/rclone",
            "/usr/bin/rclone",
        ] {
            if Path::new(candidate).exists() {
                return candidate.to_string();
            }
        }
    }

    p.clone()
}

/// Read a child pipe to the end. Lossy because rclone echoes filenames, which
/// are not guaranteed to be UTF-8.
///
/// Errors propagate rather than yielding a short read: output that silently went
/// missing is indistinguishable from a clean run with nothing to report, and
/// callers turn "no output" into verdicts.
fn drain<R: Read>(pipe: Option<R>, which: &str) -> Result<String, String> {
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut bytes)
            .map_err(|e| format!("Failed to read rclone {}: {}", which, e))?;
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Run rclone and return combined stdout+stderr as a string. `key` is the
/// project name the operation is registered under, which is how a cancel
/// request finds this process.
fn run_rclone(cfg: &AppConfig, key: &str, args: &[String]) -> Result<(String, i32), String> {
    let rclone = resolve_rclone(cfg);
    let mut cmd = rclone_command(&rclone);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());

    let child = Arc::new(
        SharedChild::spawn(&mut cmd)
            .map_err(|e| format!("Failed to start rclone at '{}': {}", rclone, e))?,
    );

    // Drain both pipes on their own threads. `-v` produces more than a pipe
    // buffer holds, and rclone blocks forever on a full pipe — so reading them
    // one after the other, or only after waiting, would hang the sync.
    let out_reader = std::thread::spawn({
        let stdout = child.take_stdout();
        move || drain(stdout, "stdout")
    });
    let err_reader = std::thread::spawn({
        let stderr = child.take_stderr();
        move || drain(stderr, "stderr")
    });

    // Publish the child and re-read the cancel flag under one lock. Either a
    // cancel got here first and we see its flag, or it arrives later and finds
    // the child we just published — no ordering slips between the two.
    let cancelled_while_spawning = {
        let mut ops = ops();
        ops.running.insert(key.to_string(), child.clone());
        ops.cancelled.contains(key)
    };
    if cancelled_while_spawning {
        let _ = child.kill();
    }

    let status = child.wait();
    ops().running.remove(key);

    // Join both before deciding anything, so neither thread is left detached.
    let stdout = out_reader
        .join()
        .map_err(|_| "rclone stdout reader thread panicked".to_string());
    let stderr = err_reader
        .join()
        .map_err(|_| "rclone stderr reader thread panicked".to_string());

    // A killed rclone exits on a signal and its output ends mid-transfer. Report
    // the cancel the user asked for instead of the failure it resembles. This
    // precedes the pipe checks because a cancel explains them.
    if ops().cancelled.contains(key) {
        return Err(CANCELLED.to_string());
    }

    // A panicked reader or a failed read used to become an empty string, which
    // downstream reads as "rclone said nothing" — i.e. a clean result.
    let stdout = stdout??;
    let stderr = stderr??;
    let status = status.map_err(|e| format!("Failed to wait for rclone: {}", e))?;
    let mut result = String::new();
    for line in stdout.lines().chain(stderr.lines()) {
        if !line.is_empty() {
            result.push_str(line);
            result.push('\n');
        }
    }
    let code = status.code().unwrap_or(-1);
    Ok((result, code))
}

/// How many files rclone itself would sync out of `local`, asked with exactly the
/// arguments the sync is about to use.
///
/// This replaced a local `globset` matcher that re-implemented rclone's filter
/// language, and the two disagreed in a way that could destroy data: rclone
/// matches an unrooted `node_modules/**` at any depth, `globset` only at the
/// root, so a tree whose only content sat under a nested `node_modules` looked
/// non-empty to the guard and empty to rclone — and `sync` deletes whatever the
/// source does not have. Asking rclone removes the second filtering authority
/// rather than trying to keep it in step. Failing to get an answer is an error,
/// never an assumption in either direction.
///
/// The guarantee is that the probe and the sync receive the same *app-supplied*
/// filter arguments — not that every rclone configuration is accounted for. A
/// remote whose rclone config sets `global.*` overrides is instantiated by the
/// sync but not by this local-path probe, and is outside the supported
/// configuration (see README). There is also an unavoidable gap between probe
/// and sync: a source emptied in between can still be synced as empty.
fn rclone_source_file_count(
    cfg: &AppConfig,
    project: &Project,
    local: &str,
) -> Result<i64, String> {
    let mut args = vec!["size".to_string(), local.to_string(), "--json".to_string()];
    args.extend(build_exclude_args(cfg));
    args.extend(project_exclude_args(project));

    let (output, code) = run_rclone(cfg, &project.name, &args)?;
    if code != 0 {
        return Err(format!(
            "Could not check whether '{}' has anything to sync (rclone size exited {}):\n{}",
            local, code, output
        ));
    }
    parse_rclone_size_count(&output)
}

/// Refuse an operation whose source rclone considers empty. `sync` and `bisync`
/// make the destination match the source, so an empty source deletes the remote
/// copy — the single worst outcome this app can produce.
fn ensure_source_not_empty(
    cfg: &AppConfig,
    project: &Project,
    local: &str,
    verb: &str,
) -> Result<(), String> {
    if rclone_source_file_count(cfg, project, local)? == 0 {
        return Err(format!(
            "Refusing to {}: rclone sees no files in '{}' after excludes. \
             Continuing would delete the remote copy of '{}'.",
            verb, local, project.name
        ));
    }
    Ok(())
}

pub fn sync_project(
    cfg: &AppConfig,
    project: &Project,
    mode: &str,
    dry_run: bool,
) -> Result<String, String> {
    let local = if mode == "pull" {
        expand_tilde(&project.local_path)
    } else {
        check_local_path(project)?
    };
    let remote = cfg.remote_path_for_project(project);

    if mode == "push" {
        ensure_source_not_empty(cfg, project, &local, "push")?;
    }

    let (src, dst) = match mode {
        "pull" => (remote, local),
        _ => (local, remote),
    };

    let mut args = vec!["sync".to_string(), src, dst];
    args.extend(build_exclude_args(cfg));
    args.extend(project_exclude_args(project));
    args.extend(build_transfer_args(cfg)?);
    args.push("-v".to_string());
    if dry_run {
        args.push("--dry-run".to_string());
    }

    let (output, code) = run_rclone(cfg, &project.name, &args)?;
    if code == 0 {
        Ok(output)
    } else {
        Err(format!("{}\nExited with code {}", output, code))
    }
}

pub fn bisync_project(cfg: &AppConfig, project: &Project) -> Result<String, String> {
    let local = check_local_path(project)?;

    ensure_source_not_empty(cfg, project, &local, "bi-sync")?;

    let remote = cfg.remote_path_for_project(project);

    let mut args = vec!["bisync".to_string(), local, remote];
    args.extend(build_exclude_args(cfg));
    args.extend(project_exclude_args(project));
    args.extend(build_transfer_args(cfg)?);
    args.push("-v".to_string());

    let (output, code) = run_rclone(cfg, &project.name, &args)?;
    if code == 0 {
        Ok(output)
    } else {
        Err(format!("{}\nExited with code {}", output, code))
    }
}

pub fn check_project(cfg: &AppConfig, project: &Project) -> Result<CheckOutcome, String> {
    let local = check_local_path(project)?;
    let remote = cfg.remote_path_for_project(project);

    let mut args = vec![
        "check".to_string(),
        local,
        remote,
        "--combined".to_string(),
        "-".to_string(),
    ];
    args.extend(build_exclude_args(cfg));
    args.extend(project_exclude_args(project));

    let (raw_output, code) = run_rclone(cfg, &project.name, &args)?;
    parse_check_output(&raw_output, code)
}

/// Turn `rclone check --combined` output into a verdict, or refuse to give one.
///
/// The previous version returned `Ok` for every exit code and let the frontend
/// decide by searching the human text for "N differences" — so an auth failure,
/// an unreachable remote, or a killed process produced no such phrase and was
/// read as "synced". A sync verdict is only ever derived from itemized per-file
/// lines here, and anything unexpected is an error.
fn parse_check_output(raw: &str, code: i32) -> Result<CheckOutcome, String> {
    // rclone check uses 0 for "all matched" and 1 for "differences found", but 1
    // is also its generic usage-error code, so exit status alone cannot be
    // trusted — the combined stream is the authority.
    if code != 0 && code != 1 {
        return Err(format!("rclone check failed (exit {}):\n{}", code, raw));
    }

    let mut details = String::new();
    let (mut differences, mut matches, mut unreadable) = (0u32, 0u32, 0u32);

    for line in raw.lines() {
        if line.len() < 2 {
            continue;
        }
        let (marker, rest) = line.split_at(2);
        let label = match marker {
            "= " => { matches += 1; continue }
            "* " => "CHANGED",
            "+ " => "REMOTE ONLY",
            "- " => "LOCAL ONLY",
            // rclone could not read the file on one side; it has no verdict for
            // it, so neither do we.
            "! " => { unreadable += 1; "UNREADABLE" }
            _ => {
                if let Some(msg) = line.split("NOTICE: ").nth(1) {
                    details.push_str(msg);
                    details.push('\n');
                }
                continue;
            }
        };
        if marker != "! " {
            differences += 1;
        }
        details.push_str(&format!("[{}] {}\n", label, rest));
    }

    if unreadable > 0 {
        return Err(format!(
            "{} file(s) could not be compared, so the sync status is unknown:\n{}",
            unreadable, details
        ));
    }
    // Differences were signalled but none itemized: the check did not really
    // run (bad usage, unreachable remote). Claiming "synced" here is the exact
    // failure this function exists to prevent.
    if code == 1 && differences == 0 {
        return Err(format!(
            "rclone check reported a failure but listed no differences:\n{}",
            raw
        ));
    }

    details.push_str(&if differences == 0 {
        format!("All {} files match.\n", matches)
    } else {
        format!("{} differences, {} matching.\n", differences, matches)
    });

    Ok(CheckOutcome { synced: differences == 0, differences, matches, details })
}

/// List remote projects. If `remote_name` is provided, use that remote; otherwise use active.
pub fn list_remote(cfg: &AppConfig, remote_name: Option<&str>) -> Result<Vec<RemoteDir>, String> {
    let rclone = resolve_rclone(cfg);
    let rc = if let Some(name) = remote_name {
        cfg.remotes.iter().find(|r| r.name == name).cloned()
            .unwrap_or(config::RemoteConfig { name: name.to_string(), base_path: "proj".into() })
    } else {
        cfg.active_remote()
    };
    let output = rclone_command(&rclone)
        .args(["lsd", &format!("{}:{}", rc.name, rc.base_path)])
        .output()
        .map_err(|e| format!("Failed to run rclone lsd: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("rclone lsd failed: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let config_names: std::collections::HashSet<&str> =
        cfg.projects.iter().map(|p| p.name.as_str()).collect();

    Ok(stdout
        .lines()
        .filter_map(|line| {
            let name = line.split_whitespace().last()?;
            let local_path = find_local_path(cfg, name);
            Some(RemoteDir {
                name: name.to_string(),
                has_local: local_path.is_some(),
                local_path,
                in_config: config_names.contains(name),
            })
        })
        .collect())
}

/// OS-generated files worth ignoring when deciding whether a directory a user is
/// about to pull into is "empty enough" to overwrite. rclone does NOT skip these
/// by default — the shipped `defaults.json` excludes them explicitly. This is no
/// longer part of the destructive-push guard, which asks rclone directly.
fn is_os_junk(name: &str) -> bool {
    name == ".DS_Store"
        || name == "Thumbs.db"
        || name == "desktop.ini"
        || name.starts_with("._")
}

pub fn local_dir_has_content(path: &str) -> bool {
    let expanded = expand_tilde(path);
    let p = Path::new(&expanded);
    p.exists()
        && p.read_dir()
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .any(|e| !is_os_junk(&e.file_name().to_string_lossy()))
            })
            .unwrap_or(false)
}

#[cfg(test)]
mod cancel_tests {
    use super::*;

    // Each test uses a distinct project name so they can share the process-wide
    // registry while running in parallel.

    #[test]
    fn cancel_while_queued_stops_the_operation_before_it_spawns() {
        // The case that carries the feature: during a "push all" most projects
        // are parked on the semaphore with no process to kill. If cancelling
        // only killed live children, every queued project would start anyway.
        let name = "test-queued";
        let _op = start_op(name).unwrap();
        assert!(request_cancel(name), "an in-flight operation should accept a cancel");
        assert!(
            check_cancelled(name).is_err(),
            "an operation cancelled while queued must refuse to start rclone"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancel_kills_a_running_child() {
        let name = "test-kill";
        let _op = start_op(name).unwrap();
        let child = Arc::new(SharedChild::spawn(Command::new("sleep").arg("30")).unwrap());
        ops().running.insert(name.to_string(), child.clone());

        assert!(request_cancel(name));

        let status = child.wait().unwrap();
        assert!(
            !status.success(),
            "the rclone process must actually die — a cancel that only updates the UI is a lie"
        );
    }

    #[test]
    fn a_finished_operation_does_not_leak_its_cancel_onto_the_next_one() {
        // A cancel landing in the moment an operation completes must not sit in
        // the registry and kill whatever the user starts next.
        let name = "test-stale";
        {
            let _op = start_op(name).unwrap();
            request_cancel(name);
        }
        let _op = start_op(name).unwrap();
        assert!(check_cancelled(name).is_ok(), "a fresh operation must start clean");
    }

    #[test]
    fn cancelling_an_idle_project_cannot_poison_its_next_run() {
        let name = "test-idle";
        assert!(!request_cancel(name), "nothing was running to cancel");
        let _op = start_op(name).unwrap();
        assert!(
            check_cancelled(name).is_ok(),
            "a cancel for an idle project must not block the run that follows it"
        );
    }

    /// Exercises `run_rclone` end to end with a stand-in for rclone that floods
    /// both pipes and then blocks. Two things would break silently without it:
    /// reading the pipes only after waiting (which deadlocks once a pipe buffer
    /// fills, hanging the sync forever), and reporting a killed process as a
    /// sync failure instead of the cancel the user asked for.
    #[cfg(unix)]
    #[test]
    fn cancelling_a_live_run_returns_the_cancel_marker_without_deadlocking() {
        let name = "test-run-cancel";
        let _op = start_op(name).unwrap();

        let mut cfg = super::tests::test_cfg(vec![]);
        cfg.rclone_path = "/bin/sh".into();
        let args = vec![
            "-c".to_string(),
            // 200 KB down each pipe — well past any pipe buffer — then block.
            "yes out | head -c 200000; yes err | head -c 200000 >&2; sleep 30".to_string(),
        ];

        std::thread::spawn(move || {
            // Cancel as soon as the child is registered, mimicking a click
            // landing mid-transfer.
            loop {
                if ops().running.contains_key(name) {
                    request_cancel(name);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });

        let err = run_rclone(&cfg, name, &args).unwrap_err();
        assert_eq!(err, CANCELLED, "a killed process must report as cancelled");
    }

    #[test]
    fn a_second_operation_on_the_same_project_is_refused() {
        let name = "test-double";
        let _op = start_op(name).unwrap();
        assert!(
            start_op(name).is_err(),
            "two rclone runs over the same directory pair must never overlap"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, RemoteConfig};
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "rcsync-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    pub(super) fn test_cfg(excludes: Vec<&str>) -> AppConfig {
        AppConfig {
            rclone_path: "rclone".into(),
            remote: "gdrive".into(),
            remotes: vec![RemoteConfig {
                name: "gdrive".into(),
                base_path: "proj".into(),
            }],
            excludes: excludes.into_iter().map(str::to_string).collect(),
            default_excludes: vec![],
            extra_excludes: vec![],
            projects: vec![],
            scan_dirs: vec![],
            default_pull_dir: String::new(),
            auto_check_on_launch: false,
            rclone_transfers: None,
        }
    }

    fn project_with(excludes: Vec<&str>) -> Project {
        Project {
            name: "size-probe-test".into(),
            local_path: String::new(),
            remote_path: String::new(),
            remote: "gdrive".into(),
            excludes: excludes.into_iter().map(str::to_string).collect(),
        }
    }

    /// The counterexample that forced this rewrite. `node_modules/**` is a
    /// shipped default exclude; rclone matches it at ANY depth, so a tree whose
    /// only content sits under a nested `node_modules` has nothing to sync. The
    /// old `globset` guard matched only at the root, called this directory
    /// non-empty, and would have let `sync` delete the entire remote copy.
    ///
    /// Adjudicated by the real rclone binary on purpose: the whole defect was a
    /// second matcher drifting from rclone's semantics, so only rclone can
    /// settle it. rclone is a hard runtime prerequisite of this app; if it is
    /// missing, this fails loudly rather than skipping.
    #[test]
    fn nested_excluded_content_reads_as_empty_to_rclone() {
        let dir = temp_dir("nested-node-modules");
        fs::create_dir_all(dir.join("packages/app/node_modules/pkg")).unwrap();
        fs::write(dir.join("packages/app/node_modules/pkg/index.js"), "x").unwrap();

        let cfg = test_cfg(vec!["node_modules/**"]);
        let count = rclone_source_file_count(&cfg, &project_with(vec![]), dir.to_str().unwrap());

        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            count.expect("rclone must be installed — it is a runtime prerequisite"),
            0,
            "rclone excludes nested node_modules, so this source is empty and a push must refuse"
        );
    }

    #[test]
    fn real_content_is_counted() {
        let dir = temp_dir("real-content");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.ts"), "export {};\n").unwrap();

        let cfg = test_cfg(vec!["node_modules/**"]);
        let count = rclone_source_file_count(&cfg, &project_with(vec![]), dir.to_str().unwrap());

        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(count.unwrap(), 1, "a normal source file must keep the push allowed");
    }

    /// Per-project excludes must reach the probe too, or a project that excludes
    /// its only content would still be judged non-empty and wipe its remote.
    #[test]
    fn per_project_excludes_reach_the_probe() {
        let dir = temp_dir("proj-exclude");
        fs::create_dir_all(dir.join("artifacts/run")).unwrap();
        fs::write(dir.join("artifacts/run/out.json"), "{}").unwrap();

        let cfg = test_cfg(vec![]); // no GLOBAL exclude for artifacts
        let with_exclude = rclone_source_file_count(
            &cfg,
            &project_with(vec!["artifacts/**"]),
            dir.to_str().unwrap(),
        );
        let without_exclude =
            rclone_source_file_count(&cfg, &project_with(vec![]), dir.to_str().unwrap());

        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(with_exclude.unwrap(), 0, "the project exclude must empty this source");
        assert_eq!(without_exclude.unwrap(), 1, "without it the source has real content");
    }

    // --- parsing: every unreadable answer must fail closed, never read as 0 ---

    // --- transfer tuning: bounded, opt-in, and nowhere near the safety probe ---

    #[test]
    fn automatic_passes_no_transfers_flag() {
        // Absence must mean absence. Passing even the current default would
        // override the user's RCLONE_TRANSFERS or rclone config, because a
        // command-line value outranks both.
        let mut cfg = test_cfg(vec![]);
        cfg.rclone_transfers = None;
        assert!(build_transfer_args(&cfg).unwrap().is_empty());
    }

    #[test]
    fn a_configured_value_becomes_exactly_one_flag_pair() {
        let mut cfg = test_cfg(vec![]);
        for n in [1, 8] {
            cfg.rclone_transfers = Some(n);
            assert_eq!(
                build_transfer_args(&cfg).unwrap(),
                vec!["--transfers".to_string(), n.to_string()]
            );
        }
    }

    #[test]
    fn out_of_range_values_fail_before_rclone_is_spawned() {
        let mut cfg = test_cfg(vec![]);
        for n in [0, 9, -1, 999] {
            cfg.rclone_transfers = Some(n);
            assert!(
                build_transfer_args(&cfg).is_err(),
                "{} should be rejected, not clamped or passed through",
                n
            );
        }
    }

    /// The empty-source guard's correctness rests on the probe seeing exactly
    /// the filters the sync sees. `--transfers` does not alter file selection,
    /// so it is safe on the sync alone — but this pins the probe's argv so that
    /// a future flag which *does* affect selection cannot drift in unnoticed.
    #[test]
    fn the_source_probe_argv_carries_filters_and_nothing_else() {
        let mut cfg = test_cfg(vec!["node_modules/**"]);
        cfg.rclone_transfers = Some(8);
        let project = project_with(vec!["artifacts/**"]);

        let mut probe = vec!["size".to_string(), "/tmp/x".to_string(), "--json".to_string()];
        probe.extend(build_exclude_args(&cfg));
        probe.extend(project_exclude_args(&project));

        assert!(
            !probe.iter().any(|a| a == "--transfers"),
            "performance flags must never reach the empty-source probe"
        );
        assert_eq!(
            probe.iter().filter(|a| *a == "--exclude").count(),
            2,
            "the probe must carry both the global and the per-project excludes"
        );
    }

    /// Both filter sets must reach a sync in the same form the probe used.
    #[test]
    fn sync_and_probe_agree_on_the_filter_arguments() {
        let cfg = test_cfg(vec!["node_modules/**", ".git/**"]);
        let project = project_with(vec!["artifacts/**"]);

        let mut filters = build_exclude_args(&cfg);
        filters.extend(project_exclude_args(&project));

        assert_eq!(
            filters,
            vec![
                "--exclude", "node_modules/**",
                "--exclude", ".git/**",
                "--exclude", "artifacts/**",
            ]
        );
    }

    // --- check verdicts: a failure must never read as "synced" ---

    #[test]
    fn all_matching_files_are_synced() {
        let out = parse_check_output("= a.txt\n= b.txt\n", 0).unwrap();
        assert!(out.synced);
        assert_eq!((out.differences, out.matches), (0, 2));
    }

    #[test]
    fn itemized_differences_are_counted_and_not_synced() {
        let out = parse_check_output("= a.txt\n* b.txt\n+ c.txt\n- d.txt\n", 1).unwrap();
        assert!(!out.synced);
        assert_eq!((out.differences, out.matches), (3, 1));
    }

    #[test]
    fn an_rclone_failure_is_never_reported_as_synced() {
        // The original defect: check returned Ok for any exit code, and the
        // frontend called anything without "N differences" synced. Each of these
        // is a run that produced no usable verdict, and each must be an error —
        // an unreachable remote must not clear a project's diff badge.
        let failures: [(&str, i32); 4] = [
            ("Failed to create file system for \"gdrive:\": couldn't fetch token", 7),
            ("", 3),
            ("ERROR: 2 differences found", 1), // exit 1, nothing itemized
            ("! unreadable.txt\n= a.txt\n", 0), // rclone could not compare a file
        ];
        for (raw, code) in failures {
            assert!(
                parse_check_output(raw, code).is_err(),
                "must refuse to give a verdict for exit {} / {:?}",
                code,
                raw
            );
        }
    }

    #[test]
    fn an_empty_project_that_rclone_checked_cleanly_is_synced() {
        // Exit 0 with nothing to compare is a real, if degenerate, "in sync".
        let out = parse_check_output("", 0).unwrap();
        assert!(out.synced);
        assert_eq!(out.matches, 0);
    }

    #[test]
    fn parses_a_real_size_payload() {
        assert_eq!(
            parse_rclone_size_count("{\"count\":42,\"bytes\":1234}").unwrap(),
            42
        );
    }

    #[test]
    fn parses_past_leading_stderr_noise() {
        // run_rclone merges stdout and stderr, so notices can precede the JSON.
        let out = "NOTICE: Config file not found\n{\"count\":3,\"bytes\":9}\n";
        assert_eq!(parse_rclone_size_count(out).unwrap(), 3);
    }

    #[test]
    fn unreadable_output_is_an_error_not_an_empty_source() {
        // Each of these once had to be distinguished from a genuine zero. If any
        // started returning Ok(0), a push would refuse; if any returned a count,
        // a push would proceed on a guess. Both are wrong — only Err is right.
        for bad in ["", "rclone: command not found", "{\"bytes\":10}", "{ not json"] {
            assert!(
                parse_rclone_size_count(bad).is_err(),
                "unreadable rclone output must fail closed, got Ok for {:?}",
                bad
            );
        }
    }

    #[test]
    fn an_explicit_zero_count_is_read_as_zero() {
        assert_eq!(parse_rclone_size_count("{\"count\":0,\"bytes\":0}").unwrap(), 0);
    }
}
