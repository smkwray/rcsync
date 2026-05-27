use crate::config::{self, expand_tilde, find_local_path, AppConfig, Project};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::Path;
use std::process::Command;

fn rclone_command(program: &str) -> Command {
    let cmd = Command::new(program);
    #[cfg(windows)]
    let cmd = {
        use std::os::windows::process::CommandExt;
        let mut c = cmd;
        c.creation_flags(0x08000000); // CREATE_NO_WINDOW — suppress console pop-up
        c
    };
    cmd
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoteDir {
    pub name: String,
    pub has_local: bool,
    pub local_path: Option<String>,
    pub in_config: bool,
}

fn build_exclude_args(cfg: &AppConfig) -> Vec<String> {
    cfg.excludes
        .iter()
        .flat_map(|e| vec!["--exclude".to_string(), e.clone()])
        .collect()
}

fn build_exclude_set(cfg: &AppConfig) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in &cfg.excludes {
        let glob = Glob::new(pattern)
            .map_err(|e| format!("Invalid exclude pattern '{}': {}", pattern, e))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| format!("Failed to compile exclude patterns: {}", e))
}

fn check_local_path(project: &Project) -> Result<String, String> {
    let local = expand_tilde(&project.local_path);
    if !Path::new(&local).exists() {
        return Err(format!(
            "Local path does not exist: {}",
            local
        ));
    }
    Ok(local)
}

fn resolve_rclone(cfg: &AppConfig) -> String {
    let p = &cfg.rclone_path;
    if Path::new(p).is_absolute() {
        return p.clone();
    }

    #[cfg(target_os = "macos")]
    {
        for candidate in &[
            "/opt/homebrew/bin/rclone",
            "/usr/local/bin/rclone",
            "/usr/bin/rclone",
        ] {
            if Path::new(candidate).exists() {
                return candidate.to_string();
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Check common Windows install locations
        if let Ok(program_files) = std::env::var("ProgramFiles") {
            let candidate = format!("{}\\rclone\\rclone.exe", program_files);
            if Path::new(&candidate).exists() {
                return candidate;
            }
        }
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            let candidate = format!("{}\\rclone\\rclone.exe", local_app_data);
            if Path::new(&candidate).exists() {
                return candidate;
            }
        }
        // Also check scoop and chocolatey common paths via HOME
        if let Some(home) = dirs::home_dir() {
            let scoop = home.join("scoop\\shims\\rclone.exe");
            if scoop.exists() {
                return scoop.to_string_lossy().to_string();
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        for candidate in &[
            "/usr/local/bin/rclone",
            "/usr/bin/rclone",
        ] {
            if Path::new(candidate).exists() {
                return candidate.to_string();
            }
        }
    }

    p.clone()
}

/// Run rclone and return combined stdout+stderr as a string.
fn run_rclone(cfg: &AppConfig, args: &[String]) -> Result<(String, i32), String> {
    let rclone = resolve_rclone(cfg);
    let output = rclone_command(&rclone)
        .args(args)
        .output()
        .map_err(|e| format!("Failed to start rclone at '{}': {}", rclone, e))?;

    let mut result = String::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stdout.lines().chain(stderr.lines()) {
        if !line.is_empty() {
            result.push_str(line);
            result.push('\n');
        }
    }
    let code = output.status.code().unwrap_or(-1);
    Ok((result, code))
}

pub fn sync_project(
    cfg: &AppConfig,
    project: &Project,
    mode: &str,
    dry_run: bool,
) -> Result<String, String> {
    let local = if mode == "pull" {
        expand_tilde(&project.local_path)
    } else {
        check_local_path(project)?
    };
    let remote = cfg.remote_path_for_project(project);

    // Safety: refuse to push an empty local directory — rclone sync would
    // wipe every file on the remote to match the empty source.
    if mode == "push" && !local_dir_has_syncable_content(cfg, &local)? {
        return Err(format!(
            "Refusing to push: local directory '{}' has no syncable contents after excludes. \
             This would delete remote files for '{}'.",
            local, project.name
        ));
    }

    let (src, dst) = match mode {
        "pull" => (remote, local),
        _ => (local, remote),
    };

    let mut args = vec!["sync".to_string(), src, dst];
    args.extend(build_exclude_args(cfg));
    args.push("-v".to_string());
    if dry_run {
        args.push("--dry-run".to_string());
    }

    let (output, code) = run_rclone(cfg, &args)?;
    if code == 0 {
        Ok(output)
    } else {
        Err(format!("{}\nExited with code {}", output, code))
    }
}

pub fn bisync_project(cfg: &AppConfig, project: &Project) -> Result<String, String> {
    let local = check_local_path(project)?;

    // Safety: refuse to bisync an empty local directory — could propagate
    // deletions to the remote.
    if !local_dir_has_syncable_content(cfg, &local)? {
        return Err(format!(
            "Refusing to bi-sync: local directory '{}' has no syncable contents after excludes. \
             This could delete remote files for '{}'.",
            local, project.name
        ));
    }

    let remote = cfg.remote_path_for_project(project);

    let mut args = vec!["bisync".to_string(), local, remote];
    args.extend(build_exclude_args(cfg));
    args.push("-v".to_string());

    let (output, code) = run_rclone(cfg, &args)?;
    if code == 0 {
        Ok(output)
    } else {
        Err(format!("{}\nExited with code {}", output, code))
    }
}

pub fn check_project(cfg: &AppConfig, project: &Project) -> Result<String, String> {
    let local = check_local_path(project)?;
    let remote = cfg.remote_path_for_project(project);

    let mut args = vec![
        "check".to_string(),
        local,
        remote,
        "--combined".to_string(),
        "-".to_string(),
    ];
    args.extend(build_exclude_args(cfg));

    let (raw_output, code) = run_rclone(cfg, &args)?;

    // Filter: only show differences (* changed, + remote only, - local only) and summary lines
    let mut result = String::new();
    let mut diff_count = 0u32;
    let mut match_count = 0u32;
    for line in raw_output.lines() {
        if line.starts_with("= ") {
            match_count += 1;
        } else if line.starts_with("* ") || line.starts_with("+ ") || line.starts_with("- ") {
            diff_count += 1;
            let label = match &line[..2] {
                "* " => "CHANGED",
                "+ " => "REMOTE ONLY",
                "- " => "LOCAL ONLY",
                _ => "",
            };
            result.push_str(&format!("[{}] {}\n", label, &line[2..]));
        } else if line.contains("NOTICE:") {
            // Show summary lines from rclone
            if let Some(msg) = line.split("NOTICE: ").nth(1) {
                result.push_str(&format!("{}\n", msg));
            }
        }
    }

    if diff_count == 0 {
        result.push_str(&format!("All {} files match.\n", match_count));
    } else {
        result.push_str(&format!("{} differences, {} matching.\n", diff_count, match_count));
    }

    // code 0 = all match, code 1 = differences found (both normal)
    // code 2+ = errors during check — still return results, but append a warning
    if code > 1 {
        result.push_str(&format!(
            "(rclone exited with code {} — some files may not have been checked)\n",
            code
        ));
    }
    Ok(result)
}

/// List remote projects. If `remote_name` is provided, use that remote; otherwise use active.
pub fn list_remote(cfg: &AppConfig, remote_name: Option<&str>) -> Result<Vec<RemoteDir>, String> {
    let rclone = resolve_rclone(cfg);
    let rc = if let Some(name) = remote_name {
        cfg.remotes.iter().find(|r| r.name == name).cloned()
            .unwrap_or(config::RemoteConfig { name: name.to_string(), base_path: "proj".into() })
    } else {
        cfg.active_remote()
    };
    let output = rclone_command(&rclone)
        .args(["lsd", &format!("{}:{}", rc.name, rc.base_path)])
        .output()
        .map_err(|e| format!("Failed to run rclone lsd: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("rclone lsd failed: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let config_names: std::collections::HashSet<&str> =
        cfg.projects.iter().map(|p| p.name.as_str()).collect();

    Ok(stdout
        .lines()
        .filter_map(|line| {
            let name = line.split_whitespace().last()?;
            let local_path = find_local_path(cfg, name);
            Some(RemoteDir {
                name: name.to_string(),
                has_local: local_path.is_some(),
                local_path,
                in_config: config_names.contains(name),
            })
        })
        .collect())
}

/// OS-generated files that rclone excludes by default.  A directory
/// containing *only* these is effectively empty after sync, so we must
/// not count them as real content.
fn is_os_junk(name: &str) -> bool {
    name == ".DS_Store"
        || name == "Thumbs.db"
        || name == "desktop.ini"
        || name.starts_with("._")
}

pub fn local_dir_has_content(path: &str) -> bool {
    let expanded = expand_tilde(path);
    let p = Path::new(&expanded);
    p.exists()
        && p.read_dir()
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .any(|e| !is_os_junk(&e.file_name().to_string_lossy()))
            })
            .unwrap_or(false)
}

fn normalize_for_match(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn has_syncable_entries(root: &Path, dir: &Path, excludes: &GlobSet) -> Result<bool, String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("Failed to read {}: {}", dir.display(), e))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read {}: {}", dir.display(), e))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if is_os_junk(&name) {
            continue;
        }

        let rel = path
            .strip_prefix(root)
            .map_err(|e| format!("Failed to normalize {}: {}", path.display(), e))?;
        let rel_str = normalize_for_match(rel);
        if excludes.is_match(&rel_str) {
            continue;
        }

        if path.is_file() || path.is_symlink() {
            return Ok(true);
        }

        if path.is_dir() && has_syncable_entries(root, &path, excludes)? {
            return Ok(true);
        }
    }

    Ok(false)
}

pub fn local_dir_has_syncable_content(cfg: &AppConfig, path: &str) -> Result<bool, String> {
    let expanded = expand_tilde(path);
    let root = Path::new(&expanded);
    if !root.exists() || !root.is_dir() {
        return Ok(false);
    }

    let excludes = build_exclude_set(cfg)?;
    has_syncable_entries(root, root, &excludes)
}

#[cfg(test)]
mod tests {
    use super::local_dir_has_syncable_content;
    use crate::config::{AppConfig, RemoteConfig};
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "rcsync-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn test_cfg(excludes: Vec<&str>) -> AppConfig {
        AppConfig {
            rclone_path: "rclone".into(),
            remote: "gdrive".into(),
            remotes: vec![RemoteConfig {
                name: "gdrive".into(),
                base_path: "proj".into(),
            }],
            excludes: excludes.into_iter().map(str::to_string).collect(),
            default_excludes: vec![],
            extra_excludes: vec![],
            projects: vec![],
            scan_dirs: vec![],
            default_pull_dir: String::new(),
            auto_check_on_launch: false,
        }
    }

    #[test]
    fn ignores_only_os_junk() {
        let dir = temp_dir("junk");
        fs::write(dir.join(".DS_Store"), "").unwrap();

        let has_content = local_dir_has_syncable_content(&test_cfg(vec![]), dir.to_str().unwrap()).unwrap();

        fs::remove_dir_all(&dir).unwrap();
        assert!(!has_content);
    }

    #[test]
    fn ignores_only_excluded_files() {
        let dir = temp_dir("excluded");
        fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        fs::write(dir.join("node_modules/pkg/index.js"), "x").unwrap();

        let has_content = local_dir_has_syncable_content(
            &test_cfg(vec!["node_modules/**"]),
            dir.to_str().unwrap(),
        )
        .unwrap();

        fs::remove_dir_all(&dir).unwrap();
        assert!(!has_content);
    }

    #[test]
    fn finds_nested_non_excluded_files() {
        let dir = temp_dir("nested");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.ts"), "export {};\n").unwrap();

        let has_content = local_dir_has_syncable_content(
            &test_cfg(vec!["node_modules/**"]),
            dir.to_str().unwrap(),
        )
        .unwrap();

        fs::remove_dir_all(&dir).unwrap();
        assert!(has_content);
    }
}
