export interface Project {
  id: string;
  name: string;
  local_path: string;
  remote_path: string;
  remote: string;
  /** Per-project rclone excludes, applied on top of the global excludes for this project only. */
  excludes?: string[];
  schedule?: Schedule | null;
}

export type IntervalUnit = "hours" | "days";

export interface IntervalSchedule {
  kind: "interval";
  every: number;
  unit: IntervalUnit;
  origin_ms: number;
}

export interface WeeklySchedule {
  kind: "weekly";
  /** Sunday = 0 through Saturday = 6. */
  weekdays: number[];
  minute: number;
}

export type Schedule = IntervalSchedule | WeeklySchedule;

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
  retired_targets: RetiredTarget[];
  retired_targets_unreadable: boolean;
  scan_dirs: string[];
  default_pull_dir: string;
  auto_check_on_launch: boolean;
  queue_scheduled_pushes: boolean;
  legacy_schedule_count: number;
  legacy_queue_policy: boolean;
  legacy_host_config_available: boolean;
  /** Parallel file transfers per rclone process. `null` means Automatic — pass
   *  no flag, leaving rclone's own config and RCLONE_TRANSFERS in charge. */
  rclone_transfers: number | null;
  config_warnings: string[];
}

export interface RetiredTarget {
  remote: string;
  remote_path: string;
  name_at_retirement: string;
  project_id_at_retirement: string;
  retired_at_ms: number;
  retired_by_device: string;
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
  ambiguous: boolean;
  project_id: string | null;
  remote: string;
  remote_path: string;
}

export interface ProjectStatus {
  id: string;
  name: string;
  local_path: string;
  remote_path: string;
  remote: string;
  exists_locally: boolean;
  schedule: Schedule | null;
  retired: boolean;
  retired_target: string | null;
}

export interface OperationSnapshot {
  project_id: string;
  project: string;
  mode: string;
  scheduled: boolean;
}

export interface ScheduleStatus {
  project_id: string;
  project: string;
  schedule: Schedule | null;
  next_run_ms: number | null;
  next_run: string | null;
  pending: boolean;
  running: boolean;
  scheduled_running: boolean;
  warning: string | null;
}

export interface ScheduledPushEvent {
  project_id: string;
  project: string;
  phase: "deferred" | "started" | "succeeded" | "failed" | "cancelled";
  error: string | null;
}

export type SyncMode = "push" | "pull" | "check" | "dry-run" | "bisync" | "resync";

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
