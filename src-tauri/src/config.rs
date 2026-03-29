use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub local_path: String,
    pub remote_path: String,
    /// Which remote this project syncs with. Empty/missing = first remote in list.
    #[serde(default)]
    pub remote: String,
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
    #[serde(default)]
    pub scan_dirs: Vec<String>,
    #[serde(default)]
    pub default_pull_dir: String,
    #[serde(default)]
    pub auto_check_on_launch: bool,
    /// User-added excludes (merged with defaults, not replacing them)
    #[serde(default)]
    pub extra_excludes: Vec<String>,
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
    #[serde(default = "default_scan_dirs")]
    pub scan_dirs: Vec<String>,
    /// Default directory for pulling new projects into
    #[serde(default = "default_pull_dir")]
    pub default_pull_dir: String,
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
        self.remotes.first().map(|r| r.name.clone()).unwrap_or_else(|| self.remote.clone())
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

    /// Build the full rclone remote path for a project
    pub fn remote_path_for_project(&self, project: &Project) -> String {
        let rc = self.project_remote(project);
        format!("{}:{}/{}", rc.name, rc.base_path, project.name)
    }
}

fn default_scan_dirs() -> Vec<String> {
    vec!["~/projects".into()]
}

fn default_pull_dir() -> String {
    "~/projects".into()
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            rclone_path: default_rclone_path(),
            remote: default_remote(),
            remotes: default_remotes(),
            projects: vec![],
            scan_dirs: vec![],
            default_pull_dir: String::new(),
            auto_check_on_launch: false,
            extra_excludes: vec![],
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

/// Get the machine hostname, lowercased and sanitised for use in filenames.
pub fn machine_name() -> String {
    std::env::var("RCSYNC_MACHINE")
        .or_else(|_| hostname::get()
            .map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|_| "default".into())
        .to_lowercase()
}

/// Portable user config path. Resolution order:
///   1. $RCSYNC_CONFIG (explicit override)
///   2. Machine-specific file next to the executable: `rcsync-config-{hostname}.json`
///      Falls back to the legacy `rcsync-config.json` if the host file doesn't exist
///      but the legacy one does (smooth migration for existing setups).
///   3. Fallback: platform config dir (also machine-specific)
fn config_path() -> PathBuf {
    // 1. Env var override
    if let Ok(p) = std::env::var("RCSYNC_CONFIG") {
        return PathBuf::from(p);
    }

    let host = machine_name();

    // 2. Next to executable (portable — works when app is in a synced folder)
    if let Some(dir) = exe_dir() {
        let host_file = dir.join(format!("rcsync-config-{}.json", host));
        if host_file.exists() {
            return host_file;
        }
        // Legacy fallback: existing rcsync-config.json (pre-hostname era)
        let legacy = dir.join("rcsync-config.json");
        if legacy.exists() {
            return legacy;
        }
        // New install: use host-specific file
        if !platform_config_path(&host).exists() {
            return host_file;
        }
    }

    // 3. Platform fallback
    platform_config_path(&host)
}

fn platform_config_path(host: &str) -> PathBuf {
    let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("rcsync");
    fs::create_dir_all(&p).ok();
    p.push(format!("config-{}.json", host));
    p
}

/// Load and merge: defaults (public) + user config (private) → AppConfig.
pub fn load_config() -> AppConfig {
    let defaults = load_defaults();
    let user: UserConfig = match fs::read_to_string(config_path()) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => UserConfig::default(),
    };

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
        projects: user.projects,
        scan_dirs,
        default_pull_dir,
        auto_check_on_launch: user.auto_check_on_launch,
    }
}

/// Save only the private user config. Defaults are never written by the app.
pub fn save_config(cfg: &AppConfig) -> Result<(), String> {
    let user = UserConfig {
        rclone_path: cfg.rclone_path.clone(),
        remote: cfg.remote.clone(),
        remotes: cfg.remotes.clone(),
        projects: cfg.projects.clone(),
        scan_dirs: cfg.scan_dirs.clone(),
        default_pull_dir: cfg.default_pull_dir.clone(),
        auto_check_on_launch: cfg.auto_check_on_launch,
        extra_excludes: cfg.extra_excludes.clone(),
    };
    let path = config_path();
    let json = serde_json::to_string_pretty(&user).map_err(|e| e.to_string())?;
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
