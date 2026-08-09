<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import type { CheckStatus, Project, RemoteConfig, SyncMode } from "./types";

  let {
    project,
    running = false,
    runningMode = "",
    cancelling = false,
    checkStatus = null,
    pinned = false,
    onaction,
    oncancel,
    ondelete,
    onpin,
    onupdated,
  }: {
    project: Project;
    running: boolean;
    runningMode: string;
    cancelling: boolean;
    checkStatus: CheckStatus | null;
    pinned: boolean;
    onaction: (project: Project, mode: SyncMode) => void;
    oncancel: (project: Project) => void;
    ondelete: (project: Project) => void;
    onpin: (project: Project) => void;
    onupdated?: () => void;
  } = $props();

  /** The action row, in display order. While an operation runs, the button that
   *  started it becomes the Cancel control and the rest gray out — so stopping
   *  is a zero-travel click from wherever it was started, and it is never
   *  ambiguous which project is being stopped. */
  const ACTIONS: { mode: SyncMode; label: string; cls: string }[] = [
    { mode: "push", label: "Push", cls: "primary" },
    { mode: "dry-run", label: "Dry Run", cls: "" },
    { mode: "check", label: "Check", cls: "" },
    { mode: "bisync", label: "Bi-Sync", cls: "warn" },
    { mode: "pull", label: "Pull", cls: "danger" },
  ];

  function openFolder() {
    invoke("open_folder", { localPath: project.local_path });
  }

  // --- Project-specific ignore patterns ---
  let showIgnores = $state(false);
  let ignoreText = $state("");
  let savingIgnores = $state(false);
  let ignoreError = $state("");
  let ignoreTextarea: HTMLTextAreaElement | undefined = $state(undefined);

  $effect(() => { if (showIgnores) ignoreTextarea?.focus(); });

  async function openIgnores() {
    ignoreError = "";
    try {
      const patterns = await invoke<string[]>("get_project_excludes", { projectName: project.name });
      ignoreText = patterns.join("\n");
    } catch (e) {
      ignoreText = "";
      ignoreError = `Could not load existing patterns: ${e}`;
    }
    showIgnores = true;
  }

  function closeIgnores() {
    showIgnores = false;
  }

  async function saveIgnores() {
    savingIgnores = true;
    ignoreError = "";
    const patterns = ignoreText
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => l.length > 0);
    try {
      await invoke("set_project_excludes", { projectName: project.name, excludes: patterns });
      showIgnores = false;
    } catch (e) {
      ignoreError = `${e}`;
    } finally {
      savingIgnores = false;
    }
  }

  function handleIgnoreKey(e: KeyboardEvent) {
    if (e.key === "Escape") { e.preventDefault(); closeIgnores(); }
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") { e.preventDefault(); saveIgnores(); }
  }

  // --- Per-project remote ---
  let showRemote = $state(false);
  let remoteChoice = $state("");
  let remotePathInput = $state("");
  let savingRemote = $state(false);
  let remoteError = $state("");
  let availableRemotes: RemoteConfig[] = $state([]);

  async function openRemoteEditor() {
    remoteError = "";
    remoteChoice = project.remote;
    remotePathInput = project.remote_path;
    try {
      availableRemotes = await invoke<RemoteConfig[]>("get_remotes");
    } catch (e) {
      availableRemotes = [];
      remoteError = `Could not load remotes: ${e}`;
    }
    showRemote = true;
  }

  function closeRemoteEditor() {
    showRemote = false;
  }

  async function saveRemote() {
    savingRemote = true;
    remoteError = "";
    try {
      await invoke("set_project_remote", {
        projectName: project.name,
        remote: remoteChoice,
        remotePath: remotePathInput.trim(),
      });
      showRemote = false;
      onupdated?.();
    } catch (e) {
      remoteError = `${e}`;
    } finally {
      savingRemote = false;
    }
  }

  function handleRemoteKey(e: KeyboardEvent) {
    if (e.key === "Escape") { e.preventDefault(); closeRemoteEditor(); }
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") { e.preventDefault(); saveRemote(); }
  }

  function runLabel(): string {
    if (cancelling) return "stopping";
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

<div class="card" class:running
  class:synced={checkStatus?.state === "synced"}
  class:unsynced={checkStatus?.state === "diffs"}
  class:modified={checkStatus?.state === "modified"}
  class:unknown={checkStatus?.state === "unknown"}>
  <div class="card-header">
    <div class="name-row">
      <button class="pin-btn" class:pinned title={pinned ? "Unpin" : "Pin to top"} onclick={() => onpin(project)}>
        <svg width="12" height="12" viewBox="0 0 24 24" fill={pinned ? "currentColor" : "none"} stroke="currentColor" stroke-width="2">
          <path d="M12 2L15.09 8.26L22 9.27L17 14.14L18.18 21.02L12 17.77L5.82 21.02L7 14.14L2 9.27L8.91 8.26L12 2Z"/>
        </svg>
      </button>
      <span class="name">{project.name}</span>
      <button class="icon-btn name-folder-btn" title="Open in file manager" onclick={openFolder}>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>
        </svg>
      </button>
    </div>
    <div class="header-right">
      {#if running}
        <span class="badge running-badge">{runLabel()}</span>
      {:else if checkStatus}
        <div class="check-info">
          {#if checkStatus.state === "synced"}
            <span class="badge synced-badge">synced</span>
          {:else if checkStatus.state === "modified"}
            <span class="badge modified-badge">modified</span>
          {:else if checkStatus.state === "unknown"}
            <!-- The last operation failed, so any previous verdict is withdrawn
                 rather than left standing. -->
            <span class="badge unknown-badge" title="The last operation failed — run Check">unknown</span>
          {:else}
            <span class="badge unsynced-badge">{checkStatus.diffs} diff{checkStatus.diffs !== 1 ? "s" : ""}</span>
          {/if}
          <span class="check-time">{checkStatus.time}</span>
        </div>
      {/if}
      <button class="icon-btn" title="Project-specific ignores" onclick={openIgnores}>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <polygon points="22 3 2 3 10 12.46 10 19 14 21 14 12.46 22 3"/>
        </svg>
      </button>
      <button class="trash-btn" title="Delete local copy" onclick={() => ondelete(project)}>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>
        </svg>
      </button>
    </div>
  </div>

  <div class="paths">
    <div class="path"><span class="label">local</span> {project.local_path}</div>
    <div class="path">
      <span class="label">remote</span> <span class="remote-tag">{project.remote}</span>{project.remote_path}
      <button class="edit-remote-btn" title="Change remote for this project" onclick={openRemoteEditor}>
        <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12 20h9"/><path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4 12.5-12.5z"/>
        </svg>
      </button>
    </div>
  </div>

  <div class="actions">
    {#each ACTIONS as { mode, label, cls } (mode)}
      {#if running && runningMode === mode}
        <button class="cancel-op" disabled={cancelling} title="Stop this {label.toLowerCase()}"
          onclick={() => oncancel(project)}>
          {cancelling ? "Stopping…" : "Cancel"}
        </button>
      {:else}
        <button class={cls} disabled={running} onclick={() => onaction(project, mode)}>{label}</button>
      {/if}
    {/each}
  </div>
</div>

{#if showIgnores}
  <div class="overlay" role="dialog" aria-modal="true" tabindex="-1" onkeydown={handleIgnoreKey}>
    <div class="ignore-dialog">
      <h3>Ignore patterns — {project.name}</h3>
      <p class="hint">
        Patterns excluded for <strong>this project only</strong>, one per line (e.g.
        <code>artifacts/**</code>). Applied on top of the global excludes when pushing,
        pulling, checking, or bi-syncing. Paths are relative to the project root.
      </p>
      <textarea
        bind:this={ignoreTextarea}
        bind:value={ignoreText}
        spellcheck="false"
        autocapitalize="off"
        {...{ autocorrect: "off" }}
        placeholder={"artifacts/**\ndata/cache/**"}
        rows="7"
      ></textarea>
      {#if ignoreError}<p class="err">{ignoreError}</p>{/if}
      <div class="dialog-actions">
        <button class="cancel-btn" onclick={closeIgnores} disabled={savingIgnores}>Cancel</button>
        <button class="primary" onclick={saveIgnores} disabled={savingIgnores}>
          {savingIgnores ? "Saving…" : "Save"}
        </button>
      </div>
    </div>
  </div>
{/if}

{#if showRemote}
  <div class="overlay" role="dialog" aria-modal="true" tabindex="-1" onkeydown={handleRemoteKey}>
    <div class="ignore-dialog">
      <h3>Remote — {project.name}</h3>
      <p class="hint">
        Which remote this project syncs to, and the path on that remote. Changing this only
        re-targets future syncs — it does not move or delete anything already uploaded elsewhere.
      </p>
      <label class="field">
        <span>Remote</span>
        <select bind:value={remoteChoice}>
          {#each availableRemotes as r}
            <option value={r.name}>{r.name}</option>
          {/each}
        </select>
      </label>
      <label class="field">
        <span>Path on remote</span>
        <input type="text" bind:value={remotePathInput} spellcheck="false" autocorrect="off" autocapitalize="off" placeholder={"proj/" + project.name} />
      </label>
      {#if remoteError}<p class="err">{remoteError}</p>{/if}
      <div class="dialog-actions">
        <button class="cancel-btn" onclick={closeRemoteEditor} disabled={savingRemote}>Cancel</button>
        <button class="primary" onclick={saveRemote} disabled={savingRemote || !remoteChoice}>
          {savingRemote ? "Saving…" : "Save"}
        </button>
      </div>
    </div>
  </div>
{/if}

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
  .card.modified { border-left: 3px solid var(--orange, var(--yellow)); }
  .card.unknown { border-left: 3px solid var(--text-muted); }

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
  .modified-badge { background: var(--yellow-dim); color: var(--yellow); font-style: italic; }
  .unknown-badge { background: var(--bg-hover); color: var(--text-muted); }

  .check-time {
    font-size: 10px;
    color: var(--text-muted);
    font-family: var(--font-mono);
  }

  .icon-btn, .trash-btn {
    padding: 3px 5px;
    border: none;
    background: transparent;
    color: var(--text-muted);
    cursor: pointer;
    opacity: 0.4;
    transition: opacity 0.15s, color 0.15s;
  }
  .icon-btn:hover { opacity: 1; color: var(--accent); background: transparent; border: none; }
  .trash-btn:hover { opacity: 1; color: var(--red); background: transparent; border: none; }

  /* Folder icon relocated next to the project name */
  .name-folder-btn { padding: 2px 3px; opacity: 0.35; }

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

  /* Filled rather than outlined, so the one live control stands out against
     the four grayed-out siblings around it. */
  .cancel-op {
    border-color: var(--red);
    background: var(--red-dim);
    color: var(--red);
    font-weight: 600;
  }
  .cancel-op:hover:not(:disabled) { background: var(--red); color: var(--bg); }

  /* Project-specific ignores dialog */
  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 300;
  }
  .ignore-dialog {
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 22px;
    max-width: 460px;
    width: 90%;
  }
  .ignore-dialog h3 { font-size: 15px; font-weight: 700; margin-bottom: 8px; }
  .hint {
    font-size: 12px;
    color: var(--text-muted);
    line-height: 1.6;
    margin-bottom: 12px;
  }
  .hint code {
    font-family: var(--font-mono);
    background: var(--bg-hover);
    padding: 0 4px;
    border-radius: 3px;
    color: var(--accent);
  }
  .ignore-dialog textarea {
    width: 100%;
    box-sizing: border-box;
    font-family: var(--font-mono);
    font-size: 12px;
    line-height: 1.5;
    color: var(--text);
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 8px;
    resize: vertical;
  }
  .ignore-dialog textarea:focus { outline: none; border-color: var(--accent); }
  .err { color: var(--red); font-size: 12px; margin-top: 8px; }
  .dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 14px;
  }
  .cancel-btn { font-weight: 600; }

  /* Inline pencil for the remote line */
  .edit-remote-btn {
    display: inline-flex;
    align-items: center;
    padding: 1px 3px;
    margin-left: 5px;
    border: none;
    background: transparent;
    color: var(--text-muted);
    cursor: pointer;
    opacity: 0.35;
    vertical-align: middle;
    transition: opacity 0.15s, color 0.15s;
  }
  .edit-remote-btn:hover { opacity: 1; color: var(--accent); background: transparent; border: none; }

  /* Remote dialog form fields */
  .field { display: flex; flex-direction: column; gap: 4px; margin-bottom: 12px; }
  .field > span {
    font-size: 11px;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }
  .ignore-dialog select,
  .ignore-dialog input[type="text"] {
    width: 100%;
    box-sizing: border-box;
    font-family: var(--font-mono);
    font-size: 12px;
    color: var(--text);
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 7px 8px;
  }
  .ignore-dialog select:focus,
  .ignore-dialog input[type="text"]:focus { outline: none; border-color: var(--accent); }
</style>
