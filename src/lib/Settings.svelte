<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { enable, disable, isEnabled } from "@tauri-apps/plugin-autostart";
  import type { AppConfig, Project } from "./types";

  let {
    config,
    onclose,
  }: {
    config: AppConfig;
    onclose: () => void;
  } = $props();

  let editConfig: AppConfig = $state(JSON.parse(JSON.stringify(config)));
  let newExclude = $state("");
  let newScanDir = $state("");
  let saving = $state(false);
  let newProject: Project = $state({ name: "", local_path: "", remote_path: "" });
  let startAtLogin = $state(false);

  // Load current autostart state
  isEnabled().then((v) => { startAtLogin = v; }).catch(() => {});

  async function toggleAutostart() {
    try {
      if (startAtLogin) {
        await enable();
      } else {
        await disable();
      }
    } catch (e) {
      alert(`Failed to update login item: ${e}`);
      startAtLogin = !startAtLogin; // revert
    }
  }

  async function save() {
    saving = true;
    try {
      await invoke("update_config", { cfg: editConfig });
      onclose();
    } catch (e) {
      alert(`Failed to save: ${e}`);
    } finally {
      saving = false;
    }
  }

  function addExclude() {
    const val = newExclude.trim();
    if (val && !editConfig.excludes.includes(val) && !editConfig.extra_excludes.includes(val)) {
      editConfig.extra_excludes = [...editConfig.extra_excludes, val];
      editConfig.excludes = [...editConfig.excludes, val];
      newExclude = "";
    }
  }

  function removeExclude(idx: number) {
    const val = editConfig.extra_excludes[idx];
    editConfig.extra_excludes = editConfig.extra_excludes.filter((_, i) => i !== idx);
    editConfig.excludes = editConfig.excludes.filter((e) => e !== val);
  }

  function addProject() {
    if (newProject.name && newProject.local_path && newProject.remote_path) {
      editConfig.projects = [...editConfig.projects, { ...newProject }];
      newProject = { name: "", local_path: "", remote_path: "" };
    }
  }

  function removeProject(idx: number) {
    const name = editConfig.projects[idx].name;
    if (confirm(`Remove project "${name}"?`)) {
      editConfig.projects = editConfig.projects.filter((_, i) => i !== idx);
    }
  }

  function addScanDir() {
    const val = newScanDir.trim();
    if (val && !editConfig.scan_dirs.includes(val)) {
      editConfig.scan_dirs = [...editConfig.scan_dirs, val];
      newScanDir = "";
    }
  }

  function removeScanDir(idx: number) {
    editConfig.scan_dirs = editConfig.scan_dirs.filter((_, i) => i !== idx);
  }
</script>

<div class="settings-overlay" role="dialog">
  <div class="settings-panel">
    <div class="settings-header">
      <h2>Settings</h2>
      <button onclick={onclose}>Close</button>
    </div>

    <div class="settings-body">
      <section>
        <h3>General</h3>
        <div class="field">
          <label>rclone path</label>
          <input type="text" bind:value={editConfig.rclone_path} />
        </div>
        <div class="field">
          <label>Remote name</label>
          <input type="text" bind:value={editConfig.remote} />
        </div>
        <label class="checkbox-field">
          <input type="checkbox" bind:checked={editConfig.auto_check_on_launch} />
          Auto-check all projects on launch
        </label>
        <label class="checkbox-field">
          <input type="checkbox" bind:checked={startAtLogin} onchange={toggleAutostart} />
          Start at login
        </label>
      </section>

      <section>
        <h3>Scan Directories</h3>
        <p class="section-desc">
          Local directories to scan when detecting whether a remote project exists on this device.
        </p>
        <div class="scan-list">
          {#each editConfig.scan_dirs as dir, i}
            <div class="scan-item">
              <code>{dir}</code>
              <button class="small danger" onclick={() => removeScanDir(i)}>x</button>
            </div>
          {/each}
        </div>
        <div class="add-row">
          <input
            type="text"
            bind:value={newScanDir}
            placeholder="e.g. ~/proj"
            onkeydown={(e) => e.key === "Enter" && addScanDir()}
          />
          <button onclick={addScanDir}>Add</button>
        </div>
      </section>

      <section>
        <h3>Excludes</h3>
        <p class="section-desc">
          Default excludes come from defaults.json and are shared across devices. Add your own below.
        </p>
        <div class="exclude-list">
          {#each editConfig.default_excludes as exc}
            <div class="exclude-item default">
              <code>{exc}</code>
              <span class="badge">default</span>
            </div>
          {/each}
          {#each editConfig.extra_excludes as exc, i}
            <div class="exclude-item">
              <code>{exc}</code>
              <button class="small danger" onclick={() => removeExclude(i)}>x</button>
            </div>
          {/each}
        </div>
        <div class="add-row">
          <input
            type="text"
            bind:value={newExclude}
            placeholder="e.g. dist/**"
            onkeydown={(e) => e.key === "Enter" && addExclude()}
          />
          <button onclick={addExclude}>Add</button>
        </div>
      </section>

      <section>
        <h3>Projects</h3>
        <div class="project-list">
          {#each editConfig.projects as proj, i}
            <div class="project-row">
              <span class="proj-name">{proj.name}</span>
              <code class="proj-path">{proj.local_path}</code>
              <button class="small danger" onclick={() => removeProject(i)}>x</button>
            </div>
          {/each}
        </div>
        <div class="add-project">
          <input type="text" bind:value={newProject.name} placeholder="name" />
          <input type="text" bind:value={newProject.local_path} placeholder="~/path/to/local" />
          <input type="text" bind:value={newProject.remote_path} placeholder="proj/name" />
          <button onclick={addProject}>Add</button>
        </div>
      </section>
    </div>

    <div class="settings-footer">
      <button onclick={onclose}>Cancel</button>
      <button class="primary" onclick={save} disabled={saving}>
        {saving ? "Saving..." : "Save"}
      </button>
    </div>
  </div>
</div>

<style>
  .settings-overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 100;
  }

  .settings-panel {
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 12px;
    width: 600px;
    max-width: 90vw;
    max-height: 80vh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  .settings-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 16px 20px;
    border-bottom: 1px solid var(--border);
  }

  h2 {
    font-size: 16px;
    font-weight: 700;
  }

  .settings-body {
    padding: 20px;
    overflow-y: auto;
    overflow-x: hidden;
    flex: 1;
  }

  section {
    margin-bottom: 24px;
  }

  h3 {
    font-size: 13px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--text-muted);
    margin-bottom: 10px;
  }

  .field {
    margin-bottom: 10px;
  }

  label {
    display: block;
    font-size: 12px;
    color: var(--text-muted);
    margin-bottom: 4px;
  }

  .field input {
    width: 100%;
  }

  .checkbox-field {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    cursor: pointer;
    margin-top: 6px;
  }

  .checkbox-field input {
    width: 16px;
    height: 16px;
    cursor: pointer;
  }

  .section-desc {
    font-size: 11px;
    color: var(--text-muted);
    margin-bottom: 8px;
    line-height: 1.4;
  }

  .scan-list {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-bottom: 8px;
  }

  .scan-item {
    display: flex;
    align-items: center;
    gap: 4px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 3px 8px;
    font-size: 12px;
  }

  .scan-item code {
    font-family: var(--font-mono);
    font-size: 11px;
  }

  .exclude-list {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-bottom: 8px;
  }

  .exclude-item {
    display: flex;
    align-items: center;
    gap: 4px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 3px 8px;
    font-size: 12px;
  }

  .exclude-item code {
    font-family: var(--font-mono);
    font-size: 11px;
  }

  .exclude-item.default {
    opacity: 0.7;
  }

  .badge {
    font-size: 9px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--text-muted);
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 1px 5px;
  }

  .add-row {
    display: flex;
    gap: 8px;
  }

  .add-row input {
    flex: 1;
  }

  .project-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-bottom: 10px;
  }

  .project-row {
    display: flex;
    align-items: center;
    gap: 10px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 6px 10px;
    font-size: 12px;
  }

  .proj-name {
    font-weight: 600;
    min-width: 80px;
  }

  .proj-path {
    font-family: var(--font-mono);
    font-size: 11px;
    color: var(--text-muted);
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .add-project {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 6px;
  }

  .add-project input {
    min-width: 0;
  }

  .add-project input:first-child {
    grid-column: 1 / -1;
  }

  .add-project button {
    grid-column: 1 / -1;
  }

  button.small {
    padding: 2px 6px;
    font-size: 11px;
    line-height: 1;
  }

  .settings-footer {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    padding: 12px 20px;
    border-top: 1px solid var(--border);
  }
</style>
