<script lang="ts">
  import type { Project, SyncMode } from "./types";

  let {
    project,
    running = false,
    runningMode = "",
    checkStatus = null,
    pinned = false,
    onaction,
    ondelete,
    onpin,
  }: {
    project: Project;
    running: boolean;
    runningMode: string;
    checkStatus: { time: string; synced: boolean; diffs: number } | null;
    pinned: boolean;
    onaction: (project: Project, mode: SyncMode) => void;
    ondelete: (project: Project) => void;
    onpin: (project: Project) => void;
  } = $props();

  function runLabel(): string {
    switch (runningMode) {
      case "check": return "checking";
      case "push": return "pushing";
      case "pull": return "pulling";
      case "bisync": return "bi-syncing";
      case "dry-run": return "dry run";
      default: return "syncing";
    }
  }
</script>

<div class="card" class:running class:synced={checkStatus?.synced} class:unsynced={checkStatus && !checkStatus.synced}>
  <div class="card-header">
    <div class="name-row">
      <button class="pin-btn" class:pinned title={pinned ? "Unpin" : "Pin to top"} onclick={() => onpin(project)}>
        <svg width="12" height="12" viewBox="0 0 24 24" fill={pinned ? "currentColor" : "none"} stroke="currentColor" stroke-width="2">
          <path d="M12 2L15.09 8.26L22 9.27L17 14.14L18.18 21.02L12 17.77L5.82 21.02L7 14.14L2 9.27L8.91 8.26L12 2Z"/>
        </svg>
      </button>
      <span class="name">{project.name}</span>
    </div>
    <div class="header-right">
      {#if running}
        <span class="badge running-badge">{runLabel()}</span>
      {:else if checkStatus}
        <div class="check-info">
          {#if checkStatus.synced}
            <span class="badge synced-badge">synced</span>
          {:else}
            <span class="badge unsynced-badge">{checkStatus.diffs} diff{checkStatus.diffs !== 1 ? "s" : ""}</span>
          {/if}
          <span class="check-time">{checkStatus.time}</span>
        </div>
      {/if}
      <button class="trash-btn" title="Delete local copy" onclick={() => ondelete(project)}>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>
        </svg>
      </button>
    </div>
  </div>

  <div class="paths">
    <div class="path"><span class="label">local</span> {project.local_path}</div>
    <div class="path"><span class="label">remote</span> <span class="remote-tag">{project.remote}</span>{project.remote_path}</div>
  </div>

  <div class="actions">
    <button class="primary" disabled={running} onclick={() => onaction(project, "push")}>Push</button>
    <button disabled={running} onclick={() => onaction(project, "dry-run")}>Dry Run</button>
    <button disabled={running} onclick={() => onaction(project, "check")}>Check</button>
    <button class="warn" disabled={running} onclick={() => onaction(project, "bisync")}>Bi-Sync</button>
    <button class="danger" disabled={running} onclick={() => onaction(project, "pull")}>Pull</button>
  </div>
</div>

<style>
  .card {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: 16px;
    transition: border-color 0.15s;
  }

  .card:hover { border-color: var(--accent); }
  .card.running { border-color: var(--yellow); }
  .card.synced { border-left: 3px solid var(--green); }
  .card.unsynced { border-left: 3px solid var(--yellow); }

  .card-header {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    margin-bottom: 10px;
    gap: 8px;
  }

  .name-row { display: flex; align-items: center; gap: 6px; }
  .name { font-size: 16px; font-weight: 600; }

  .pin-btn {
    padding: 2px;
    border: none;
    background: transparent;
    color: var(--text-muted);
    cursor: pointer;
    opacity: 0.3;
    transition: opacity 0.15s, color 0.15s;
  }
  .pin-btn:hover { opacity: 1; color: var(--yellow); background: transparent; border: none; }
  .pin-btn.pinned { opacity: 1; color: var(--yellow); }

  .header-right { display: flex; align-items: flex-start; gap: 6px; }

  .check-info {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 2px;
  }

  .badge {
    font-size: 10px;
    padding: 2px 8px;
    border-radius: 12px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    white-space: nowrap;
  }

  .running-badge { background: var(--yellow-dim); color: var(--yellow); }
  .synced-badge { background: var(--green-dim); color: var(--green); }
  .unsynced-badge { background: var(--yellow-dim); color: var(--yellow); }

  .check-time {
    font-size: 10px;
    color: var(--text-muted);
    font-family: var(--font-mono);
  }

  .trash-btn {
    padding: 3px 5px;
    border: none;
    background: transparent;
    color: var(--text-muted);
    cursor: pointer;
    opacity: 0.4;
    transition: opacity 0.15s, color 0.15s;
  }
  .trash-btn:hover { opacity: 1; color: var(--red); background: transparent; border: none; }

  .paths { margin-bottom: 10px; }
  .path {
    font-family: var(--font-mono);
    font-size: 12px;
    color: var(--text-muted);
    margin-bottom: 2px;
  }
  .label { display: inline-block; width: 52px; color: var(--text-muted); opacity: 0.6; }

  .remote-tag {
    display: inline-block;
    background: var(--bg-hover);
    border-radius: 3px;
    padding: 0 4px;
    margin-right: 2px;
    font-size: 10px;
    color: var(--accent);
    font-weight: 600;
  }

  .actions { display: flex; gap: 8px; flex-wrap: wrap; }
  button.warn { border-color: var(--yellow); color: var(--yellow); }
  button.warn:hover { background: var(--yellow-dim); }
</style>
