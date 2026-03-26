export interface Project {
  name: string;
  local_path: string;
  remote_path: string;
  remote: string;
}

export interface RemoteConfig {
  name: string;
  base_path: string;
}

export interface AppConfig {
  rclone_path: string;
  remote: string;
  /** Combined excludes: defaults + user extras */
  excludes: string[];
  /** Excludes from defaults.json (read-only in UI) */
  default_excludes: string[];
  /** User-added excludes only (editable in UI) */
  extra_excludes: string[];
  remotes: RemoteConfig[];
  projects: Project[];
  scan_dirs: string[];
  default_pull_dir: string;
  auto_check_on_launch: boolean;
}

export interface SyncEvent {
  project: string;
  line: string;
  done: boolean;
  success: boolean;
}

export interface RemoteDir {
  name: string;
  has_local: boolean;
  local_path: string | null;
  in_config: boolean;
}

export interface ProjectStatus {
  name: string;
  local_path: string;
  remote_path: string;
  remote: string;
  exists_locally: boolean;
}

export type SyncMode = "push" | "pull" | "check" | "dry-run" | "bisync";
