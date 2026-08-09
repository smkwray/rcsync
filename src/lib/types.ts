export interface Project {
  name: string;
  local_path: string;
  remote_path: string;
  remote: string;
  /** Per-project rclone excludes, applied on top of the global excludes for this project only. */
  excludes?: string[];
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
  /** Parallel file transfers per rclone process. `null` means Automatic — pass
   *  no flag, leaving rclone's own config and RCLONE_TRANSFERS in charge. */
  rclone_transfers: number | null;
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

/** The modes that have an "all projects" variant. A subset of SyncMode, so a
 *  bulk run can be logged and cancelled through the same per-project paths. */
export type BulkMode = Extract<SyncMode, "push" | "check" | "bisync">;

/** The backend's check verdict. Typed on purpose: sync state used to be inferred
 *  by searching rclone's human output for "N differences", which quietly turned
 *  every failure that lacked the phrase into "synced". */
export interface CheckOutcome {
  synced: boolean;
  differences: number;
  matches: number;
  details: string;
}

/** What a card shows for its last known state. `unknown` exists so a failed or
 *  interrupted operation can retract a stale verdict instead of leaving the
 *  previous, now-unfounded one on screen. */
export type SyncState = "synced" | "diffs" | "modified" | "unknown";

export interface CheckStatus {
  time: string;
  state: SyncState;
  diffs: number;
}
