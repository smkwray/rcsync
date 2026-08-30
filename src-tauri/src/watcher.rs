use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

use crate::config::{expand_tilde, load_config, project_id_for_fields};

/// Payload emitted to the frontend when local files change.
#[derive(Clone, serde::Serialize)]
pub struct FileChangeEvent {
    /// Projects that had local changes. The name is a display label; the ID is
    /// the join key for Dashboard state and survives a rename.
    pub projects: Vec<ChangedProject>,
}

#[derive(Clone, serde::Serialize)]
pub struct ChangedProject {
    pub project_id: String,
    pub project: String,
}

/// Start watching all project directories. Emits "file-change" events to the frontend.
/// Returns a handle that keeps the watcher alive; drop it to stop watching.
///
/// Uses NonRecursive mode so only direct project children generate events. On macOS,
/// `notify` must use its default FSEvents backend: kqueue can promote a directory watch
/// to recursive after directory churn and consume one file descriptor per descendant.
pub fn start_watcher(
    app: AppHandle,
) -> Option<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>> {
    let cfg = load_config();

    // Build a map of watched path prefix → immutable project ID + display name.
    let mut watch_dirs: Vec<(PathBuf, String, String)> = Vec::new();

    // Collect project dirs from configured projects
    for p in &cfg.projects {
        let expanded = expand_tilde(&p.local_path);
        let path = PathBuf::from(&expanded);
        if path.exists() && path.is_dir() {
            watch_dirs.push((path, p.id.clone(), p.name.clone()));
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
                            if !watch_dirs.iter().any(|(_, _, n)| n == name) {
                                let project_id = project_id_for_fields(
                                    name,
                                    &format!("{}/{}", dir, name),
                                    "",
                                    &cfg.default_remote_name(),
                                );
                                watch_dirs.push((path, project_id, name.to_string()));
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

    // Watch each project dir non-recursively to limit status invalidations.
    for (path, _, _) in &watch_dirs {
        let _ = debouncer
            .watcher()
            .watch(path, notify::RecursiveMode::NonRecursive);
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

                        // Find which project this belongs to
                        for (dir, project_id, _) in &dirs {
                            if event.path.starts_with(dir) {
                                changed_projects.insert(project_id.clone());
                                break;
                            }
                        }
                    }

                    if !changed_projects.is_empty() {
                        let mut projects: Vec<ChangedProject> = changed_projects
                            .into_iter()
                            .filter_map(|project_id| {
                                dirs.iter().find(|(_, id, _)| id == &project_id).map(
                                    |(_, _, project)| ChangedProject {
                                        project_id,
                                        project: project.clone(),
                                    },
                                )
                            })
                            .collect();
                        projects.sort_by(|a, b| a.project_id.cmp(&b.project_id));
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

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use notify::Watcher;

    #[test]
    fn recommended_watcher_uses_fsevents_on_macos() {
        assert_eq!(
            <notify::RecommendedWatcher as Watcher>::kind(),
            notify::WatcherKind::Fsevent
        );
    }
}
