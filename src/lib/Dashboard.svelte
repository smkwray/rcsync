<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import type { AppConfig, BulkMode, CheckOutcome, CheckStatus, Project, ProjectStatus, SyncMode, SyncState } from "./types";
  import ProjectCard from "./ProjectCard.svelte";
  import LogOutput from "./LogOutput.svelte";
  import ShortcutsHelp from "./ShortcutsHelp.svelte";
  import ConfirmDialog from "./ConfirmDialog.svelte";

  let projects: ProjectStatus[] = $state([]);
  let logLines: string[] = $state([]);
  let runningProjects: Map<string, string> = $state(new Map()); // name -> mode
  let cancellingProjects: Set<string> = $state(new Set());
  /** Latest progress snapshot per project. Deliberately not part of `logLines`:
   *  rclone re-reports the same counters on a timer, so appending them filled
   *  the panel with page after page of identical text whenever a transfer sat
   *  still. One row that updates in place says the same thing. */
  let progress: Map<string, string> = $state(new Map());
  /** Projects whose rclone output must not reach the log. A silent launch check
   *  suppresses its own result lines; without this the live stream would put
   *  them back, which is the whole thing "silent" exists to avoid. Progress is
   *  still shown — that is a status, not log noise. */
  const silentProjects = new Set<string>();
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
    requirePhrase?: string;
    resolve: (v: boolean) => void;
  } | null = $state(null);

  function customConfirm(title: string, message: string, confirmLabel = "Confirm", danger = true, requirePhrase = ""): Promise<boolean> {
    return new Promise((resolve) => { confirmState = { title, message, confirmLabel, danger, requirePhrase, resolve }; });
  }
  function onConfirmYes() { confirmState?.resolve(true); confirmState = null; }
  function onConfirmNo() { confirmState?.resolve(false); confirmState = null; }

  // Key bumped to v2 with the shape change: the old {synced, diffs} entries are a
  // cache any Check rebuilds, so they are dropped rather than migrated.
  let checkStatuses: Record<string, CheckStatus> = $state(
    JSON.parse(localStorage.getItem("rcsync-check-statuses-v2") || "{}")
  );

  function setStatus(name: string, state: SyncState, diffs = 0) {
    checkStatuses = { ...checkStatuses, [name]: { time: shortTime(), state, diffs } };
  }

  function applyCheckOutcome(name: string, outcome: CheckOutcome) {
    setStatus(name, outcome.synced ? "synced" : "diffs", outcome.differences);
  }

  // Pinned projects (stored by name)
  let pinnedNames: string[] = $state(
    JSON.parse(localStorage.getItem("rcsync-pinned") || "[]")
  );

  $effect(() => { localStorage.setItem("rcsync-check-statuses-v2", JSON.stringify(checkStatuses)); });
  $effect(() => { localStorage.setItem("rcsync-shortcuts", String(shortcutsEnabled)); });
  $effect(() => { localStorage.setItem("rcsync-pinned", JSON.stringify(pinnedNames)); });

  $effect(() => {
    if (selectedIndex >= 0 && gridEl) {
      const cards = gridEl.querySelectorAll(".card-wrapper");
      cards[selectedIndex]?.scrollIntoView({ block: "nearest", behavior: "smooth" });
    }
  });

  let localProjects = $derived(projects.filter((p) => p.exists_locally));
  let anyRunning = $derived(runningProjects.size > 0 || bulkMode !== null);
  // Gated on `runningProjects` as well as its own map, so a snapshot that
  // arrived just as an operation ended cannot leave a stale row on screen.
  let activeProgress = $derived([...progress].filter(([name]) => runningProjects.has(name)));

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

  // "On launch" means once. Reloading the list after Settings or Browse closes
  // goes through here too, and previously each one kicked off another silent
  // check of every project.
  let didAutoCheck = false;

  async function loadProjects() {
    projects = await invoke<ProjectStatus[]>("get_projects_status");
    loaded = true;
    if (didAutoCheck) return;
    didAutoCheck = true;
    try {
      const cfg = await invoke<AppConfig>("get_config");
      if (cfg.auto_check_on_launch) runCheckAll(true);
    } catch (_) {}
  }

  /** The log is a live view, not an archive: one push of a source tree emits a
   *  line per file, and every line is a DOM node. Past this many the oldest go —
   *  but they are counted and said to be gone, because a log that quietly loses
   *  its beginning is worse than one that admits it. */
  const MAX_LOG_LINES = 4000;
  /** Trim in chunks so a steady stream doesn't re-slice the array on every line. */
  const TRIM_CHUNK = 500;
  let droppedLines = 0;

  function appendLog(...incoming: string[]) {
    if (incoming.length === 0) return;
    let next = [...logLines, ...incoming];
    if (next.length > MAX_LOG_LINES) {
      const cut = next.length - MAX_LOG_LINES + TRIM_CHUNK;
      // The marker itself occupies index 0 once anything has been dropped, so it
      // must not be counted as one of the lines this round removes.
      droppedLines += droppedLines > 0 ? cut - 1 : cut;
      next = [
        `⋯ ${droppedLines} earlier lines dropped — the log keeps the last ${MAX_LOG_LINES}`,
        ...next.slice(cut),
      ];
    }
    logLines = next;
  }

  function addLog(project: string, text: string) {
    const lines = text.split("\n").filter((l) => l.trim());
    appendLog(...lines.map((l) => `[${project}] ${l}`));
  }

  function markRunning(name: string, mode: string) {
    runningProjects = new Map([...runningProjects, [name, mode]]);
  }

  function markDone(name: string) {
    const m = new Map(runningProjects);
    m.delete(name);
    runningProjects = m;
    if (progress.has(name)) {
      const p = new Map(progress);
      p.delete(name);
      progress = p;
    }
    if (cancellingProjects.has(name)) {
      const c = new Set(cancellingProjects);
      c.delete(name);
      cancellingProjects = c;
    }
  }

  /** The backend returns exactly this when the user stopped an operation. */
  function isCancelled(e: unknown): boolean {
    return String(e) === "CANCELLED";
  }

  /** Report a failed operation, distinguishing a deliberate cancel from a real
   *  error. Either way the card must stop showing a verdict it no longer has
   *  grounds for: a stopped push/pull/bisync moved an unknown subset of files,
   *  and a failed operation means the last verdict rested on a run that has
   *  since failed. Leaving the old badge up is the dishonest option. */
  function noteFailure(name: string, mode: SyncMode, e: unknown, log = true) {
    if (!isCancelled(e)) {
      // The backend's failure text spans several lines — the exit code and the
      // tail of rclone's own output. Pushing it as one entry made a paragraph
      // pretending to be a log line.
      if (log) addLog(name, `ERROR: ${e}`);
      // Always, even for a silent launch check: the badge on screen was earned
      // by a run that has now failed, so it has to be withdrawn. Suppressing
      // this with the log was how an expired token left a project reading
      // "synced" after the app had just failed to verify it.
      setStatus(name, "unknown");
      return;
    }
    if (log) appendLog(`[${name}] Cancelled.`);
    if (mode === "push" || mode === "pull" || mode === "bisync") {
      setStatus(name, "modified", -1);
    }
    if (mode === "bisync" && log) {
      appendLog(`[${name}] NOTE: an interrupted bi-sync leaves rclone's listings stale — the next bi-sync will likely need --resync.`);
    }
  }

  async function handleCancel(project: Project) {
    if (!runningProjects.has(project.name) || cancellingProjects.has(project.name)) return;
    cancellingProjects = new Set([...cancellingProjects, project.name]);
    appendLog(`[${project.name}] Stopping ${runningProjects.get(project.name)}...`);
    await invoke("cancel_op", { projectName: project.name });
  }

  /** Stop everything in flight. Deliberately unlabelled with a count: during a
   *  "push all" only a few projects are actually running, but this also stops
   *  the many still queued behind them. */
  async function cancelAll() {
    if (!anyRunning) return;
    // Cancels only the run that is actually in flight — a later run gets its own
    // token and is unaffected.
    if (activeBulk) activeBulk.cancelled = true;
    const names = [...runningProjects.keys()];
    appendLog("--- CANCEL ALL ---");
    cancellingProjects = new Set([...cancellingProjects, ...names]);
    await Promise.all(names.map((n) => invoke("cancel_op", { projectName: n })));
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
      appendLog(`[${project.name}] Skipped — ${runningProjects.get(project.name)} already in progress.`);
      return;
    }
    if (mode === "pull") {
      const ok = await customConfirm(
        `Pull "${project.name}"?`,
        `This will OVERWRITE local files with ${project.remote} contents.\nLocal is normally authoritative — only pull if the remote version is what you want.`,
        "Pull from Remote",
        true,
        "pull",
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
    appendLog(`--- ${mode.toUpperCase()} ${project.name} ---`);

    try {
      // Only a check contributes anything to the log at the end: its verdict is
      // built by the backend from output the user never sees. Everything else
      // returns rclone's own log, which already streamed here line by line while
      // it ran — logging it again would print the whole run twice.
      let summary = "";
      if (mode === "push") {
        await invoke<string>("push", { projectName: project.name, dryRun: false });
      } else if (mode === "dry-run") {
        await invoke<string>("push", { projectName: project.name, dryRun: true });
      } else if (mode === "pull") {
        await invoke<string>("pull", { projectName: project.name, dryRun: false });
      } else if (mode === "bisync") {
        await invoke<string>("bisync", { projectName: project.name });
      } else if (mode === "check") {
        const outcome = await invoke<CheckOutcome>("check", { projectName: project.name });
        applyCheckOutcome(project.name, outcome);
        summary = outcome.details;
      }
      if (summary) addLog(project.name, summary);
      appendLog(`[${project.name}] Done.`);
      // After successful push/pull/bisync, mark as synced
      if (mode === "push" || mode === "pull" || mode === "bisync") {
        setStatus(project.name, "synced");
      }
    } catch (e) {
      noteFailure(project.name, mode, e);
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
    const really = await customConfirm(
      "Final confirmation",
      `Type delete to permanently remove "${project.name}" from this device. The remote copy on ${project.remote} is not affected.`,
      "Delete",
      true,
      "delete",
    );
    if (!really) return;

    try {
      await invoke("delete_local", { projectName: project.name });
      appendLog(`[${project.name}] Local copy deleted.`);
      projects = await invoke<ProjectStatus[]>("get_projects_status");
    } catch (e) {
      addLog(project.name, `DELETE ERROR: ${e}`);
    }
  }

  /** The one bulk coordinator.
   *
   *  Each mode used to have its own boolean and all three shared a single
   *  `bulkCancelled` that every entry point reset to `false` — so starting any
   *  bulk run un-cancelled one that was still winding down, and it resumed.
   *  A run now carries its own token, and only that token can be cancelled.
   *  Control-flow state is a plain object, not `$state`: the loop closes over it
   *  and has to observe the mutation directly. */
  const BULK: Record<BulkMode, {
    title: string;
    concurrency: number;
    verb: string;
    run: (name: string) => Promise<string>;
    onSuccess: (name: string, result: string, silent: boolean) => void;
  }> = {
    check: {
      title: "Check all", concurrency: 4, verb: "checked",
      run: async (name) => {
        const outcome = await invoke<CheckOutcome>("check", { projectName: name });
        applyCheckOutcome(name, outcome);
        return outcome.details;
      },
      onSuccess(name, details, silent) {
        if (!silent) addLog(name, details);
      },
    },
    push: {
      title: "Push all", concurrency: 3, verb: "pushed",
      run: (name) => invoke<string>("push", { projectName: name, dryRun: false }),
      onSuccess: (name) => noteSuccess(name),
    },
    bisync: {
      title: "Bi-sync all", concurrency: 3, verb: "bi-synced",
      run: (name) => invoke<string>("bisync", { projectName: name }),
      onSuccess: (name) => noteSuccess(name),
    },
  };

  /** The transfer's own output already streamed into the log as it happened, so
   *  this only marks the end of it. */
  function noteSuccess(name: string) {
    appendLog(`[${name}] Done.`);
    setStatus(name, "synced");
  }

  let activeBulk: { id: number; mode: BulkMode; cancelled: boolean } | null = null;
  let bulkMode: BulkMode | null = $state(null); // mirror of activeBulk, for the UI only
  let nextBulkId = 0;

  async function runBulk(mode: BulkMode, silent = false) {
    if (activeBulk) {
      if (!silent) appendLog(`Skipped ${BULK[mode].title} — ${BULK[activeBulk.mode].title} is still running.`);
      return;
    }
    const run = { id: ++nextBulkId, mode, cancelled: false };
    activeBulk = run;
    bulkMode = mode;

    const spec = BULK[mode];
    if (!silent) appendLog(`--- ${spec.title.toUpperCase()} ---`);

    const queue = [...localProjects];
    let skipped = 0;

    async function one(ps: ProjectStatus) {
      // Busy with a manual operation. Counted rather than ignored, so the
      // summary below can never call a run complete that silently passed
      // projects over.
      if (runningProjects.has(ps.name)) { skipped++; return; }
      markRunning(ps.name, mode);
      if (silent) silentProjects.add(ps.name);
      try {
        spec.onSuccess(ps.name, await spec.run(ps.name), silent);
      } catch (e) {
        noteFailure(ps.name, mode, e, !silent);
      }
      silentProjects.delete(ps.name);
      markDone(ps.name);
    }

    try {
      while (queue.length > 0 && !run.cancelled) {
        await Promise.all(queue.splice(0, spec.concurrency).map(one));
      }
    } finally {
      if (activeBulk?.id === run.id) { activeBulk = null; bulkMode = null; }
    }

    if (!silent) {
      const notes: string[] = [];
      if (queue.length > 0) notes.push(`${queue.length} not ${spec.verb}`);
      if (skipped > 0) notes.push(`${skipped} skipped (already running)`);
      const head = run.cancelled ? `${spec.title} cancelled` : `${spec.title} complete`;
      appendLog(notes.length ? `${head} — ${notes.join(", ")}.` : `${head}.`);
    }
  }

  const runCheckAll = (silent = false) => runBulk("check", silent);

  async function handlePushAll() {
    const count = localProjects.length;
    const ok = await customConfirm(
      `Push All (${count} projects)?`,
      `This will push all ${count} local projects to their remotes.\nLocal is authoritative — remote files will be overwritten.`,
      "Push All",
      false,
    );
    if (ok) runBulk("push");
  }

  async function handleBisyncAll() {
    const count = localProjects.length;
    const ok = await customConfirm(
      `Bi-Sync All (${count} projects)?`,
      `Two-way sync all ${count} local projects with their remotes.\nChanges on both sides will be merged. Conflicts may arise.`,
      "Bi-Sync All",
    );
    if (ok) runBulk("bisync");
  }

  function clearLog() { logLines = []; droppedLines = 0; }
  function toggleShortcuts() { shortcutsEnabled = !shortcutsEnabled; }
  function toggleOutput() { showOutput = !showOutput; }

  function handleKeydown(e: KeyboardEvent) {
    const inInput = document.activeElement?.tagName === "INPUT";

    if (confirmState) {
      if (e.key === "Escape") { e.preventDefault(); onConfirmNo(); }
      // Let the confirmation field receive text, and let dialog controls work naturally.
      if (e.target instanceof HTMLInputElement || e.key === "Tab" || e.key === "Enter") return;
      e.preventDefault();
      return;
    }

    // The macOS stop chord, always live. Deliberately not Escape, which already
    // means "deselect / close" — killing a sync by reflex would be worse than
    // having to learn one key.
    if (e.metaKey && e.key === ".") { e.preventDefault(); cancelAll(); return; }
    if (e.metaKey && e.key === ",") { e.preventDefault(); window.dispatchEvent(new CustomEvent("open-settings")); return; }
    if (e.metaKey && e.key === "k") { e.preventDefault(); toggleShortcuts(); return; }
    if (e.metaKey && e.key === "o") { e.preventDefault(); toggleOutput(); return; }

    if (e.key === "Escape") {
      e.preventDefault();
      if (showShortcutsHelp) { showShortcutsHelp = false; return; }
      window.dispatchEvent(new CustomEvent("close-overlays"));
      if (inInput) {
        const active = document.activeElement as HTMLElement;
        // In the filter box, also clear the text so the list resets.
        if (active === filterInput) { search = ""; selectedIndex = -1; }
        active.blur();
        return;
      }
      if (selectedIndex >= 0) { selectedIndex = -1; return; }
      return;
    }

    // Enter in the filter: commit it — exit the box and select the first result
    // so hotkeys act on it immediately.
    if (e.key === "Enter" && document.activeElement === filterInput) {
      e.preventDefault();
      filterInput?.blur();
      selectedIndex = filteredProjects.length > 0 ? 0 : -1;
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
      case "e": { const s = ensureSelected(); if (s) invoke("open_folder", { localPath: s.local_path }); } break;
      case "i": { const s = ensureSelected(); if (s) togglePin(toProject(s)); } break;
      case "q": { const s = ensureSelected(); if (s) handleCancel(toProject(s)); } break;
      case "/": e.preventDefault(); filterInput?.focus(); break;
      case "c": if (!bulkMode) runCheckAll(); break;
      case "p": if (!bulkMode) handlePushAll(); break;
      case "v": if (!bulkMode) handleBisyncAll(); break;
      case "x": clearLog(); break;
      case "o": toggleOutput(); break;
      case "b": window.dispatchEvent(new CustomEvent("open-browse")); break;
    }
  }

  loadProjects();

  // Reloading the project list is how App asks for a refresh after Settings or
  // Browse closes. It deliberately does NOT touch runningProjects — an operation
  // in flight has to stay visible and cancellable across an overlay.
  function onReloadProjects() {
    loadProjects();
  }

  $effect(() => {
    window.addEventListener("reload-projects", onReloadProjects);

    // Retain the unlisten handles: without them every listener outlived its
    // component and the handlers stacked up. `disposed` covers teardown landing
    // before the async registration resolves.
    const unlisten: (() => void)[] = [];
    let disposed = false;
    const track = (p: Promise<() => void>) => {
      p.then((fn) => (disposed ? fn() : unlisten.push(fn)));
    };

    track(listen<{ projects: string[] }>("file-change", (event) => {
      const changed = event.payload.projects;
      const updated = { ...checkStatuses };
      for (const name of changed) {
        if (updated[name]?.state === "synced") {
          updated[name] = { ...updated[name], state: "modified", diffs: -1 };
        }
      }
      checkStatuses = updated;
    }));

    // rclone's output as it happens, in batches of whatever arrived together.
    track(listen<{ project: string; lines: string[] }>("rclone-log", (event) => {
      const { project, lines } = event.payload;
      if (silentProjects.has(project)) return;
      appendLog(...lines.map((l) => `[${project}] ${l}`));
    }));

    track(listen<{ project: string; text: string }>("rclone-progress", (event) => {
      const { project, text } = event.payload;
      progress = new Map([...progress, [project, text]]);
    }));

    return () => {
      disposed = true;
      window.removeEventListener("reload-projects", onReloadProjects);
      for (const fn of unlisten) fn();
    };
  });
</script>

<svelte:window onkeydown={handleKeydown} />

<div class="dashboard">
  <div class="toolbar">
    <h1>rcsync</h1>
    <div class="toolbar-actions">
      <div class="search-wrapper">
        <input class="search-input" type="text" placeholder="Filter..." bind:value={search} bind:this={filterInput}
          autocorrect="off" autocapitalize="off" autocomplete="off" spellcheck="false"
          onfocus={() => selectedIndex = -1} />
        {#if search}
          <span class="search-count">{filteredProjects.length}/{localProjects.length}</span>
        {/if}
      </div>
      <label class="shortcuts-toggle" title="Press ? for shortcut list">
        <input type="checkbox" bind:checked={shortcutsEnabled} />
        <span class="shortcuts-label">Keys</span>
      </label>
      <!-- All three lock while any bulk run is active: they share the project
           set, so overlapping runs skip each other's projects and confuse the
           completion summary. -->
      <button disabled={bulkMode !== null || !loaded} onclick={() => runCheckAll()}>
        {bulkMode === "check" ? "Checking..." : "Check All"}
      </button>
      <button class="primary" disabled={bulkMode !== null || !loaded || localProjects.length === 0} onclick={handlePushAll}>
        {bulkMode === "push" ? "Pushing..." : "Push All"}
      </button>
      <button class="warn" disabled={bulkMode !== null || !loaded || localProjects.length === 0} onclick={handleBisyncAll}>
        {bulkMode === "bisync" ? "Bi-Syncing..." : "Bi-Sync All"}
      </button>
      {#if anyRunning}
        <button class="cancel-all" onclick={cancelAll} title="Stop everything running and queued (Cmd+.)">
          Cancel All
        </button>
      {/if}
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
          <div class="card-wrapper" class:selected={i === selectedIndex} onclick={() => selectedIndex = i}>
            <ProjectCard
              project={toProject(project)}
              running={runningProjects.has(project.name)}
              runningMode={runningProjects.get(project.name) ?? ""}
              cancelling={cancellingProjects.has(project.name)}
              checkStatus={checkStatuses[project.name] ?? null}
              pinned={pinnedNames.includes(project.name)}
              onaction={handleAction}
              oncancel={handleCancel}
              ondelete={handleDelete}
              onpin={togglePin}
              onupdated={loadProjects}
            />
          </div>
        {/each}
      {/if}
    </div>

    <!-- Outside the collapsible log on purpose: knowing a transfer is moving is
         exactly what you want when you have hidden the log. -->
    {#if activeProgress.length > 0}
      <div class="progress-strip">
        {#each activeProgress as [name, text] (name)}
          <div class="progress-row">
            <span class="progress-name">{name}</span>
            <span class="progress-text">{text}</span>
          </div>
        {/each}
      </div>
    {/if}

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
    requirePhrase={confirmState.requirePhrase ?? ""}
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
  .progress-strip { display: flex; flex-direction: column; gap: 2px; flex-shrink: 0; }
  .progress-row { display: flex; gap: 10px; align-items: baseline; font-family: var(--font-mono); font-size: 12px; padding: 4px 10px; background: var(--bg-card); border: 1px solid var(--border); border-radius: var(--radius); }
  .progress-name { color: var(--accent); font-weight: 600; flex-shrink: 0; }
  .progress-text { color: var(--text); font-variant-numeric: tabular-nums; }
  .log-section { display: flex; flex-direction: column; min-height: 32px; max-height: 280px; transition: max-height 0.25s ease, min-height 0.25s ease; }
  .log-section.collapsed { max-height: 32px; min-height: 32px; }
  .log-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 6px; cursor: pointer; user-select: none; }
  .log-title { font-size: 13px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted); }
  .log-meta { display: flex; gap: 8px; align-items: center; }
  .log-count { font-size: 12px; color: var(--text-muted); font-family: var(--font-mono); }
  .log-chevron { font-size: 10px; color: var(--text-muted); }
  /* `display: flex` and `min-height: 0` are load-bearing, not cosmetic. As a
     plain block this box was a second scroller wrapping the log's own, and the
     inner one then had no height constraint — so it grew to fit its content,
     never overflowed, and every attempt to scroll it to the bottom did nothing.
     That is why a finished run left the view sitting at line 1 of 17,000. */
  .log-body { flex: 1; min-height: 0; display: flex; animation: slideDown 0.2s ease; }
  @keyframes slideDown { from { opacity: 0; max-height: 0; } to { opacity: 1; max-height: 280px; } }
  .loading { color: var(--text-muted); font-style: italic; }
  button.warn { border-color: var(--yellow); color: var(--yellow); }
  button.warn:hover { background: var(--yellow-dim); }
  .cancel-all { border-color: var(--red); background: var(--red-dim); color: var(--red); font-weight: 600; }
  .cancel-all:hover { background: var(--red); color: var(--bg); }
</style>
