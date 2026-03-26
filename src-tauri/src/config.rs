use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub local_path: String,
    pub remote_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub name: String,
    /// Base path on this remote, e.g. "proj" means projects are at remote:proj/name
    pub base_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub rclone_path: String,
    /// Active remote name (must match a name in `remotes`)
    pub remote: String,
    /// Available remotes
    #[serde(default = "default_remotes")]
    pub remotes: Vec<RemoteConfig>,
    pub excludes: Vec<String>,
    pub projects: Vec<Project>,
    #[serde(default = "default_scan_dirs")]
    pub scan_dirs: Vec<String>,
    #[serde(default)]
    pub auto_check_on_launch: bool,
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

    /// Build the full rclone remote path for a project name
    pub fn remote_path_for(&self, project_name: &str) -> String {
        let rc = self.active_remote();
        format!("{}:{}/{}", rc.name, rc.base_path, project_name)
    }
}

fn default_scan_dirs() -> Vec<String> {
    vec!["~/projects".into()]
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            rclone_path: "rclone".into(),
            remote: "gdrive".into(),
            excludes: vec![
                "node_modules/**".into(),
                ".git/**".into(),
                ".venv/**".into(),
                ".venv312/**".into(),
                ".tmp-validate-venv/**".into(),
                "__pycache__/**".into(),
                ".pytest_cache/**".into(),
                ".mypy_cache/**".into(),
                ".DS_Store".into(),
                "._*".into(),
            ],
            projects: vec![],
            remotes: default_remotes(),
            scan_dirs: default_scan_dirs(),
            auto_check_on_launch: false,
        }
    }
}

/// Portable config: lives next to the executable (or in the project dir via
/// the RCSYNC_CONFIG env var) so it syncs between devices with Syncthing.
/// Resolution order:
///   1. $RCSYNC_CONFIG (explicit override)
///   2. Next to the running executable (portable — works when app is in a synced folder)
///   3. Fallback: platform config dir
fn config_path() -> PathBuf {
    // 1. Env var override
    if let Ok(p) = std::env::var("RCSYNC_CONFIG") {
        return PathBuf::from(p);
    }

    // 2. Next to executable
    if let Ok(exe) = std::env::current_exe() {
        // On macOS the exe is inside .app/Contents/MacOS/ — walk up to the .app's parent
        let mut dir = exe
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

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

        let portable = dir.join("rcsync-config.json");
        if portable.exists() || !platform_config_path().exists() {
            return portable;
        }
    }

    // 3. Platform fallback
    platform_config_path()
}

fn platform_config_path() -> PathBuf {
    let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("rcsync");
    fs::create_dir_all(&p).ok();
    p.push("config.json");
    p
}

pub fn load_config() -> AppConfig {
    let path = config_path();
    match fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => {
            let cfg = AppConfig::default();
            save_config(&cfg).ok();
            cfg
        }
    }
}

pub fn save_config(cfg: &AppConfig) -> Result<(), String> {
    let path = config_path();
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| e.to_string())
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

    // Scan directories for a matching folder
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
