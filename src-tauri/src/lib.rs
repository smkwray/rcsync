mod config;
mod instance_lock;
mod rclone;
mod scheduler;
mod watcher;

use config::{expand_tilde, load_config, AppConfig, Project, RemoteConfig, Schedule};
use rclone::{
    bisync_project, check_project, list_remote, local_dir_has_content, sync_project, RemoteDir,
};
use serde::Serialize;
use std::path::Path;
use std::sync::OnceLock;
use tokio::sync::Semaphore;

/// Limit concurrent rclone processes to avoid overwhelming the remote or
/// exhausting file descriptors (macOS default ulimit is 256).
fn rclone_semaphore() -> &'static Semaphore {
    static SEM: OnceLock<Semaphore> = OnceLock::new();
    SEM.get_or_init(|| Semaphore::new(3))
}

#[derive(Clone, Serialize)]
struct ProjectStatus {
    id: String,
    name: String,
    local_path: String,
    remote_path: String,
    remote: String,
    exists_locally: bool,
    schedule: Option<Schedule>,
    retired: bool,
    retired_target: Option<String>,
}

#[tauri::command]
fn get_config() -> AppConfig {
    let config = load_config();
    for warning in &config.config_warnings {
        eprintln!("rcsync config warning: {warning}");
    }
    config
}

#[tauri::command]
fn get_machine_name() -> String {
    config::machine_name()
}

#[tauri::command]
fn get_projects_status() -> Vec<ProjectStatus> {
    let cfg = load_config();
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();

    let default_remote = cfg.default_remote_name();

    for p in &cfg.projects {
        seen.insert(p.name.clone());
        let expanded = expand_tilde(&p.local_path);
        let remote = if p.remote.is_empty() {
            default_remote.clone()
        } else {
            p.remote.clone()
        };
        result.push(ProjectStatus {
            id: p.id.clone(),
            name: p.name.clone(),
            local_path: p.local_path.clone(),
            remote_path: cfg.project_remote_path(p),
            remote,
            exists_locally: Path::new(&expanded).exists(),
            schedule: p.schedule.clone(),
            retired: cfg.retired_target_for(p).is_some(),
            retired_target: cfg
                .retired_target_for(p)
                .map(|target| format!("{}:{}", target.remote, target.remote_path)),
        });
    }

    for dir in &cfg.scan_dirs {
        let expanded = expand_tilde(dir);
        let dir_path = Path::new(&expanded);
        if let Ok(entries) = std::fs::read_dir(dir_path) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        if name.starts_with('.') || seen.contains(name) {
                            continue;
                        }
                        seen.insert(name.to_string());
                        let discovered = Project {
                            id: config::project_id_for_fields(
                                name,
                                &format!("{}/{}", dir, name),
                                "",
                                &default_remote,
                            ),
                            name: name.to_string(),
                            local_path: format!("{}/{}", dir, name),
                            remote_path: String::new(),
                            remote: default_remote.clone(),
                            excludes: Vec::new(),
                            schedule: None,
                            schedule_error: None,
                            legacy_schedule: None,
                            legacy_schedule_raw: None,
                        };
                        result.push(ProjectStatus {
                            id: discovered.id.clone(),
                            name: discovered.name.clone(),
                            local_path: discovered.local_path.clone(),
                            remote_path: cfg.project_remote_path(&discovered),
                            remote: default_remote.clone(),
                            exists_locally: true,
                            schedule: discovered.schedule.clone(),
                            retired: cfg.retired_target_for(&discovered).is_some(),
                            retired_target: cfg
                                .retired_target_for(&discovered)
                                .map(|target| format!("{}:{}", target.remote, target.remote_path)),
                        });
                    }
                }
            }
        }
    }

    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

#[tauri::command]
fn update_config(cfg: AppConfig) -> Result<(), String> {
    config::replace_config(&cfg, |before, effective| {
        for project in &before.projects {
            let changed = effective
                .projects
                .iter()
                .find(|candidate| candidate.id == project.id)
                .map(|candidate| candidate.schedule != project.schedule)
                .unwrap_or(true);
            if changed {
                scheduler::clear_pending(&project.id);
            }
        }
        scheduler::notify_config_changed();
    })?;
    Ok(())
}

#[tauri::command]
fn get_active_operations() -> Vec<rclone::OperationSnapshot> {
    rclone::active_operations()
}

#[tauri::command]
fn get_schedule_status() -> Vec<scheduler::ScheduleStatus> {
    scheduler::snapshot()
}

/// Persist or clear one project's schedule. Scan-discovered projects are
/// materialized exactly like project-specific excludes so later operations see
/// the same record.
#[tauri::command]
fn set_project_schedule(
    project_name: String,
    project_id: Option<String>,
    schedule: Option<Schedule>,
) -> Result<String, String> {
    let mut schedule = schedule;
    if let Some(value) = &mut schedule {
        if let Schedule::Interval { origin_ms, .. } = value {
            if *origin_ms == 0 {
                *origin_ms = epoch_millis();
            }
        }
        value.validate()?;
    }
    let project_id =
        config::set_local_project_schedule(&project_name, project_id.as_deref(), schedule)?;
    scheduler::clear_pending(&project_id);
    scheduler::notify_config_changed();
    Ok(project_id)
}

/// Set the per-device scheduler policy. When enabled, scheduled Pushes are
/// admitted one at a time; pending projects remain queued until the current
/// scheduled Push reaches a terminal state.
#[tauri::command]
fn set_queue_scheduled_pushes(enabled: bool) -> Result<(), String> {
    config::set_local_queue_policy(enabled)?;
    scheduler::notify_config_changed();
    Ok(())
}

/// Explicitly move schedules from the legacy shared config into this device's
/// local automation layer. Loading never performs this migration implicitly.
#[tauri::command]
fn migrate_legacy_automation() -> Result<usize, String> {
    let migrated = config::migrate_legacy_automation()?;
    scheduler::notify_config_changed();
    Ok(migrated)
}

#[tauri::command]
fn migrate_legacy_host_config() -> Result<bool, String> {
    let migrated = config::migrate_legacy_host_config()?;
    if migrated {
        scheduler::notify_config_changed();
    }
    Ok(migrated)
}

fn epoch_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(1)
}

/// Read the per-project excludes for a project. Returns an empty list for
/// projects that exist only via scan discovery (no config entry yet).
#[tauri::command]
fn get_project_excludes(
    project_name: String,
    project_id: Option<String>,
) -> Result<Vec<String>, String> {
    let project = project_for_operation(&project_name, project_id.as_deref())?;
    Ok(project.excludes)
}

/// Set the per-project excludes for a project and persist them. If the project
/// is not yet in the config (scan-discovered), it is materialized into the
/// config so the excludes have a home — exactly the same record a manual
/// `push` would resolve, so nothing else about the project changes.
#[tauri::command]
fn set_project_excludes(
    project_name: String,
    project_id: Option<String>,
    excludes: Vec<String>,
) -> Result<(), String> {
    // Validated, not normalised. Trimming here would silently repair a pattern
    // that means different things to a filters file and to a command-line
    // argument, and would make the rule unreachable from every production path.
    let mut seen = std::collections::HashSet::new();
    let mut cleaned: Vec<String> = Vec::new();
    for pattern in excludes {
        if let Some(valid) = config::validate_exclude_pattern(&pattern)? {
            if seen.insert(valid.to_string()) {
                cleaned.push(valid.to_string());
            }
        }
    }

    let project = materialize_if_discovered(&project_name, project_id.as_deref())?;
    let result = config::edit_config(|cfg| {
        let p = find_project_by_id(cfg, &project.id)?;
        cfg.projects
            .iter_mut()
            .find(|candidate| candidate.id == p.id)
            .expect("project found by ID must still exist")
            .excludes = cleaned;
        Ok(())
    });
    if result.is_ok() {
        scheduler::notify_config_changed();
    }
    result
}

/// Change which remote (and path on that remote) a project syncs to, and
/// persist it. Materializes a config entry for scan-discovered projects, same
/// as `set_project_excludes`. The remote must be one of the configured remotes.
/// This only changes future sync targeting — it never moves or deletes anything
/// already uploaded to the old remote.
#[tauri::command]
fn set_project_remote(
    project_name: String,
    project_id: Option<String>,
    remote: String,
    remote_path: String,
) -> Result<(), String> {
    let cfg = load_config();
    if !cfg.remotes.iter().any(|r| r.name == remote) {
        return Err(format!("Remote '{}' is not configured", remote));
    }
    let project = materialize_if_discovered(&project_name, project_id.as_deref())?;
    config::edit_config(|cfg| {
        let remote_path = config::AppConfig::canonical_remote_path(&remote_path);
        let p = cfg
            .projects
            .iter_mut()
            .find(|candidate| candidate.id == project.id)
            .ok_or_else(|| format!("Project ID '{}' not found", project.id))?;
        p.remote = remote;
        p.remote_path = remote_path;
        Ok(())
    })
}

/// All rclone commands run in spawn_blocking and return their output as a string.
/// Stop the in-flight operation for a project, killing rclone if it has already
/// started. Returns false when there was nothing to stop.
#[tauri::command]
fn cancel_op(project_id: String) -> bool {
    // A pending scheduled Push belongs to the scheduler ticket, not to an
    // unrelated manual operation on the same project. Only Cancel All,
    // schedule edits, and deleting the project clear pending tickets.
    rclone::request_cancel(&project_id)
}

#[tauri::command]
fn cancel_scheduled_pending() -> usize {
    scheduler::clear_all_pending()
}

/// Every operation claims its project up front (so a cancel can reach it while
/// it is still queued) and re-checks for a cancel after the queue lets it
/// through (so a cancelled operation never spawns rclone at all).
#[tauri::command]
async fn push(
    project_name: String,
    project_id: Option<String>,
    dry_run: bool,
) -> Result<String, String> {
    let project = project_for_operation(&project_name, project_id.as_deref())?;
    let mode = if dry_run { "dry-run" } else { "push" };
    let op = rclone::start_op_with(&project.id, &project.name, mode, false)?;
    push_claimed(project.id, project.name, dry_run, &op).await
}

/// The single production Push body. Both the Tauri command and the scheduler
/// pass through this function after claiming the project. Config/project are
/// deliberately loaded only after the shared semaphore is acquired so a queued
/// Push uses the current excludes and remote target when it actually executes.
pub(crate) async fn push_claimed(
    project_id: String,
    project_name: String,
    dry_run: bool,
    // Borrow the guard so the caller controls exactly when the operation is
    // released. Scheduled Push uses that lifetime to emit its terminal event
    // before the next scheduled claim can begin.
    _op: &rclone::OpGuard,
) -> Result<String, String> {
    let _permit = rclone_semaphore()
        .acquire()
        .await
        .map_err(|_| "Operation queue closed".to_string())?;
    rclone::check_cancelled(&project_id)?;
    let cfg = load_config();
    // Scheduled work carries the stable ID through the semaphore and the
    // reload. The display name is only the operation/log label; it must not
    // decide which project receives a destructive Push after a rename or when
    // two records share a name.
    let project = resolve_project_for_push(&cfg, &project_id, &project_name)?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    tokio::task::spawn_blocking(move || sync_project(&cfg2, &proj2, "push", dry_run))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn pull(
    project_name: String,
    project_id: Option<String>,
    dry_run: bool,
) -> Result<String, String> {
    let project = project_for_operation(&project_name, project_id.as_deref())?;
    let _op = rclone::start_op_with(
        &project.id,
        &project.name,
        if dry_run { "dry-run" } else { "pull" },
        false,
    )?;
    let cfg = load_config();
    let _permit = rclone_semaphore()
        .acquire()
        .await
        .map_err(|_| "Operation queue closed".to_string())?;
    rclone::check_cancelled(&project.id)?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    tokio::task::spawn_blocking(move || sync_project(&cfg2, &proj2, "pull", dry_run))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn check(
    project_name: String,
    project_id: Option<String>,
) -> Result<rclone::CheckOutcome, String> {
    let project = project_for_operation(&project_name, project_id.as_deref())?;
    let _op = rclone::start_op_with(&project.id, &project.name, "check", false)?;
    let cfg = load_config();
    let _permit = rclone_semaphore()
        .acquire()
        .await
        .map_err(|_| "Operation queue closed".to_string())?;
    rclone::check_cancelled(&project.id)?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    tokio::task::spawn_blocking(move || check_project(&cfg2, &proj2))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn bisync(project_name: String, project_id: Option<String>) -> Result<String, String> {
    run_bisync(project_name, project_id, false).await
}

/// Rebuild a project's bi-sync listings. Separate from `bisync` so the ordinary
/// button cannot reach it: a resync is how a filter change is migrated, and
/// rclone resolves it by giving the local side precedence, so it can overwrite
/// the remote. The frontend gates it behind its own typed confirmation.
#[tauri::command]
async fn bisync_resync(project_name: String, project_id: Option<String>) -> Result<String, String> {
    run_bisync(project_name, project_id, true).await
}

async fn run_bisync(
    project_name: String,
    project_id: Option<String>,
    resync: bool,
) -> Result<String, String> {
    let project = project_for_operation(&project_name, project_id.as_deref())?;
    let _op = rclone::start_op_with(
        &project.id,
        &project.name,
        if resync { "resync" } else { "bisync" },
        false,
    )?;
    let cfg = load_config();
    let _permit = rclone_semaphore()
        .acquire()
        .await
        .map_err(|_| "Operation queue closed".to_string())?;
    rclone::check_cancelled(&project.id)?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    tokio::task::spawn_blocking(move || bisync_project(&cfg2, &proj2, resync))
        .await
        .map_err(|e| e.to_string())?
}

/// Delete local project directory AND remove it from config so a
/// subsequent "push all" can never accidentally sync the now-empty path.
#[tauri::command]
fn delete_local(project_name: String, project_id: Option<String>) -> Result<(), String> {
    let project = project_for_operation(&project_name, project_id.as_deref())?;
    // Claim the project for the whole destructive operation. This closes the
    // race where a scheduled Push becomes due after the active-operation check
    // but before the local directory is removed.
    let _op = rclone::start_op_with(&project.id, &project.name, "delete", false)?;
    scheduler::clear_pending(&project.id);
    scheduler::notify_config_changed();
    let expanded = expand_tilde(&project.local_path);
    let path = Path::new(&expanded);
    if !path.exists() {
        return Err("Local directory does not exist".into());
    }
    // Publish the cross-device retirement record before deleting local bytes.
    // If this save fails, leave the directory intact so another device cannot
    // see an unrecorded retirement and later push stale contents.
    config::retire_target_and_remove_project(&project).map_err(|e| {
        format!("Local deletion aborted; could not record retired remote target: {e}")
    })?;
    std::fs::remove_dir_all(path).map_err(|e| {
        format!(
            "Remote target was retired, but failed to delete local {}: {}",
            expanded, e
        )
    })?;
    Ok(())
}

#[tauri::command]
fn reattach_retired_target(
    project_name: String,
    project_id: Option<String>,
) -> Result<String, String> {
    let project = project_for_operation(&project_name, project_id.as_deref())?;
    let old_id = project.id.clone();
    let new_id = config::reattach_retired_target(&project)?;
    scheduler::clear_pending(&old_id);
    scheduler::notify_config_changed();
    Ok(new_id)
}

#[tauri::command]
async fn browse_remote(remote_name: Option<String>) -> Result<Vec<RemoteDir>, String> {
    let cfg = load_config();
    let _permit = rclone_semaphore()
        .acquire()
        .await
        .map_err(|_| "Operation queue closed".to_string())?;
    let rn = remote_name;
    tokio::task::spawn_blocking(move || list_remote(&cfg, rn.as_deref()))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
fn get_remotes() -> Vec<RemoteConfig> {
    load_config().remotes
}

#[tauri::command]
fn switch_remote(remote_name: String) -> Result<(), String> {
    config::edit_config(|cfg| {
        if !cfg.remotes.iter().any(|r| r.name == remote_name) {
            return Err(format!("Remote '{}' not found in config", remote_name));
        }
        cfg.remote = remote_name;
        Ok(())
    })
}

#[tauri::command]
fn check_local_exists(local_path: String) -> Result<bool, String> {
    Ok(local_dir_has_content(&local_path))
}

#[tauri::command]
async fn pull_new_project(
    name: String,
    local_path: String,
    project_id: Option<String>,
    remote: String,
    remote_path: String,
) -> Result<String, String> {
    let cfg = load_config();
    let canonical_remote_path = config::AppConfig::canonical_remote_path(&remote_path);
    let existing = resolve_browse_pull_project(
        &cfg,
        &name,
        project_id.as_deref(),
        &remote,
        &canonical_remote_path,
    )?;
    let existing_configured = existing.is_some();
    let operation_id = existing
        .as_ref()
        .map(|project| project.id.clone())
        .unwrap_or_else(config::fresh_project_id_for_new_record);
    let operation_name = existing
        .as_ref()
        .map(|project| project.name.clone())
        .unwrap_or_else(|| name.clone());
    let _op = rclone::start_op_with(&operation_id, &operation_name, "pull", false)?;

    if local_dir_has_content(&local_path) {
        return Err(format!(
            "Local directory '{}' already has content. Pulling would overwrite local files.",
            expand_tilde(&local_path)
        ));
    }

    let expanded = expand_tilde(&local_path);

    // Download into a dot-prefixed sibling, not the final path. An interrupted
    // pull leaves a partial tree, and the default pull directory is normally
    // also a scan directory — so a partial tree at the final path would be
    // auto-discovered as an ordinary project, and the next Push would make the
    // remote match that incomplete copy. `get_projects_status` skips names
    // starting with '.', which is exactly the property needed here.
    let staging_token = config::fresh_project_id_for_new_record();
    let staging = staging_path(&expanded, &staging_token)?;
    let staging_parent = Path::new(&staging)
        .parent()
        .ok_or_else(|| format!("Cannot derive a parent for staging directory {staging}"))?;
    std::fs::create_dir_all(staging_parent)
        .map_err(|e| format!("Failed to create staging parent {staging_parent:?}: {e}"))?;
    std::fs::create_dir(&staging)
        .map_err(|e| format!("Failed to reserve staging directory {staging}: {e}"))?;

    // A project can already be configured yet have no local copy — that is
    // exactly when Browse offers to pull it. The resolver above chose it by
    // immutable ID plus the selected remote/path, so carry its excludes and
    // local schedule without consulting a mutable display name.
    let project = if let Some(mut existing) = existing {
        existing.local_path = staging.clone();
        existing
    } else {
        Project {
            id: operation_id,
            name: name.clone(),
            local_path: staging.clone(),
            remote_path: canonical_remote_path.clone(),
            remote: remote.clone(),
            excludes: Vec::new(),
            schedule: None,
            schedule_error: None,
            legacy_schedule: None,
            legacy_schedule_raw: None,
        }
    };

    let _permit = rclone_semaphore()
        .acquire()
        .await
        .map_err(|_| "Operation queue closed".to_string())?;
    rclone::check_cancelled(&project.id)?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    let output = tokio::task::spawn_blocking(move || sync_project(&cfg2, &proj2, "pull", false))
        .await
        .map_err(|e| e.to_string())??;

    // Only a complete pull becomes a real project. Finalization revalidates
    // the target under the config writer lock before publishing the staging
    // directory, so a concurrent retarget/removal cannot attach old bytes to
    // a new identity.
    let record = Project {
        local_path,
        ..project
    };
    finalize_browse_pull(
        &record,
        existing_configured,
        &remote,
        &canonical_remote_path,
        &staging,
        &expanded,
    )?;

    Ok(output)
}

/// Revalidate and publish a completed Browse Pull as one config/filesystem
/// transaction. The lock is held from the current-config read through the
/// final config save, preventing an in-process editor from changing ownership
/// between validation and publication.
fn finalize_browse_pull(
    record: &Project,
    existing_configured: bool,
    original_remote: &str,
    original_remote_path: &str,
    staging: &str,
    expanded: &str,
) -> Result<(), String> {
    config::with_config_lock(|| {
        let mut cfg = config::load_config_unlocked();
        let original_remote_path = config::AppConfig::canonical_remote_path(original_remote_path);
        let owners: Vec<&Project> = cfg
            .projects
            .iter()
            .filter(|project| {
                cfg.project_remote(project).name == original_remote
                    && cfg.project_remote_path(project) == original_remote_path
            })
            .collect();

        if existing_configured {
            let matching_id: Vec<&Project> = owners
                .iter()
                .copied()
                .filter(|project| project.id == record.id)
                .collect();
            if owners.len() != 1 || matching_id.len() != 1 {
                return Err(format!(
                    "Browse Pull target {}:{} changed or became ambiguous while pulling; refresh and retry",
                    original_remote, original_remote_path
                ));
            }
        } else if !owners.is_empty() {
            return Err(format!(
                "Browse Pull target {}:{} became configured while pulling; refresh and retry",
                original_remote, original_remote_path
            ));
        }

        if std::path::Path::new(expanded).exists() {
            return Err(format!(
                "Local destination '{}' appeared while pulling; refresh and retry",
                expanded
            ));
        }
        if !std::path::Path::new(staging).exists() {
            return Err(format!(
                "Browse Pull staging directory '{}' is missing",
                staging
            ));
        }

        std::fs::rename(staging, expanded).map_err(|error| {
            format!("Pulled to {staging} but could not move it to {expanded}: {error}")
        })?;

        if existing_configured {
            let slot = cfg
                .projects
                .iter_mut()
                .find(|project| project.id == record.id)
                .expect("validated Browse Pull owner must still exist");
            slot.local_path = record.local_path.clone();
        } else {
            cfg.projects.push(record.clone());
        }

        if let Err(error) = config::save_config_unlocked(&cfg) {
            let rollback = std::fs::rename(expanded, staging);
            return match rollback {
                Ok(()) => Err(format!(
                    "Browse Pull config publication failed; downloaded files remain hidden in {staging}: {error}"
                )),
                Err(rollback_error) => Err(format!(
                    "Browse Pull config publication failed ({error}) and rollback failed ({rollback_error}); downloaded files remain at {expanded}"
                )),
            };
        }
        Ok(())
    })
}

/// Resolve a Browse Remote row without using its display name as identity.
/// `project_id` is supplied for a uniquely matched row; a missing ID is only
/// allowed when the selected remote/path has no configured exact target and
/// therefore represents a genuinely new project. Multiple exact targets fail
/// closed instead of selecting the first same-name record.
fn resolve_browse_pull_project(
    cfg: &AppConfig,
    name: &str,
    project_id: Option<&str>,
    remote: &str,
    remote_path: &str,
) -> Result<Option<Project>, String> {
    let remote_config = cfg
        .remotes
        .iter()
        .find(|candidate| candidate.name == remote)
        .ok_or_else(|| format!("Remote '{}' is not configured", remote))?;
    let remote_path = config::AppConfig::canonical_remote_path(remote_path);
    if remote_path.is_empty() {
        return Err("Browse Remote returned an empty remote path".into());
    }

    if let Some(project_id) = project_id.filter(|id| !id.trim().is_empty()) {
        let id_matches: Vec<&Project> = cfg
            .projects
            .iter()
            .filter(|project| project.id == project_id)
            .collect();
        let project = match id_matches.as_slice() {
            [project] => *project,
            [] => {
                return Err(format!(
                    "Browse Remote selection '{}' is stale; refresh the remote list",
                    project_id
                ));
            }
            _ => return Err(format!("Project ID '{}' is duplicated", project_id)),
        };
        let exact: Vec<&Project> = cfg
            .projects
            .iter()
            .filter(|candidate| {
                cfg.project_remote(candidate).name == remote
                    && cfg.project_remote_path(candidate) == remote_path
            })
            .collect();
        if exact.len() != 1 || exact[0].id != project.id {
            return Err(format!(
                "Browse Remote selection for '{}' no longer matches the selected remote path",
                name
            ));
        }
        return Ok(Some(project.clone()));
    }

    let exact: Vec<&Project> = cfg
        .projects
        .iter()
        .filter(|project| {
            cfg.project_remote(project).name == remote
                && cfg.project_remote_path(project) == remote_path
        })
        .collect();
    match exact.as_slice() {
        [project] => Ok(Some((*project).clone())),
        [] => {
            let expected_path =
                config::AppConfig::join_remote_child_path(&remote_config.base_path, name);
            if expected_path != remote_path {
                return Err(format!(
                    "Browse Remote path '{}' is not a listed child of remote '{}'",
                    remote_path, remote
                ));
            }
            Ok(None)
        }
        _ => Err(format!(
            "Browse Remote target '{}' is ambiguous for {}:{}; refresh and resolve the duplicate project IDs",
            name, remote, remote_path
        )),
    }
}

/// Unique sibling staging directory for an in-progress pull. The token is
/// generated by the backend for every invocation, so concurrent pulls aimed at
/// the same final destination cannot write into one another's staging tree.
fn staging_path(final_path: &str, token: &str) -> Result<String, String> {
    let p = Path::new(final_path);
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("Cannot derive a staging directory for '{}'", final_path))?;
    let parent = p.parent().unwrap_or_else(|| Path::new("."));
    Ok(parent
        .join(format!(".rcsync-partial-{}-{}", name, token))
        .to_string_lossy()
        .to_string())
}

#[tauri::command]
fn open_folder(local_path: String) -> Result<(), String> {
    let expanded = expand_tilde(&local_path);
    let path = Path::new(&expanded);
    if !path.exists() {
        return Err(format!("Directory does not exist: {}", expanded));
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&expanded)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(&expanded)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(&expanded)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn find_project(cfg: &AppConfig, name: &str) -> Result<Project, String> {
    let matches: Vec<&Project> = cfg.projects.iter().filter(|p| p.name == name).collect();
    if matches.len() > 1 {
        return Err(format!("Project name '{}' is ambiguous", name));
    }
    if let Some(p) = matches.first() {
        return Ok((*p).clone());
    }
    if let Some(local_path) = config::find_local_path(cfg, name) {
        return Ok(Project {
            id: config::project_id_for_fields(name, &local_path, "", &cfg.default_remote_name()),
            name: name.to_string(),
            local_path,
            // Empty, so `remote_path_for_project` applies the remote's configured
            // base_path. Hardcoding "proj/" sent every operation on a
            // scan-discovered project to the wrong tree whenever base_path was
            // anything else — and Push would then overwrite whatever lived there.
            remote_path: String::new(),
            remote: cfg.default_remote_name(),
            excludes: Vec::new(),
            schedule: None,
            schedule_error: None,
            legacy_schedule: None,
            legacy_schedule_raw: None,
        });
    }
    Err(format!("Project '{}' not found", name))
}

/// Resolve the project selected by a frontend snapshot before claiming an
/// operation. The ID is authoritative when supplied, while the name is only a
/// display label and a compatibility fallback for older clients.
fn project_for_operation(
    project_name: &str,
    requested_id: Option<&str>,
) -> Result<Project, String> {
    let cfg = load_config();
    if let Some(requested_id) = requested_id.filter(|id| !id.trim().is_empty()) {
        let matching_ids = cfg
            .projects
            .iter()
            .filter(|project| project.id == requested_id)
            .count();
        if matching_ids > 1 {
            return Err(format!("Project ID '{}' is duplicated", requested_id));
        }
        if let Ok(project) = find_project_by_id(&cfg, requested_id) {
            return Ok(project);
        }
        // A scan-discovered ID is an exact selector until the project is
        // materialized. Never fall back to an arbitrary same-named record.
        let discovered = find_project(&cfg, project_name)?;
        if cfg
            .projects
            .iter()
            .any(|project| project.name == project_name)
            || discovered.id != requested_id
        {
            return Err(format!(
                "Project selection is stale for '{}'; refresh the project list",
                project_name
            ));
        }
        return Ok(discovered);
    }
    find_project(&cfg, project_name)
}

/// Materialize a discovered selector for editors that persist project fields.
/// Ordinary Push can use the selector without writing config, but excludes and
/// remote edits need a durable record before they can be saved.
fn materialize_if_discovered(
    project_name: &str,
    requested_id: Option<&str>,
) -> Result<Project, String> {
    let cfg = load_config();
    let project = project_for_operation(project_name, requested_id)?;
    if cfg
        .projects
        .iter()
        .any(|candidate| candidate.id == project.id)
    {
        Ok(project)
    } else {
        config::materialize_discovered_project(&project)
    }
}

fn find_project_by_id(cfg: &AppConfig, project_id: &str) -> Result<Project, String> {
    let matches: Vec<&Project> = cfg
        .projects
        .iter()
        .filter(|project| project.id == project_id)
        .collect();
    match matches.as_slice() {
        [project] => Ok((*project).clone()),
        [] => Err(format!("Project ID '{}' not found", project_id)),
        _ => Err(format!("Project ID '{}' is duplicated", project_id)),
    }
}

fn resolve_project_for_push(
    cfg: &AppConfig,
    project_id: &str,
    project_name: &str,
) -> Result<Project, String> {
    let matching_ids = cfg
        .projects
        .iter()
        .filter(|project| project.id == project_id)
        .count();
    if matching_ids > 1 {
        return Err(format!("Project ID '{}' is duplicated", project_id));
    }
    match find_project_by_id(cfg, project_id) {
        Ok(project) => Ok(project),
        Err(_) => {
            // Scan discovery IDs are ephemeral selectors, not persisted
            // identities. Accept one only when it still resolves exactly to
            // the same current discovered project after the semaphore wait.
            let discovered = find_project(cfg, project_name)?;
            if cfg
                .projects
                .iter()
                .any(|project| project.name == project_name)
            {
                return Err(format!("Project ID '{}' not found", project_id));
            }
            if discovered.id == project_id {
                Ok(discovered)
            } else {
                Err(format!("Project ID '{}' is stale", project_id))
            }
        }
    }
}

/// Try to raise the open-file-descriptor soft limit so rclone processes
/// don't hit "Too many open files" on macOS (default soft limit is 256).
#[cfg(unix)]
fn raise_fd_limit() {
    use std::io;
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) == 0 {
            let target = rlim.rlim_max.min(8192);
            if rlim.rlim_cur < target {
                rlim.rlim_cur = target;
                if libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) != 0 {
                    eprintln!("warning: setrlimit failed: {}", io::Error::last_os_error());
                }
            }
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(unix)]
    raise_fd_limit();

    // The operation registry and semaphore are process-local. Hold an OS lock
    // before Tauri setup so a second login/manual launch cannot start another
    // scheduler over the same project trees.
    let _instance_lock = match instance_lock::acquire(&config::instance_lock_path()) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .invoke_handler(tauri::generate_handler![
            get_config,
            get_machine_name,
            get_projects_status,
            update_config,
            get_active_operations,
            get_schedule_status,
            set_project_schedule,
            set_queue_scheduled_pushes,
            migrate_legacy_automation,
            migrate_legacy_host_config,
            get_project_excludes,
            set_project_excludes,
            set_project_remote,
            push,
            pull,
            check,
            bisync,
            bisync_resync,
            cancel_op,
            cancel_scheduled_pending,
            delete_local,
            reattach_retired_target,
            open_folder,
            browse_remote,
            get_remotes,
            switch_remote,
            check_local_exists,
            pull_new_project,
        ])
        .setup(|app| {
            // Give rclone somewhere to send its output while it is still
            // running. Without this every operation is silent until it exits.
            rclone::set_app_handle(app.handle().clone());

            // Start file watcher — keep the handle alive for the app's lifetime.
            // Wrapped in catch_unwind so a watcher failure never crashes the app.
            let handle = app.handle().clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                watcher::start_watcher(handle.clone())
            }));
            if let Ok(Some(w)) = result {
                std::mem::forget(w);
            }
            scheduler::start(handle.clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod command_tests {
    use super::*;
    use std::fs;

    /// `set_project_excludes` is the command the ignore editor calls, and it is
    /// where a silently-repaired pattern would enter the config. Reinstating a
    /// `.trim()` there once left the whole suite green, because the only
    /// coverage went through `save_config` instead.
    #[test]
    fn the_ignore_editor_command_refuses_a_pattern_it_would_otherwise_repair() {
        let _env = config::TestConfigEnv::new("ignore-command");
        let initial = AppConfig {
            projects: vec![Project {
                id: String::new(),
                name: "p".into(),
                local_path: "~/p".into(),
                remote_path: String::new(),
                remote: "gdrive".into(),
                excludes: Vec::new(),
                schedule: None,
                schedule_error: None,
                legacy_schedule: None,
                legacy_schedule_raw: None,
            }],
            ..Default::default()
        };
        config::save_config(&initial).unwrap();

        let bad_patterns = [" leading/**", "trailing/** ", "break/**\n", "cr/**\r"];
        let outcomes: Vec<Result<(), String>> = bad_patterns
            .iter()
            .map(|bad| set_project_excludes("p".into(), None, vec![bad.to_string()]))
            .collect();

        // Asserted on the REASON, not merely on Err. The first version used an
        // unknown project, so an unrelated lookup failure made a reinstated
        // trim look correctly rejected.
        for (bad, outcome) in bad_patterns.iter().zip(&outcomes) {
            let err = outcome
                .as_ref()
                .expect_err(&format!("{:?} must be rejected", bad));
            assert!(
                err.contains("whitespace") || err.contains("line break"),
                "{:?} was not rejected as a bad pattern — it was trimmed on the way in and failed \
                 for some other reason: {}",
                bad,
                err
            );
        }

        // Positive controls through the same command: blank rows are dropped,
        // and an accepted pattern is persisted exactly rather than canonicalised.
        set_project_excludes("p".into(), None, vec!["   ".into()]).unwrap();
        assert!(load_config().projects[0].excludes.is_empty());
        set_project_excludes("p".into(), None, vec!["artifacts/**".into()]).unwrap();
        assert_eq!(load_config().projects[0].excludes, vec!["artifacts/**"]);
    }

    #[test]
    fn duplicate_name_project_editors_use_the_selected_project_id() {
        let _env = config::TestConfigEnv::new("duplicate-name-editors");
        let project = |id: &str, excludes: &[&str]| Project {
            id: id.into(),
            name: "same-name".into(),
            local_path: format!("~/projects/{id}"),
            remote_path: format!("proj/{id}"),
            remote: "gdrive".into(),
            excludes: excludes.iter().map(|value| (*value).into()).collect(),
            schedule: None,
            schedule_error: None,
            legacy_schedule: None,
            legacy_schedule_raw: None,
        };
        let initial = AppConfig {
            remotes: vec![
                RemoteConfig {
                    name: "gdrive".into(),
                    base_path: "proj".into(),
                },
                RemoteConfig {
                    name: "backup".into(),
                    base_path: "backup".into(),
                },
            ],
            projects: vec![project("p_first", &["first/**"]), project("p_second", &[])],
            ..AppConfig::default()
        };
        config::save_config(&initial).unwrap();

        assert_eq!(
            get_project_excludes("same-name".into(), Some("p_second".into())).unwrap(),
            Vec::<String>::new()
        );
        set_project_excludes(
            "same-name".into(),
            Some("p_second".into()),
            vec!["second/**".into()],
        )
        .unwrap();
        set_project_remote(
            "same-name".into(),
            Some("p_second".into()),
            "backup".into(),
            "archive/second".into(),
        )
        .unwrap();

        let saved = load_config();
        let first = saved.projects.iter().find(|p| p.id == "p_first").unwrap();
        let second = saved.projects.iter().find(|p| p.id == "p_second").unwrap();
        assert_eq!(first.excludes, vec!["first/**"]);
        assert_eq!(first.remote, "gdrive");
        assert_eq!(second.excludes, vec!["second/**"]);
        assert_eq!(second.remote, "backup");
        assert_eq!(second.remote_path, "archive/second");
    }

    #[test]
    fn browse_pull_duplicate_names_use_selected_project_id() {
        let cfg = AppConfig {
            remotes: vec![
                RemoteConfig {
                    name: "gdrive".into(),
                    base_path: "proj".into(),
                },
                RemoteConfig {
                    name: "backup".into(),
                    base_path: "backup".into(),
                },
            ],
            projects: vec![
                Project {
                    id: "p_gdrive".into(),
                    name: "same-name".into(),
                    local_path: "~/projects/gdrive-same".into(),
                    remote_path: "proj/same-name".into(),
                    remote: "gdrive".into(),
                    excludes: vec!["gdrive-only/**".into()],
                    schedule: None,
                    schedule_error: None,
                    legacy_schedule: None,
                    legacy_schedule_raw: None,
                },
                Project {
                    id: "p_backup".into(),
                    name: "same-name".into(),
                    local_path: "~/projects/backup-same".into(),
                    remote_path: "backup/same-name".into(),
                    remote: "backup".into(),
                    excludes: vec!["backup-only/**".into()],
                    schedule: None,
                    schedule_error: None,
                    legacy_schedule: None,
                    legacy_schedule_raw: None,
                },
            ],
            ..AppConfig::default()
        };

        let selected = resolve_browse_pull_project(
            &cfg,
            "same-name",
            Some("p_backup"),
            "backup",
            "backup/same-name",
        )
        .unwrap()
        .expect("the selected remote/path should resolve to the backup project");
        assert_eq!(selected.id, "p_backup");
        assert_eq!(selected.excludes, vec!["backup-only/**"]);

        let inferred =
            resolve_browse_pull_project(&cfg, "same-name", None, "gdrive", "proj/same-name")
                .unwrap()
                .expect("an exact remote/path match may be inferred when it is unique");
        assert_eq!(inferred.id, "p_gdrive");

        assert!(resolve_browse_pull_project(
            &cfg,
            "same-name",
            Some("p_gdrive"),
            "backup",
            "backup/same-name",
        )
        .is_err());

        // The remote leaf is not the mutable project display name. A renamed
        // configured project with a pinned remote path must still be selected
        // by that path rather than materialized as a second record.
        let mut renamed_cfg = cfg.clone();
        renamed_cfg.projects[0].name = "renamed-display-name".into();
        let renamed = resolve_browse_pull_project(
            &renamed_cfg,
            "same-name",
            None,
            "gdrive",
            "proj/same-name",
        )
        .unwrap()
        .expect("a pinned path remains owned after a display-name rename");
        assert_eq!(renamed.id, "p_gdrive");

        let mut ambiguous_cfg = cfg.clone();
        ambiguous_cfg.projects[1].remote = "gdrive".into();
        ambiguous_cfg.projects[1].remote_path = "proj/same-name".into();
        assert!(resolve_browse_pull_project(
            &ambiguous_cfg,
            "same-name",
            None,
            "gdrive",
            "proj/same-name",
        )
        .is_err());

        assert!(
            resolve_browse_pull_project(&cfg, "new-name", None, "backup", "backup/new-name",)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn browse_pull_slash_equivalent_owner_is_not_recreated() {
        let mut cfg = AppConfig {
            remotes: vec![RemoteConfig {
                name: "gdrive".into(),
                base_path: "proj/".into(),
            }],
            projects: vec![Project {
                id: "p-slash-owner".into(),
                name: "Project".into(),
                local_path: "~/projects/project".into(),
                remote_path: "proj/Project/".into(),
                remote: "gdrive".into(),
                excludes: vec![],
                schedule: None,
                schedule_error: None,
                legacy_schedule: None,
                legacy_schedule_raw: None,
            }],
            ..AppConfig::default()
        };

        let selected = resolve_browse_pull_project(&cfg, "Project", None, "gdrive", "proj/Project")
            .unwrap()
            .expect("a slash-equivalent configured owner must be retained");
        assert_eq!(selected.id, "p-slash-owner");

        cfg.projects.push(Project {
            id: "p-slash-collision".into(),
            name: "different-label".into(),
            local_path: "~/projects/other".into(),
            remote_path: "/proj/Project/".into(),
            remote: "gdrive".into(),
            excludes: vec![],
            schedule: None,
            schedule_error: None,
            legacy_schedule: None,
            legacy_schedule_raw: None,
        });
        assert!(
            resolve_browse_pull_project(&cfg, "Project", None, "gdrive", "proj/Project").is_err(),
            "canonical target collisions must fail closed"
        );
    }

    #[test]
    fn browse_pull_preserves_boundary_whitespace_without_aliasing() {
        let cfg = AppConfig {
            remotes: vec![RemoteConfig {
                name: "gdrive".into(),
                base_path: "proj".into(),
            }],
            projects: vec![Project {
                id: "p-no-space".into(),
                name: "Project".into(),
                local_path: "~/projects/project".into(),
                remote_path: "proj/Project".into(),
                remote: "gdrive".into(),
                excludes: vec![],
                schedule: None,
                schedule_error: None,
                legacy_schedule: None,
                legacy_schedule_raw: None,
            }],
            ..AppConfig::default()
        };

        // The selected remote row is a distinct directory, not a spelling
        // variant of the configured project. It must remain a new target.
        let selected =
            resolve_browse_pull_project(&cfg, "Project ", None, "gdrive", "proj/Project ").unwrap();
        assert!(selected.is_none());

        // Leading slash normalization is deliberate, but whitespace is not
        // discarded at either boundary.
        let selected =
            resolve_browse_pull_project(&cfg, " Project ", None, "gdrive", "/proj/ Project ")
                .unwrap();
        assert!(selected.is_none());
    }

    #[test]
    fn browse_pull_uses_one_remote_child_path_authority() {
        for (base, expected_path) in [
            ("", "Project"),
            ("/", "Project"),
            ("proj", "proj/Project"),
            ("proj/", "proj/Project"),
        ] {
            let cfg = AppConfig {
                remotes: vec![RemoteConfig {
                    name: "gdrive".into(),
                    base_path: base.into(),
                }],
                projects: vec![Project {
                    id: format!("p-{}", expected_path.replace('/', "-")),
                    name: "Project".into(),
                    local_path: "~/projects/project".into(),
                    remote_path: String::new(),
                    remote: "gdrive".into(),
                    excludes: vec![],
                    schedule: None,
                    schedule_error: None,
                    legacy_schedule: None,
                    legacy_schedule_raw: None,
                }],
                ..AppConfig::default()
            };

            assert_eq!(cfg.project_remote_path(&cfg.projects[0]), expected_path);
            let selected =
                resolve_browse_pull_project(&cfg, "Project", None, "gdrive", expected_path)
                    .unwrap()
                    .expect("the canonical child path should retain its configured owner");
            assert_eq!(selected.id, cfg.projects[0].id);
        }
    }

    #[cfg(unix)]
    #[test]
    fn browse_pull_revalidates_target_before_publish() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;

        let _env = config::TestConfigEnv::new("browse-pull-revalidate");
        let root = std::env::temp_dir().join(format!(
            "rcsync-browse-pull-revalidate-{}-{}",
            std::process::id(),
            epoch_millis()
        ));
        let final_path = root.join("Project");
        let fake = root.join("fake-rclone");
        let started = root.join("started");
        let release = root.join("release");
        fs::create_dir_all(&root).unwrap();
        let script = format!(
            r#"#!/bin/sh
case "$1" in
  sync)
    printf started > "{}"
    while [ ! -f "{}" ]; do sleep 0.02; done
    mkdir -p "$3"
    printf downloaded > "$3/from-remote.txt"
    ;;
  *) exit 97 ;;
esac
"#,
            started.display(),
            release.display()
        );
        fs::write(&fake, script).unwrap();
        let mut permissions = fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&fake, permissions).unwrap();
        fs::write(
            std::env::var("RCSYNC_CONFIG").unwrap(),
            serde_json::to_vec_pretty(&serde_json::json!({
                "rclone_path": fake,
                "remote": "gdrive",
                "remotes": [{"name": "gdrive", "base_path": "proj"}],
                "projects": [{
                    "id": "p-pull",
                    "name": "Project",
                    "local_path": "~/projects/Project",
                    "remote_path": "proj/Project",
                    "remote": "gdrive"
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let task = runtime.spawn(pull_new_project(
            "Project".into(),
            final_path.to_string_lossy().into_owned(),
            Some("p-pull".into()),
            "gdrive".into(),
            "proj/Project".into(),
        ));

        for _ in 0..250 {
            if started.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            started.exists(),
            "the fake transfer did not reach its barrier"
        );

        config::edit_config(|cfg| {
            cfg.projects[0].remote_path = "other/Project".into();
            Ok(())
        })
        .unwrap();
        fs::write(&release, b"go").unwrap();

        let error = runtime
            .block_on(task)
            .unwrap()
            .expect_err("a target changed during Pull must not publish stale bytes");
        assert!(error.contains("changed or became ambiguous"));
        assert!(
            !final_path.exists(),
            "stale Pull bytes must remain unpublished"
        );
        let staging_paths: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".rcsync-partial-Project-"))
            })
            .collect();
        assert_eq!(
            staging_paths.len(),
            1,
            "one hidden staging tree should remain"
        );
        assert!(
            staging_paths[0].join("from-remote.txt").exists(),
            "the completed bytes must remain recoverable in hidden staging"
        );
        let current = load_config();
        assert_eq!(current.projects.len(), 1);
        assert_eq!(current.projects[0].remote_path, "other/Project");

        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn browse_pull_concurrent_same_destination_keeps_staging_isolated() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;

        let _env = config::TestConfigEnv::new("browse-pull-same-destination");
        let root = std::env::temp_dir().join(format!(
            "rcsync-browse-pull-same-destination-{}-{}",
            std::process::id(),
            epoch_millis()
        ));
        let final_path = root.join("Project");
        let fake = root.join("fake-rclone");
        let started_a = root.join("started-a");
        let started_b = root.join("started-b");
        let release_a = root.join("release-a");
        let release_b = root.join("release-b");
        let written_a = root.join("written-a");
        let written_b = root.join("written-b");
        let destination_a = root.join("destination-a");
        let destination_b = root.join("destination-b");
        fs::create_dir_all(&root).unwrap();
        let script = format!(
            r#"#!/bin/sh
case "$2" in
  gdrive:proj/alpha)
    printf started > "{}"
    while [ ! -f "{}" ]; do sleep 0.02; done
    printf '%s' "$3" > "{}"
    mkdir -p "$3"
    printf A > "$3/payload.txt"
    printf written > "{}"
    while [ ! -f "{}" ]; do sleep 0.02; done
    ;;
  gdrive:proj/beta)
    printf started > "{}"
    while [ ! -f "{}" ]; do sleep 0.02; done
    printf '%s' "$3" > "{}"
    mkdir -p "$3"
    printf B > "$3/payload.txt"
    printf written > "{}"
    while [ ! -f "{}" ]; do sleep 0.02; done
    ;;
  *) exit 97 ;;
esac
"#,
            started_a.display(),
            release_a.display(),
            destination_a.display(),
            written_a.display(),
            written_b.display(),
            started_b.display(),
            written_a.display(),
            destination_b.display(),
            written_b.display(),
            release_b.display(),
        );
        fs::write(&fake, script).unwrap();
        let mut permissions = fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&fake, permissions).unwrap();
        fs::write(
            std::env::var("RCSYNC_CONFIG").unwrap(),
            serde_json::to_vec_pretty(&serde_json::json!({
                "rclone_path": fake,
                "remote": "gdrive",
                "remotes": [{"name": "gdrive", "base_path": "proj"}],
                "projects": []
            }))
            .unwrap(),
        )
        .unwrap();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let task_a = runtime.spawn(pull_new_project(
            "alpha".into(),
            final_path.to_string_lossy().into_owned(),
            None,
            "gdrive".into(),
            "proj/alpha".into(),
        ));
        let task_b = runtime.spawn(pull_new_project(
            "beta".into(),
            final_path.to_string_lossy().into_owned(),
            None,
            "gdrive".into(),
            "proj/beta".into(),
        ));

        for _ in 0..250 {
            if started_a.exists() && started_b.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            started_a.exists(),
            "alpha did not reach the transfer barrier"
        );
        assert!(
            started_b.exists(),
            "beta did not reach the transfer barrier"
        );

        fs::write(&release_a, b"go").unwrap();
        for _ in 0..250 {
            if written_b.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            written_b.exists(),
            "beta did not write its isolated staging tree"
        );

        let result_a = runtime
            .block_on(task_a)
            .unwrap()
            .expect("alpha should publish its own completed Pull");
        assert!(result_a.is_empty());
        assert_eq!(
            fs::read_to_string(final_path.join("payload.txt")).unwrap(),
            "A"
        );

        let staging_a = fs::read_to_string(&destination_a).unwrap();
        let staging_b = fs::read_to_string(&destination_b).unwrap();
        assert_ne!(
            staging_a, staging_b,
            "concurrent Pulls must reserve distinct staging paths"
        );
        assert!(
            !Path::new(&staging_a).exists(),
            "alpha staging should have been published"
        );
        assert!(Path::new(&staging_b).join("payload.txt").exists());
        assert_eq!(
            fs::read_to_string(Path::new(&staging_b).join("payload.txt")).unwrap(),
            "B"
        );

        let current = load_config();
        assert_eq!(current.projects.len(), 1);
        assert_eq!(current.projects[0].name, "alpha");
        assert_eq!(current.projects[0].remote_path, "proj/alpha");

        fs::write(&release_b, b"go").unwrap();
        let error = runtime
            .block_on(task_b)
            .unwrap()
            .expect_err("the losing Pull must not replace the published destination");
        assert!(
            error.contains("Local destination"),
            "unexpected error: {error}"
        );
        assert_eq!(
            fs::read_to_string(final_path.join("payload.txt")).unwrap(),
            "A"
        );

        fs::remove_dir_all(root).ok();
    }

    /// Exercise the real Push command with a scan-discovered project. The
    /// selector ID is not present in the shared config, so the post-semaphore
    /// reload must re-resolve that exact discovery selector rather than using
    /// the configured-project-only lookup.
    #[cfg(unix)]
    #[test]
    fn scan_discovered_push_resolves_after_reload() {
        use std::os::unix::fs::PermissionsExt;

        let _env = config::TestConfigEnv::new("discovered-push");
        let root = std::env::temp_dir().join(format!(
            "rcsync-discovered-push-{}-{}",
            std::process::id(),
            epoch_millis()
        ));
        let project_dir = root.join("discovered");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("keep.txt"), "keep").unwrap();

        let fake = root.join("fake-rclone");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &fake,
            "#!/bin/sh\ncase \"$1\" in\n  size) printf '%s\\n' '{\"count\":1,\"bytes\":4}' ;;\n  sync) exit 0 ;;\n  *) exit 97 ;;\nesac\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&fake, permissions).unwrap();

        let config_path = std::env::var("RCSYNC_CONFIG").unwrap();
        fs::write(
            config_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "rclone_path": fake,
                "scan_dirs": [root],
                "projects": []
            }))
            .unwrap(),
        )
        .unwrap();

        let selected = get_projects_status()
            .into_iter()
            .find(|project| project.name == "discovered")
            .expect("the scan-discovered project must be visible")
            .id;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(push("discovered".into(), Some(selected), true));
        assert!(
            result.is_ok(),
            "discovered Push failed after reload: {result:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_discovered_push_is_blocked_for_a_retired_remote_target() {
        let _env = config::TestConfigEnv::new("discovered-retired-target");
        let root = std::env::temp_dir().join(format!(
            "rcsync-discovered-retired-{}-{}",
            std::process::id(),
            epoch_millis()
        ));
        let project_dir = root.join("discovered");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("stale.txt"), "stale").unwrap();
        let root_string = root.to_string_lossy().into_owned();
        fs::write(
            std::env::var("RCSYNC_CONFIG").unwrap(),
            serde_json::to_vec_pretty(&serde_json::json!({
                "scan_dirs": [root_string],
                "projects": [],
                "retired_targets": [{
                    "remote": "gdrive",
                    "remote_path": "proj/discovered",
                    "name_at_retirement": "discovered",
                    "project_id_at_retirement": "p_old",
                    "retired_at_ms": 1,
                    "retired_by_device": "device-a"
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let selected = get_projects_status()
            .into_iter()
            .find(|project| project.name == "discovered")
            .expect("the leftover directory must remain visible")
            .id;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(push("discovered".into(), Some(selected), false));
        let error = result.expect_err("a retired target must block a real Push");
        assert!(error.contains("Remote target gdrive:proj/discovered was retired"));
        assert!(
            project_dir.exists(),
            "the blocked Push must not delete the source"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recreated_materialized_project_does_not_reactivate_orphan_schedule() {
        let _env = config::TestConfigEnv::new("materialized-orphan");
        let root = std::env::temp_dir().join(format!(
            "rcsync-materialized-orphan-{}-{}",
            std::process::id(),
            epoch_millis()
        ));
        let project_dir = root.join("discovered");
        fs::create_dir_all(&project_dir).unwrap();
        let root_string = root.to_string_lossy().into_owned();
        fs::write(
            std::env::var("RCSYNC_CONFIG").unwrap(),
            serde_json::to_vec_pretty(&serde_json::json!({
                "scan_dirs": [root_string],
                "projects": []
            }))
            .unwrap(),
        )
        .unwrap();

        let selector = get_projects_status()
            .into_iter()
            .find(|project| project.name == "discovered")
            .expect("the discovered project must be visible")
            .id;
        let old_id = set_project_schedule(
            "discovered".into(),
            Some(selector.clone()),
            Some(Schedule::Interval {
                every: 24,
                unit: config::IntervalUnit::Hours,
                origin_ms: 1,
            }),
        )
        .unwrap();
        config::edit_config(|cfg| {
            cfg.projects.clear();
            Ok(())
        })
        .unwrap();

        // This is the production materialization path used by the ignore
        // editor. It must allocate a new identity, leaving the old local
        // schedule orphaned rather than reviving it for the recreated record.
        set_project_excludes(
            "discovered".into(),
            Some(selector),
            vec!["artifacts/**".into()],
        )
        .unwrap();
        let cfg = load_config();
        assert_ne!(cfg.projects[0].id, old_id);
        assert!(cfg.projects[0].schedule.is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_stale_settings_save_preserves_a_project_card_schedule() {
        let _env = config::TestConfigEnv::new("settings-schedule-merge");
        let initial = AppConfig {
            projects: vec![Project {
                id: String::new(),
                name: "p".into(),
                local_path: "~/keep".into(),
                remote_path: "remote/keep".into(),
                remote: "gdrive".into(),
                excludes: vec!["keep/**".into()],
                schedule: None,
                schedule_error: None,
                legacy_schedule: None,
                legacy_schedule_raw: None,
            }],
            ..Default::default()
        };
        config::save_config(&initial).unwrap();
        let current_id = load_config().projects[0].id.clone();
        config::set_local_project_schedule(
            "p",
            Some(&current_id),
            Some(Schedule::Interval {
                every: 6,
                unit: config::IntervalUnit::Hours,
                origin_ms: 1,
            }),
        )
        .unwrap();

        // This is the shape a Settings window opened before the card editor
        // changed the schedule would send back.
        let mut stale = initial.clone();
        stale.projects[0].id = current_id;
        stale.rclone_path = "rclone-updated".into();
        stale.projects[0].local_path = "~/stale".into();
        stale.projects[0].remote_path = "remote/stale".into();
        stale.projects[0].excludes = vec!["stale/**".into()];
        stale.projects[0].schedule = None;
        update_config(stale).unwrap();

        let saved = load_config();
        assert_eq!(saved.rclone_path, "rclone-updated");
        assert_eq!(saved.projects[0].local_path, "~/keep");
        assert_eq!(saved.projects[0].remote_path, "remote/keep");
        assert_eq!(saved.projects[0].excludes, vec!["keep/**"]);
        assert!(matches!(
            saved.projects[0].schedule,
            Some(Schedule::Interval { .. })
        ));
    }

    #[test]
    fn settings_payload_recreation_cannot_reactivate_orphan_schedule() {
        let _env = config::TestConfigEnv::new("settings-payload-recreation");
        let mut payload = serde_json::to_value(AppConfig::default()).unwrap();
        payload["projects"] = serde_json::json!([{
            "id": "",
            "name": "recreated",
            "local_path": "~/projects/recreated",
            "remote_path": "proj/recreated",
            "remote": "gdrive"
        }]);

        // Settings sends a JSON-deserialized AppConfig, not a hand-built Rust
        // Project. This round trip is important because Project::deserialize
        // leaves the blank ID blank until update_config reconciles it.
        let first_payload: AppConfig = serde_json::from_value(payload.clone()).unwrap();
        update_config(first_payload).unwrap();
        let first = load_config().projects[0].clone();
        let old_id = first.id.clone();
        set_project_schedule(
            first.name.clone(),
            Some(old_id.clone()),
            Some(Schedule::Interval {
                every: 24,
                unit: config::IntervalUnit::Hours,
                origin_ms: 1,
            }),
        )
        .unwrap();

        // This is the same Settings update path after the project was removed.
        config::edit_config(|cfg| {
            cfg.projects.clear();
            Ok(())
        })
        .unwrap();
        let second_payload: AppConfig = serde_json::from_value(payload).unwrap();
        update_config(second_payload).unwrap();

        let recreated = load_config().projects[0].clone();
        assert_ne!(recreated.id, old_id);
        assert!(recreated.schedule.is_none());
    }

    #[test]
    fn settings_blank_bootstrap_does_not_match_different_legacy_record() {
        let _env = config::TestConfigEnv::new("settings-bootstrap-replacement");
        let old_local = "~/projects/legacy-same";
        let old_remote_path = "proj/legacy-same";
        let old_id =
            config::project_id_for_fields("same-name", old_local, old_remote_path, "gdrive");
        let original = AppConfig {
            projects: vec![Project {
                id: old_id.clone(),
                name: "same-name".into(),
                local_path: old_local.into(),
                remote_path: old_remote_path.into(),
                remote: "gdrive".into(),
                excludes: Vec::new(),
                schedule: None,
                schedule_error: None,
                legacy_schedule: None,
                legacy_schedule_raw: None,
            }],
            ..AppConfig::default()
        };
        config::save_config(&original).unwrap();
        set_project_schedule(
            "same-name".into(),
            Some(old_id.clone()),
            Some(Schedule::Interval {
                every: 24,
                unit: config::IntervalUnit::Hours,
                origin_ms: 1,
            }),
        )
        .unwrap();

        // One Settings submission removes the old row and adds a new blank-ID
        // row with the same display name but different identity-bearing fields.
        // The JSON round trip reproduces the Tauri payload/deserializer path.
        let mut replacement_value = serde_json::to_value(AppConfig::default()).unwrap();
        replacement_value["projects"] = serde_json::json!([{
            "id": "",
            "name": "same-name",
            "local_path": "~/projects/replacement",
            "remote_path": "proj/replacement",
            "remote": "gdrive"
        }]);
        let replacement: AppConfig = serde_json::from_value(replacement_value).unwrap();
        update_config(replacement).unwrap();

        let saved = load_config();
        assert_eq!(saved.projects.len(), 1);
        assert_eq!(saved.projects[0].local_path, "~/projects/replacement");
        assert_eq!(saved.projects[0].remote_path, "proj/replacement");
        assert_ne!(saved.projects[0].id, old_id);
        assert!(saved.projects[0].schedule.is_none());
    }

    #[test]
    fn settings_identical_blank_replacement_cannot_reactivate_schedule() {
        let _env = config::TestConfigEnv::new("settings-identical-blank-replacement");
        let local_path = "~/projects/identical";
        let remote_path = "proj/identical";
        let old_id = config::project_id_for_fields("identical", local_path, remote_path, "gdrive");
        let initial = AppConfig {
            projects: vec![Project {
                id: old_id.clone(),
                name: "identical".into(),
                local_path: local_path.into(),
                remote_path: remote_path.into(),
                remote: "gdrive".into(),
                excludes: Vec::new(),
                schedule: None,
                schedule_error: None,
                legacy_schedule: None,
                legacy_schedule_raw: None,
            }],
            ..AppConfig::default()
        };
        config::save_config(&initial).unwrap();
        set_project_schedule(
            "identical".into(),
            Some(old_id.clone()),
            Some(Schedule::Interval {
                every: 24,
                unit: config::IntervalUnit::Hours,
                origin_ms: 1,
            }),
        )
        .unwrap();

        // Settings can remove and add a row in one payload. The blank ID must
        // remain distinguishable from the deterministic ID of the old legacy
        // record until replace_config decides whether it is new.
        let mut value = serde_json::to_value(AppConfig::default()).unwrap();
        value["projects"] = serde_json::json!([{
            "id": "",
            "name": "identical",
            "local_path": local_path,
            "remote_path": remote_path,
            "remote": "gdrive"
        }]);
        let replacement: AppConfig = serde_json::from_value(value).unwrap();
        update_config(replacement).unwrap();

        let saved = load_config();
        assert_eq!(saved.projects.len(), 1);
        assert_eq!(saved.projects[0].name, "identical");
        assert_eq!(saved.projects[0].local_path, local_path);
        assert_ne!(saved.projects[0].id, old_id);
        assert!(saved.projects[0].schedule.is_none());
    }

    #[test]
    fn settings_duplicate_name_delete_keeps_the_exact_id_and_deactivates_removed_schedule() {
        let _env = config::TestConfigEnv::new("settings-duplicate-name");
        let initial = AppConfig {
            projects: vec![
                Project {
                    id: "p_first".into(),
                    name: "same".into(),
                    local_path: "~/projects/first".into(),
                    remote_path: "proj/same".into(),
                    remote: "gdrive".into(),
                    excludes: Vec::new(),
                    schedule: None,
                    schedule_error: None,
                    legacy_schedule: None,
                    legacy_schedule_raw: None,
                },
                Project {
                    id: "p_second".into(),
                    name: "same".into(),
                    local_path: "~/projects/second".into(),
                    remote_path: "proj/same".into(),
                    remote: "gdrive".into(),
                    excludes: Vec::new(),
                    schedule: None,
                    schedule_error: None,
                    legacy_schedule: None,
                    legacy_schedule_raw: None,
                },
            ],
            ..AppConfig::default()
        };
        config::save_config(&initial).unwrap();
        set_project_schedule(
            "same".into(),
            Some("p_first".into()),
            Some(Schedule::Interval {
                every: 24,
                unit: config::IntervalUnit::Hours,
                origin_ms: 1,
            }),
        )
        .unwrap();

        // Settings removes the first row but retains the second exact ID. This
        // is the production command path, including the current-config refresh
        // and replace_config reconciliation.
        let mut edited = load_config();
        edited.projects = vec![edited
            .projects
            .iter()
            .find(|project| project.id == "p_second")
            .cloned()
            .unwrap()];
        update_config(edited).unwrap();

        let saved = load_config();
        assert_eq!(saved.projects.len(), 1);
        assert_eq!(saved.projects[0].id, "p_second");
        assert!(saved.projects[0].schedule.is_none());
    }

    #[test]
    fn settings_stale_opaque_id_cannot_reactivate_a_same_name_schedule() {
        let _env = config::TestConfigEnv::new("settings-stale-id");
        let initial = AppConfig {
            projects: vec![Project {
                id: "p_current".into(),
                name: "same".into(),
                local_path: "~/projects/same".into(),
                remote_path: "proj/same".into(),
                remote: "gdrive".into(),
                excludes: Vec::new(),
                schedule: None,
                schedule_error: None,
                legacy_schedule: None,
                legacy_schedule_raw: None,
            }],
            ..AppConfig::default()
        };
        config::save_config(&initial).unwrap();
        set_project_schedule(
            "same".into(),
            Some("p_current".into()),
            Some(Schedule::Interval {
                every: 24,
                unit: config::IntervalUnit::Hours,
                origin_ms: 1,
            }),
        )
        .unwrap();

        let mut incoming_value = serde_json::to_value(AppConfig::default()).unwrap();
        incoming_value["projects"] = serde_json::json!([{
            "id": "p_stale",
            "name": "same",
            "local_path": "~/projects/same",
            "remote_path": "proj/same",
            "remote": "gdrive"
        }]);
        let incoming: AppConfig = serde_json::from_value(incoming_value).unwrap();
        update_config(incoming).unwrap();

        let saved = load_config();
        assert_ne!(saved.projects[0].id, "p_current");
        assert!(saved.projects[0].schedule.is_none());
    }

    #[test]
    fn scheduled_push_resolves_the_claimed_project_by_id_after_a_rename() {
        let cfg = AppConfig {
            projects: vec![Project {
                id: "p_stable".into(),
                name: "renamed".into(),
                local_path: "~/projects/renamed".into(),
                remote_path: "proj/renamed".into(),
                remote: "gdrive".into(),
                excludes: Vec::new(),
                schedule: None,
                schedule_error: None,
                legacy_schedule: None,
                legacy_schedule_raw: None,
            }],
            ..AppConfig::default()
        };
        let project = resolve_project_for_push(&cfg, "p_stable", "renamed").unwrap();
        assert_eq!(project.name, "renamed");
        assert!(resolve_project_for_push(&cfg, "p_missing", "renamed").is_err());
    }

    #[test]
    fn delete_local_publishes_a_retired_target_before_removing_the_project() {
        let _env = config::TestConfigEnv::new("delete-retired-target");
        let root = std::env::temp_dir().join(format!(
            "rcsync-delete-retired-{}-{}",
            std::process::id(),
            epoch_millis()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("stale.txt"), "stale").unwrap();
        let project = Project {
            id: "p_delete".into(),
            name: "example".into(),
            local_path: root.to_string_lossy().into_owned(),
            remote_path: String::new(),
            remote: "gdrive".into(),
            excludes: Vec::new(),
            schedule: None,
            schedule_error: None,
            legacy_schedule: None,
            legacy_schedule_raw: None,
        };
        config::save_config(&AppConfig {
            projects: vec![project.clone()],
            ..AppConfig::default()
        })
        .unwrap();

        delete_local(project.name.clone(), Some(project.id.clone())).unwrap();

        let cfg = load_config();
        assert!(cfg.projects.is_empty());
        assert_eq!(cfg.retired_targets.len(), 1);
        assert_eq!(cfg.retired_targets[0].remote_path, "proj/example");
        assert!(!root.exists());
    }
}
