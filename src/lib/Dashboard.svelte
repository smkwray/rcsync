<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import type { AppConfig, Project, ProjectStatus, SyncMode } from "./types";
  import ProjectCard from "./ProjectCard.svelte";
  import LogOutput from "./LogOutput.svelte";
  import ShortcutsHelp from "./ShortcutsHelp.svelte";
  import ConfirmDialog from "./ConfirmDialog.svelte";

  let projects: ProjectStatus[] = $state([]);
  let logLines: string[] = $state([]);
  let runningProjects: Map<string, string> = $state(new Map()); // name -> mode
  let pushingAll = $state(false);
  let bisyncingAll = $state(false);
  let checkingAll = $state(false);
  let loaded = $state(false);
  let search = $state("");
  let selectedIndex = $state(-1);
  let shortcutsEnabled = $state(localStorage.getItem("rcsync-shortcuts") === "true");
  let showShortcutsHelp = $state(false);
  let showOutput = $state(true);
  let gridEl: HTMLDivElement | undefined = $state(undefined);
  let filterInput: HTMLInputElement | undefined = $state(undefined);

  let confirmState: {
    title: string; message: string; confirmLabel: string; danger: boolean;
    resolve: (v: boolean) => void;
  } | null = $state(null);

  function customConfirm(title: string, message: string, confirmLabel = "Confirm", danger = true): Promise<boolean> {
    return new Promise((resolve) => { confirmState = { title, message, confirmLabel, danger, resolve }; });
  }
  function onConfirmYes() { confirmState?.resolve(true); confirmState = null; }
  function onConfirmNo() { confirmState?.resolve(false); confirmState = null; }

  let checkStatuses: Record<string, { time: string; synced: boolean; diffs: number }> = $state(
    JSON.parse(localStorage.getItem("rcsync-check-statuses") || "{}")
  );

  // Pinned projects (stored by name)
  let pinnedNames: string[] = $state(
    JSON.parse(localStorage.getItem("rcsync-pinned") || "[]")
  );

  $effect(() => { localStorage.setItem("rcsync-check-statuses", JSON.stringify(checkStatuses)); });
  $effect(() => { localStorage.setItem("rcsync-shortcuts", String(shortcutsEnabled)); });
  $effect(() => { localStorage.setItem("rcsync-pinned", JSON.stringify(pinnedNames)); });

  $effect(() => {
    if (selectedIndex >= 0 && gridEl) {
      const cards = gridEl.querySelectorAll(".card-wrapper");
      cards[selectedIndex]?.scrollIntoView({ block: "nearest", behavior: "smooth" });
    }
  });

  let localProjects = $derived(projects.filter((p) => p.exists_locally));

  // Sort: pinned first, then alphabetical
  let sortedProjects = $derived(() => {
    const pinSet = new Set(pinnedNames);
    return [...localProjects].sort((a, b) => {
      const ap = pinSet.has(a.name) ? 0 : 1;
      const bp = pinSet.has(b.name) ? 0 : 1;
      if (ap !== bp) return ap - bp;
      return a.name.localeCompare(b.name);
    });
  });

  function fuzzyMatch(q: string, t: string): boolean {
    const ql = q.toLowerCase(), tl = t.toLowerCase();
    let qi = 0;
    for (let ti = 0; ti < tl.length && qi < ql.length; ti++) { if (tl[ti] === ql[qi]) qi++; }
    return qi === ql.length;
  }

  let filteredProjects = $derived(
    search.trim() ? sortedProjects().filter((p) => fuzzyMatch(search.trim(), p.name)) : sortedProjects()
  );

  function shortTime(): string {
    const d = new Date();
    const h = d.getHours(), m = String(d.getMinutes()).padStart(2, "0");
    return `${d.getMonth()+1}/${d.getDate()} ${h%12||12}:${m}${h>=12?"p":"a"}`;
  }

  function parseCheckResult(output: string): { synced: boolean; diffs: number } {
    const m = output.match(/(\d+) differences/);
    if (m) return { synced: false, diffs: parseInt(m[1]) };
    return { synced: true, diffs: 0 };
  }

  function getGridCols(): number {
    if (!gridEl) return 2;
    return getComputedStyle(gridEl).gridTemplateColumns.split(" ").length;
  }

  function toProject(ps: ProjectStatus): Project {
    return { name: ps.name, local_path: ps.local_path, remote_path: ps.remote_path, remote: ps.remote };
  }

  function ensureSelected(): ProjectStatus | null {
    const len = filteredProjects.length;
    if (len === 0) return null;
    if (selectedIndex < 0 || selectedIndex >= len) selectedIndex = 0;
    return filteredProjects[selectedIndex];
  }

  async function loadProjects() {
    projects = await invoke<ProjectStatus[]>("get_projects_status");
    loaded = true;
    try {
      const cfg = await invoke<AppConfig>("get_config");
      if (cfg.auto_check_on_launch) runCheckAll(true);
    } catch (_) {}
  }

  function addLog(project: string, text: string) {
    const lines = text.split("\n").filter((l) => l.trim());
    logLines = [...logLines, ...lines.map((l) => `[${project}] ${l}`)];
  }

  function markRunning(name: string, mode: string) {
    runningProjects = new Map([...runningProjects, [name, mode]]);
  }

  function markDone(name: string) {
    const m = new Map(runningProjects);
    m.delete(name);
    runningProjects = m;
  }

  function togglePin(project: Project) {
    const idx = pinnedNames.indexOf(project.name);
    if (idx >= 0) {
      pinnedNames = pinnedNames.filter((n) => n !== project.name);
    } else {
      pinnedNames = [...pinnedNames, project.name];
    }
  }

  async function handleAction(project: Project, mode: SyncMode) {
    if (runningProjects.has(project.name)) {
      logLines = [...logLines, `[${project.name}] Skipped — ${runningProjects.get(project.name)} already in progress.`];
      return;
    }
    if (mode === "pull") {
      const ok = await customConfirm(
        `Pull "${project.name}"?`,
        `This will overwrite local files with ${project.remote} contents.\nLocal is always authoritative — only pull if the remote version is what you want.`,
        "Pull from Remote",
      );
      if (!ok) return;
    }
    if (mode === "bisync") {
      const ok = await customConfirm(
        `Bi-Sync "${project.name}"?`,
        `Two-way sync between local and ${project.remote}.\nChanges on both sides will be merged. Conflicts may arise.`,
        "Bi-Sync",
      );
      if (!ok) return;
    }

    markRunning(project.name, mode);
    logLines = [...logLines, `--- ${mode.toUpperCase()} ${project.name} ---`];

    try {
      let result = "";
      if (mode === "push") {
        result = await invoke<string>("push", { projectName: project.name, dryRun: false });
      } else if (mode === "dry-run") {
        result = await invoke<string>("push", { projectName: project.name, dryRun: true });
      } else if (mode === "pull") {
        result = await invoke<string>("pull", { projectName: project.name, dryRun: false });
      } else if (mode === "bisync") {
        result = await invoke<string>("bisync", { projectName: project.name });
      } else if (mode === "check") {
        result = await invoke<string>("check", { projectName: project.name });
        checkStatuses = { ...checkStatuses, [project.name]: { time: shortTime(), ...parseCheckResult(result) } };
      }
      if (result) addLog(project.name, result);
      logLines = [...logLines, `[${project.name}] Done.`];
      // After successful push/pull/bisync, mark as synced
      if (mode === "push" || mode === "pull" || mode === "bisync") {
        checkStatuses = { ...checkStatuses, [project.name]: { time: shortTime(), synced: true, diffs: 0 } };
      }
    } catch (e) {
      logLines = [...logLines, `[${project.name}] ERROR: ${e}`];
    }
    markDone(project.name);
  }

  async function handleDelete(project: Project) {
    const ok = await customConfirm(
      `Delete "${project.name}" locally?`,
      `Path: ${project.local_path}\n\nThe remote copy on ${project.remote} is NOT affected.\nThis permanently deletes the local directory.`,
      "Delete Local Copy",
    );
    if (!ok) return;
    const really = await customConfirm("Final confirmation", `Permanently delete "${project.name}" from this device?`, "Yes, Delete");
    if (!really) return;

    try {
      await invoke("delete_local", { projectName: project.name });
      logLines = [...logLines, `[${project.name}] Local copy deleted.`];
      projects = await invoke<ProjectStatus[]>("get_projects_status");
    } catch (e) {
      logLines = [...logLines, `[${project.name}] DELETE ERROR: ${e}`];
    }
  }

  // Parallel check all — runs up to 4 checks concurrently
  async function runCheckAll(silent = false) {
    checkingAll = true;
    if (!silent) logLines = [...logLines, "--- CHECK ALL ---"];

    const CONCURRENCY = 4;
    const queue = [...localProjects];

    async function checkOne(ps: ProjectStatus) {
      if (runningProjects.has(ps.name)) return;
      markRunning(ps.name, "check");
      try {
        const result = await invoke<string>("check", { projectName: ps.name });
        checkStatuses = { ...checkStatuses, [ps.name]: { time: shortTime(), ...parseCheckResult(result) } };
        if (!silent) addLog(ps.name, result);
      } catch (e) {
        if (!silent) logLines = [...logLines, `[${ps.name}] ERROR: ${e}`];
      }
      markDone(ps.name);
    }

    // Process in batches of CONCURRENCY
    while (queue.length > 0) {
      const batch = queue.splice(0, CONCURRENCY);
      await Promise.all(batch.map(checkOne));
    }

    if (!silent) logLines = [...logLines, "Check all complete."];
    checkingAll = false;
  }

  async function handlePushAll() {
    const count = localProjects.length;
    const ok = await customConfirm(
      `Push All (${count} projects)?`,
      `This will push all ${count} local projects to their remotes.\nLocal is authoritative — remote files will be overwritten.`,
      "Push All",
      false,
    );
    if (!ok) return;

    pushingAll = true;
    for (const p of localProjects) markRunning(p.name, "push");
    logLines = [...logLines, "--- PUSH ALL ---"];
    try {
      const result = await invoke<string>("push_all", { dryRun: false });
      logLines = [...logLines, ...result.split("\n").filter((l) => l.trim())];
    } catch (e) {
      logLines = [...logLines, `[ERROR] ${e}`];
    }
    pushingAll = false;
    runningProjects = new Map();
  }

  async function handleBisyncAll() {
    const count = localProjects.length;
    const ok = await customConfirm(
      `Bi-Sync All (${count} projects)?`,
      `Two-way sync all ${count} local projects with their remotes.\nChanges on both sides will be merged. Conflicts may arise.`,
      "Bi-Sync All",
    );
    if (!ok) return;

    bisyncingAll = true;
    for (const p of localProjects) markRunning(p.name, "bisync");
    logLines = [...logLines, "--- BI-SYNC ALL ---"];
    try {
      const result = await invoke<string>("bisync_all");
      logLines = [...logLines, ...result.split("\n").filter((l) => l.trim())];
    } catch (e) {
      logLines = [...logLines, `[ERROR] ${e}`];
    }
    bisyncingAll = false;
    runningProjects = new Map();
  }

  function clearLog() { logLines = []; }
  function toggleShortcuts() { shortcutsEnabled = !shortcutsEnabled; }
  function toggleOutput() { showOutput = !showOutput; }

  function handleKeydown(e: KeyboardEvent) {
    const inInput = document.activeElement?.tagName === "INPUT";

    if (confirmState) {
      if (e.key === "Escape") { e.preventDefault(); onConfirmNo(); }
      // Let Tab and Enter through so the dialog buttons work naturally
      if (e.key === "Tab" || e.key === "Enter") return;
      e.preventDefault();
      return;
    }

    if (e.metaKey && e.key === ",") { e.preventDefault(); window.dispatchEvent(new CustomEvent("open-settings")); return; }
    if (e.metaKey && e.key === "k") { e.preventDefault(); toggleShortcuts(); return; }
    if (e.metaKey && e.key === "o") { e.preventDefault(); toggleOutput(); return; }

    if (e.key === "Escape") {
      e.preventDefault();
      if (showShortcutsHelp) { showShortcutsHelp = false; return; }
      window.dispatchEvent(new CustomEvent("close-overlays"));
      if (inInput) { (document.activeElement as HTMLElement).blur(); return; }
      if (selectedIndex >= 0) { selectedIndex = -1; return; }
      return;
    }

    if (e.key === "?" && !inInput && !e.metaKey && !e.ctrlKey) {
      e.preventDefault(); showShortcutsHelp = !showShortcutsHelp; return;
    }

    if (!shortcutsEnabled || inInput) return;

    const len = filteredProjects.length;
    if (len === 0) return;
    const cols = getGridCols();

    switch (e.key) {
      case "j": e.preventDefault(); selectedIndex = selectedIndex < 0 ? 0 : Math.min(selectedIndex + cols, len - 1); break;
      case "k": e.preventDefault(); selectedIndex = selectedIndex < 0 ? 0 : Math.max(selectedIndex - cols, 0); break;
      case ";": e.preventDefault(); selectedIndex = selectedIndex < 0 ? 0 : Math.min(selectedIndex + 1, len - 1); break;
      case "l": e.preventDefault(); selectedIndex = selectedIndex < 0 ? 0 : Math.max(selectedIndex - 1, 0); break;
      case "a": { const s = ensureSelected(); if (s) handleAction(toProject(s), "push"); } break;
      case "s": { const s = ensureSelected(); if (s) handleAction(toProject(s), "dry-run"); } break;
      case "d": { const s = ensureSelected(); if (s) handleAction(toProject(s), "check"); } break;
      case "f": { const s = ensureSelected(); if (s) handleAction(toProject(s), "bisync"); } break;
      case "g": { const s = ensureSelected(); if (s) handleAction(toProject(s), "pull"); } break;
      case "h": { const s = ensureSelected(); if (s) handleDelete(toProject(s)); } break;
      case "i": { const s = ensureSelected(); if (s) togglePin(toProject(s)); } break;
      case "/": e.preventDefault(); filterInput?.focus(); break;
      case "c": if (!checkingAll) runCheckAll(); break;
      case "p": if (!pushingAll) handlePushAll(); break;
      case "v": if (!bisyncingAll) handleBisyncAll(); break;
      case "x": clearLog(); break;
      case "o": toggleOutput(); break;
      case "b": window.dispatchEvent(new CustomEvent("open-browse")); break;
    }
  }

  loadProjects();

  // Listen for local file changes from the watcher and invalidate sync status
  listen<{ projects: string[] }>("file-change", (event) => {
    const changed = event.payload.projects;
    const updated = { ...checkStatuses };
    for (const name of changed) {
      if (updated[name]?.synced) {
        updated[name] = { ...updated[name], synced: false, diffs: -1 };
      }
    }
    checkStatuses = updated;
  });
</script>

<svelte:window onkeydown={handleKeydown} />

<div class="dashboard">
  <div class="toolbar">
    <h1>rcsync</h1>
    <div class="toolbar-actions">
      <div class="search-wrapper">
        <input class="search-input" type="text" placeholder="Filter..." bind:value={search} bind:this={filterInput}
          onfocus={() => selectedIndex = -1} />
        {#if search}
          <span class="search-count">{filteredProjects.length}/{localProjects.length}</span>
        {/if}
      </div>
      <label class="shortcuts-toggle" title="Press ? for shortcut list">
        <input type="checkbox" bind:checked={shortcutsEnabled} />
        <span class="shortcuts-label">Keys</span>
      </label>
      <button disabled={checkingAll || !loaded} onclick={() => runCheckAll()}>
        {checkingAll ? "Checking..." : "Check All"}
      </button>
      <button class="primary" disabled={pushingAll || !loaded || localProjects.length === 0} onclick={handlePushAll}>
        Push All
      </button>
      <button class="warn" disabled={bisyncingAll || !loaded || localProjects.length === 0} onclick={handleBisyncAll}>
        {bisyncingAll ? "Bi-Syncing..." : "Bi-Sync All"}
      </button>
      <button onclick={toggleOutput} title="Cmd+O">{showOutput ? "Hide Log" : "Show Log"}</button>
      <button onclick={clearLog}>Clear</button>
    </div>
  </div>

  <div class="content">
    <div class="project-grid" bind:this={gridEl}>
      {#if !loaded}
        <p class="loading">Loading projects...</p>
      {:else if localProjects.length === 0}
        <p class="loading">No projects found locally. Use Browse Remote to pull projects.</p>
      {:else if filteredProjects.length === 0}
        <p class="loading">No matches for "{search}"</p>
      {:else}
        {#each filteredProjects as project, i (project.name)}
          <div class="card-wrapper" class:selected={shortcutsEnabled && i === selectedIndex}>
            <ProjectCard
              project={toProject(project)}
              running={runningProjects.has(project.name)}
              runningMode={runningProjects.get(project.name) ?? ""}
              checkStatus={checkStatuses[project.name] ?? null}
              pinned={pinnedNames.includes(project.name)}
              onaction={handleAction}
              ondelete={handleDelete}
              onpin={togglePin}
            />
          </div>
        {/each}
      {/if}
    </div>

    <div class="log-section" class:collapsed={!showOutput}>
      <div class="log-header" onclick={toggleOutput} role="button" tabindex="-1">
        <span class="log-title">Output</span>
        <span class="log-meta">
          <span class="log-count">{logLines.length} lines</span>
          <span class="log-chevron">{showOutput ? "\u25BC" : "\u25B2"}</span>
        </span>
      </div>
      {#if showOutput}
        <div class="log-body">
          <LogOutput bind:lines={logLines} />
        </div>
      {/if}
    </div>
  </div>
</div>

{#if showShortcutsHelp}
  <ShortcutsHelp onclose={() => showShortcutsHelp = false} />
{/if}

{#if confirmState}
  <ConfirmDialog
    title={confirmState.title}
    message={confirmState.message}
    confirmLabel={confirmState.confirmLabel}
    danger={confirmState.danger}
    onconfirm={onConfirmYes}
    oncancel={onConfirmNo}
  />
{/if}

<style>
  .dashboard { display: flex; flex-direction: column; flex: 1; padding: 20px; gap: 16px; overflow: hidden; }
  .toolbar { display: flex; justify-content: space-between; align-items: center; gap: 12px; }
  h1 { font-size: 20px; font-weight: 700; letter-spacing: -0.5px; white-space: nowrap; }
  .toolbar-actions { display: flex; gap: 6px; align-items: center; flex-wrap: wrap; }
  .search-wrapper { position: relative; display: flex; align-items: center; }
  .search-input { width: 120px; font-family: var(--font-sans); font-size: 12px; padding: 5px 10px; }
  .search-count { position: absolute; right: 8px; font-size: 10px; color: var(--text-muted); font-family: var(--font-mono); pointer-events: none; }
  .shortcuts-toggle { display: flex; align-items: center; gap: 4px; cursor: pointer; font-size: 11px; color: var(--text-muted); user-select: none; }
  .shortcuts-toggle input { width: 14px; height: 14px; cursor: pointer; }
  .shortcuts-label { font-size: 11px; }
  .content { display: flex; flex-direction: column; flex: 1; gap: 16px; overflow: hidden; }
  .project-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 12px; overflow-y: auto; flex: 1; }
  .card-wrapper { border-radius: var(--radius); transition: box-shadow 0.15s; }
  .card-wrapper.selected { box-shadow: 0 0 0 2px #22d3ee, 0 0 12px rgba(34, 211, 238, 0.25); border-radius: var(--radius); }
  .log-section { display: flex; flex-direction: column; min-height: 32px; max-height: 280px; transition: max-height 0.25s ease, min-height 0.25s ease; }
  .log-section.collapsed { max-height: 32px; min-height: 32px; }
  .log-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 6px; cursor: pointer; user-select: none; }
  .log-title { font-size: 13px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted); }
  .log-meta { display: flex; gap: 8px; align-items: center; }
  .log-count { font-size: 12px; color: var(--text-muted); font-family: var(--font-mono); }
  .log-chevron { font-size: 10px; color: var(--text-muted); }
  .log-body { flex: 1; overflow: hidden; animation: slideDown 0.2s ease; }
  @keyframes slideDown { from { opacity: 0; max-height: 0; } to { opacity: 1; max-height: 280px; } }
  .loading { color: var(--text-muted); font-style: italic; }
  button.warn { border-color: var(--yellow); color: var(--yellow); }
  button.warn:hover { background: var(--yellow-dim); }
</style>
