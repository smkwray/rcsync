use crate::config::{self, expand_tilde, find_local_path, AppConfig, Project};
use std::path::Path;
use std::process::Command;

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
    let output = Command::new(&rclone)
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
    let output = Command::new(&rclone)
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

pub fn local_dir_has_content(path: &str) -> bool {
    let expanded = expand_tilde(path);
    let p = Path::new(&expanded);
    p.exists()
        && p.read_dir()
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
}
