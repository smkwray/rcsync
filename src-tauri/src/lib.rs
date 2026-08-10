mod config;
mod rclone;
mod watcher;

use config::{expand_tilde, load_config, save_config, AppConfig, Project, RemoteConfig};
use rclone::{bisync_project, check_project, list_remote, local_dir_has_content, sync_project, RemoteDir};
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
    name: String,
    local_path: String,
    remote_path: String,
    remote: String,
    exists_locally: bool,
}

#[tauri::command]
fn get_config() -> AppConfig {
    load_config()
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
        if Path::new(&expanded).exists() {
            let remote = if p.remote.is_empty() { default_remote.clone() } else { p.remote.clone() };
            result.push(ProjectStatus {
                name: p.name.clone(),
                local_path: p.local_path.clone(),
                remote_path: cfg.project_remote_path(p),
                remote,
                exists_locally: true,
            });
        }
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
                            name: name.to_string(),
                            local_path: format!("{}/{}", dir, name),
                            remote_path: String::new(),
                            remote: default_remote.clone(),
                            excludes: Vec::new(),
                        };
                        result.push(ProjectStatus {
                            name: discovered.name.clone(),
                            local_path: discovered.local_path.clone(),
                            remote_path: cfg.project_remote_path(&discovered),
                            remote: default_remote.clone(),
                            exists_locally: true,
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
    save_config(&cfg)
}

/// Read the per-project excludes for a project. Returns an empty list for
/// projects that exist only via scan discovery (no config entry yet).
#[tauri::command]
fn get_project_excludes(project_name: String) -> Vec<String> {
    load_config()
        .projects
        .iter()
        .find(|p| p.name == project_name)
        .map(|p| p.excludes.clone())
        .unwrap_or_default()
}

/// Set the per-project excludes for a project and persist them. If the project
/// is not yet in the config (scan-discovered), it is materialized into the
/// config so the excludes have a home — exactly the same record a manual
/// `push` would resolve, so nothing else about the project changes.
#[tauri::command]
fn set_project_excludes(project_name: String, excludes: Vec<String>) -> Result<(), String> {
    // Normalize: trim, drop blank lines, dedupe while preserving order.
    let mut seen = std::collections::HashSet::new();
    let cleaned: Vec<String> = excludes
        .into_iter()
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .filter(|e| seen.insert(e.clone()))
        .collect();

    let mut cfg = load_config();
    if let Some(p) = cfg.projects.iter_mut().find(|p| p.name == project_name) {
        p.excludes = cleaned;
    } else {
        let mut project = find_project(&cfg, &project_name)?;
        project.excludes = cleaned;
        cfg.projects.push(project);
    }
    save_config(&cfg)
}

/// Change which remote (and path on that remote) a project syncs to, and
/// persist it. Materializes a config entry for scan-discovered projects, same
/// as `set_project_excludes`. The remote must be one of the configured remotes.
/// This only changes future sync targeting — it never moves or deletes anything
/// already uploaded to the old remote.
#[tauri::command]
fn set_project_remote(
    project_name: String,
    remote: String,
    remote_path: String,
) -> Result<(), String> {
    let mut cfg = load_config();
    if !cfg.remotes.iter().any(|r| r.name == remote) {
        return Err(format!("Remote '{}' is not configured", remote));
    }
    let remote_path = remote_path.trim().to_string();
    if let Some(p) = cfg.projects.iter_mut().find(|p| p.name == project_name) {
        p.remote = remote;
        p.remote_path = remote_path;
    } else {
        let mut project = find_project(&cfg, &project_name)?;
        project.remote = remote;
        project.remote_path = remote_path;
        cfg.projects.push(project);
    }
    save_config(&cfg)
}

/// All rclone commands run in spawn_blocking and return their output as a string.
/// Stop the in-flight operation for a project, killing rclone if it has already
/// started. Returns false when there was nothing to stop.
#[tauri::command]
fn cancel_op(project_name: String) -> bool {
    rclone::request_cancel(&project_name)
}

/// Every operation claims its project up front (so a cancel can reach it while
/// it is still queued) and re-checks for a cancel after the queue lets it
/// through (so a cancelled operation never spawns rclone at all).
#[tauri::command]
async fn push(project_name: String, dry_run: bool) -> Result<String, String> {
    let _op = rclone::start_op(&project_name)?;
    let cfg = load_config();
    let project = find_project(&cfg, &project_name)?;
    let _permit = rclone_semaphore().acquire().await.map_err(|_| "Operation queue closed".to_string())?;
    rclone::check_cancelled(&project_name)?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    tokio::task::spawn_blocking(move || sync_project(&cfg2, &proj2, "push", dry_run))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn pull(project_name: String, dry_run: bool) -> Result<String, String> {
    let _op = rclone::start_op(&project_name)?;
    let cfg = load_config();
    let project = find_project(&cfg, &project_name)?;
    let _permit = rclone_semaphore().acquire().await.map_err(|_| "Operation queue closed".to_string())?;
    rclone::check_cancelled(&project_name)?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    tokio::task::spawn_blocking(move || sync_project(&cfg2, &proj2, "pull", dry_run))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn check(project_name: String) -> Result<rclone::CheckOutcome, String> {
    let _op = rclone::start_op(&project_name)?;
    let cfg = load_config();
    let project = find_project(&cfg, &project_name)?;
    let _permit = rclone_semaphore().acquire().await.map_err(|_| "Operation queue closed".to_string())?;
    rclone::check_cancelled(&project_name)?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    tokio::task::spawn_blocking(move || check_project(&cfg2, &proj2))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn bisync(project_name: String) -> Result<String, String> {
    let _op = rclone::start_op(&project_name)?;
    let cfg = load_config();
    let project = find_project(&cfg, &project_name)?;
    let _permit = rclone_semaphore().acquire().await.map_err(|_| "Operation queue closed".to_string())?;
    rclone::check_cancelled(&project_name)?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    tokio::task::spawn_blocking(move || bisync_project(&cfg2, &proj2))
        .await
        .map_err(|e| e.to_string())?
}

/// Delete local project directory AND remove it from config so a
/// subsequent "push all" can never accidentally sync the now-empty path.
#[tauri::command]
fn delete_local(project_name: String) -> Result<(), String> {
    let mut cfg = load_config();
    let project = find_project(&cfg, &project_name)?;
    let expanded = expand_tilde(&project.local_path);
    let path = Path::new(&expanded);
    if !path.exists() {
        return Err("Local directory does not exist".into());
    }
    std::fs::remove_dir_all(path)
        .map_err(|e| format!("Failed to delete {}: {}", expanded, e))?;

    // Remove from config to prevent any future push of a recreated empty dir
    let before = cfg.projects.len();
    cfg.projects.retain(|p| p.name != project_name);
    if cfg.projects.len() != before {
        save_config(&cfg)
            .map_err(|e| format!("Deleted locally but failed to update config: {e}"))?;
    }
    Ok(())
}

#[tauri::command]
async fn browse_remote(remote_name: Option<String>) -> Result<Vec<RemoteDir>, String> {
    let cfg = load_config();
    let _permit = rclone_semaphore().acquire().await.map_err(|_| "Operation queue closed".to_string())?;
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
    let mut cfg = load_config();
    if !cfg.remotes.iter().any(|r| r.name == remote_name) {
        return Err(format!("Remote '{}' not found in config", remote_name));
    }
    cfg.remote = remote_name;
    save_config(&cfg)
}

#[tauri::command]
fn check_local_exists(local_path: String) -> Result<bool, String> {
    Ok(local_dir_has_content(&local_path))
}

#[tauri::command]
async fn pull_new_project(name: String, local_path: String) -> Result<String, String> {
    let _op = rclone::start_op(&name)?;
    let mut cfg = load_config();

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
    let staging = staging_path(&expanded)?;
    std::fs::create_dir_all(&staging)
        .map_err(|e| format!("Failed to create staging directory {staging}: {e}"))?;

    // A project can already be configured yet have no local copy — that is
    // exactly when Browse offers to pull it. Carry its excludes across so the
    // pull applies the same filter policy its other operations do.
    let existing_excludes = cfg
        .projects
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.excludes.clone())
        .unwrap_or_default();

    let project = Project {
        name: name.clone(),
        local_path: staging.clone(),
        // Empty, so `remote_path_for_project` resolves the remote's configured
        // base_path. Hardcoding "proj/" pulled from the wrong place on any
        // remote whose base_path is not "proj" — which Browse itself lists from.
        remote_path: String::new(),
        remote: cfg.remote.clone(), // Browse switches the active remote on select
        excludes: existing_excludes,
    };

    let _permit = rclone_semaphore().acquire().await.map_err(|_| "Operation queue closed".to_string())?;
    rclone::check_cancelled(&name)?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    let output =
        tokio::task::spawn_blocking(move || sync_project(&cfg2, &proj2, "pull", false))
            .await
            .map_err(|e| e.to_string())??;

    // Only a complete pull becomes a real project. Re-running Pull re-uses the
    // staging directory, so an interrupted download resumes rather than
    // restarting — rclone skips what already matches.
    std::fs::rename(&staging, &expanded)
        .map_err(|e| format!("Pulled to {staging} but could not move it to {expanded}: {e}"))?;

    let record = Project {
        local_path,
        remote_path: String::new(),
        ..project
    };
    // Replace, don't append. Pushing a second record with the same name left the
    // stale one first in the list, and `find_project` returns the first match —
    // so every later operation resolved the old, missing local path.
    match cfg.projects.iter_mut().find(|p| p.name == record.name) {
        Some(slot) => *slot = record,
        None => cfg.projects.push(record),
    }
    save_config(&cfg)?;

    Ok(output)
}

/// Sibling staging directory for an in-progress pull: `<parent>/.rcsync-partial-<name>`.
fn staging_path(final_path: &str) -> Result<String, String> {
    let p = Path::new(final_path);
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("Cannot derive a staging directory for '{}'", final_path))?;
    let parent = p.parent().unwrap_or_else(|| Path::new("."));
    Ok(parent
        .join(format!(".rcsync-partial-{}", name))
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
    { std::process::Command::new("open").arg(&expanded).spawn().map_err(|e| e.to_string())?; }
    #[cfg(target_os = "windows")]
    { std::process::Command::new("explorer").arg(&expanded).spawn().map_err(|e| e.to_string())?; }
    #[cfg(target_os = "linux")]
    { std::process::Command::new("xdg-open").arg(&expanded).spawn().map_err(|e| e.to_string())?; }
    Ok(())
}

fn find_project(cfg: &AppConfig, name: &str) -> Result<Project, String> {
    if let Some(p) = cfg.projects.iter().find(|p| p.name == name) {
        return Ok(p.clone());
    }
    if let Some(local_path) = config::find_local_path(cfg, name) {
        return Ok(Project {
            name: name.to_string(),
            local_path,
            // Empty, so `remote_path_for_project` applies the remote's configured
            // base_path. Hardcoding "proj/" sent every operation on a
            // scan-discovered project to the wrong tree whenever base_path was
            // anything else — and Push would then overwrite whatever lived there.
            remote_path: String::new(),
            remote: cfg.default_remote_name(),
            excludes: Vec::new(),
        });
    }
    Err(format!("Project '{}' not found", name))
}

/// Try to raise the open-file-descriptor soft limit so rclone processes
/// don't hit "Too many open files" on macOS (default soft limit is 256).
#[cfg(unix)]
fn raise_fd_limit() {
    use std::io;
    let mut rlim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
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
            get_project_excludes,
            set_project_excludes,
            set_project_remote,
            push,
            pull,
            check,
            bisync,
            cancel_op,
            delete_local,
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
                watcher::start_watcher(handle)
            }));
            if let Ok(Some(w)) = result {
                std::mem::forget(w);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
