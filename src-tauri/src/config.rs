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
    /// Per-project rclone excludes, applied IN ADDITION to the global excludes
    /// but only when syncing THIS project. Each entry is an rclone filter
    /// pattern matched relative to the project root, e.g. "artifacts/**".
    /// Empty/missing = no project-specific excludes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<String>,
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
    /// Parallel file transfers per rclone process. `None` means Automatic: pass
    /// no flag at all, so rclone's own config or RCLONE_TRANSFERS still decides.
    /// Machine-level policy, deliberately not per-project — it is about this
    /// host's memory and this link, not about any one directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rclone_transfers: Option<i32>,
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
    #[serde(default = "default_scan_dirs")]
    pub scan_dirs: Vec<String>,
    /// Default directory for pulling new projects into
    #[serde(default = "default_pull_dir")]
    pub default_pull_dir: String,
    #[serde(default)]
    pub auto_check_on_launch: bool,
    #[serde(default)]
    pub rclone_transfers: Option<i32>,
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

    /// Build the full rclone remote path for a project.
    /// If `project.remote_path` is non-empty, it overrides the default `{base_path}/{name}`
    /// — allowing a project to live anywhere under its remote (e.g. "docs/important-thing"
    /// instead of "proj/important-thing"). A leading "/" in the override is stripped so
    /// the result is always relative to the remote root.
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
            format!("{}/{}", self.project_remote(project).base_path, project.name)
        } else {
            project.remote_path.trim_start_matches('/').to_string()
        }
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
            rclone_transfers: None,
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
/// Strips a trailing ".local" (macOS Bonjour) so e.g. "BMST.local" → "bmst",
/// matching the user's per-device config naming on other platforms.
pub fn machine_name() -> String {
    let raw = std::env::var("RCSYNC_MACHINE")
        .or_else(|_| hostname::get()
            .map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|_| "default".into())
        .to_lowercase();
    raw.strip_suffix(".local").unwrap_or(&raw).to_string()
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
        rclone_transfers: user.rclone_transfers,
    }
}

/// Save only the private user config. Defaults are never written by the app.
pub fn save_config(cfg: &AppConfig) -> Result<(), String> {
    validate_rclone_transfers(cfg.rclone_transfers)?;
    validate_excludes(&cfg.extra_excludes)?;
    for project in &cfg.projects {
        validate_excludes(&project.excludes)?;
    }
    let user = UserConfig {
        rclone_path: cfg.rclone_path.clone(),
        remote: cfg.remote.clone(),
        remotes: cfg.remotes.clone(),
        projects: cfg.projects.clone(),
        scan_dirs: cfg.scan_dirs.clone(),
        default_pull_dir: cfg.default_pull_dir.clone(),
        auto_check_on_launch: cfg.auto_check_on_launch,
        extra_excludes: cfg.extra_excludes.clone(),
        rclone_transfers: cfg.rclone_transfers,
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

#[cfg(test)]
mod tests {
    use super::*;

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
            name: "example".into(),
            local_path: "~/projects/example".into(),
            remote_path: remote_path.into(),
            remote: "onedrive".into(),
            excludes: vec![],
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
        assert_eq!(cfg.project_remote_path(&project("/rooted/path")), "rooted/path");
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

    /// Point config reads and writes at a scratch file. Without this a failing
    /// assertion below would let `save_config` overwrite the real user config
    /// with a default one — a test that can destroy the thing it is testing.
    /// The env var is process-wide, so every test that writes config must go
    /// through here and they must not run concurrently with each other.
    fn with_scratch_config<T>(body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = std::env::temp_dir().join(format!(
            "rcsync-cfg-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        std::env::set_var("RCSYNC_CONFIG", &path);
        let out = body();
        let _ = fs::remove_file(&path);
        std::env::remove_var("RCSYNC_CONFIG");
        out
    }

    fn cfg_with_excludes(extra: Vec<&str>, project: Vec<&str>) -> AppConfig {
        AppConfig {
            remote: "gdrive".into(),
            extra_excludes: extra.into_iter().map(str::to_string).collect(),
            projects: vec![Project {
                name: "p".into(),
                local_path: "~/p".into(),
                remote_path: String::new(),
                remote: "gdrive".into(),
                excludes: project.into_iter().map(str::to_string).collect(),
            }],
            ..Default::default()
        }
    }

    const DIVERGENT: [&str; 4] = [" leading/**", "trailing/** ", "break/**\n", "cr/**\r"];

    /// The write path is where a bad pattern must be stopped. Trimming used to
    /// happen here and in both UI forms, which silently repaired input and left
    /// the rule unreachable from every production path — so the guarantee held
    /// only for configs nobody edited by hand.
    #[test]
    fn saving_a_config_refuses_patterns_that_the_two_filter_forms_read_differently() {
        with_scratch_config(|| {
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
            // A clean set still saves, so the rule is not simply refusing everything.
            assert!(save_config(&cfg_with_excludes(vec!["node_modules/**"], vec!["artifacts/**"])).is_ok());
        });
    }

    #[test]
    fn a_blank_row_is_not_a_filter_but_a_blank_row_with_a_line_break_is_refused() {
        assert_eq!(validate_exclude_pattern("").unwrap(), None);
        assert_eq!(validate_exclude_pattern("   ").unwrap(), None);
        assert!(
            validate_exclude_pattern("  \n").is_err(),
            "the line-break rule is checked first, so this is refused rather than skipped"
        );
        assert_eq!(validate_exclude_pattern("node_modules/**").unwrap(), Some("node_modules/**"));
    }

    #[test]
    fn transfers_accepts_only_automatic_or_one_through_eight() {
        assert!(validate_rclone_transfers(None).is_ok());
        for n in 1..=8 {
            assert!(validate_rclone_transfers(Some(n)).is_ok(), "{} should be allowed", n);
        }
        for n in [0, 9, -1, 1000] {
            assert!(validate_rclone_transfers(Some(n)).is_err(), "{} should be rejected", n);
        }
    }
}
