<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { enable, disable, isEnabled } from "@tauri-apps/plugin-autostart";
  import type { AppConfig, Project } from "./types";
  import ConfirmDialog from "./ConfirmDialog.svelte";
  import { mergeSettingsProjects } from "./settingsState.js";

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
  let error = $state("");
  let newProject: Project = $state({ id: "", name: "", local_path: "", remote_path: "", remote: "", schedule: null });
  let startAtLogin = $state(false);
  let confirmRemoveIndex = $state<number | null>(null);
  let migratingLegacyHost = $state(false);
  let legacyHostMigrated = $state(false);

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
      error = `Failed to update login item: ${e}`;
      startAtLogin = !startAtLogin; // revert
    }
  }

  async function save() {
    saving = true;
    error = "";
    try {
      // Settings predates the project-card editors and holds a full config
      // snapshot. Refresh the project list immediately before saving so a
      // concurrent schedule/ignore/remote edit is not overwritten. Explicit
      // project removals from this Settings session still win; projects that
      // appeared after it opened are retained.
      const current = await invoke<AppConfig>("get_config");
      const projects = mergeSettingsProjects(config.projects, current.projects, editConfig.projects);
      await invoke("update_config", { cfg: { ...editConfig, projects } });
      onclose();
    } catch (e) {
      error = `Failed to save: ${e}`;
    } finally {
      saving = false;
    }
  }

  async function migrateLegacyHost() {
    if (migratingLegacyHost) return;
    migratingLegacyHost = true;
    error = "";
    try {
      const migrated = await invoke<boolean>("migrate_legacy_host_config");
      if (!migrated) {
        error = "No legacy host-specific config was found.";
      } else {
        legacyHostMigrated = true;
        window.dispatchEvent(new CustomEvent("reload-projects"));
      }
    } catch (e) {
      error = `Failed to convert the legacy config: ${e}`;
    } finally {
      migratingLegacyHost = false;
    }
  }

  function addExclude() {
    // Not trimmed: the backend refuses a padded pattern rather than repairing
    // it, because the padding means different things to a filters file and to a
    // command-line argument.
    const val = newExclude;
    if (val.trim() && !editConfig.excludes.includes(val) && !editConfig.extra_excludes.includes(val)) {
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
      newProject = { id: "", name: "", local_path: "", remote_path: "", remote: "", schedule: null };
    }
  }

  function removeProject(idx: number) {
    confirmRemoveIndex = idx;
  }

  function confirmRemoveProject() {
    if (confirmRemoveIndex === null) return;
    editConfig.projects = editConfig.projects.filter((_, i) => i !== confirmRemoveIndex);
    confirmRemoveIndex = null;
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

    {#if error}
      <p class="error-banner" role="alert">{error}</p>
    {/if}

    <div class="settings-body">
      {#if config.legacy_host_config_available && !legacyHostMigrated}
        <div class="migration-notice">
          <div>
            <strong>Legacy device-specific config found</strong>
            <p>This older config is not selected automatically. Convert it to the shared base once, then manage schedules in Schedules.</p>
          </div>
          <button onclick={migrateLegacyHost} disabled={migratingLegacyHost}>
            {migratingLegacyHost ? "Converting…" : "Convert here"}
          </button>
        </div>
      {/if}
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
        <label class="field">
          <span>Parallel file transfers</span>
          <input
            type="number"
            min="1"
            max="8"
            placeholder="Automatic"
            value={editConfig.rclone_transfers ?? ""}
            oninput={(e) => {
              // Blank must stay null, never a substituted number: passing an
              // explicit value would override the user's own rclone config or
              // RCLONE_TRANSFERS, since a command-line flag outranks both.
              const raw = (e.currentTarget as HTMLInputElement).value.trim();
              editConfig.rclone_transfers = raw === "" ? null : Number(raw);
            }}
          />
        </label>
        <p class="section-desc">
          Files rclone uploads at once, per project. Blank leaves rclone's own setting alone.
          1&ndash;8; up to three projects can sync at the same time, so memory use is roughly
          this figure times three.
        </p>
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

{#if confirmRemoveIndex !== null}
  <ConfirmDialog
    title="Remove project?"
    message={`Remove "${editConfig.projects[confirmRemoveIndex]?.name ?? "this project"}" from rcsync? This does not delete local or remote files.`}
    confirmLabel="Remove"
    danger={true}
    onconfirm={confirmRemoveProject}
    oncancel={() => confirmRemoveIndex = null}
  />
{/if}

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

  .error-banner {
    margin: 12px 20px 0;
    padding: 9px 11px;
    color: var(--red);
    background: var(--bg-card);
    border: 1px solid var(--red);
    border-radius: 6px;
    font-size: 12px;
    white-space: pre-wrap;
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

  .migration-notice {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 14px;
    margin-bottom: 20px;
    padding: 10px 12px;
    color: var(--yellow);
    background: var(--bg-card);
    border: 1px solid var(--yellow);
    border-radius: 7px;
    font-size: 12px;
  }

  .migration-notice strong { display: block; margin-bottom: 3px; }
  .migration-notice p { color: var(--text-muted); line-height: 1.4; }

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
