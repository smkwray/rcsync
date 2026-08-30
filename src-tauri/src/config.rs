use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, Local, LocalResult, NaiveDateTime, TimeZone,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, OnceLock,
};

#[cfg(test)]
static TEST_CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serialises tests that redirect the process-wide config path, restores any
/// pre-existing override even during unwinding, and removes only the scratch
/// file it created. Keeping this guard shared across test modules matters:
/// independent locks still race on the same environment variable.
#[cfg(test)]
pub(crate) struct TestConfigEnv {
    _lock: std::sync::MutexGuard<'static, ()>,
    path: PathBuf,
    local_path: PathBuf,
    local_data_dir: PathBuf,
    previous: Option<std::ffi::OsString>,
    previous_local: Option<std::ffi::OsString>,
    previous_local_data_dir: Option<std::ffi::OsString>,
    previous_device_id: Option<std::ffi::OsString>,
    previous_device_id_file: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl TestConfigEnv {
    pub(crate) fn new(label: &str) -> Self {
        let lock = TEST_CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "rcsync-{}-{}-{}.json",
            label,
            std::process::id(),
            nonce
        ));
        let previous = std::env::var_os("RCSYNC_CONFIG");
        let previous_local = std::env::var_os("RCSYNC_LOCAL_CONFIG");
        let previous_local_data_dir = std::env::var_os("RCSYNC_LOCAL_DATA_DIR");
        let previous_device_id = std::env::var_os("RCSYNC_DEVICE_ID");
        let previous_device_id_file = std::env::var_os("RCSYNC_DEVICE_ID_FILE");
        let local_path = std::env::temp_dir().join(format!(
            "rcsync-local-{}-{}-{}.json",
            label,
            std::process::id(),
            nonce
        ));
        let local_data_dir = std::env::temp_dir().join(format!(
            "rcsync-local-data-{}-{}-{}",
            label,
            std::process::id(),
            nonce
        ));
        std::env::set_var("RCSYNC_CONFIG", &path);
        std::env::set_var("RCSYNC_LOCAL_CONFIG", &local_path);
        std::env::set_var("RCSYNC_LOCAL_DATA_DIR", &local_data_dir);
        std::env::remove_var("RCSYNC_DEVICE_ID");
        std::env::remove_var("RCSYNC_DEVICE_ID_FILE");
        Self {
            _lock: lock,
            path,
            local_path,
            local_data_dir,
            previous,
            previous_local,
            previous_local_data_dir,
            previous_device_id,
            previous_device_id_file,
        }
    }
}

#[cfg(test)]
impl Drop for TestConfigEnv {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(&self.local_path);
        let _ = fs::remove_dir_all(&self.local_data_dir);
        match self.previous.take() {
            Some(value) => std::env::set_var("RCSYNC_CONFIG", value),
            None => std::env::remove_var("RCSYNC_CONFIG"),
        }
        match self.previous_local.take() {
            Some(value) => std::env::set_var("RCSYNC_LOCAL_CONFIG", value),
            None => std::env::remove_var("RCSYNC_LOCAL_CONFIG"),
        }
        match self.previous_local_data_dir.take() {
            Some(value) => std::env::set_var("RCSYNC_LOCAL_DATA_DIR", value),
            None => std::env::remove_var("RCSYNC_LOCAL_DATA_DIR"),
        }
        match self.previous_device_id.take() {
            Some(value) => std::env::set_var("RCSYNC_DEVICE_ID", value),
            None => std::env::remove_var("RCSYNC_DEVICE_ID"),
        }
        match self.previous_device_id_file.take() {
            Some(value) => std::env::set_var("RCSYNC_DEVICE_ID_FILE", value),
            None => std::env::remove_var("RCSYNC_DEVICE_ID_FILE"),
        }
    }
}

/// The two interval units intentionally mean elapsed time, not a local clock
/// time.  A day is exactly 24 hours; weekly schedules below are the civil-time
/// form that follows local wall-clock rules.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IntervalUnit {
    Hours,
    Days,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Schedule {
    Interval {
        every: u32,
        unit: IntervalUnit,
        /// Unix epoch milliseconds.  This anchors the cadence and is never
        /// moved by a manual Push.
        origin_ms: i64,
    },
    Weekly {
        /// Sunday = 0 through Saturday = 6, matching JavaScript Date.getDay.
        weekdays: Vec<u8>,
        /// Minutes after midnight in local time.
        minute: u16,
    },
}

impl Schedule {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Interval {
                every, origin_ms, ..
            } => {
                if *every == 0 {
                    return Err("Schedule interval must be at least 1".into());
                }
                if *origin_ms <= 0 {
                    return Err(
                        "Schedule interval origin must be a positive epoch timestamp".into(),
                    );
                }
            }
            Self::Weekly { weekdays, minute } => {
                if weekdays.is_empty() {
                    return Err("Weekly schedule needs at least one day".into());
                }
                if weekdays.iter().any(|day| *day > 6) {
                    return Err("Weekly schedule days must be Sunday through Saturday".into());
                }
                if *minute >= 24 * 60 {
                    return Err("Weekly schedule time must be within a day".into());
                }
            }
        }
        Ok(())
    }

    /// Return the first occurrence strictly after `now`.
    ///
    /// Ambiguous fall-back times use the first occurrence.  A nonexistent
    /// spring-forward minute advances one minute at a time until local time
    /// becomes valid, which lands on the first valid minute after the gap.
    pub fn next_after(&self, now: DateTime<Local>) -> Option<DateTime<Local>> {
        if self.validate().is_err() {
            return None;
        }
        match self {
            Self::Interval {
                every,
                unit,
                origin_ms,
            } => {
                let period = i64::from(*every).checked_mul(match unit {
                    IntervalUnit::Hours => 60 * 60 * 1000,
                    IntervalUnit::Days => 24 * 60 * 60 * 1000,
                })?;
                let now_ms = now.timestamp_millis();
                let elapsed = now_ms.saturating_sub(*origin_ms);
                let steps = if elapsed < 0 { 0 } else { elapsed / period + 1 };
                let next_ms = origin_ms.checked_add(period.checked_mul(steps)?)?;
                Local.timestamp_millis_opt(next_ms).single()
            }
            Self::Weekly { weekdays, minute } => {
                let base = now.date_naive();
                let hour = u32::from(*minute / 60);
                let min = u32::from(*minute % 60);
                for offset in 0..=7 {
                    let date = base.checked_add_signed(ChronoDuration::days(offset))?;
                    if !weekdays.contains(&(date.weekday().num_days_from_sunday() as u8)) {
                        continue;
                    }
                    let naive = date.and_hms_opt(hour, min, 0)?;
                    if let Some(candidate) = first_valid_local(naive) {
                        if candidate > now {
                            return Some(candidate);
                        }
                    }
                }
                None
            }
        }
    }

    /// Return the most recent occurrence at or before `now`. This is used only
    /// for recovering one coalesced stale occurrence after the app was closed;
    /// it never changes the anchored cadence.
    pub fn latest_at_or_before(&self, now: DateTime<Local>) -> Option<DateTime<Local>> {
        if self.validate().is_err() {
            return None;
        }
        match self {
            Self::Interval {
                every,
                unit,
                origin_ms,
            } => {
                let period = i64::from(*every).checked_mul(match unit {
                    IntervalUnit::Hours => 60 * 60 * 1000,
                    IntervalUnit::Days => 24 * 60 * 60 * 1000,
                })?;
                let elapsed = now.timestamp_millis().checked_sub(*origin_ms)?;
                if elapsed < period {
                    return None;
                }
                let steps = elapsed / period;
                let latest_ms = origin_ms.checked_add(period.checked_mul(steps)?)?;
                Local.timestamp_millis_opt(latest_ms).single()
            }
            Self::Weekly { weekdays, minute } => {
                let base = now.date_naive();
                let hour = u32::from(*minute / 60);
                let min = u32::from(*minute % 60);
                for offset in 0..=7 {
                    let date = base.checked_sub_signed(ChronoDuration::days(offset))?;
                    if !weekdays.contains(&(date.weekday().num_days_from_sunday() as u8)) {
                        continue;
                    }
                    let naive = date.and_hms_opt(hour, min, 0)?;
                    if let Some(candidate) = first_valid_local(naive) {
                        if candidate <= now {
                            return Some(candidate);
                        }
                    }
                }
                None
            }
        }
    }
}

fn first_valid_local(naive: NaiveDateTime) -> Option<DateTime<Local>> {
    first_valid_with(naive, |candidate| Local.from_local_datetime(&candidate))
}

fn first_valid_with<F>(naive: NaiveDateTime, mut resolve: F) -> Option<DateTime<Local>>
where
    F: FnMut(NaiveDateTime) -> LocalResult<DateTime<Local>>,
{
    for minute in 0..=(4 * 60) {
        let candidate = naive.checked_add_signed(ChronoDuration::minutes(minute))?;
        match resolve(candidate) {
            LocalResult::Single(dt) => return Some(dt),
            LocalResult::Ambiguous(first, _) => return Some(first),
            LocalResult::None => continue,
        }
    }
    None
}

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    /// Stable identity used by device-local automation. The display name is
    /// mutable; schedules must not follow a renamed project by name alone.
    pub id: String,
    pub name: String,
    pub local_path: String,
    pub remote_path: String,
    /// Which remote this project syncs with. Empty/missing = first remote in list.
    #[serde(default)]
    pub remote: String,
    /// Per-project rclone excludes, applied IN ADDITION to the global excludes
    /// but only when syncing THIS project. Each entry is an rclone filter
    /// pattern matched relative to the project root, e.g. "artifacts/**".
    /// Empty/missing = no project-specific excludes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Schedule>,
    #[serde(skip)]
    pub(crate) schedule_error: Option<String>,
    /// A schedule found in the shared config. It is retained only long enough
    /// to offer an explicit one-time migration and is never effective.
    #[serde(skip)]
    pub(crate) legacy_schedule: Option<Schedule>,
    /// Preserve even a malformed legacy schedule until the owner explicitly
    /// migrates or repairs it; ordinary shared-config saves must not erase it.
    #[serde(skip)]
    pub(crate) legacy_schedule_raw: Option<serde_json::Value>,
}

/// Schedule is optional, so a malformed schedule must not make serde discard
/// the entire UserConfig.  Parse that field as JSON first and retain the rest
/// of the project while exposing an explicit warning to the UI.
impl<'de> Deserialize<'de> for Project {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            id: String,
            name: String,
            local_path: String,
            remote_path: String,
            #[serde(default)]
            remote: String,
            #[serde(default)]
            excludes: Vec<String>,
            #[serde(default)]
            schedule: Option<serde_json::Value>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let raw_schedule = wire.schedule;
        let (legacy_schedule, schedule_error) = match raw_schedule.clone() {
            None => (None, None),
            Some(value) => match serde_json::from_value::<Schedule>(value) {
                Ok(schedule) => match schedule.validate() {
                    Ok(()) => (Some(schedule), None),
                    Err(error) => (None, Some(error)),
                },
                Err(error) => (None, Some(format!("Invalid project schedule: {}", error))),
            },
        };
        Ok(Self {
            // Keep a blank wire ID blank. Settings uses that fact to
            // distinguish a new row from an explicitly retained identity;
            // shared-config loading bootstraps old blank records separately.
            id: wire.id,
            name: wire.name,
            local_path: wire.local_path,
            remote_path: wire.remote_path,
            remote: wire.remote,
            excludes: wire.excludes,
            schedule: legacy_schedule.clone(),
            schedule_error,
            legacy_schedule,
            legacy_schedule_raw: raw_schedule,
        })
    }
}

/// A shared cross-device record of a remote target that the owner explicitly
/// retired. This is keyed by the resolved target, not by project name or ID:
/// both names and IDs can be reused after a project is removed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetiredTarget {
    pub remote: String,
    pub remote_path: String,
    pub name_at_retirement: String,
    pub project_id_at_retirement: String,
    pub retired_at_ms: i64,
    pub retired_by_device: String,
}

/// Generate a stable project identifier from a legacy record. The identifier
/// is only a bootstrap for records without one; once written, it follows the
/// project through renames and path edits.
pub fn project_id_for_fields(
    name: &str,
    local_path: &str,
    remote_path: &str,
    remote: &str,
) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for part in [name, local_path, remote_path, remote] {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("p_{hash:016x}")
}

fn ensure_project_id(project: &mut Project) {
    if project.id.trim().is_empty() {
        project.id = fresh_project_id();
    }
}

/// Give an old on-disk record with no ID a stable bootstrap identity before
/// device-local schedules are merged. This is deliberately separate from
/// `ensure_project_id`: a blank Settings row is new and must receive a fresh
/// opaque ID instead of being treated as an old disk record.
fn bootstrap_legacy_project_id(project: &mut Project) {
    if project.id.trim().is_empty() {
        project.id = project_id_for_fields(
            &project.name,
            &project.local_path,
            &project.remote_path,
            &project.remote,
        );
    }
}

/// IDs for newly-created records must not be reproducible from mutable project
/// fields. Otherwise deleting a project and recreating an identical record
/// could silently reactivate a device-local schedule for the old project.
fn fresh_project_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("p_{nonce:016x}{serial:016x}")
}

/// Materialize a scan-discovered project as a configured record. Discovery IDs
/// are selectors only; every record created by the app gets a fresh opaque ID
/// so deleting and recreating a directory cannot silently revive its local
/// schedule.
pub(crate) fn materialize_discovered_project(project: &Project) -> Result<Project, String> {
    let _lock = config_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut cfg = load_config_unlocked();
    if let Some(existing) = cfg
        .projects
        .iter()
        .find(|candidate| candidate.id == project.id)
    {
        return Ok(existing.clone());
    }
    if cfg
        .projects
        .iter()
        .any(|candidate| candidate.name == project.name)
    {
        return Err(format!(
            "Project '{}' already exists with a different identity",
            project.name
        ));
    }

    let mut record = project.clone();
    record.id = fresh_project_id();
    record.schedule = None;
    record.schedule_error = None;
    record.legacy_schedule = None;
    record.legacy_schedule_raw = None;
    cfg.projects.push(record.clone());
    save_config_unlocked(&cfg)?;
    Ok(record)
}

/// Allocate the ID for a project that will be persisted after a successful
/// operation, such as Browse Remote's first pull.
pub(crate) fn fresh_project_id_for_new_record() -> String {
    fresh_project_id()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub name: String,
    /// Base path on this remote, e.g. "proj" means projects are at remote:proj/name
    pub base_path: String,
}

/// Public defaults — shipped with the app, checked into git.
/// Lives at `defaults.json` next to the executable or in the project root.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Defaults {
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub scan_dirs: Vec<String>,
    #[serde(default)]
    pub default_pull_dir: String,
}

/// Private user config — gitignored, contains user-specific settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserConfig {
    #[serde(default = "default_rclone_path")]
    pub rclone_path: String,
    #[serde(default = "default_remote")]
    pub remote: String,
    #[serde(default = "default_remotes")]
    pub remotes: Vec<RemoteConfig>,
    #[serde(default)]
    pub projects: Vec<Project>,
    /// Shared retirement intent. Unlike schedules, this deliberately roams
    /// with the project definitions so another device can block a stale
    /// auto-discovered copy before it writes the remote.
    #[serde(default)]
    pub retired_targets: Vec<RetiredTarget>,
    /// Safety state must not degrade to an empty list when its field is
    /// malformed. Populated by the raw config reader and never serialized.
    #[serde(skip)]
    pub retired_targets_unreadable: bool,
    #[serde(default)]
    pub scan_dirs: Vec<String>,
    #[serde(default)]
    pub default_pull_dir: String,
    #[serde(default)]
    pub auto_check_on_launch: bool,
    /// Legacy shared value. It is deserialized so migration can offer it and
    /// preserved by ordinary saves until explicit migration consumes it. It
    /// is never used by the scheduler.
    #[serde(default, skip_serializing)]
    pub queue_scheduled_pushes: Option<bool>,
    /// User-added excludes (merged with defaults, not replacing them)
    #[serde(default)]
    pub extra_excludes: Vec<String>,
    /// Parallel file transfers per rclone process. `None` means Automatic: pass
    /// no flag at all, so rclone's own config or RCLONE_TRANSFERS still decides.
    /// Machine-level policy, deliberately not per-project — it is about this
    /// host's memory and this link, not about any one directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rclone_transfers: Option<i32>,
}

/// Device-local automation layer. This file is deliberately outside the
/// portable project config so syncing project definitions cannot sync an
/// automatic Push schedule to another machine.
#[derive(Debug, Clone, Serialize)]
pub struct LocalAutomationConfig {
    pub schema: String,
    pub device_id: String,
    pub display_name: String,
    pub aliases: Vec<String>,
    pub queue_scheduled_pushes: bool,
    pub schedules: Vec<LocalSchedule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalSchedule {
    pub project_id: String,
    pub last_seen_name: String,
    pub schedule: Schedule,
}

#[derive(Debug, Deserialize)]
struct LocalAutomationWire {
    #[serde(default)]
    schema: Option<String>,
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default = "default_queue_scheduled_pushes")]
    queue_scheduled_pushes: bool,
    #[serde(default)]
    schedules: serde_json::Value,
}

/// The one rule for what an exclude pattern may be, applied where a pattern is
/// SAVED and again where it is read.
///
/// Refusing rather than repairing is the point. rclone strips a filters-file
/// line but hands a command-line value to its matcher untouched, so a padded
/// pattern selects one set for the empty-source probe and another for the
/// bi-sync that probe guards — and quietly trimming it on the way in makes the
/// rule unreachable, untested, and false for any config edited by hand.
///
/// Order matters: a line break is rejected before anything else, so an entry
/// that is only whitespace and a newline is refused rather than passed over as
/// a blank row.
pub fn validate_exclude_pattern(pattern: &str) -> Result<Option<&str>, String> {
    if pattern.contains('\n') || pattern.contains('\r') {
        return Err(format!(
            "Exclude pattern contains a line break, which would forge extra filter rules: {:?}",
            pattern
        ));
    }
    // A row that is empty or only spaces is a blank line in a settings box, not
    // a filter. Checked after the line-break rule above, so an entry that is
    // whitespace *plus* a newline is refused rather than passed over as blank.
    if pattern.trim().is_empty() {
        return Ok(None);
    }
    if pattern != pattern.trim() {
        return Err(format!(
            "Exclude pattern has leading or trailing whitespace. rclone strips it from a filters \
             file but not from a command-line argument, so the two would select different files. \
             Remove the spaces: {:?}",
            pattern
        ));
    }
    Ok(Some(pattern))
}

/// Validate a whole list, rejecting on the first bad entry.
pub fn validate_excludes(patterns: &[String]) -> Result<(), String> {
    for p in patterns {
        validate_exclude_pattern(p)?;
    }
    Ok(())
}

/// Accept only a bounded range. `i32` rather than a smaller unsigned type so a
/// hand-edited negative still deserializes and reports a real error, instead of
/// failing the whole-config parse and silently reverting every other setting.
pub fn validate_rclone_transfers(value: Option<i32>) -> Result<(), String> {
    match value {
        None | Some(1..=8) => Ok(()),
        Some(v) => Err(format!(
            "rclone transfers must be between 1 and 8, or unset for Automatic; got {}",
            v
        )),
    }
}

fn default_rclone_path() -> String {
    "rclone".into()
}

fn default_remote() -> String {
    "gdrive".into()
}

/// The merged config exposed to the rest of the app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub rclone_path: String,
    /// Active remote name (must match a name in `remotes`)
    pub remote: String,
    /// Available remotes
    #[serde(default = "default_remotes")]
    pub remotes: Vec<RemoteConfig>,
    /// Combined excludes: defaults + user extras
    pub excludes: Vec<String>,
    /// Which excludes come from defaults (so the UI can mark them)
    pub default_excludes: Vec<String>,
    /// User-added excludes only
    pub extra_excludes: Vec<String>,
    pub projects: Vec<Project>,
    pub retired_targets: Vec<RetiredTarget>,
    #[serde(skip)]
    pub retired_targets_unreadable: bool,
    #[serde(default = "default_scan_dirs")]
    pub scan_dirs: Vec<String>,
    /// Default directory for pulling new projects into
    #[serde(default = "default_pull_dir")]
    pub default_pull_dir: String,
    #[serde(default)]
    pub auto_check_on_launch: bool,
    #[serde(default = "default_queue_scheduled_pushes")]
    pub queue_scheduled_pushes: bool,
    /// Number of valid/invalid schedules found in the shared legacy config.
    /// They are intentionally not effective until explicitly migrated.
    #[serde(default)]
    pub legacy_schedule_count: usize,
    /// Whether the shared config still contains the legacy queue policy.
    #[serde(default)]
    pub legacy_queue_policy: bool,
    /// A prior release may have written a whole config selected by hostname.
    /// It is never read automatically; the Settings action can convert it.
    #[serde(default)]
    pub legacy_host_config_available: bool,
    /// Raw legacy shared queue value retained across ordinary saves. This is
    /// intentionally not sent to the frontend; migration consumes it only
    /// after the user explicitly chooses Move here.
    #[serde(skip)]
    pub(crate) legacy_queue_scheduled_pushes: Option<bool>,
    #[serde(default)]
    pub rclone_transfers: Option<i32>,
    /// Non-fatal config issues retained for explicit UI/log reporting. Never
    /// serialized back into the private user config.
    #[serde(default)]
    pub config_warnings: Vec<String>,
}

fn default_remotes() -> Vec<RemoteConfig> {
    vec![RemoteConfig {
        name: "gdrive".into(),
        base_path: "proj".into(),
    }]
}

impl AppConfig {
    /// Get the active remote config. Falls back to constructing one from `remote` field.
    pub fn active_remote(&self) -> RemoteConfig {
        self.remotes
            .iter()
            .find(|r| r.name == self.remote)
            .cloned()
            .unwrap_or(RemoteConfig {
                name: self.remote.clone(),
                base_path: "proj".into(),
            })
    }

    /// Get a specific remote config by name
    pub fn get_remote(&self, name: &str) -> RemoteConfig {
        self.remotes
            .iter()
            .find(|r| r.name == name)
            .cloned()
            .unwrap_or(RemoteConfig {
                name: name.to_string(),
                base_path: "proj".into(),
            })
    }

    /// The default remote name (first in list)
    pub fn default_remote_name(&self) -> String {
        self.remotes
            .first()
            .map(|r| r.name.clone())
            .unwrap_or_else(|| self.remote.clone())
    }

    /// Resolve which remote a project uses. Falls back to first remote if unset.
    pub fn project_remote(&self, project: &Project) -> RemoteConfig {
        let name = if project.remote.is_empty() {
            self.default_remote_name()
        } else {
            project.remote.clone()
        };
        self.get_remote(&name)
    }

    /// Join a remote base path and one immediate child without changing the
    /// child's identity. Only slash delimiters are normalized; whitespace and
    /// every other character remain significant to the remote backend.
    pub fn canonical_remote_path(path: &str) -> String {
        path.trim_matches('/').to_string()
    }

    pub fn join_remote_child_path(base_path: &str, child: &str) -> String {
        let base = Self::canonical_remote_path(base_path);
        let child = Self::canonical_remote_path(child);
        match (base.is_empty(), child.is_empty()) {
            (true, true) => String::new(),
            (true, false) => child,
            (false, true) => base,
            (false, false) => format!("{base}/{child}"),
        }
    }

    /// Build the full rclone remote path for a project.
    /// If `project.remote_path` is non-empty, it overrides the default `{base_path}/{name}`
    /// — allowing a project to live anywhere under its remote (e.g. "docs/important-thing"
    /// instead of "proj/important-thing"). Boundary "/" delimiters in the override are
    /// stripped so the result is always relative to the remote root.
    pub fn remote_path_for_project(&self, project: &Project) -> String {
        format!(
            "{}:{}",
            self.project_remote(project).name,
            self.project_remote_path(project)
        )
    }

    /// The path portion alone, without the `remote:` prefix. The single place
    /// that answers "where does this project live on its remote" — hardcoding
    /// `proj/<name>` anywhere else silently ignores a remote whose configured
    /// `base_path` is something different, and points operations at a tree the
    /// user never chose.
    pub fn project_remote_path(&self, project: &Project) -> String {
        if project.remote_path.is_empty() {
            Self::join_remote_child_path(&self.project_remote(project).base_path, &project.name)
        } else {
            Self::canonical_remote_path(&project.remote_path)
        }
    }

    /// Return the exact remote target used by rclone for this project.
    /// Retirement deliberately uses this resolved pair rather than mutable
    /// project names or IDs so a stale discovered folder is still recognized.
    pub fn retired_target_for(&self, project: &Project) -> Option<&RetiredTarget> {
        let remote = self.project_remote(project).name;
        let remote_path = self.project_remote_path(project);
        self.retired_targets.iter().find(|target| {
            target.remote == remote
                && AppConfig::canonical_remote_path(&target.remote_path) == remote_path
        })
    }

    /// Gate every operation that can write to a remote. A malformed shared
    /// retirement list fails closed instead of becoming an empty list.
    pub fn ensure_remote_target_writable(&self, project: &Project) -> Result<(), String> {
        if self.retired_targets_unreadable {
            return Err(
                "Shared retired-target safety records are unreadable; remote writes are disabled until the config is repaired".into(),
            );
        }
        if let Some(target) = self.retired_target_for(project) {
            return Err(format!(
                "Remote target {}:{} was retired on {}. Reattach it explicitly before writing again.",
                target.remote, target.remote_path, target.retired_by_device
            ));
        }
        Ok(())
    }
}

fn default_scan_dirs() -> Vec<String> {
    vec!["~/projects".into()]
}

fn default_pull_dir() -> String {
    "~/projects".into()
}

fn default_queue_scheduled_pushes() -> bool {
    true
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            rclone_path: default_rclone_path(),
            remote: default_remote(),
            remotes: default_remotes(),
            projects: vec![],
            retired_targets: vec![],
            retired_targets_unreadable: false,
            scan_dirs: vec![],
            default_pull_dir: String::new(),
            auto_check_on_launch: false,
            queue_scheduled_pushes: None,
            extra_excludes: vec![],
            rclone_transfers: None, // Automatic — let rclone decide
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        let defaults = load_defaults();
        Self {
            rclone_path: "rclone".into(),
            remote: "gdrive".into(),
            excludes: defaults.excludes.clone(),
            default_excludes: defaults.excludes.clone(),
            extra_excludes: vec![],
            projects: vec![],
            retired_targets: vec![],
            retired_targets_unreadable: false,
            remotes: default_remotes(),
            scan_dirs: if defaults.scan_dirs.is_empty() {
                default_scan_dirs()
            } else {
                defaults.scan_dirs
            },
            default_pull_dir: if defaults.default_pull_dir.is_empty() {
                default_pull_dir()
            } else {
                defaults.default_pull_dir
            },
            auto_check_on_launch: false,
            queue_scheduled_pushes: default_queue_scheduled_pushes(),
            legacy_schedule_count: 0,
            legacy_queue_policy: false,
            legacy_host_config_available: false,
            legacy_queue_scheduled_pushes: None,
            rclone_transfers: None,
            config_warnings: Vec::new(),
        }
    }
}

/// Resolve the directory next to the executable (climbing out of .app bundles on macOS).
fn exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?.to_path_buf();
    // Detect .app bundle: climb out of Contents/MacOS
    if dir.ends_with("MacOS") {
        if let Some(contents) = dir.parent() {
            if let Some(app_bundle) = contents.parent() {
                if let Some(app_parent) = app_bundle.parent() {
                    dir = app_parent.to_path_buf();
                }
            }
        }
    }
    Some(dir)
}

/// Load the public defaults file. Searched in order:
///   1. $RCSYNC_DEFAULTS env var
///   2. Next to the executable / .app bundle
///   3. Embedded fallback (compiled-in)
fn load_defaults() -> Defaults {
    // 1. Env var override
    if let Ok(p) = std::env::var("RCSYNC_DEFAULTS") {
        if let Ok(contents) = fs::read_to_string(&p) {
            if let Ok(d) = serde_json::from_str(&contents) {
                return d;
            }
        }
    }

    // 2. Next to executable
    if let Some(dir) = exe_dir() {
        let path = dir.join("defaults.json");
        if let Ok(contents) = fs::read_to_string(&path) {
            if let Ok(d) = serde_json::from_str(&contents) {
                return d;
            }
        }
    }

    // 3. Compiled-in fallback
    serde_json::from_str(include_str!("../defaults.json")).unwrap_or_default()
}

/// Get the current machine label for display and diagnostics. This is not the
/// durable identity used by the local automation file.
pub fn machine_name() -> String {
    let raw = std::env::var("RCSYNC_MACHINE")
        .or_else(|_| hostname::get().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|_| "default".into())
        .to_lowercase();
    sanitize_machine_label(raw.strip_suffix(".local").unwrap_or(&raw))
}

fn sanitize_machine_label(raw: &str) -> String {
    let mut label = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            label.push(ch);
        } else if !label.ends_with('-') {
            label.push('-');
        }
    }
    let label = label.trim_matches('-').to_string();
    if label.is_empty() {
        "default".into()
    } else {
        label.chars().take(63).collect()
    }
}

/// Portable user config path. Resolution order:
///   1. $RCSYNC_CONFIG (explicit override)
///   2. `rcsync-config.json` next to the executable / app bundle
///   3. `rcsync/rcsync-config.json` in the platform config directory
///
/// The portable base is deliberately not selected by hostname. Host-specific
/// whole-config files are legacy inputs and must be converted explicitly; a
/// machine rename must never silently switch the project base.
fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("RCSYNC_CONFIG") {
        return PathBuf::from(p);
    }
    if let Some(dir) = exe_dir() {
        return dir.join("rcsync-config.json");
    }
    platform_config_path()
}

fn legacy_host_config_path() -> Option<PathBuf> {
    if std::env::var_os("RCSYNC_CONFIG").is_some() {
        return None;
    }
    let host = machine_name();
    let candidates = [
        exe_dir().map(|dir| dir.join(format!("rcsync-config-{host}.json"))),
        dirs::config_dir().map(|dir| dir.join("rcsync").join(format!("config-{host}.json"))),
    ];
    candidates.into_iter().flatten().find(|path| path.exists())
}

fn platform_config_path() -> PathBuf {
    let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("rcsync");
    fs::create_dir_all(&p).ok();
    p.push("rcsync-config.json");
    p
}

/// Device-local application data. Tests override this directory so no test
/// can create or modify the owner's real automation file.
fn local_data_dir() -> PathBuf {
    if let Ok(path) = std::env::var("RCSYNC_LOCAL_DATA_DIR") {
        return PathBuf::from(path);
    }
    dirs::data_local_dir()
        .or_else(dirs::config_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("rcsync")
}

pub(crate) fn local_data_dir_for_scheduler() -> PathBuf {
    local_data_dir()
}

fn device_id_path() -> PathBuf {
    if let Ok(path) = std::env::var("RCSYNC_DEVICE_ID_FILE") {
        return PathBuf::from(path);
    }
    local_data_dir().join("device-id")
}

/// Stable per-install identity. Hostnames are deliberately only labels: a
/// rename must not make a device silently lose or acquire schedules.
pub fn device_id() -> String {
    if let Ok(value) = std::env::var("RCSYNC_DEVICE_ID") {
        if !value.trim().is_empty() {
            return value.trim().to_string();
        }
    }
    static GENERATED: OnceLock<String> = OnceLock::new();
    GENERATED
        .get_or_init(|| {
            let path = device_id_path();
            if let Ok(value) = fs::read_to_string(&path) {
                if !value.trim().is_empty() {
                    return value.trim().to_string();
                }
            }
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let source = format!("{}:{}:{}", machine_name(), nonce, std::process::id());
            let mut low = 0xcbf29ce484222325u64;
            for byte in source.as_bytes() {
                low ^= u64::from(*byte);
                low = low.wrapping_mul(0x100000001b3);
            }
            let id = format!("d_{low:016x}");
            let _ = atomic_write(&path, id.as_bytes());
            id
        })
        .clone()
}

fn local_config_path() -> PathBuf {
    if let Ok(path) = std::env::var("RCSYNC_LOCAL_CONFIG") {
        return PathBuf::from(path);
    }
    local_data_dir().join(format!("local-config-{}.json", device_id()))
}

fn config_io_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Run a callback while holding the process-local config I/O lock. Callers that
/// must coordinate a config mutation with scheduler state use this to keep the
/// lock order `config I/O -> scheduler state`.
pub(crate) fn with_config_lock<F, T>(callback: F) -> T
where
    F: FnOnce() -> T,
{
    let _lock = config_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    callback()
}

const LOCAL_SCHEMA: &str = "rcsync.local.v1";

fn default_local_automation() -> LocalAutomationConfig {
    LocalAutomationConfig {
        schema: LOCAL_SCHEMA.into(),
        device_id: device_id(),
        display_name: machine_name(),
        aliases: Vec::new(),
        queue_scheduled_pushes: default_queue_scheduled_pushes(),
        schedules: Vec::new(),
    }
}

/// Read the device-local automation layer. A malformed local layer fails
/// closed for automation while leaving shared project data usable.
fn load_local_automation_unlocked() -> (LocalAutomationConfig, Vec<String>) {
    let path = local_config_path();
    let expected_device = device_id();
    let mut warnings = Vec::new();
    let Ok(bytes) = fs::read(&path) else {
        return (default_local_automation(), warnings);
    };
    let wire: LocalAutomationWire = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            warnings.push(format!(
                "Could not parse device-local automation config; schedules are disabled: {error}"
            ));
            return (default_local_automation(), warnings);
        }
    };
    if wire.schema.as_deref() != Some(LOCAL_SCHEMA) {
        warnings.push(format!(
            "Unsupported device-local automation schema; schedules are disabled: {:?}",
            wire.schema
        ));
        return (default_local_automation(), warnings);
    }
    if wire.device_id.as_deref() != Some(expected_device.as_str()) {
        warnings.push(
            "Device-local automation belongs to a different device; schedules are disabled".into(),
        );
        return (default_local_automation(), warnings);
    }

    let mut local = LocalAutomationConfig {
        schema: LOCAL_SCHEMA.into(),
        device_id: expected_device,
        display_name: wire.display_name,
        aliases: wire.aliases,
        queue_scheduled_pushes: wire.queue_scheduled_pushes,
        schedules: Vec::new(),
    };
    let Some(entries) = wire.schedules.as_array() else {
        if !wire.schedules.is_null() {
            warnings.push("Device-local schedules must be an array; schedules are disabled".into());
        }
        return (local, warnings);
    };

    let mut duplicate_ids = HashSet::new();
    let mut seen_ids = HashSet::new();
    for entry in entries {
        match serde_json::from_value::<LocalSchedule>(entry.clone()) {
            Ok(schedule) if schedule.project_id.trim().is_empty() => {
                warnings.push("Device-local schedule has no project ID; it was ignored".into());
            }
            Ok(schedule) => {
                if schedule.schedule.validate().is_err() {
                    warnings.push(format!(
                        "Device-local schedule for '{}' is invalid; it was ignored",
                        schedule.last_seen_name
                    ));
                    continue;
                }
                if !seen_ids.insert(schedule.project_id.clone()) {
                    duplicate_ids.insert(schedule.project_id.clone());
                }
                local.schedules.push(schedule);
            }
            Err(error) => warnings.push(format!(
                "Invalid device-local schedule; it was ignored: {error}"
            )),
        }
    }
    if !duplicate_ids.is_empty() {
        local
            .schedules
            .retain(|schedule| !duplicate_ids.contains(&schedule.project_id));
        for id in duplicate_ids {
            warnings.push(format!(
                "Duplicate device-local schedules for project ID '{id}'; that schedule is disabled"
            ));
        }
    }
    (local, warnings)
}

fn save_local_automation_unlocked(local: &mut LocalAutomationConfig) -> Result<(), String> {
    local.schema = LOCAL_SCHEMA.into();
    local.device_id = device_id();
    let label = machine_name();
    if !local.display_name.is_empty()
        && local.display_name != label
        && !local.aliases.contains(&local.display_name)
    {
        local.aliases.push(local.display_name.clone());
    }
    local.display_name = label;
    local.aliases.retain(|alias| alias != &local.display_name);
    local
        .schedules
        .sort_by(|a, b| a.project_id.cmp(&b.project_id));
    let json = serde_json::to_string_pretty(local).map_err(|e| e.to_string())?;
    atomic_write(&local_config_path(), json.as_bytes())
}

fn read_user_config_unlocked(config_warnings: &mut Vec<String>) -> UserConfig {
    match fs::read_to_string(config_path()) {
        Ok(contents) => {
            // Parse the retirement field independently so one malformed
            // safety record does not discard the otherwise usable project
            // config and silently turn the safety state into "none".
            let mut raw: serde_json::Value = match serde_json::from_str(&contents) {
                Ok(value) => value,
                Err(error) => {
                    config_warnings.push(format!(
                        "Could not parse rcsync config; remote writes are disabled: {error}"
                    ));
                    return UserConfig {
                        retired_targets_unreadable: true,
                        ..UserConfig::default()
                    };
                }
            };
            let retired_raw = raw
                .as_object_mut()
                .and_then(|object| object.remove("retired_targets"));
            let mut user: UserConfig = match serde_json::from_value(raw) {
                Ok(value) => value,
                Err(error) => {
                    config_warnings.push(format!(
                        "Could not parse rcsync config; remote writes are disabled: {error}"
                    ));
                    return UserConfig {
                        retired_targets_unreadable: true,
                        ..UserConfig::default()
                    };
                }
            };
            if let Some(value) = retired_raw {
                match serde_json::from_value::<Vec<RetiredTarget>>(value) {
                    Ok(targets) => user.retired_targets = targets,
                    Err(error) => {
                        user.retired_targets_unreadable = true;
                        config_warnings.push(format!(
                            "Could not parse shared retired-target safety records; remote writes are disabled: {error}"
                        ));
                    }
                }
            }
            user
        }
        Err(_) => UserConfig::default(),
    }
}

/// Load and merge: defaults (public) + user config (private) → AppConfig.
/// Reads share the writer lock so Windows' delete-then-rename replacement
/// cannot look like a missing config to the scheduler.
pub fn load_config() -> AppConfig {
    let _lock = config_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    load_config_unlocked()
}

pub(crate) fn load_config_unlocked() -> AppConfig {
    let defaults = load_defaults();
    let mut config_warnings = Vec::new();
    let legacy_host_config_available =
        !config_path().exists() && legacy_host_config_path().is_some();
    if let Some(path) = legacy_host_config_path() {
        if legacy_host_config_available {
            config_warnings.push(format!(
                "Legacy host-specific config found at {}; convert it explicitly in Settings before using it",
                path.display()
            ));
        }
    }
    let user = read_user_config_unlocked(&mut config_warnings);
    let (local, local_warnings) = load_local_automation_unlocked();
    config_warnings.extend(local_warnings);

    let mut projects = user.projects;
    let mut legacy_schedule_count = 0;
    let mut seen_project_ids = HashSet::new();
    let mut duplicate_project_ids = HashSet::new();
    for project in &mut projects {
        bootstrap_legacy_project_id(project);
        ensure_project_id(project);
        if !seen_project_ids.insert(project.id.clone()) {
            duplicate_project_ids.insert(project.id.clone());
        }
        if let Some(error) = project.schedule_error.take() {
            config_warnings.push(format!("Project '{}': {error}", project.name));
            legacy_schedule_count += 1;
        } else if project.legacy_schedule_raw.is_some() {
            legacy_schedule_count += 1;
            config_warnings.push(format!(
                "Project '{}': shared legacy schedule is disabled; move it to this device from the Schedules manager",
                project.name
            ));
        }
        project.schedule = None;
    }

    for id in &duplicate_project_ids {
        config_warnings.push(format!(
            "Duplicate shared project ID '{}'; device-local automation for that ID is inactive",
            id
        ));
    }

    for local_schedule in &local.schedules {
        if duplicate_project_ids.contains(&local_schedule.project_id) {
            config_warnings.push(format!(
                "Device-local schedule for '{}' is inactive because its shared project ID is duplicated",
                local_schedule.last_seen_name
            ));
        } else if let Some(project) = projects
            .iter_mut()
            .find(|project| project.id == local_schedule.project_id)
        {
            project.schedule = Some(local_schedule.schedule.clone());
        } else {
            config_warnings.push(format!(
                "Device-local schedule for '{}' has no matching project and is inactive",
                local_schedule.last_seen_name
            ));
        }
    }

    // Merge excludes: defaults + user extras (deduplicated)
    let mut excludes = defaults.excludes.clone();
    for ex in &user.extra_excludes {
        if !excludes.contains(ex) {
            excludes.push(ex.clone());
        }
    }

    // User scan_dirs override defaults if non-empty
    let scan_dirs = if user.scan_dirs.is_empty() {
        if defaults.scan_dirs.is_empty() {
            default_scan_dirs()
        } else {
            defaults.scan_dirs.clone()
        }
    } else {
        user.scan_dirs
    };

    let default_pull_dir = if user.default_pull_dir.is_empty() {
        if defaults.default_pull_dir.is_empty() {
            default_pull_dir()
        } else {
            defaults.default_pull_dir.clone()
        }
    } else {
        user.default_pull_dir
    };

    AppConfig {
        rclone_path: user.rclone_path,
        remote: user.remote,
        remotes: user.remotes,
        excludes,
        default_excludes: defaults.excludes,
        extra_excludes: user.extra_excludes,
        projects,
        retired_targets: user.retired_targets,
        retired_targets_unreadable: user.retired_targets_unreadable,
        scan_dirs,
        default_pull_dir,
        auto_check_on_launch: user.auto_check_on_launch,
        queue_scheduled_pushes: local.queue_scheduled_pushes,
        legacy_schedule_count,
        legacy_queue_policy: user.queue_scheduled_pushes.is_some(),
        legacy_host_config_available,
        legacy_queue_scheduled_pushes: user.queue_scheduled_pushes,
        rclone_transfers: user.rclone_transfers,
        config_warnings,
    }
}

/// Apply one load-modify-save transaction while serialising all writers. This
/// is the path for commands that edit one project field; it prevents a schedule
/// save from racing an ignore/remote save and losing the other field.
pub fn edit_config<F>(edit: F) -> Result<(), String>
where
    F: FnOnce(&mut AppConfig) -> Result<(), String>,
{
    let _lock = config_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut cfg = load_config_unlocked();
    edit(&mut cfg)?;
    save_config_unlocked(&cfg)
}

/// Publish retirement intent and remove the project in the same shared-config
/// write. The caller deletes local bytes only after this succeeds, so another
/// device cannot observe a removed project without the matching safety record.
pub fn retire_target_and_remove_project(project: &Project) -> Result<(), String> {
    edit_config(|cfg| {
        if cfg.retired_targets_unreadable {
            return Err(
                "Shared retired-target safety records are unreadable; local deletion was aborted"
                    .into(),
            );
        }
        let current = cfg
            .projects
            .iter()
            .find(|candidate| candidate.id == project.id)
            .cloned()
            .unwrap_or_else(|| project.clone());
        let remote = cfg.project_remote(&current).name;
        let remote_path = cfg.project_remote_path(&current);
        if !cfg.retired_targets.iter().any(|target| {
            target.remote == remote
                && AppConfig::canonical_remote_path(&target.remote_path) == remote_path
        }) {
            cfg.retired_targets.push(RetiredTarget {
                remote,
                remote_path,
                name_at_retirement: current.name.clone(),
                project_id_at_retirement: current.id.clone(),
                retired_at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis() as i64)
                    .unwrap_or(0),
                retired_by_device: device_id(),
            });
        }
        cfg.projects.retain(|candidate| candidate.id != project.id);
        Ok(())
    })
}

/// Explicitly reattach a retired target as a new project generation. The old
/// ID and any device-local schedule are never reused.
pub fn reattach_retired_target(project: &Project) -> Result<String, String> {
    let mut new_id = String::new();
    edit_config(|cfg| {
        if cfg.retired_targets_unreadable {
            return Err(
                "Shared retired-target safety records are unreadable; reattach is unavailable until the config is repaired".into(),
            );
        }
        let current = cfg
            .projects
            .iter()
            .find(|candidate| candidate.id == project.id)
            .cloned()
            .unwrap_or_else(|| project.clone());
        let remote = cfg.project_remote(&current).name;
        let remote_path = cfg.project_remote_path(&current);
        let had_target = cfg.retired_targets.iter().any(|target| {
            target.remote == remote
                && AppConfig::canonical_remote_path(&target.remote_path) == remote_path
        });
        if !had_target {
            return Err(format!(
                "Remote target {}:{} is not retired",
                remote, remote_path
            ));
        }
        cfg.retired_targets.retain(|target| {
            target.remote != remote
                || AppConfig::canonical_remote_path(&target.remote_path) != remote_path
        });
        cfg.projects.retain(|candidate| candidate.id != project.id);
        let mut recreated = current;
        new_id = fresh_project_id();
        recreated.id = new_id.clone();
        recreated.schedule = None;
        recreated.schedule_error = None;
        recreated.legacy_schedule = None;
        recreated.legacy_schedule_raw = None;
        cfg.projects.push(recreated);
        Ok(())
    })?;
    Ok(new_id)
}

/// Save only the private user config. Defaults are never written by the app.
#[cfg(test)]
pub fn save_config(cfg: &AppConfig) -> Result<(), String> {
    let _lock = config_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    save_config_unlocked(cfg)
}

/// Replace the Settings-owned parts of the private config while holding the
/// writer lock. Project fields have dedicated card editors, so preserve the
/// current on-disk project fields for every project still present in the
/// Settings payload; this prevents a stale Settings window from wiping a
/// concurrent card edit.
pub fn replace_config<F>(cfg: &AppConfig, after_save: F) -> Result<(), String>
where
    F: FnOnce(&AppConfig, &AppConfig),
{
    let _lock = config_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    let before = load_config_unlocked();
    let mut effective = cfg.clone();
    // AppConfig is sent through the frontend, where this private compatibility
    // field is intentionally skipped. Carry the raw legacy value from the
    // current shared file so an ordinary Settings save cannot erase it before
    // the explicit migration action runs.
    effective.legacy_queue_scheduled_pushes = before.legacy_queue_scheduled_pushes;
    effective.retired_targets = before.retired_targets.clone();
    effective.retired_targets_unreadable = before.retired_targets_unreadable;
    let mut retained_ids = HashSet::new();
    for project in &mut effective.projects {
        // A blank ID is a new Settings row and must receive a fresh identity.
        // In particular, a delete followed by an identical Settings re-add
        // must not reacquire the deleted record's device-local schedule.
        let current = settings_current_match(&before.projects, project, &retained_ids)?;
        if let Some(current) = current {
            retained_ids.insert(current.id.clone());
            project.id = current.id.clone();
            project.local_path = current.local_path.clone();
            project.remote_path = current.remote_path.clone();
            project.remote = current.remote.clone();
            project.excludes = current.excludes.clone();
            project.schedule = current.schedule.clone();
            project.schedule_error = current.schedule_error.clone();
            project.legacy_schedule = current.legacy_schedule.clone();
            project.legacy_schedule_raw = current.legacy_schedule_raw.clone();
        } else {
            // This is a new Settings record, even when serde supplied the
            // repeatable legacy ID. Do not let any effective or legacy schedule
            // from the submitted payload attach to its newly allocated ID.
            project.id = fresh_project_id();
            project.schedule = None;
            project.schedule_error = None;
            project.legacy_schedule = None;
            project.legacy_schedule_raw = None;
        }
    }
    let mut submitted_ids = HashSet::new();
    for project in &effective.projects {
        if !submitted_ids.insert(project.id.clone()) {
            return Err(format!(
                "Settings payload contains duplicate project ID '{}'",
                project.id
            ));
        }
    }
    save_config_unlocked(&effective)?;
    after_save(&before, &effective);
    Ok(())
}

/// Match one Settings payload row to the current record without treating a
/// mutable name as an identity. Exact IDs always win, even when an earlier
/// current record has the same name. A submitted bootstrap ID is not a license
/// to match by name: a same-name row with different fields is a new record and
/// must receive a fresh identity, so its device-local schedule stays orphaned.
fn settings_current_match<'a>(
    current: &'a [Project],
    submitted: &Project,
    retained_ids: &HashSet<String>,
) -> Result<Option<&'a Project>, String> {
    if !submitted.id.trim().is_empty() {
        let exact: Vec<&Project> = current
            .iter()
            .filter(|candidate| {
                !retained_ids.contains(&candidate.id) && candidate.id == submitted.id
            })
            .collect();
        match exact.as_slice() {
            [project] => return Ok(Some(*project)),
            [] => {}
            _ => {
                return Err(format!(
                    "Settings payload project ID '{}' is duplicated",
                    submitted.id
                ));
            }
        }
    }

    // Blank IDs remain blank when a Settings payload is deserialized. That is
    // intentional: a new row must never match a current record by name or by
    // a repeatable field hash. Legacy on-disk rows are bootstrapped earlier
    // during config loading, before local schedules are merged.
    Ok(None)
}

#[derive(Serialize)]
struct SharedUserConfig {
    rclone_path: String,
    remote: String,
    remotes: Vec<RemoteConfig>,
    projects: Vec<SharedProject>,
    retired_targets: Vec<RetiredTarget>,
    scan_dirs: Vec<String>,
    default_pull_dir: String,
    auto_check_on_launch: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_scheduled_pushes: Option<bool>,
    extra_excludes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rclone_transfers: Option<i32>,
}

#[derive(Serialize)]
struct SharedProject {
    id: String,
    name: String,
    local_path: String,
    remote_path: String,
    remote: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    excludes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schedule: Option<serde_json::Value>,
}

fn shared_project(project: &Project) -> SharedProject {
    SharedProject {
        id: project.id.clone(),
        name: project.name.clone(),
        local_path: project.local_path.clone(),
        remote_path: project.remote_path.clone(),
        remote: project.remote.clone(),
        excludes: project.excludes.clone(),
        // Preserve a legacy shared schedule until the owner explicitly uses
        // the migration action. It remains non-effective while loading.
        schedule: project.legacy_schedule_raw.clone(),
    }
}

fn save_user_config_unlocked(user: &UserConfig) -> Result<(), String> {
    if user.retired_targets_unreadable {
        return Err(
            "Shared retired-target safety records are unreadable; config writes are disabled until the config is repaired".into(),
        );
    }
    let shared = SharedUserConfig {
        rclone_path: user.rclone_path.clone(),
        remote: user.remote.clone(),
        remotes: user.remotes.clone(),
        projects: user.projects.iter().map(shared_project).collect(),
        retired_targets: user.retired_targets.clone(),
        scan_dirs: user.scan_dirs.clone(),
        default_pull_dir: user.default_pull_dir.clone(),
        auto_check_on_launch: user.auto_check_on_launch,
        queue_scheduled_pushes: user.queue_scheduled_pushes,
        extra_excludes: user.extra_excludes.clone(),
        rclone_transfers: user.rclone_transfers,
    };
    let json = serde_json::to_string_pretty(&shared).map_err(|e| e.to_string())?;
    atomic_write(&config_path(), json.as_bytes())
}

pub(crate) fn save_config_unlocked(cfg: &AppConfig) -> Result<(), String> {
    if cfg.retired_targets_unreadable {
        return Err(
            "Shared retired-target safety records are unreadable; remote writes are disabled until the config is repaired".into(),
        );
    }
    validate_rclone_transfers(cfg.rclone_transfers)?;
    validate_excludes(&cfg.extra_excludes)?;
    for project in &cfg.projects {
        validate_excludes(&project.excludes)?;
        if let Some(schedule) = &project.schedule {
            schedule.validate()?;
        }
    }
    let mut projects = cfg.projects.clone();
    for project in &mut projects {
        ensure_project_id(project);
        if !project.remote_path.is_empty() {
            project.remote_path = AppConfig::canonical_remote_path(&project.remote_path);
        }
        project.schedule = None;
        project.schedule_error = None;
    }
    let mut retired_targets = cfg.retired_targets.clone();
    for target in &mut retired_targets {
        target.remote_path = AppConfig::canonical_remote_path(&target.remote_path);
    }
    let user = UserConfig {
        rclone_path: cfg.rclone_path.clone(),
        remote: cfg.remote.clone(),
        remotes: cfg.remotes.clone(),
        projects,
        retired_targets,
        retired_targets_unreadable: false,
        scan_dirs: cfg.scan_dirs.clone(),
        default_pull_dir: cfg.default_pull_dir.clone(),
        auto_check_on_launch: cfg.auto_check_on_launch,
        queue_scheduled_pushes: cfg.legacy_queue_scheduled_pushes,
        extra_excludes: cfg.extra_excludes.clone(),
        rclone_transfers: cfg.rclone_transfers,
    };
    save_user_config_unlocked(&user)
}

/// Persist one device-local project schedule. A scan-discovered project is
/// materialized in the shared base first so the local overlay always points at
/// a durable project ID.
pub fn set_local_project_schedule(
    project_name: &str,
    project_id: Option<&str>,
    schedule: Option<Schedule>,
) -> Result<String, String> {
    if let Some(value) = &schedule {
        value.validate()?;
    }
    let _lock = config_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    let local_path = local_config_path();
    let (mut local, local_warnings) = load_local_automation_unlocked();
    if local_path.exists() && !local_warnings.is_empty() {
        return Err(format!(
            "Device-local automation config needs repair before it can be edited: {}",
            local_warnings.join("; ")
        ));
    }

    let mut cfg = load_config_unlocked();
    let project_index = match project_id.filter(|id| !id.trim().is_empty()) {
        Some(id) => match cfg.projects.iter().position(|project| project.id == id) {
            Some(index) => Some(index),
            None => {
                // Scan-discovered projects are not in the shared list yet.
                // Accept only the exact deterministic ID that get_projects_status
                // issued for this discovered path; never fall back from an
                // arbitrary stale ID to a same-named project.
                let discovered_path = find_local_path(&cfg, project_name)
                    .ok_or_else(|| format!("Project ID '{}' was not found", id))?;
                let discovered_id = project_id_for_fields(
                    project_name,
                    &discovered_path,
                    "",
                    &cfg.default_remote_name(),
                );
                if id == discovered_id {
                    None
                } else {
                    return Err(format!("Project ID '{}' was not found", id));
                }
            }
        },
        None => cfg
            .projects
            .iter()
            .position(|project| project.name == project_name),
    };
    let project_id = if let Some(index) = project_index {
        ensure_project_id(&mut cfg.projects[index]);
        cfg.projects[index].id.clone()
    } else {
        let local_path_value = find_local_path(&cfg, project_name)
            .ok_or_else(|| format!("Project '{}' not found", project_name))?;
        let project = Project {
            // The deterministic ID was only the discovery-time selector. Once
            // this project becomes configured, give it an opaque durable ID so
            // deleting the record and later rediscovering the same folder
            // cannot silently reactivate its old local schedule.
            id: fresh_project_id(),
            name: project_name.to_string(),
            local_path: local_path_value,
            remote_path: String::new(),
            remote: cfg.default_remote_name(),
            excludes: Vec::new(),
            schedule: None,
            schedule_error: None,
            legacy_schedule: None,
            legacy_schedule_raw: None,
        };
        let id = project.id.clone();
        cfg.projects.push(project);
        id
    };

    // Persist any newly materialized project ID before publishing the local
    // schedule. If the second write fails, the project remains harmlessly
    // unscheduled and can be retried.
    save_config_unlocked(&cfg)?;
    local
        .schedules
        .retain(|entry| entry.project_id != project_id);
    if let Some(schedule) = schedule {
        local.schedules.push(LocalSchedule {
            project_id: project_id.clone(),
            last_seen_name: project_name.to_string(),
            schedule,
        });
    }
    save_local_automation_unlocked(&mut local)?;
    Ok(project_id)
}

pub fn set_local_queue_policy(enabled: bool) -> Result<(), String> {
    let _lock = config_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    let local_path = local_config_path();
    let (mut local, local_warnings) = load_local_automation_unlocked();
    if local_path.exists() && !local_warnings.is_empty() {
        return Err(format!(
            "Device-local automation config needs repair before it can be edited: {}",
            local_warnings.join("; ")
        ));
    }
    local.queue_scheduled_pushes = enabled;
    save_local_automation_unlocked(&mut local)
}

/// Move legacy schedules out of the shared config only after the user has
/// explicitly confirmed. Local automation is written first so a failed shared
/// cleanup cannot reactivate the legacy schedules on the next launch.
pub fn migrate_legacy_automation() -> Result<usize, String> {
    let _lock = config_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    let local_path = local_config_path();
    let (mut local, local_warnings) = load_local_automation_unlocked();
    if local_path.exists() && !local_warnings.is_empty() {
        return Err(format!(
            "Device-local automation config needs repair before migration: {}",
            local_warnings.join("; ")
        ));
    }

    let mut shared_warnings = Vec::new();
    let mut user = read_user_config_unlocked(&mut shared_warnings);
    if !shared_warnings.is_empty() {
        return Err(format!(
            "Shared config needs repair before migration: {}",
            shared_warnings.join("; ")
        ));
    }
    let mut existing_ids: HashSet<String> = local
        .schedules
        .iter()
        .map(|entry| entry.project_id.clone())
        .collect();
    let mut migrated = 0;
    for project in &mut user.projects {
        bootstrap_legacy_project_id(project);
        ensure_project_id(project);
        if project.legacy_schedule_raw.is_some() && project.schedule_error.is_some() {
            // An explicit migration must not destroy a malformed shared value
            // that the user still needs to inspect or repair.
            continue;
        }
        if let Some(schedule) = project.schedule.take() {
            if !existing_ids.contains(&project.id) {
                local.schedules.push(LocalSchedule {
                    project_id: project.id.clone(),
                    last_seen_name: project.name.clone(),
                    schedule,
                });
                existing_ids.insert(project.id.clone());
                migrated += 1;
            }
        }
        if project.legacy_schedule_raw.is_some() {
            project.legacy_schedule = None;
            project.legacy_schedule_raw = None;
            project.schedule_error = None;
        }
    }
    if let Some(queue) = user.queue_scheduled_pushes {
        local.queue_scheduled_pushes = queue;
    }
    // The local copy is durable now. Only this explicit migration may remove
    // the shared compatibility value.
    user.queue_scheduled_pushes = None;
    save_local_automation_unlocked(&mut local)?;
    // This serializer omits all legacy schedule and queue fields.
    save_user_config_unlocked(&user)?;
    Ok(migrated)
}

/// Convert the old hostname-selected whole config into the canonical shared
/// base. This is explicit because silently importing it on every machine can
/// resurrect the rejected cross-device config split. Legacy schedules remain
/// disabled until the separate Move here action is chosen.
pub fn migrate_legacy_host_config() -> Result<bool, String> {
    let _lock = config_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    if config_path().exists() {
        return Err("The canonical shared config already exists".into());
    }
    let Some(path) = legacy_host_config_path() else {
        return Ok(false);
    };
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read legacy config {}: {error}", path.display()))?;
    let user: UserConfig = serde_json::from_str(&contents)
        .map_err(|error| format!("Could not parse legacy config {}: {error}", path.display()))?;
    save_user_config_unlocked(&user)?;
    Ok(true)
}

/// Replace the config in one rename so a scheduler read can never observe a
/// half-written JSON document. On Windows rename-over-existing is not allowed,
/// so remove the old file only after the complete temporary file exists.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    if let Err(error) = fs::rename(&tmp, path) {
        #[cfg(windows)]
        if !path.exists() {
            return match fs::rename(&tmp, path) {
                Ok(()) => Ok(()),
                Err(restore_error) => Err(format!(
                    "{error}; config destination was removed and restore also failed: {restore_error}. Temporary config remains at {}",
                    tmp.display()
                )),
            };
        }
        let _ = fs::remove_file(&tmp);
        return Err(error.to_string());
    }
    Ok(())
}

/// Stable machine-local path used by the process-wide instance lock. The lock
/// file is never removed: Unix flock and Windows share-mode locks are released
/// by the OS when the owning process exits, including a crash. Keeping it out
/// of the portable config directory prevents it becoming sync-facing data.
pub fn instance_lock_path() -> PathBuf {
    let mut path = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    path.push("rcsync");
    path.push("instance.lock");
    path
}

/// Expand ~ to the user's home directory
pub fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return format!("{}{}", home.display(), &path[1..]);
        }
    }
    path.to_string()
}

/// Scan configured directories and return the first local path where `name` exists as a subdirectory.
pub fn find_local_path(cfg: &AppConfig, name: &str) -> Option<String> {
    // First check if it's already a configured project with a valid path
    if let Some(proj) = cfg.projects.iter().find(|p| p.name == name) {
        let expanded = expand_tilde(&proj.local_path);
        if Path::new(&expanded).exists() {
            return Some(proj.local_path.clone());
        }
    }

    find_scanned_local_path(cfg, name)
}

/// Find a local directory from scan roots without consulting configured project
/// names. Browse Remote uses this only when the selected remote/path has no
/// configured target; a same-name project on another remote must not supply its
/// local path to the new remote row.
pub fn find_scanned_local_path(cfg: &AppConfig, name: &str) -> Option<String> {
    for dir in &cfg.scan_dirs {
        let expanded = expand_tilde(dir);
        let candidate = Path::new(&expanded).join(name);
        if candidate.exists() && candidate.is_dir() {
            // Return with ~ prefix for portability
            return Some(format!("{}/{}", dir, name));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    fn cfg_with_base(base: &str) -> AppConfig {
        AppConfig {
            remotes: vec![RemoteConfig {
                name: "onedrive".into(),
                base_path: base.into(),
            }],
            remote: "onedrive".into(),
            ..Default::default()
        }
    }

    fn project(remote_path: &str) -> Project {
        Project {
            id: String::new(),
            name: "example".into(),
            local_path: "~/projects/example".into(),
            remote_path: remote_path.into(),
            remote: "onedrive".into(),
            excludes: vec![],
            schedule: None,
            schedule_error: None,
            legacy_schedule: None,
            legacy_schedule_raw: None,
        }
    }

    /// The defect this pins: several call sites used to build `proj/<name>` by
    /// hand. On a remote configured with any other base path that silently
    /// targeted a tree the user never chose — and Push makes the destination
    /// match the source, so it could overwrite or delete whatever lived there.
    #[test]
    fn an_unset_remote_path_follows_the_remotes_configured_base() {
        let cfg = cfg_with_base("Projects");
        assert_eq!(cfg.project_remote_path(&project("")), "Projects/example");
        assert_eq!(
            cfg.remote_path_for_project(&project("")),
            "onedrive:Projects/example"
        );
    }

    #[test]
    fn remote_child_path_normalizes_slashes_but_preserves_whitespace() {
        let cases = [
            ("", "Project", "Project"),
            ("/", "Project", "Project"),
            ("proj", "Project", "proj/Project"),
            ("proj/", "Project", "proj/Project"),
            ("/proj/", " Project ", "proj/ Project "),
        ];
        for (base, child, expected) in cases {
            assert_eq!(
                AppConfig::join_remote_child_path(base, child),
                expected,
                "base={base:?}, child={child:?}"
            );
        }
    }

    #[test]
    fn an_explicit_remote_path_still_overrides_the_base() {
        let cfg = cfg_with_base("Projects");
        assert_eq!(
            cfg.project_remote_path(&project("archive/old-example")),
            "archive/old-example"
        );
    }

    #[test]
    fn a_leading_slash_in_an_override_is_stripped() {
        let cfg = cfg_with_base("Projects");
        assert_eq!(
            cfg.project_remote_path(&project("/rooted/path")),
            "rooted/path"
        );
    }

    #[test]
    fn the_path_and_the_full_location_cannot_disagree() {
        // One authority: the full `remote:path` form is built from the path form,
        // so a future change cannot make them diverge.
        let cfg = cfg_with_base("Backups");
        for rp in ["", "custom/place", "/rooted"] {
            let p = project(rp);
            assert_eq!(
                cfg.remote_path_for_project(&p),
                format!("onedrive:{}", cfg.project_remote_path(&p))
            );
        }
    }

    fn cfg_with_excludes(extra: Vec<&str>, project: Vec<&str>) -> AppConfig {
        AppConfig {
            remote: "gdrive".into(),
            extra_excludes: extra.into_iter().map(str::to_string).collect(),
            projects: vec![Project {
                id: String::new(),
                name: "p".into(),
                local_path: "~/p".into(),
                remote_path: String::new(),
                remote: "gdrive".into(),
                excludes: project.into_iter().map(str::to_string).collect(),
                schedule: None,
                schedule_error: None,
                legacy_schedule: None,
                legacy_schedule_raw: None,
            }],
            ..Default::default()
        }
    }

    fn settings_project(id: &str, name: &str, local_path: &str) -> Project {
        Project {
            id: id.into(),
            name: name.into(),
            local_path: local_path.into(),
            remote_path: format!("proj/{name}"),
            remote: "gdrive".into(),
            excludes: Vec::new(),
            schedule: None,
            schedule_error: None,
            legacy_schedule: None,
            legacy_schedule_raw: None,
        }
    }

    const DIVERGENT: [&str; 4] = [" leading/**", "trailing/** ", "break/**\n", "cr/**\r"];

    /// The write path is where a bad pattern must be stopped. Trimming used to
    /// happen here and in both UI forms, which silently repaired input and left
    /// the rule unreachable from every production path — so the guarantee held
    /// only for configs nobody edited by hand.
    #[test]
    fn saving_a_config_refuses_patterns_that_the_two_filter_forms_read_differently() {
        let _env = TestConfigEnv::new("save-validation");
        for bad in DIVERGENT {
            assert!(
                save_config(&cfg_with_excludes(vec![bad], vec![])).is_err(),
                "{:?} must be refused when saved, not repaired into agreement",
                bad
            );
            assert!(
                save_config(&cfg_with_excludes(vec![], vec![bad])).is_err(),
                "{:?} must be refused on a project too",
                bad
            );
        }

        // A clean set must make it through the real serializer and come back
        // byte-for-byte, so an unrelated write failure cannot make every bad
        // case above look correctly rejected.
        save_config(&cfg_with_excludes(
            vec!["node_modules/**"],
            vec!["artifacts/**"],
        ))
        .unwrap();
        let loaded = load_config();
        assert_eq!(loaded.extra_excludes, vec!["node_modules/**"]);
        assert_eq!(loaded.projects[0].excludes, vec!["artifacts/**"]);
    }

    #[test]
    fn a_blank_row_is_not_a_filter_but_a_blank_row_with_a_line_break_is_refused() {
        assert_eq!(validate_exclude_pattern("").unwrap(), None);
        assert_eq!(validate_exclude_pattern("   ").unwrap(), None);
        assert!(
            validate_exclude_pattern("  \n").is_err(),
            "the line-break rule is checked first, so this is refused rather than skipped"
        );
        assert_eq!(
            validate_exclude_pattern("node_modules/**").unwrap(),
            Some("node_modules/**")
        );
    }

    #[test]
    fn transfers_accepts_only_automatic_or_one_through_eight() {
        assert!(validate_rclone_transfers(None).is_ok());
        for n in 1..=8 {
            assert!(
                validate_rclone_transfers(Some(n)).is_ok(),
                "{} should be allowed",
                n
            );
        }
        for n in [0, 9, -1, 1000] {
            assert!(
                validate_rclone_transfers(Some(n)).is_err(),
                "{} should be rejected",
                n
            );
        }
    }

    #[test]
    fn invalid_schedule_does_not_discard_projects_or_remotes() {
        let env = TestConfigEnv::new("invalid-schedule");
        let raw = serde_json::json!({
            "rclone_path": "rclone",
            "remote": "gdrive",
            "remotes": [{"name": "gdrive", "base_path": "proj"}],
            "projects": [{
                "name": "p",
                "local_path": "~/p",
                "remote_path": "proj/p",
                "remote": "gdrive",
                "schedule": {"kind": "interval", "every": 0, "unit": "hours", "origin_ms": 1}
            }]
        });
        fs::write(config_path(), serde_json::to_vec_pretty(&raw).unwrap()).unwrap();

        let cfg = load_config();
        assert_eq!(cfg.remotes[0].name, "gdrive");
        assert_eq!(cfg.projects.len(), 1);
        assert_eq!(cfg.projects[0].name, "p");
        assert!(cfg.projects[0].schedule.is_none());
        assert_eq!(cfg.config_warnings.len(), 1);
        assert_eq!(migrate_legacy_automation().unwrap(), 0);
        save_config(&cfg).unwrap();
        let preserved: serde_json::Value =
            serde_json::from_slice(&fs::read(config_path()).unwrap()).unwrap();
        assert!(preserved["projects"][0].get("schedule").is_some());
        drop(env);
    }

    #[test]
    fn shared_legacy_automation_stays_disabled_until_explicit_migration() {
        let _env = TestConfigEnv::new("legacy-automation");
        let raw = serde_json::json!({
            "rclone_path": "rclone",
            "remote": "gdrive",
            "remotes": [{"name": "gdrive", "base_path": "proj"}],
            "queue_scheduled_pushes": false,
            "projects": [{
                "id": "p_example",
                "name": "example",
                "local_path": "~/projects/example",
                "remote_path": "proj/example",
                "remote": "gdrive",
                "schedule": {"kind": "interval", "every": 24, "unit": "hours", "origin_ms": 1}
            }]
        });
        fs::write(config_path(), serde_json::to_vec_pretty(&raw).unwrap()).unwrap();

        let before = load_config();
        assert!(before.projects[0].schedule.is_none());
        assert_eq!(before.legacy_schedule_count, 1);
        assert!(before.legacy_queue_policy);
        assert!(before
            .config_warnings
            .iter()
            .any(|warning| warning.contains("shared legacy schedule is disabled")));

        assert_eq!(migrate_legacy_automation().unwrap(), 1);
        let after = load_config();
        assert!(matches!(
            after.projects[0].schedule,
            Some(Schedule::Interval { every: 24, .. })
        ));
        assert_eq!(after.legacy_schedule_count, 0);
        assert!(!after.legacy_queue_policy);

        let shared: serde_json::Value =
            serde_json::from_slice(&fs::read(config_path()).unwrap()).unwrap();
        assert!(shared["projects"][0].get("schedule").is_none());
        assert!(shared.get("queue_scheduled_pushes").is_none());
        let local: serde_json::Value =
            serde_json::from_slice(&fs::read(local_config_path()).unwrap()).unwrap();
        assert_eq!(local["schema"], LOCAL_SCHEMA);
        assert_eq!(local["schedules"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn ordinary_save_preserves_legacy_queue_until_explicit_migration() {
        let _env = TestConfigEnv::new("legacy-queue-preservation");
        let raw = serde_json::json!({
            "queue_scheduled_pushes": false,
            "projects": []
        });
        fs::write(config_path(), serde_json::to_vec_pretty(&raw).unwrap()).unwrap();

        let cfg = load_config();
        assert_eq!(cfg.legacy_queue_scheduled_pushes, Some(false));
        save_config(&cfg).unwrap();
        let after_save: serde_json::Value =
            serde_json::from_slice(&fs::read(config_path()).unwrap()).unwrap();
        assert_eq!(after_save["queue_scheduled_pushes"], false);

        migrate_legacy_automation().unwrap();
        let after_migration: serde_json::Value =
            serde_json::from_slice(&fs::read(config_path()).unwrap()).unwrap();
        assert!(after_migration.get("queue_scheduled_pushes").is_none());
        let local: serde_json::Value =
            serde_json::from_slice(&fs::read(local_config_path()).unwrap()).unwrap();
        assert_eq!(local["queue_scheduled_pushes"], false);
    }

    #[test]
    fn platform_config_uses_one_canonical_portable_base_name() {
        assert_eq!(
            platform_config_path()
                .file_name()
                .and_then(|name| name.to_str()),
            Some("rcsync-config.json")
        );
    }

    #[test]
    fn duplicate_shared_project_ids_disable_local_automation() {
        let _env = TestConfigEnv::new("duplicate-project-ids");
        let raw = serde_json::json!({
            "projects": [
                {"id": "p_duplicate", "name": "first", "local_path": "~/projects/first", "remote_path": "proj/first", "remote": "gdrive"},
                {"id": "p_duplicate", "name": "second", "local_path": "~/projects/second", "remote_path": "proj/second", "remote": "gdrive"}
            ]
        });
        fs::write(config_path(), serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
        let local = serde_json::json!({
            "schema": LOCAL_SCHEMA,
            "device_id": device_id(),
            "schedules": [{
                "project_id": "p_duplicate",
                "last_seen_name": "first",
                "schedule": {"kind": "interval", "every": 24, "unit": "hours", "origin_ms": 1}
            }]
        });
        fs::write(
            local_config_path(),
            serde_json::to_vec_pretty(&local).unwrap(),
        )
        .unwrap();

        let cfg = load_config();
        assert!(cfg
            .projects
            .iter()
            .all(|project| project.schedule.is_none()));
        assert!(cfg
            .config_warnings
            .iter()
            .any(|warning| warning.contains("Duplicate shared project ID")));
    }

    #[test]
    fn settings_matching_prefers_exact_id_over_an_earlier_same_name() {
        let current = vec![
            settings_project("p_first", "same", "~/projects/first"),
            settings_project("p_second", "same", "~/projects/second"),
        ];
        let submitted = current[1].clone();
        let matched = settings_current_match(&current, &submitted, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(matched.id, "p_second");
    }

    #[test]
    fn settings_matching_stale_opaque_id_does_not_fall_back_by_name() {
        let current = vec![settings_project("p_current", "same", "~/projects/same")];
        let submitted = settings_project("p_stale", "same", "~/projects/same");
        assert!(
            settings_current_match(&current, &submitted, &HashSet::new())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn settings_matching_accepts_a_unique_legacy_bootstrap_record() {
        let mut current = settings_project("", "legacy", "~/projects/legacy");
        current.id = project_id_for_fields(
            &current.name,
            &current.local_path,
            &current.remote_path,
            &current.remote,
        );
        let submitted = current.clone();
        let current_records = [current];
        let matched = settings_current_match(&current_records, &submitted, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(matched.name, "legacy");
    }

    #[test]
    fn deserialize_keeps_a_blank_wire_id_blank() {
        let project: Project = serde_json::from_value(serde_json::json!({
            "id": "",
            "name": "new-row",
            "local_path": "~/projects/new-row",
            "remote_path": "proj/new-row",
            "remote": "gdrive"
        }))
        .unwrap();
        assert!(project.id.is_empty());
    }

    #[test]
    fn settings_matching_blank_id_never_matches_a_current_record() {
        let current = [settings_project(
            &project_id_for_fields("same", "~/projects/same", "proj/same", "gdrive"),
            "same",
            "~/projects/same",
        )];
        let mut submitted = current[0].clone();
        submitted.id.clear();
        assert!(
            settings_current_match(&current, &submitted, &HashSet::new())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn load_config_bootstraps_legacy_blank_disk_ids_before_schedules() {
        let _env = TestConfigEnv::new("legacy-blank-id-load");
        let name = "legacy-disk";
        let local_path = "~/projects/legacy-disk";
        let remote_path = "proj/legacy-disk";
        let legacy_id = project_id_for_fields(name, local_path, remote_path, "gdrive");
        fs::write(
            config_path(),
            serde_json::to_vec_pretty(&serde_json::json!({
                "projects": [{
                    "id": "",
                    "name": name,
                    "local_path": local_path,
                    "remote_path": remote_path,
                    "remote": "gdrive"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            local_config_path(),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": LOCAL_SCHEMA,
                "device_id": device_id(),
                "queue_scheduled_pushes": true,
                "schedules": [{
                    "project_id": legacy_id,
                    "last_seen_name": name,
                    "schedule": {"kind": "interval", "every": 24, "unit": "hours", "origin_ms": 1}
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let cfg = load_config();
        assert_eq!(cfg.projects[0].id, legacy_id);
        assert!(matches!(
            cfg.projects[0].schedule,
            Some(Schedule::Interval { .. })
        ));
    }

    #[test]
    fn recreating_a_deleted_project_does_not_reactivate_its_orphan_schedule() {
        let _env = TestConfigEnv::new("orphan-schedule");
        let old_id = "p_deleted";
        let original = AppConfig {
            projects: vec![Project {
                id: old_id.into(),
                name: "same-name".into(),
                local_path: "~/projects/same-name".into(),
                remote_path: "proj/same-name".into(),
                remote: "gdrive".into(),
                excludes: Vec::new(),
                schedule: None,
                schedule_error: None,
                legacy_schedule: None,
                legacy_schedule_raw: None,
            }],
            ..AppConfig::default()
        };
        save_config(&original).unwrap();
        set_local_project_schedule(
            "same-name",
            Some(old_id),
            Some(Schedule::Interval {
                every: 24,
                unit: IntervalUnit::Hours,
                origin_ms: 1,
            }),
        )
        .unwrap();

        let recreated = AppConfig {
            projects: vec![Project {
                id: String::new(),
                name: "same-name".into(),
                local_path: "~/projects/same-name".into(),
                remote_path: "proj/same-name".into(),
                remote: "gdrive".into(),
                excludes: Vec::new(),
                schedule: None,
                schedule_error: None,
                legacy_schedule: None,
                legacy_schedule_raw: None,
            }],
            ..AppConfig::default()
        };
        save_config(&recreated).unwrap();
        let cfg = load_config();
        assert_ne!(cfg.projects[0].id, old_id);
        assert!(cfg.projects[0].schedule.is_none());
    }

    #[test]
    fn local_automation_merges_by_project_id_and_writes_outside_shared_config() {
        let _env = TestConfigEnv::new("local-automation");
        let raw = serde_json::json!({
            "projects": [{
                "id": "p_alpha",
                "name": "alpha",
                "local_path": "~/projects/alpha",
                "remote_path": "proj/alpha",
                "remote": "gdrive"
            }]
        });
        fs::write(config_path(), serde_json::to_vec_pretty(&raw).unwrap()).unwrap();

        let schedule = Schedule::Interval {
            every: 48,
            unit: IntervalUnit::Hours,
            origin_ms: 1,
        };
        assert_eq!(
            set_local_project_schedule("alpha", Some("p_alpha"), Some(schedule.clone())).unwrap(),
            "p_alpha"
        );
        let cfg = load_config();
        assert_eq!(cfg.projects[0].schedule, Some(schedule));

        let shared: serde_json::Value =
            serde_json::from_slice(&fs::read(config_path()).unwrap()).unwrap();
        assert!(shared["projects"][0].get("schedule").is_none());
        let local: serde_json::Value =
            serde_json::from_slice(&fs::read(local_config_path()).unwrap()).unwrap();
        assert_eq!(local["schedules"][0]["project_id"], "p_alpha");
    }

    #[test]
    fn malformed_local_automation_fails_closed_without_hiding_shared_projects() {
        let _env = TestConfigEnv::new("malformed-local-automation");
        let raw = serde_json::json!({
            "projects": [{
                "id": "p_beta",
                "name": "beta",
                "local_path": "~/projects/beta",
                "remote_path": "proj/beta",
                "remote": "gdrive"
            }]
        });
        fs::write(config_path(), serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
        fs::write(local_config_path(), b"{not-json").unwrap();

        let cfg = load_config();
        assert_eq!(cfg.projects.len(), 1);
        assert!(cfg.projects[0].schedule.is_none());
        assert!(cfg
            .config_warnings
            .iter()
            .any(|warning| warning.contains("schedules are disabled")));
    }

    #[test]
    fn a_stale_project_id_cannot_fall_back_to_a_name_match() {
        let _env = TestConfigEnv::new("stale-project-id");
        let raw = serde_json::json!({
            "projects": [{
                "id": "p_real",
                "name": "same-name",
                "local_path": "~/same-name",
                "remote_path": "proj/same-name",
                "remote": "gdrive"
            }]
        });
        fs::write(config_path(), serde_json::to_vec_pretty(&raw).unwrap()).unwrap();

        let result = set_local_project_schedule(
            "same-name",
            Some("p_stale"),
            Some(Schedule::Interval {
                every: 24,
                unit: IntervalUnit::Hours,
                origin_ms: 1,
            }),
        );
        assert!(result.is_err());
        assert!(load_config().projects[0].schedule.is_none());
    }

    #[test]
    fn an_exact_discovered_project_id_can_materialize_a_local_schedule() {
        let _env = TestConfigEnv::new("discovered-schedule");
        let root = std::env::temp_dir().join(format!(
            "rcsync-discovered-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project_dir = root.join("discovered");
        fs::create_dir_all(&project_dir).unwrap();
        let scan_dir = root.to_string_lossy().into_owned();
        fs::write(
            config_path(),
            serde_json::to_vec_pretty(&serde_json::json!({
                "scan_dirs": [scan_dir],
                "projects": []
            }))
            .unwrap(),
        )
        .unwrap();
        let discovered_path = format!("{}/discovered", root.display());
        let discovered_id = project_id_for_fields("discovered", &discovered_path, "", "gdrive");
        set_local_project_schedule(
            "discovered",
            Some(&discovered_id),
            Some(Schedule::Interval {
                every: 24,
                unit: IntervalUnit::Hours,
                origin_ms: 1,
            }),
        )
        .unwrap();
        let cfg = load_config();
        assert_eq!(cfg.projects.len(), 1);
        assert_ne!(cfg.projects[0].id, discovered_id);
        assert!(cfg.projects[0].schedule.is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_ids_are_stable_for_the_same_record_and_distinguish_similar_records() {
        let first = project_id_for_fields("a-b", "~/p/a-b", "proj/a-b", "gdrive");
        assert_eq!(
            first,
            project_id_for_fields("a-b", "~/p/a-b", "proj/a-b", "gdrive")
        );
        assert_ne!(
            first,
            project_id_for_fields("a b", "~/p/a-b", "proj/a-b", "gdrive")
        );
        assert!(first.starts_with("p_"));
    }

    #[test]
    fn device_id_survives_a_machine_label_change() {
        let _env = TestConfigEnv::new("device-id");
        let previous_machine = std::env::var_os("RCSYNC_MACHINE");
        std::env::set_var("RCSYNC_MACHINE", "Workstation.local");
        let first = device_id();
        std::env::set_var("RCSYNC_MACHINE", "Laptop.local");
        let second = device_id();
        match previous_machine {
            Some(value) => std::env::set_var("RCSYNC_MACHINE", value),
            None => std::env::remove_var("RCSYNC_MACHINE"),
        }
        assert_eq!(first, second);
    }

    #[test]
    fn retired_target_round_trips_and_reattach_creates_a_fresh_generation() {
        let _env = TestConfigEnv::new("retired-target");
        let original = Project {
            id: "p_original".into(),
            name: "example".into(),
            local_path: "~/projects/example".into(),
            remote_path: String::new(),
            remote: "gdrive".into(),
            excludes: Vec::new(),
            schedule: None,
            schedule_error: None,
            legacy_schedule: None,
            legacy_schedule_raw: None,
        };
        save_config(&AppConfig {
            projects: vec![original.clone()],
            ..AppConfig::default()
        })
        .unwrap();

        retire_target_and_remove_project(&original).unwrap();
        let retired = load_config();
        assert!(retired.projects.is_empty());
        assert_eq!(retired.retired_targets.len(), 1);
        assert_eq!(retired.retired_targets[0].remote, "gdrive");
        assert_eq!(retired.retired_targets[0].remote_path, "proj/example");

        let discovered = Project {
            id: project_id_for_fields("example", "~/projects/example", "", "gdrive"),
            ..original.clone()
        };
        assert!(retired.retired_target_for(&discovered).is_some());
        assert!(retired.ensure_remote_target_writable(&discovered).is_err());
        let different_target = Project {
            remote_path: "archive/example".into(),
            ..discovered.clone()
        };
        assert!(retired.retired_target_for(&different_target).is_none());

        let new_id = reattach_retired_target(&discovered).unwrap();
        let recreated = load_config();
        assert_eq!(recreated.projects.len(), 1);
        assert_eq!(recreated.projects[0].id, new_id);
        assert_ne!(new_id, original.id);
        assert!(recreated.projects[0].schedule.is_none());
        assert!(recreated.retired_targets.is_empty());
    }

    #[test]
    fn malformed_retired_target_data_fails_closed_for_remote_writes() {
        let _env = TestConfigEnv::new("retired-target-invalid");
        fs::write(
            config_path(),
            serde_json::to_vec_pretty(&serde_json::json!({
                "projects": [],
                "retired_targets": {"not": "an array"}
            }))
            .unwrap(),
        )
        .unwrap();
        let cfg = load_config();
        assert!(cfg.retired_targets_unreadable);
        let project = Project {
            id: "p_example".into(),
            name: "example".into(),
            local_path: "~/projects/example".into(),
            remote_path: String::new(),
            remote: "gdrive".into(),
            excludes: Vec::new(),
            schedule: None,
            schedule_error: None,
            legacy_schedule: None,
            legacy_schedule_raw: None,
        };
        assert!(cfg.ensure_remote_target_writable(&project).is_err());
    }

    #[test]
    fn ordinary_settings_save_preserves_retired_targets() {
        let _env = TestConfigEnv::new("retired-target-settings");
        let retired = RetiredTarget {
            remote: "gdrive".into(),
            remote_path: "proj/example".into(),
            name_at_retirement: "example".into(),
            project_id_at_retirement: "p_old".into(),
            retired_at_ms: 1,
            retired_by_device: "device-a".into(),
        };
        save_config(&AppConfig {
            retired_targets: vec![retired.clone()],
            ..AppConfig::default()
        })
        .unwrap();

        replace_config(&AppConfig::default(), |_, _| {}).unwrap();
        assert_eq!(load_config().retired_targets, vec![retired]);
    }

    #[test]
    fn weekly_schedule_returns_the_next_local_occurrence() {
        let now = Local
            .with_ymd_and_hms(2026, 8, 29, 12, 0, 0)
            .single()
            .unwrap();
        let schedule = Schedule::Weekly {
            weekdays: vec![6],
            minute: 13 * 60,
        };
        let next = schedule.next_after(now).unwrap();
        assert_eq!(next.date_naive(), now.date_naive());
        assert_eq!(next.hour(), 13);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn interval_days_are_exactly_24_elapsed_hours_and_skip_past_slots() {
        let now = Local
            .with_ymd_and_hms(2026, 8, 29, 12, 0, 0)
            .single()
            .unwrap();
        let schedule = Schedule::Interval {
            every: 1,
            unit: IntervalUnit::Days,
            origin_ms: (now - ChronoDuration::hours(49)).timestamp_millis(),
        };
        let next = schedule.next_after(now).unwrap();
        assert_eq!(
            next.timestamp_millis() - now.timestamp_millis(),
            23 * 60 * 60 * 1000
        );
    }

    #[test]
    fn ambiguous_weekly_time_uses_the_first_local_occurrence() {
        let naive = NaiveDateTime::new(
            now_date(),
            chrono::NaiveTime::from_hms_opt(1, 30, 0).unwrap(),
        );
        let first = Local.timestamp_millis_opt(1_000).single().unwrap();
        let second = Local.timestamp_millis_opt(2_000).single().unwrap();
        let result = first_valid_with(naive, |_| LocalResult::Ambiguous(first, second)).unwrap();
        assert_eq!(result, first);
    }

    #[test]
    fn nonexistent_weekly_time_advances_to_the_first_valid_minute() {
        let naive = NaiveDateTime::new(
            now_date(),
            chrono::NaiveTime::from_hms_opt(2, 30, 0).unwrap(),
        );
        let valid = Local.timestamp_millis_opt(3_000).single().unwrap();
        let mut attempts = 0;
        let result = first_valid_with(naive, |_| {
            attempts += 1;
            if attempts < 3 {
                LocalResult::None
            } else {
                LocalResult::Single(valid)
            }
        })
        .unwrap();
        assert_eq!(attempts, 3);
        assert_eq!(result, valid);
    }

    fn now_date() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 8, 29).unwrap()
    }
}
