<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import type { RemoteConfig, RemoteDir } from "./types";

  let { onclose }: { onclose: () => void } = $props();

  let dirs: RemoteDir[] = $state([]);
  let remotes: RemoteConfig[] = $state([]);
  let activeRemote = $state("");
  let loading = $state(true);
  let error = $state("");
  let search = $state("");
  let pullingName = $state("");
  let pullPath = $state("");
  let showPullConfirm = $state(false);
  let pullTarget: RemoteDir | null = $state(null);

  // Fuzzy match: each character of the query must appear in order in the target
  function fuzzyMatch(query: string, target: string): boolean {
    const q = query.toLowerCase();
    const t = target.toLowerCase();
    let qi = 0;
    for (let ti = 0; ti < t.length && qi < q.length; ti++) {
      if (t[ti] === q[qi]) qi++;
    }
    return qi === q.length;
  }

  let filtered = $derived(
    search.trim()
      ? dirs.filter((d) => fuzzyMatch(search.trim(), d.name))
      : dirs
  );

  async function loadRemotes() {
    remotes = await invoke<RemoteConfig[]>("get_remotes");
    const cfg = await invoke<{ remote: string }>("get_config");
    activeRemote = cfg.remote;
  }

  async function load(remoteName?: string) {
    loading = true;
    error = "";
    try {
      dirs = await invoke<RemoteDir[]>("browse_remote", { remoteName: remoteName || null });
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  async function switchRemote(name: string) {
    activeRemote = name;
    await invoke("switch_remote", { remoteName: name });
    await load(name);
  }

  async function startPull(dir: RemoteDir) {
    pullTarget = dir;
    const cfg = await invoke<{ default_pull_dir: string }>("get_config");
    pullPath = dir.local_path || `${cfg.default_pull_dir}/${dir.name}`;
    showPullConfirm = true;
  }

  async function confirmPull() {
    if (!pullTarget) return;
    const name = pullTarget.name;
    pullingName = name;
    showPullConfirm = false;

    try {
      const hasContent = await invoke<boolean>("check_local_exists", { localPath: pullPath });
      if (hasContent) {
        alert(
          `"${pullPath}" already has content on this device.\n\n` +
          `Pulling would overwrite local files. Local is always authoritative.\n\n` +
          `If you really want to pull, remove or rename the local directory first.`
        );
        return;
      }

      await invoke("pull_new_project", {
        name,
        localPath: pullPath,
      });

      await load();
    } catch (e) {
      alert(`Pull failed: ${e}`);
    } finally {
      pullingName = "";
      pullTarget = null;
    }
  }

  function cancelPull() {
    showPullConfirm = false;
    pullTarget = null;
    pullPath = "";
  }

  function statusLabel(dir: RemoteDir): string {
    if (dir.in_config && dir.has_local) return "configured";
    if (dir.in_config && !dir.has_local) return "configured (missing locally)";
    if (!dir.in_config && dir.has_local) return "found locally";
    return "remote only";
  }

  function badgeClass(dir: RemoteDir): string {
    if (dir.has_local) return "local-badge";
    if (dir.in_config) return "warn-badge";
    return "remote-badge";
  }

  async function debugScan(name: string) {
    const result = await invoke<string>("debug_scan", { name });
    alert(result);
  }

  loadRemotes().then(() => load());
</script>

<div class="overlay" role="dialog">
  <div class="panel">
    <div class="panel-header">
      <h2>Browse Remote</h2>
      <div class="header-right">
        <button onclick={() => load(activeRemote)} disabled={loading}>Refresh</button>
        <button onclick={onclose}>Close</button>
      </div>
    </div>

    <div class="search-bar">
      <input
        type="text"
        placeholder="Search projects..."
        bind:value={search}
        autofocus
      />
      <span class="search-count">
        {filtered.length}/{dirs.length}
      </span>
    </div>

    {#if remotes.length > 1}
      <div class="remote-pills">
        {#each remotes as rc}
          <button
            class="pill"
            class:active={rc.name === activeRemote}
            onclick={() => switchRemote(rc.name)}
          >{rc.name}</button>
        {/each}
      </div>
    {/if}

    <p class="subtitle">
      Projects on <code>{activeRemote}:{remotes.find(r => r.name === activeRemote)?.base_path || "proj"}/</code> — local is always authoritative.
    </p>

    <div class="panel-body">
      {#if loading}
        <div class="status">Loading remote projects...</div>
      {:else if error}
        <div class="status error">{error}</div>
      {:else if dirs.length === 0}
        <div class="status">No projects found on remote.</div>
      {:else if filtered.length === 0}
        <div class="status">No matches for "{search}"</div>
      {:else}
        <div class="dir-list">
          {#each filtered as dir (dir.name)}
            <div class="dir-row" class:local={dir.has_local} class:warn={dir.in_config && !dir.has_local}>
              <div class="dir-info">
                <span class="dir-name">{dir.name}</span>
                <span class="badge {badgeClass(dir)}">{statusLabel(dir)}</span>
                {#if dir.local_path}
                  <code class="dir-path">{dir.local_path}</code>
                {/if}
              </div>
              <div class="dir-actions">
                <button class="pull-btn" style="opacity:0.4;font-size:10px" onclick={() => debugScan(dir.name)}>?</button>
                {#if dir.has_local && dir.in_config}
                  <span class="synced-label">Ready</span>
                {:else if dir.has_local && !dir.in_config}
                  <span class="found-label">Found locally</span>
                {:else if pullingName === dir.name}
                  <span class="pulling-label">Pulling...</span>
                {:else}
                  <button class="pull-btn" onclick={() => startPull(dir)}>
                    Pull to Local
                  </button>
                {/if}
              </div>
            </div>
          {/each}
        </div>
      {/if}
    </div>

    {#if showPullConfirm && pullTarget}
      <div class="confirm-overlay">
        <div class="confirm-box">
          <div class="confirm-icon">&#9888;</div>
          <h3>Pull "{pullTarget.name}" from Drive?</h3>
          <p class="confirm-warn">
            This will download the remote project to your local device.
            Make sure this project does not already exist locally under a different path.
          </p>
          <div class="confirm-field">
            <label for="pull-path">Local path:</label>
            <input id="pull-path" type="text" bind:value={pullPath} />
          </div>
          <p class="confirm-note">
            Local is always authoritative. Once pulled, use <strong>Push</strong> to sync changes back.
            Never pull over existing local work.
          </p>
          <div class="confirm-actions">
            <button onclick={cancelPull}>Cancel</button>
            <button class="danger" onclick={confirmPull}>
              I understand — Pull from Drive
            </button>
          </div>
        </div>
      </div>
    {/if}
  </div>
</div>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 100;
  }

  .panel {
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 12px;
    width: 660px;
    max-height: 85vh;
    display: flex;
    flex-direction: column;
  }

  .panel-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 16px 20px;
    border-bottom: 1px solid var(--border);
  }

  .header-right {
    display: flex;
    gap: 8px;
  }

  h2 {
    font-size: 16px;
    font-weight: 700;
  }

  .search-bar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 12px 20px 0;
  }

  .search-bar input {
    flex: 1;
    font-family: var(--font-sans);
    font-size: 13px;
  }

  .search-count {
    font-size: 11px;
    color: var(--text-muted);
    font-family: var(--font-mono);
    white-space: nowrap;
  }

  .remote-pills {
    display: flex;
    gap: 6px;
    padding: 10px 20px 0;
  }

  .pill {
    font-size: 12px;
    padding: 4px 14px;
    border-radius: 20px;
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-muted);
    cursor: pointer;
    transition: all 0.15s;
  }

  .pill:hover { border-color: var(--accent); color: var(--text); }
  .pill.active { background: var(--accent); border-color: var(--accent); color: #fff; font-weight: 600; }

  .subtitle {
    padding: 8px 20px 0;
    font-size: 12px;
    color: var(--text-muted);
  }

  .subtitle code {
    font-family: var(--font-mono);
    color: var(--accent);
  }

  .panel-body {
    padding: 12px 20px 16px;
    overflow-y: auto;
    flex: 1;
  }

  .status {
    color: var(--text-muted);
    font-style: italic;
    padding: 20px 0;
    text-align: center;
  }

  .status.error {
    color: var(--red);
  }

  .dir-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .dir-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: 10px 14px;
    transition: border-color 0.15s;
  }

  .dir-row.local {
    border-color: var(--green-dim);
  }

  .dir-row.warn {
    border-color: var(--yellow-dim);
  }

  .dir-info {
    display: flex;
    align-items: center;
    gap: 10px;
    flex: 1;
    min-width: 0;
  }

  .dir-name {
    font-weight: 600;
    font-size: 14px;
  }

  .dir-path {
    font-family: var(--font-mono);
    font-size: 11px;
    color: var(--text-muted);
    opacity: 0.7;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .badge {
    font-size: 10px;
    padding: 2px 7px;
    border-radius: 10px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.4px;
    white-space: nowrap;
  }

  .local-badge {
    background: var(--green-dim);
    color: var(--green);
  }

  .warn-badge {
    background: var(--yellow-dim);
    color: var(--yellow);
  }

  .remote-badge {
    background: var(--bg-hover);
    color: var(--text-muted);
  }

  .synced-label {
    font-size: 12px;
    color: var(--green);
    font-weight: 500;
  }

  .found-label {
    font-size: 12px;
    color: var(--accent);
    font-weight: 500;
  }

  .pulling-label {
    font-size: 12px;
    color: var(--yellow);
    font-weight: 500;
  }

  .pull-btn {
    font-size: 12px;
    padding: 4px 12px;
  }

  .confirm-overlay {
    position: absolute;
    inset: 0;
    background: rgba(0, 0, 0, 0.7);
    display: flex;
    align-items: center;
    justify-content: center;
    border-radius: 12px;
  }

  .confirm-box {
    background: var(--bg);
    border: 1px solid var(--red);
    border-radius: 12px;
    padding: 24px;
    max-width: 440px;
    text-align: center;
  }

  .confirm-icon {
    font-size: 32px;
    color: var(--yellow);
    margin-bottom: 8px;
  }

  .confirm-box h3 {
    font-size: 15px;
    font-weight: 700;
    margin-bottom: 10px;
  }

  .confirm-warn {
    font-size: 13px;
    color: var(--text-muted);
    margin-bottom: 12px;
    line-height: 1.5;
  }

  .confirm-field {
    text-align: left;
    margin-bottom: 12px;
  }

  .confirm-field label {
    display: block;
    font-size: 11px;
    color: var(--text-muted);
    margin-bottom: 4px;
  }

  .confirm-field input {
    width: 100%;
  }

  .confirm-note {
    font-size: 11px;
    color: var(--yellow);
    background: var(--yellow-dim);
    border-radius: 6px;
    padding: 8px 10px;
    margin-bottom: 16px;
    line-height: 1.5;
    text-align: left;
  }

  .confirm-actions {
    display: flex;
    justify-content: center;
    gap: 10px;
  }
</style>
