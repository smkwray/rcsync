use crate::config::{load_config, Schedule};
use crate::rclone;
use chrono::{DateTime, Local};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

#[derive(Clone, serde::Serialize)]
pub struct ScheduledPushEvent {
    pub project_id: String,
    pub project: String,
    pub phase: String,
    pub error: Option<String>,
}

#[derive(Clone, serde::Serialize)]
pub struct ScheduleStatus {
    pub project_id: String,
    pub project: String,
    pub schedule: Option<Schedule>,
    pub next_run_ms: Option<i64>,
    pub next_run: Option<String>,
    pub pending: bool,
    pub running: bool,
    pub scheduled_running: bool,
    pub warning: Option<String>,
}

#[derive(Clone)]
struct ScheduledProject {
    name: String,
    schedule: Schedule,
}

#[derive(Clone)]
struct NextRun {
    schedule: Schedule,
    due: DateTime<Local>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct PersistedSchedule {
    signature: String,
    last_completed_ms: Option<i64>,
}

struct State {
    next_runs: HashMap<String, NextRun>,
    pending: HashSet<String>,
    pending_due: HashMap<String, i64>,
    scheduled_running: HashSet<String>,
    running_due: HashMap<String, i64>,
    persisted: HashMap<String, PersistedSchedule>,
    startup: bool,
    history_valid: bool,
    history_warning: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            next_runs: HashMap::new(),
            pending: HashSet::new(),
            pending_due: HashMap::new(),
            scheduled_running: HashSet::new(),
            running_due: HashMap::new(),
            persisted: HashMap::new(),
            startup: false,
            history_valid: true,
            history_warning: None,
        }
    }
}

struct Handle {
    state: Mutex<State>,
    wake: Condvar,
}

static HANDLE: OnceLock<Arc<Handle>> = OnceLock::new();

fn handle() -> Option<Arc<Handle>> {
    HANDLE.get().cloned()
}

/// Missed-occurrence bookkeeping is deliberately device-local. The project
/// The local automation layer and its stale-run history are device-local, and
/// use a stable device ID rather than a hostname that can change.
fn persisted_path() -> PathBuf {
    let mut path = crate::config::local_data_dir_for_scheduler();
    path.push(format!(
        "schedule-state-{}.json",
        crate::config::device_id()
    ));
    path
}

fn history_marker_path() -> PathBuf {
    let mut path = persisted_path();
    path.set_extension("initialized");
    path
}

fn has_orphan_history_temp(path: &Path) -> Result<bool, std::io::Error> {
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return Ok(false);
    };
    let prefix = format!("{stem}.tmp-");
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let name = entry?.file_name();
        if name.to_string_lossy().starts_with(&prefix) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn load_persisted() -> (HashMap<String, PersistedSchedule>, bool, Option<String>) {
    let path = persisted_path();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let initialized = history_marker_path().exists();
            let interrupted = match has_orphan_history_temp(&path) {
                Ok(interrupted) => interrupted,
                Err(directory_error) => {
                    return (
                        HashMap::new(),
                        false,
                        Some(format!(
                            "Could not inspect schedule history recovery files; automatic Pushes are disabled: {directory_error}"
                        )),
                    );
                }
            };
            if initialized || interrupted {
                return (
                    HashMap::new(),
                    false,
                    Some(
                        "Schedule history is missing after an interrupted replacement; automatic Pushes are disabled".into(),
                    ),
                );
            }
            return (HashMap::new(), true, None);
        }
        Err(error) => {
            return (
                HashMap::new(),
                false,
                Some(format!(
                    "Could not read schedule history; automatic Pushes are disabled: {error}"
                )),
            );
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(records) => (records, true, None),
        Err(error) => (
            HashMap::new(),
            false,
            Some(format!(
                "Could not parse schedule history; stale scheduled Pushes are disabled: {error}"
            )),
        ),
    }
}

fn save_persisted(records: &HashMap<String, PersistedSchedule>) -> Result<(), String> {
    let path = persisted_path();
    let bytes = serde_json::to_vec_pretty(records).map_err(|error| error.to_string())?;
    let marker = history_marker_path();
    if !marker.exists() {
        crate::config::atomic_write(&marker, b"rcsync schedule history initialized\n")?;
    }
    crate::config::atomic_write(&path, &bytes)
}

fn persist_state(state: &mut State) -> bool {
    match save_persisted(&state.persisted) {
        Ok(()) => {
            state.history_valid = true;
            true
        }
        Err(error) => {
            state.history_valid = false;
            state.history_warning = Some(format!(
                "Could not save schedule history; automatic Pushes are disabled: {error}"
            ));
            false
        }
    }
}

fn schedule_signature(schedule: &Schedule) -> String {
    serde_json::to_string(schedule).unwrap_or_default()
}

fn set_persisted(
    state: &mut State,
    project_id: &str,
    schedule: &Schedule,
    last_completed_ms: Option<i64>,
) -> bool {
    let record = PersistedSchedule {
        signature: schedule_signature(schedule),
        last_completed_ms,
    };
    if state.persisted.get(project_id) == Some(&record) {
        return false;
    }
    state.persisted.insert(project_id.to_string(), record);
    true
}

/// Start the one scheduler thread for this process. It deliberately uses a
/// wall-clock recomputation loop rather than a long monotonic timer: wake,
/// sleep, DST, and manual clock changes all get reevaluated on the next pass.
pub fn start(app: AppHandle) {
    let (persisted, history_valid, history_warning) = load_persisted();
    let initial_state = State {
        persisted,
        startup: true,
        history_valid,
        history_warning,
        ..Default::default()
    };
    let handle = Arc::new(Handle {
        state: Mutex::new(initial_state),
        wake: Condvar::new(),
    });
    if HANDLE.set(handle.clone()).is_err() {
        return;
    }
    thread::Builder::new()
        .name("rcsync-scheduler".into())
        .spawn(move || run_loop(app, handle))
        .expect("failed to start rcsync scheduler");
}

pub fn notify_config_changed() {
    if let Some(handle) = handle() {
        handle.wake.notify_one();
    }
}

pub fn clear_pending(project: &str) -> bool {
    let Some(handle) = handle() else {
        return false;
    };
    let mut state = handle.state.lock().unwrap_or_else(|e| e.into_inner());
    let removed = state.pending.remove(project);
    state.pending_due.remove(project);
    handle.wake.notify_one();
    removed
}

pub fn clear_all_pending() -> usize {
    let Some(handle) = handle() else {
        return 0;
    };
    let mut state = handle.state.lock().unwrap_or_else(|e| e.into_inner());
    // Due occurrences are persisted before a pending ticket is released to
    // the scheduler. Cancelling the in-memory ticket therefore needs no
    // second history write; attempting one here could fail and leave an
    // older restart record that replays the cancelled occurrence.
    let count = clear_pending_state(&mut state);
    handle.wake.notify_one();
    count
}

fn clear_pending_state(state: &mut State) -> usize {
    let count = state.pending.len();
    state.pending.clear();
    state.pending_due.clear();
    count
}

pub fn snapshot() -> Vec<ScheduleStatus> {
    let cfg = load_config();
    let operations = rclone::active_operations();
    let state = handle().map(|h| {
        let state = h.state.lock().unwrap_or_else(|e| e.into_inner());
        let next_runs = state
            .next_runs
            .iter()
            .map(|(project_id, next)| (project_id.clone(), next.due))
            .collect::<HashMap<_, _>>();
        (
            next_runs,
            state.pending.clone(),
            state.scheduled_running.clone(),
            state.history_warning.clone(),
        )
    });
    cfg.projects
        .iter()
        .filter_map(|project| {
            let schedule = project.schedule.clone();
            let next = state
                .as_ref()
                .and_then(|(next_runs, _, _, _)| next_runs.get(&project.id));
            let fallback_next = if next.is_none() {
                schedule
                    .as_ref()
                    .and_then(|value| value.next_after(Local::now()))
            } else {
                None
            };
            let pending = state
                .as_ref()
                .map(|(_, pending, _, _)| pending.contains(&project.id))
                .unwrap_or(false);
            let scheduled_running = state
                .as_ref()
                .map(|(_, _, scheduled_running, _)| scheduled_running.contains(&project.id))
                .unwrap_or(false);
            let operation = operations.iter().find(|op| op.project_id == project.id);
            if schedule.is_none() && !pending && operation.is_none() {
                return None;
            }
            Some(ScheduleStatus {
                project_id: project.id.clone(),
                project: project.name.clone(),
                schedule,
                next_run_ms: next
                    .map(|n| n.timestamp_millis())
                    .or_else(|| fallback_next.map(|n| n.timestamp_millis())),
                next_run: next
                    .map(|n| n.format("%a %I:%M %p").to_string())
                    .or_else(|| fallback_next.map(|n| n.format("%a %I:%M %p").to_string())),
                pending,
                running: operation.is_some(),
                scheduled_running,
                warning: state
                    .as_ref()
                    .and_then(|(_, _, _, warning)| warning.clone()),
            })
        })
        .collect()
}

fn run_loop(app: AppHandle, handle: Arc<Handle>) {
    loop {
        let now = Local::now();
        let cfg = load_config();
        let schedules: HashMap<String, ScheduledProject> = cfg
            .projects
            .iter()
            .filter_map(|project| {
                project.schedule.clone().map(|schedule| {
                    (
                        project.id.clone(),
                        ScheduledProject {
                            name: project.name.clone(),
                            schedule,
                        },
                    )
                })
            })
            .collect();

        let active: HashSet<String> = rclone::active_operations()
            .into_iter()
            .map(|op| op.project_id)
            .collect();
        let due = {
            let mut state = handle.state.lock().unwrap_or_else(|e| e.into_inner());
            reconcile_state(now, &schedules, &active, &mut state)
        };

        // Due work and pending work both go through the same claim attempt. A
        // busy project remains pending; a successful claim clears the bit.
        // Queueing is deliberately only for scheduled Pushes. Manual work can
        // still use the existing three-process semaphore independently.
        let scheduled_running = !handle
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .scheduled_running
            .is_empty();
        let pending_due = handle
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_due
            .clone();
        let due = select_scheduled_work(
            due,
            cfg.queue_scheduled_pushes,
            scheduled_running,
            &pending_due,
        );
        for project_id in due {
            if let Some(project) = schedules.get(&project_id) {
                try_start(&app, &handle, project_id, project.name.clone());
            }
        }

        let wait = next_wait(&handle, &schedules, Local::now());
        let guard = handle.state.lock().unwrap_or_else(|e| e.into_inner());
        let _ = handle.wake.wait_timeout(guard, wait);
    }
}

fn reconcile_state(
    now: DateTime<Local>,
    schedules: &HashMap<String, ScheduledProject>,
    active: &HashSet<String>,
    state: &mut State,
) -> Vec<String> {
    let mut work = Vec::new();
    let startup = state.startup;
    let mut persistence_changed = false;
    state
        .next_runs
        .retain(|project_id, _| schedules.contains_key(project_id));
    state
        .pending
        .retain(|project_id| schedules.contains_key(project_id));
    state
        .pending_due
        .retain(|project_id, _| state.pending.contains(project_id));
    state
        .persisted
        .retain(|project_id, _| schedules.contains_key(project_id));

    // History is the at-most-once boundary for destructive automatic Pushes.
    // Once it cannot be read or written, keep schedules visible but do not
    // manufacture pending work from memory or stale wall-clock state.
    if !state.history_valid {
        state.pending.clear();
        state.pending_due.clear();
        for (project_id, scheduled) in schedules {
            if let Some(next) = scheduled.schedule.next_after(now) {
                state.next_runs.insert(
                    project_id.clone(),
                    NextRun {
                        schedule: scheduled.schedule.clone(),
                        due: next,
                    },
                );
            }
        }
        state.startup = false;
        return Vec::new();
    }

    for (project_id, scheduled) in schedules {
        let schedule = &scheduled.schedule;
        let needs_rebase = state
            .next_runs
            .get(project_id)
            .map(|next| next.schedule != *schedule)
            .unwrap_or(true);
        if needs_rebase {
            // A changed schedule invalidates an occurrence that was collected
            // under the old cadence. The editor also clears this bit, but
            // doing it here closes the reload/manual-file edit path as well.
            state.pending.remove(project_id);
            state.pending_due.remove(project_id);
            let signature = schedule_signature(schedule);
            let persisted = state.persisted.get(project_id).cloned();
            let same_schedule = persisted
                .as_ref()
                .is_some_and(|record| record.signature == signature);
            let last_completed_ms = persisted
                .as_ref()
                .and_then(|record| record.last_completed_ms);

            // On the first pass after process start, a matching persisted
            // schedule (or a schedule with no history yet) may have crossed
            // one or many occurrences while the app was closed. Coalesce all
            // of them into one pending Push. A changed schedule is rebased
            // without replaying the old cadence.
            if startup && state.history_valid && (same_schedule || persisted.is_none()) {
                let mut completed_for_history = last_completed_ms;
                if let Some(latest) = schedule.latest_at_or_before(now) {
                    if last_completed_ms
                        .map(|completed| latest.timestamp_millis() > completed)
                        .unwrap_or(true)
                    {
                        let latest_ms = latest.timestamp_millis();
                        state.pending.insert(project_id.clone());
                        state.pending_due.insert(project_id.clone(), latest_ms);
                        // The coalesced ticket has consumed every missed
                        // occurrence through `latest_ms`. Keep that value
                        // when the history record is persisted below; a
                        // second write of the old completion time here would
                        // reopen the same stale ticket after a restart.
                        completed_for_history = Some(latest_ms);
                        work.push(project_id.clone());
                    }
                }
                persistence_changed |=
                    set_persisted(state, project_id, schedule, completed_for_history);
            } else {
                // A schedule created or changed while the process is already
                // alive starts at its next future occurrence. Past wall-clock
                // times are not treated as stale work in that case.
                let baseline = schedule
                    .latest_at_or_before(now)
                    .map(|latest| latest.timestamp_millis());
                persistence_changed |= set_persisted(state, project_id, schedule, baseline);
            }
            if let Some(next) = schedule.next_after(now) {
                state.next_runs.insert(
                    project_id.clone(),
                    NextRun {
                        schedule: schedule.clone(),
                        due: next,
                    },
                );
            }
            continue;
        }

        if state
            .next_runs
            .get(project_id)
            .is_some_and(|next| next.due <= now)
        {
            let latest_due = schedule
                .latest_at_or_before(now)
                .or_else(|| state.next_runs.get(project_id).map(|next| next.due));
            if let Some(next) = schedule.next_after(now) {
                state.next_runs.insert(
                    project_id.clone(),
                    NextRun {
                        schedule: schedule.clone(),
                        due: next,
                    },
                );
            }
            if let Some(due) = latest_due {
                let due_ms = due.timestamp_millis();
                // A busy project may cross several cadence slots before its
                // one queued Push starts. The ticket represents the latest
                // coalesced occurrence, not the first one that made it busy.
                let is_new = state.pending.insert(project_id.clone());
                let should_update = state
                    .pending_due
                    .get(project_id)
                    .map(|previous| due_ms > *previous)
                    .unwrap_or(true);
                if should_update {
                    state.pending_due.insert(project_id.clone(), due_ms);
                    persistence_changed |= set_persisted(state, project_id, schedule, Some(due_ms));
                }
                if is_new {
                    work.push(project_id.clone());
                }
            } else if state.pending.insert(project_id.clone()) {
                work.push(project_id.clone());
            }
        }
    }

    for project_id in state.pending.clone() {
        if !active.contains(&project_id) && !work.iter().any(|work_id| work_id == &project_id) {
            work.push(project_id);
        }
    }
    work.sort();
    work.dedup();
    if startup {
        state.startup = false;
    }
    if persistence_changed && !persist_state(state) {
        // A due ticket must never be released to `try_start` unless the
        // occurrence it represents is durable. Keep the warning and leave the
        // scheduler fail-closed for this process.
        work.clear();
    }
    if state.history_valid {
        work
    } else {
        Vec::new()
    }
}

fn select_scheduled_work(
    mut due: Vec<String>,
    queue_enabled: bool,
    scheduled_running: bool,
    pending_due: &HashMap<String, i64>,
) -> Vec<String> {
    due.sort_by_key(|project_id| {
        (
            pending_due.get(project_id).copied().unwrap_or(i64::MAX),
            project_id.clone(),
        )
    });
    if queue_enabled {
        if scheduled_running {
            due.clear();
        } else {
            due.truncate(1);
        }
    }
    due
}

fn next_wait(
    handle: &Handle,
    schedules: &HashMap<String, ScheduledProject>,
    now: DateTime<Local>,
) -> Duration {
    let state = handle.state.lock().unwrap_or_else(|e| e.into_inner());
    let until_next = state
        .next_runs
        .iter()
        .filter(|(project_id, _)| schedules.contains_key(*project_id))
        .map(|(_, next)| (next.due - now).to_std().unwrap_or(Duration::ZERO))
        .min()
        .unwrap_or(Duration::from_secs(30));
    until_next
        .min(Duration::from_secs(30))
        .max(Duration::from_millis(100))
}

enum ClaimResult {
    Stale,
    Deferred,
    Started {
        operation: rclone::OpGuard,
        due_ms: i64,
    },
}

/// Claim the scheduler ticket and project operation under the same lock order
/// used by schedule edits: config I/O, scheduler state, then operation state.
/// This makes a persisted disable/change and a due claim mutually exclusive.
fn claim_due(handle: &Arc<Handle>, project_id: &str, name: &str) -> ClaimResult {
    crate::config::with_config_lock(|| {
        let current_schedule = crate::config::load_config_unlocked()
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .and_then(|project| project.schedule.clone());
        let mut state = handle.state.lock().unwrap_or_else(|e| e.into_inner());
        let still_current = current_schedule.as_ref().is_some_and(|schedule| {
            state.pending.contains(project_id)
                && state
                    .next_runs
                    .get(project_id)
                    .is_some_and(|next| next.schedule == *schedule)
        });
        if !still_current {
            state.pending.remove(project_id);
            state.pending_due.remove(project_id);
            return ClaimResult::Stale;
        }

        match rclone::start_op_with(project_id, name, "push", true) {
            Ok(operation) => {
                let due_ms = state
                    .pending_due
                    .remove(project_id)
                    .unwrap_or_else(|| Local::now().timestamp_millis());
                state.pending.remove(project_id);
                state.scheduled_running.insert(project_id.to_string());
                state.running_due.insert(project_id.to_string(), due_ms);
                ClaimResult::Started { operation, due_ms }
            }
            Err(_) => ClaimResult::Deferred,
        }
    })
}

fn finish_scheduled(handle: &Arc<Handle>, project_id: &str, due_ms: i64) {
    // Read config before taking the scheduler lock: schedule edits take the
    // locks in the opposite order (config, then scheduler).
    let current_schedule = load_config()
        .projects
        .iter()
        .find(|project| project.id == project_id)
        .and_then(|project| project.schedule.clone());
    let mut state = handle.state.lock().unwrap_or_else(|e| e.into_inner());
    state.scheduled_running.remove(project_id);
    state.running_due.remove(project_id);
    if let Some(schedule) = current_schedule {
        let signature = schedule_signature(&schedule);
        let last_completed_ms = state
            .persisted
            .get(project_id)
            .filter(|record| record.signature == signature)
            .and_then(|record| record.last_completed_ms)
            .map(|last| last.max(due_ms))
            .or(Some(due_ms));
        if set_persisted(&mut state, project_id, &schedule, last_completed_ms) {
            persist_state(&mut state);
        }
    }
    handle.wake.notify_one();
}

fn try_start(app: &AppHandle, handle: &Arc<Handle>, project_id: String, name: String) {
    match claim_due(handle, &project_id, &name) {
        ClaimResult::Stale => (),
        ClaimResult::Deferred => {
            emit(app, &project_id, &name, "deferred", None);
        }
        ClaimResult::Started { operation, due_ms } => {
            let app = app.clone();
            let handle = handle.clone();
            emit(&app, &project_id, &name, "started", None);
            tauri::async_runtime::spawn(async move {
                // Keep the claim alive through the terminal event. Dropping it before
                // emitting lets the scheduler or a manual Push start a replacement
                // whose UI state could then be cleared by this older completion.
                let result =
                    crate::push_claimed(project_id.clone(), name.clone(), false, &operation).await;
                match result {
                    Ok(_) => emit(&app, &project_id, &name, "succeeded", None),
                    Err(error) if error == rclone::CANCELLED => {
                        emit(&app, &project_id, &name, "cancelled", None)
                    }
                    Err(error) => emit(&app, &project_id, &name, "failed", Some(error)),
                }
                finish_scheduled(&handle, &project_id, due_ms);
                drop(operation);
            });
        }
    };
}

fn emit(app: &AppHandle, project_id: &str, project: &str, phase: &str, error: Option<String>) {
    let _ = app.emit(
        "scheduled-push-event",
        ScheduledPushEvent {
            project_id: project_id.to_string(),
            project: project.to_string(),
            phase: phase.to_string(),
            error,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{IntervalUnit, Schedule};
    use chrono::TimeZone;

    #[test]
    fn busy_project_keeps_one_pending_occurrence_until_free() {
        let now = Local
            .with_ymd_and_hms(2026, 8, 29, 12, 0, 0)
            .single()
            .unwrap();
        let schedule = Schedule::Interval {
            every: 1,
            unit: IntervalUnit::Hours,
            origin_ms: now.timestamp_millis() - 3_600_000,
        };
        let schedules = HashMap::from([(
            String::from("p"),
            ScheduledProject {
                name: "p".into(),
                schedule: schedule.clone(),
            },
        )]);
        let mut state = State::default();
        state
            .next_runs
            .insert("p".into(), NextRun { schedule, due: now });
        let busy = HashSet::from([String::from("p")]);
        assert_eq!(
            reconcile_state(now, &schedules, &busy, &mut state),
            vec!["p"]
        );
        assert!(state.pending.contains("p"));
        assert_eq!(state.pending.len(), 1);
        assert_eq!(
            reconcile_state(now, &schedules, &HashSet::new(), &mut state),
            vec!["p"]
        );
    }

    #[test]
    fn disabling_or_changing_a_schedule_drops_old_pending_work() {
        let now = Local
            .with_ymd_and_hms(2026, 8, 29, 12, 0, 0)
            .single()
            .unwrap();
        let old = Schedule::Interval {
            every: 1,
            unit: IntervalUnit::Hours,
            origin_ms: now.timestamp_millis() - 3_600_000,
        };
        let mut state = State::default();
        state.next_runs.insert(
            "p".into(),
            NextRun {
                schedule: old.clone(),
                due: now,
            },
        );
        state.pending.insert("p".into());
        assert!(reconcile_state(now, &HashMap::new(), &HashSet::new(), &mut state).is_empty());
        assert!(!state.pending.contains("p"));
        assert!(!state.next_runs.contains_key("p"));

        state.next_runs.insert(
            "p".into(),
            NextRun {
                schedule: old,
                due: now,
            },
        );
        state.pending.insert("p".into());
        let changed = Schedule::Interval {
            every: 2,
            unit: IntervalUnit::Hours,
            origin_ms: now.timestamp_millis(),
        };
        let schedules = HashMap::from([(
            String::from("p"),
            ScheduledProject {
                name: "p".into(),
                schedule: changed.clone(),
            },
        )]);
        assert!(reconcile_state(now, &schedules, &HashSet::new(), &mut state).is_empty());
        assert!(!state.pending.contains("p"));
        assert_eq!(state.next_runs["p"].schedule, changed);
    }

    #[test]
    fn interval_rebase_is_strictly_after_now() {
        let now = Local
            .with_ymd_and_hms(2026, 8, 29, 12, 0, 0)
            .single()
            .unwrap();
        let schedule = Schedule::Interval {
            every: 1,
            unit: IntervalUnit::Hours,
            origin_ms: now.timestamp_millis(),
        };
        assert!(schedule.next_after(now).unwrap() > now);
    }

    #[test]
    fn queueing_allows_one_scheduled_push_and_leaves_the_rest_pending() {
        let due = vec![String::from("a"), String::from("b")];
        let pending_due = HashMap::new();
        assert_eq!(
            select_scheduled_work(due.clone(), true, false, &pending_due),
            vec!["a"]
        );
        assert!(select_scheduled_work(due.clone(), true, true, &pending_due).is_empty());
        assert_eq!(
            select_scheduled_work(due, false, true, &pending_due),
            vec!["a", "b"]
        );
    }

    #[test]
    fn oldest_pending_project_is_selected_before_lexicographically_earlier_work() {
        let due = vec![String::from("newer"), String::from("older")];
        let pending_due = HashMap::from([
            (String::from("newer"), 200_i64),
            (String::from("older"), 100_i64),
        ]);
        assert_eq!(
            select_scheduled_work(due, true, false, &pending_due),
            vec!["older"]
        );
    }

    #[test]
    fn a_busy_multi_occurrence_ticket_records_the_latest_due() {
        let now = Local
            .with_ymd_and_hms(2026, 8, 29, 12, 0, 0)
            .single()
            .unwrap();
        let schedule = Schedule::Interval {
            every: 1,
            unit: IntervalUnit::Hours,
            origin_ms: now.timestamp_millis() - 3 * 3_600_000,
        };
        let mut state = State::default();
        state.next_runs.insert(
            "p".into(),
            NextRun {
                schedule: schedule.clone(),
                due: now,
            },
        );
        state.pending.insert("p".into());
        state
            .pending_due
            .insert("p".into(), now.timestamp_millis() - 3_600_000);
        let schedules = HashMap::from([(
            String::from("p"),
            ScheduledProject {
                name: "project".into(),
                schedule,
            },
        )]);

        reconcile_state(
            now,
            &schedules,
            &HashSet::from([String::from("project")]),
            &mut state,
        );

        assert_eq!(state.pending_due.get("p"), Some(&now.timestamp_millis()));
    }

    #[test]
    fn cancel_all_discards_only_in_memory_pending_tickets() {
        let now = Local
            .with_ymd_and_hms(2026, 8, 29, 12, 0, 0)
            .single()
            .unwrap();
        let schedule = Schedule::Interval {
            every: 24,
            unit: IntervalUnit::Hours,
            origin_ms: now.timestamp_millis(),
        };
        let due_ms = now.timestamp_millis() - 1;
        let mut state = State::default();
        state.pending.insert("p".into());
        state.pending_due.insert("p".into(), due_ms);
        state.persisted.insert(
            "p".into(),
            PersistedSchedule {
                signature: schedule_signature(&schedule),
                // Reconciliation durably consumes the occurrence before it
                // creates the pending ticket that Cancel All can discard.
                last_completed_ms: Some(due_ms),
            },
        );

        assert_eq!(clear_pending_state(&mut state), 1);
        assert!(state.pending.is_empty());
        assert!(state.pending_due.is_empty());
        assert_eq!(state.persisted["p"].last_completed_ms, Some(due_ms));
    }

    #[test]
    fn malformed_history_disables_startup_stale_replay() {
        let _env = crate::config::TestConfigEnv::new("malformed-history");
        fs::create_dir_all(crate::config::local_data_dir_for_scheduler()).unwrap();
        fs::write(persisted_path(), b"{not-json").unwrap();
        let (_, valid, warning) = load_persisted();
        assert!(!valid);
        assert!(warning.is_some());

        let now = Local
            .with_ymd_and_hms(2026, 8, 29, 12, 0, 0)
            .single()
            .unwrap();
        let schedule = Schedule::Interval {
            every: 24,
            unit: IntervalUnit::Hours,
            origin_ms: now.timestamp_millis() - 72 * 3_600_000,
        };
        let schedules = HashMap::from([(
            String::from("p"),
            ScheduledProject {
                name: "project".into(),
                schedule: schedule.clone(),
            },
        )]);
        let mut state = State {
            startup: true,
            history_valid: false,
            history_warning: warning,
            ..State::default()
        };
        assert!(reconcile_state(now, &schedules, &HashSet::new(), &mut state).is_empty());
        assert!(state.pending.is_empty());
        assert!(state.next_runs["p"].due > now);
    }

    #[test]
    fn history_write_failure_prevents_start_and_stale_replay() {
        let _env = crate::config::TestConfigEnv::new("history-write-failure");
        // Make the configured local-data path a file. Reading the history
        // below then fails with ENOTDIR, and any attempted atomic write fails
        // at the same boundary rather than creating a fallback elsewhere.
        let data_dir = crate::config::local_data_dir_for_scheduler();
        fs::write(&data_dir, b"not-a-directory").unwrap();

        let now = Local
            .with_ymd_and_hms(2026, 8, 29, 12, 0, 0)
            .single()
            .unwrap();
        let schedule = Schedule::Interval {
            every: 24,
            unit: IntervalUnit::Hours,
            origin_ms: now.timestamp_millis() - 24 * 60 * 60 * 1000,
        };
        let schedules = HashMap::from([(
            String::from("p"),
            ScheduledProject {
                name: "project".into(),
                schedule: schedule.clone(),
            },
        )]);
        let mut state = State {
            next_runs: HashMap::from([(
                String::from("p"),
                NextRun {
                    schedule: schedule.clone(),
                    due: now,
                },
            )]),
            ..State::default()
        };

        let work = reconcile_state(now, &schedules, &HashSet::new(), &mut state);
        assert!(work.is_empty(), "unwritten occurrence must not be released");
        assert!(!state.history_valid);
        assert!(state
            .history_warning
            .as_deref()
            .is_some_and(|warning| warning.contains("automatic Pushes are disabled")));

        // A new process seeing the same unreadable history must also fail
        // closed instead of treating the read error as an empty history.
        let (_, valid_after_restart, warning_after_restart) = load_persisted();
        assert!(!valid_after_restart);
        assert!(warning_after_restart.is_some());
        fs::remove_file(data_dir).unwrap();
    }

    #[test]
    fn interrupted_history_replacement_fails_closed() {
        let _env = crate::config::TestConfigEnv::new("interrupted-history");
        let now = Local
            .with_ymd_and_hms(2026, 8, 29, 12, 0, 0)
            .single()
            .unwrap();
        let schedule = Schedule::Interval {
            every: 24,
            unit: IntervalUnit::Hours,
            origin_ms: now.timestamp_millis() - 24 * 60 * 60 * 1000,
        };
        let due_ms = now.timestamp_millis() - 1;
        let records = HashMap::from([(
            String::from("p"),
            PersistedSchedule {
                signature: schedule_signature(&schedule),
                last_completed_ms: Some(due_ms),
            },
        )]);
        save_persisted(&records).unwrap();

        // The marker proves this is an established history. Removing the
        // primary and leaving an atomic-write temporary models a Windows
        // crash after the old destination was removed but before the new
        // file was renamed into place.
        fs::remove_file(persisted_path()).unwrap();
        let orphan_tmp = persisted_path().with_extension("tmp-orphan");
        fs::write(&orphan_tmp, br#"{}"#).unwrap();

        let (_, history_valid, warning) = load_persisted();
        assert!(!history_valid);
        assert!(warning
            .as_deref()
            .is_some_and(|warning| warning.contains("interrupted replacement")));

        let schedules = HashMap::from([(
            String::from("p"),
            ScheduledProject {
                name: "project".into(),
                schedule,
            },
        )]);
        let mut state = State {
            startup: true,
            history_valid: false,
            history_warning: warning,
            ..State::default()
        };
        assert!(reconcile_state(now, &schedules, &HashSet::new(), &mut state).is_empty());
        assert!(state.pending.is_empty());
        assert!(state.next_runs["p"].due > now);

        fs::remove_file(orphan_tmp).unwrap();
        fs::remove_file(history_marker_path()).unwrap();
    }

    #[test]
    fn scheduled_push_keeps_claim_and_cancellation_by_id_across_rename() {
        let _env = crate::config::TestConfigEnv::new("scheduler-rename");
        let id = "p_rename";
        let schedule = Schedule::Interval {
            every: 24,
            unit: IntervalUnit::Hours,
            origin_ms: 1,
        };
        let cfg = crate::config::AppConfig {
            projects: vec![crate::config::Project {
                id: id.into(),
                name: "old-name".into(),
                local_path: "~/projects/old-name".into(),
                remote_path: "proj/old-name".into(),
                remote: "gdrive".into(),
                excludes: Vec::new(),
                schedule: None,
                schedule_error: None,
                legacy_schedule: None,
                legacy_schedule_raw: None,
            }],
            ..crate::config::AppConfig::default()
        };
        crate::config::save_config(&cfg).unwrap();
        crate::config::set_local_project_schedule("old-name", Some(id), Some(schedule.clone()))
            .unwrap();

        let now = Local::now();
        let handle = Arc::new(Handle {
            state: Mutex::new(State {
                next_runs: HashMap::from([(id.into(), NextRun { schedule, due: now })]),
                pending: HashSet::from([id.into()]),
                pending_due: HashMap::from([(id.into(), now.timestamp_millis())]),
                ..State::default()
            }),
            wake: Condvar::new(),
        });
        let operation = match claim_due(&handle, id, "old-name") {
            ClaimResult::Started { operation, .. } => operation,
            ClaimResult::Stale => panic!("the current schedule must remain claimable"),
            ClaimResult::Deferred => panic!("the ID must not be busy"),
        };

        crate::config::edit_config(|cfg| {
            cfg.projects[0].name = "new-name".into();
            Ok(())
        })
        .unwrap();
        assert!(rclone::request_cancel(id));
        assert!(rclone::check_cancelled(id).is_err());
        assert!(rclone::active_operations()
            .iter()
            .any(|operation| { operation.project_id == id && operation.project == "old-name" }));
        drop(operation);
    }

    #[test]
    fn startup_replays_one_coalesced_stale_occurrence() {
        let now = Local
            .with_ymd_and_hms(2026, 8, 29, 12, 0, 0)
            .single()
            .unwrap();
        let schedule = Schedule::Interval {
            every: 24,
            unit: IntervalUnit::Hours,
            origin_ms: now.timestamp_millis() - 72 * 60 * 60 * 1000,
        };
        let latest = schedule.latest_at_or_before(now).unwrap();
        let mut state = State {
            startup: true,
            ..Default::default()
        };
        state.persisted.insert(
            String::from("p"),
            PersistedSchedule {
                signature: schedule_signature(&schedule),
                last_completed_ms: Some(latest.timestamp_millis() - 24 * 60 * 60 * 1000),
            },
        );
        let schedules = HashMap::from([(
            String::from("p"),
            ScheduledProject {
                name: "p".into(),
                schedule: schedule.clone(),
            },
        )]);
        assert_eq!(
            reconcile_state(now, &schedules, &HashSet::new(), &mut state),
            vec!["p"]
        );
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.pending_due.get("p"), Some(&latest.timestamp_millis()));
        assert_eq!(
            state.persisted["p"].last_completed_ms,
            Some(latest.timestamp_millis())
        );
        assert!(state.next_runs["p"].due > now);
    }

    #[test]
    fn a_disabled_schedule_cannot_claim_a_stale_due_ticket() {
        let _env = crate::config::TestConfigEnv::new("stale-schedule-claim");
        let name = format!("stale-claim-{}", std::process::id());
        let mut cfg = crate::config::AppConfig::default();
        cfg.projects.push(crate::config::Project {
            id: "p_stale".into(),
            name: name.clone(),
            local_path: "~/p".into(),
            remote_path: String::new(),
            remote: "gdrive".into(),
            excludes: Vec::new(),
            schedule: None,
            schedule_error: None,
            legacy_schedule: None,
            legacy_schedule_raw: None,
        });
        crate::config::save_config(&cfg).unwrap();

        let now = Local
            .with_ymd_and_hms(2026, 8, 29, 12, 0, 0)
            .single()
            .unwrap();
        let old_schedule = Schedule::Interval {
            every: 1,
            unit: IntervalUnit::Hours,
            origin_ms: now.timestamp_millis(),
        };
        let handle = Arc::new(Handle {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        });
        {
            let mut state = handle.state.lock().unwrap();
            state.next_runs.insert(
                "p_stale".into(),
                NextRun {
                    schedule: old_schedule,
                    due: now,
                },
            );
            state.pending.insert("p_stale".into());
        }

        assert!(matches!(
            claim_due(&handle, "p_stale", &name),
            ClaimResult::Stale
        ));
        assert!(rclone::active_operations()
            .iter()
            .all(|operation| operation.project != name));
    }
}
