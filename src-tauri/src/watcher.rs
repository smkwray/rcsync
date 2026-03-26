use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

use crate::config::{expand_tilde, load_config};

/// Payload emitted to the frontend when local files change.
#[derive(Clone, serde::Serialize)]
pub struct FileChangeEvent {
    /// Project names that had local changes.
    pub projects: Vec<String>,
}

/// Start watching all project directories. Emits "file-change" events to the frontend.
/// Returns a handle that keeps the watcher alive; drop it to stop watching.
pub fn start_watcher(app: AppHandle) -> Option<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>> {
    let cfg = load_config();

    // Build a map of watched path prefix → project name
    let mut watch_dirs: Vec<(PathBuf, String)> = Vec::new();
    let excludes: HashSet<&str> = [
        "node_modules", ".git", ".venv", ".venv312", ".tmp-validate-venv",
        "__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache", ".DS_Store",
        "target",
    ].into_iter().collect();

    // Collect project dirs from configured projects
    for p in &cfg.projects {
        let expanded = expand_tilde(&p.local_path);
        let path = PathBuf::from(&expanded);
        if path.exists() {
            watch_dirs.push((path, p.name.clone()));
        }
    }

    // Also collect from scan dirs
    for dir in &cfg.scan_dirs {
        let expanded = expand_tilde(dir);
        let dir_path = Path::new(&expanded);
        if let Ok(entries) = std::fs::read_dir(dir_path) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        if !name.starts_with('.') {
                            let path = entry.path();
                            if !watch_dirs.iter().any(|(_, n)| n == name) {
                                watch_dirs.push((path, name.to_string()));
                            }
                        }
                    }
                }
            }
        }
    }

    if watch_dirs.is_empty() {
        return None;
    }

    let (tx, rx) = mpsc::channel();

    let mut debouncer = new_debouncer(Duration::from_secs(3), tx).ok()?;

    for (path, _name) in &watch_dirs {
        let _ = debouncer.watcher().watch(path, notify::RecursiveMode::Recursive);
    }

    // Spawn a thread that reads debounced events and emits to frontend
    let dirs = watch_dirs.clone();
    std::thread::spawn(move || {
        loop {
            match rx.recv() {
                Ok(Ok(events)) => {
                    let mut changed_projects: HashSet<String> = HashSet::new();

                    for event in &events {
                        if event.kind != DebouncedEventKind::Any {
                            continue;
                        }

                        // Check if this path is in an excluded directory
                        let path_str = event.path.to_string_lossy();
                        let should_exclude = excludes.iter().any(|exc| {
                            path_str.contains(&format!("/{}/", exc))
                                || path_str.ends_with(&format!("/{}", exc))
                        });
                        if should_exclude {
                            continue;
                        }

                        // Find which project this belongs to
                        for (dir, name) in &dirs {
                            if event.path.starts_with(dir) {
                                changed_projects.insert(name.clone());
                                break;
                            }
                        }
                    }

                    if !changed_projects.is_empty() {
                        let projects: Vec<String> = changed_projects.into_iter().collect();
                        let _ = app.emit("file-change", FileChangeEvent { projects });
                    }
                }
                Ok(Err(_)) => {}
                Err(_) => break, // Channel closed, stop thread
            }
        }
    });

    Some(debouncer)
}
