mod config;
mod rclone;
mod watcher;

use config::{expand_tilde, load_config, save_config, AppConfig, Project, RemoteConfig};
use rclone::{bisync_project, check_project, list_remote, local_dir_has_content, sync_project, RemoteDir};
use serde::Serialize;
use std::path::Path;
use std::sync::OnceLock;
use tokio::sync::Semaphore;

/// Limit concurrent rclone processes to avoid overwhelming the remote or OS.
fn rclone_semaphore() -> &'static Semaphore {
    static SEM: OnceLock<Semaphore> = OnceLock::new();
    SEM.get_or_init(|| Semaphore::new(6))
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
                remote_path: p.remote_path.clone(),
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
                        result.push(ProjectStatus {
                            name: name.to_string(),
                            local_path: format!("{}/{}", dir, name),
                            remote_path: format!("proj/{}", name),
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

/// All rclone commands run in spawn_blocking and return their output as a string.

#[tauri::command]
async fn push(project_name: String, dry_run: bool) -> Result<String, String> {
    let cfg = load_config();
    let project = find_project(&cfg, &project_name)?;
    let _permit = rclone_semaphore().acquire().await.map_err(|_| "Operation queue closed".to_string())?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    tokio::task::spawn_blocking(move || sync_project(&cfg2, &proj2, "push", dry_run))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn pull(project_name: String, dry_run: bool) -> Result<String, String> {
    let cfg = load_config();
    let project = find_project(&cfg, &project_name)?;
    let _permit = rclone_semaphore().acquire().await.map_err(|_| "Operation queue closed".to_string())?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    tokio::task::spawn_blocking(move || sync_project(&cfg2, &proj2, "pull", dry_run))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn check(project_name: String) -> Result<String, String> {
    let cfg = load_config();
    let project = find_project(&cfg, &project_name)?;
    let _permit = rclone_semaphore().acquire().await.map_err(|_| "Operation queue closed".to_string())?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    tokio::task::spawn_blocking(move || check_project(&cfg2, &proj2))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn bisync(project_name: String) -> Result<String, String> {
    let cfg = load_config();
    let project = find_project(&cfg, &project_name)?;
    let _permit = rclone_semaphore().acquire().await.map_err(|_| "Operation queue closed".to_string())?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    tokio::task::spawn_blocking(move || bisync_project(&cfg2, &proj2))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn push_all(dry_run: bool) -> Result<String, String> {
    let cfg = load_config();
    let mut all_output = String::new();
    let projects: Vec<_> = cfg.projects.clone();

    for project in &projects {
        let expanded = expand_tilde(&project.local_path);
        if !Path::new(&expanded).exists() {
            continue;
        }
        let _permit = rclone_semaphore().acquire().await.map_err(|_| "Operation queue closed".to_string())?;
        let cfg2 = cfg.clone();
        let proj2 = project.clone();
        match tokio::task::spawn_blocking(move || sync_project(&cfg2, &proj2, "push", dry_run))
            .await
        {
            Ok(Ok(output)) => {
                all_output.push_str(&format!("=== {} ===\n{}\n", project.name, output));
            }
            Ok(Err(e)) => {
                all_output.push_str(&format!("=== {} ERROR ===\n{}\n", project.name, e));
            }
            Err(e) => {
                all_output.push_str(&format!("=== {} FAILED ===\n{}\n", project.name, e));
            }
        }
    }

    Ok(all_output)
}

/// Bi-sync all configured projects
#[tauri::command]
async fn bisync_all() -> Result<String, String> {
    let cfg = load_config();
    let mut all_output = String::new();
    let statuses = get_projects_status();

    for ps in &statuses {
        if !ps.exists_locally {
            continue;
        }
        let project = match find_project(&cfg, &ps.name) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let _permit = rclone_semaphore().acquire().await.map_err(|_| "Operation queue closed".to_string())?;
        let cfg2 = cfg.clone();
        let proj2 = project.clone();
        match tokio::task::spawn_blocking(move || bisync_project(&cfg2, &proj2)).await {
            Ok(Ok(output)) => {
                all_output.push_str(&format!("=== {} ===\n{}\n", ps.name, output));
            }
            Ok(Err(e)) => {
                all_output.push_str(&format!("=== {} ERROR ===\n{}\n", ps.name, e));
            }
            Err(e) => {
                all_output.push_str(&format!("=== {} FAILED ===\n{}\n", ps.name, e));
            }
        }
    }

    Ok(all_output)
}

/// Check all local projects, return a map of name -> check output
#[tauri::command]
async fn check_all() -> Result<std::collections::HashMap<String, String>, String> {
    let cfg = load_config();
    let mut results = std::collections::HashMap::new();
    let statuses = get_projects_status();

    for ps in &statuses {
        if !ps.exists_locally {
            continue;
        }
        let project = match find_project(&cfg, &ps.name) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let _permit = rclone_semaphore().acquire().await.map_err(|_| "Operation queue closed".to_string())?;
        let cfg2 = cfg.clone();
        let proj2 = project.clone();
        let name = ps.name.clone();
        match tokio::task::spawn_blocking(move || check_project(&cfg2, &proj2)).await {
            Ok(Ok(output)) => { results.insert(name, output); }
            Ok(Err(e)) => { results.insert(name, format!("ERROR: {}", e)); }
            Err(e) => { results.insert(name, format!("FAILED: {}", e)); }
        }
    }
    Ok(results)
}

/// Delete local project directory. Does NOT affect remote.
#[tauri::command]
fn delete_local(project_name: String) -> Result<(), String> {
    let cfg = load_config();
    let project = find_project(&cfg, &project_name)?;
    let expanded = expand_tilde(&project.local_path);
    let path = Path::new(&expanded);
    if !path.exists() {
        return Err("Local directory does not exist".into());
    }
    std::fs::remove_dir_all(path)
        .map_err(|e| format!("Failed to delete {}: {}", expanded, e))
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
    let mut cfg = load_config();

    if local_dir_has_content(&local_path) {
        return Err(format!(
            "Local directory '{}' already has content. Pulling would overwrite local files.",
            expand_tilde(&local_path)
        ));
    }

    let expanded = expand_tilde(&local_path);
    std::fs::create_dir_all(&expanded)
        .map_err(|e| format!("Failed to create directory {expanded}: {e}"))?;

    let project = Project {
        name: name.clone(),
        local_path: local_path.clone(),
        remote_path: format!("proj/{name}"),
        remote: cfg.remote.clone(), // Use the active remote at time of pull
    };

    let _permit = rclone_semaphore().acquire().await.map_err(|_| "Operation queue closed".to_string())?;
    let cfg2 = cfg.clone();
    let proj2 = project.clone();
    let output =
        tokio::task::spawn_blocking(move || sync_project(&cfg2, &proj2, "pull", false))
            .await
            .map_err(|e| e.to_string())??;

    cfg.projects.push(project);
    save_config(&cfg)?;

    Ok(output)
}

fn find_project(cfg: &AppConfig, name: &str) -> Result<Project, String> {
    if let Some(p) = cfg.projects.iter().find(|p| p.name == name) {
        return Ok(p.clone());
    }
    if let Some(local_path) = config::find_local_path(cfg, name) {
        return Ok(Project {
            name: name.to_string(),
            local_path,
            remote_path: format!("proj/{}", name),
            remote: cfg.default_remote_name(),
        });
    }
    Err(format!("Project '{}' not found", name))
}

/// Find project, but override its remote with a specific one
fn find_project_with_remote(cfg: &AppConfig, name: &str, remote: &str) -> Result<Project, String> {
    let mut p = find_project(cfg, name)?;
    if !remote.is_empty() {
        p.remote = remote.to_string();
    }
    Ok(p)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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
            push,
            pull,
            check,
            bisync,
            push_all,
            bisync_all,
            check_all,
            delete_local,
            browse_remote,
            get_remotes,
            switch_remote,
            check_local_exists,
            pull_new_project,
        ])
        .setup(|app| {
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
