<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import type { AppConfig, IntervalUnit, Project, ProjectStatus, Schedule, ScheduleStatus } from "./types";
  import ScheduleEditor from "./ScheduleEditor.svelte";

  let { onclose }: { onclose: () => void } = $props();

  let projects: ProjectStatus[] = $state([]);
  let statuses: Map<string, ScheduleStatus> = $state(new Map());
  let loading = $state(true);
  let error = $state("");
  let busyProjectIds: Set<string> = $state(new Set());
  let projectOrder: string[] = $state([]);
  let editorProject: Project | null = $state(null);
  let search = $state("");
  let showQuickEditor = $state(false);
  let quickError = $state("");
  let queueScheduledPushes = $state(true);
  let queueSaving = $state(false);
  let legacyScheduleCount = $state(0);
  let legacyQueuePolicy = $state(false);
  let migratingLegacy = $state(false);
  let historyWarning = $state("");

  type QuickSlot = { label: string; every: number; unit: IntervalUnit };

  function defaultQuickSlots(): QuickSlot[] {
    return [
      { label: "12 hours", every: 12, unit: "hours" },
      { label: "24 hours", every: 24, unit: "hours" },
      { label: "48 hours", every: 48, unit: "hours" },
    ];
  }

  function loadQuickSlots(): QuickSlot[] {
    try {
      const parsed = JSON.parse(localStorage.getItem("rcsync-quick-schedules-v2") || "null");
      if (Array.isArray(parsed) && parsed.length === 3 && parsed.every((slot) =>
        slot && typeof slot.label === "string" && slot.label.trim() &&
        Number.isInteger(slot.every) && slot.every >= 1 && slot.every <= 365 &&
        (slot.unit === "hours" || slot.unit === "days")
      )) return parsed;
    } catch { /* fall through to defaults */ }
    return defaultQuickSlots();
  }

  let quickSlots: QuickSlot[] = $state(loadQuickSlots());
  let quickDraft: QuickSlot[] = $state([]);

  const dayNames = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

  let activeCount = $derived(projects.filter((project) => project.schedule).length);
  let orderedProjects = $derived.by(() => {
    const byId = new Map(projects.map((project) => [project.id, project]));
    return projectOrder.flatMap((id) => {
      const project = byId.get(id);
      return project ? [project] : [];
    });
  });
  let visibleProjects = $derived(
    search.trim() ? orderedProjects.filter((project) => fuzzyMatch(search.trim(), project.name)) : orderedProjects,
  );

  function toProject(project: ProjectStatus): Project {
    return {
      id: project.id,
      name: project.name,
      local_path: project.local_path,
      remote_path: project.remote_path,
      remote: project.remote,
      schedule: project.schedule,
    };
  }

  function minuteToTime(minute: number): string {
    return `${String(Math.floor(minute / 60)).padStart(2, "0")}:${String(minute % 60).padStart(2, "0")}`;
  }

  function scheduleSummary(schedule: Schedule): string {
    if (schedule.kind === "interval") {
      return `Every ${schedule.every} ${schedule.unit}${schedule.every === 1 ? "" : "s"}`;
    }
    const days = schedule.weekdays.map((day) => dayNames[day]).join(", ");
    return `${days} at ${minuteToTime(schedule.minute)}`;
  }

  function fuzzyMatch(query: string, text: string): boolean {
    const q = query.toLowerCase();
    const t = text.toLowerCase();
    let index = 0;
    for (const character of t) {
      if (character === q[index]) index++;
      if (index === q.length) return true;
    }
    return q.length === 0;
  }

  function mergeProjectOrder(nextProjects: ProjectStatus[]): ProjectStatus[] {
    const byId = new Map(nextProjects.map((project) => [project.id, project]));
    if (projectOrder.length === 0) {
      projectOrder = [...nextProjects]
        .sort((a, b) => {
          const aGroup = a.schedule ? 0 : 1;
          const bGroup = b.schedule ? 0 : 1;
          return aGroup - bGroup || a.name.localeCompare(b.name);
        })
        .map((project) => project.id);
    } else {
      const retained = projectOrder.filter((id) => byId.has(id));
      const added = nextProjects
        .map((project) => project.id)
        .filter((id) => !retained.includes(id));
      projectOrder = [...retained, ...added];
    }
    return projectOrder.flatMap((id) => {
      const project = byId.get(id);
      return project ? [project] : [];
    });
  }

  async function load() {
    loading = true;
    error = "";
    try {
      const [nextProjects, nextStatuses, cfg] = await Promise.all([
        invoke<ProjectStatus[]>("get_projects_status"),
        invoke<ScheduleStatus[]>("get_schedule_status"),
        invoke<AppConfig>("get_config"),
      ]);
      projects = mergeProjectOrder(nextProjects);
      statuses = new Map(nextStatuses.map((status) => [status.project_id, status]));
      historyWarning = nextStatuses.find((status) => status.warning)?.warning ?? "";
      queueScheduledPushes = cfg.queue_scheduled_pushes;
      legacyScheduleCount = cfg.legacy_schedule_count;
      legacyQueuePolicy = cfg.legacy_queue_policy;
    } catch (e) {
      error = `Could not load schedules: ${e}`;
    } finally {
      loading = false;
    }
  }

  onMount(() => { void load(); });

  function notifyDashboard() {
    window.dispatchEvent(new CustomEvent("reload-projects"));
  }

  function openEditor(project: ProjectStatus) {
    editorProject = toProject(project);
  }

  function editQuickSlots() {
    quickDraft = quickSlots.map((slot) => ({ ...slot }));
    quickError = "";
    showQuickEditor = true;
  }

  function saveQuickSlots() {
    const next = quickDraft.map((slot) => ({ ...slot, label: slot.label.trim(), every: Number(slot.every) }));
    if (next.some((slot) => !slot.label || !Number.isInteger(slot.every) || slot.every < 1 || slot.every > 365)) {
      quickError = "Each quick schedule needs a name and an interval from 1 to 365.";
      return;
    }
    localStorage.setItem("rcsync-quick-schedules-v2", JSON.stringify(next));
    quickSlots = next;
    showQuickEditor = false;
  }

  async function setQueuePolicy(enabled: boolean) {
    const previous = queueScheduledPushes;
    queueScheduledPushes = enabled;
    queueSaving = true;
    error = "";
    try {
      await invoke("set_queue_scheduled_pushes", { enabled });
    } catch (e) {
      queueScheduledPushes = previous;
      error = `Could not update the queue setting: ${e}`;
    } finally {
      queueSaving = false;
    }
  }

  function quickSchedule(slot: QuickSlot): Schedule {
    return { kind: "interval", every: Number(slot.every), unit: slot.unit, origin_ms: 0 };
  }

  async function applyQuick(project: ProjectStatus, slot: QuickSlot) {
    busyProjectIds = new Set([...busyProjectIds, project.id]);
    error = "";
    try {
      const durableProjectId = await invoke<string>("set_project_schedule", {
        projectName: project.name,
        projectId: project.id,
        schedule: quickSchedule(slot),
      });
      replaceProjectOrderId(project.id, durableProjectId);
      notifyDashboard();
      await load();
    } catch (e) {
      error = `Could not apply ${slot.label} to ${project.name}: ${e}`;
    } finally {
      const nextBusy = new Set(busyProjectIds);
      nextBusy.delete(project.id);
      busyProjectIds = nextBusy;
    }
  }

  async function toggle(project: ProjectStatus) {
    if (!project.schedule) {
      openEditor(project);
      return;
    }
    busyProjectIds = new Set([...busyProjectIds, project.id]);
    error = "";
    try {
      await invoke("set_project_schedule", { projectName: project.name, projectId: project.id, schedule: null });
      notifyDashboard();
      await load();
    } catch (e) {
      error = `Could not disable ${project.name}: ${e}`;
    } finally {
      const nextBusy = new Set(busyProjectIds);
      nextBusy.delete(project.id);
      busyProjectIds = nextBusy;
    }
  }

  function replaceProjectOrderId(oldId: string, newId: string) {
    if (oldId === newId) return;
    const index = projectOrder.indexOf(oldId);
    if (index < 0) return;
    const next = [...projectOrder];
    next[index] = newId;
    projectOrder = next;
  }

  async function afterEditor(newProjectId?: string) {
    if (editorProject && newProjectId) {
      replaceProjectOrderId(editorProject.id, newProjectId);
    }
    editorProject = null;
    notifyDashboard();
    await load();
  }

  async function migrateLegacy() {
    if (migratingLegacy) return;
    migratingLegacy = true;
    error = "";
    try {
      await invoke<number>("migrate_legacy_automation");
      notifyDashboard();
      await load();
    } catch (e) {
      error = `Could not move legacy schedules: ${e}`;
    } finally {
      migratingLegacy = false;
    }
  }

  function handleKey(event: KeyboardEvent) {
    if (event.key === "Escape") {
      event.preventDefault();
      if (editorProject) editorProject = null;
      else onclose();
    }
  }
</script>

<div class="overlay" role="dialog" aria-modal="true" aria-label="Schedule manager" tabindex="-1" onclick={onclose} onkeydown={handleKey}>
  <div class="manager" onclick={(event) => event.stopPropagation()}>
    <div class="manager-header">
      <div>
        <h2>Schedules</h2>
        <p>Automatic Push runs while this device’s rcsync process is open.</p>
      </div>
      <button class="close-btn" onclick={onclose} aria-label="Close">×</button>
    </div>

    {#if !loading && (legacyScheduleCount > 0 || legacyQueuePolicy)}
      <div class="legacy-notice">
        <div>
          <strong>Legacy shared schedules are disabled</strong>
          <small>Move them to this device to enable them here. Other devices will not inherit the schedules.</small>
        </div>
        <button class="text-btn" disabled={migratingLegacy} onclick={migrateLegacy}>
          {migratingLegacy ? "Moving…" : "Move here"}
        </button>
      </div>
    {/if}

    {#if historyWarning}
      <div class="history-warning" role="alert">{historyWarning}</div>
    {/if}

    <label class="search-box">
      <svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="11" cy="11" r="7"/><path d="m20 20-4-4"/></svg>
      <input type="search" placeholder="Filter projects…" bind:value={search} aria-label="Filter schedules" />
    </label>

    <div class="quick-panel">
      <div class="quick-header">
        <span>Quick schedules <small>one-click presets</small></span>
        <button class="text-btn" onclick={() => showQuickEditor ? showQuickEditor = false : editQuickSlots()}>
          {showQuickEditor ? "Done" : "Customize"}
        </button>
      </div>
      {#if showQuickEditor}
        <div class="quick-editor">
          {#each quickDraft as slot}
            <div class="quick-edit-row">
              <input type="text" bind:value={slot.label} aria-label="Quick schedule name" placeholder="Name" />
              <input type="number" min="1" max="365" bind:value={slot.every} aria-label="Quick schedule interval" />
              <select bind:value={slot.unit} aria-label="Quick schedule unit">
                <option value="hours">hours</option>
                <option value="days">days</option>
              </select>
            </div>
          {/each}
          {#if quickError}<p class="err">{quickError}</p>{/if}
          <button class="primary save-quicks" onclick={saveQuickSlots}>Save quick schedules</button>
        </div>
      {:else}
        <div class="quick-list">
          {#each quickSlots as slot}<span>{slot.label}</span>{/each}
          <small>Use a row’s buttons to apply one.</small>
        </div>
      {/if}
    </div>

    <label class="queue-toggle">
      <input type="checkbox" checked={queueScheduledPushes} disabled={queueSaving}
        onchange={(event) => setQueuePolicy((event.currentTarget as HTMLInputElement).checked)} />
      <span>
        <strong>Queue scheduled pushes</strong>
        <small>Run scheduled projects one at a time; due projects wait in the list.</small>
      </span>
    </label>

    {#if !loading && !error}
      <div class="manager-summary">
        <strong>{activeCount} active</strong>
        <span>{projects.length} project{projects.length === 1 ? "" : "s"}</span>
      </div>
    {/if}

    {#if loading}
      <p class="empty">Loading schedules…</p>
    {:else if error}
      <p class="err">{error}</p>
      <button onclick={load}>Retry</button>
    {:else if projects.length === 0}
      <p class="empty">No projects found.</p>
    {:else}
      <div class="schedule-list">
        {#each visibleProjects as project (project.id)}
          {@const status = statuses.get(project.id)}
          <div class="schedule-row" class:unavailable={!project.exists_locally}>
            <div class="schedule-info">
              <div class="schedule-name">
                <strong>{project.name}</strong>
                {#if !project.exists_locally}<span class="missing">no local copy</span>{/if}
              </div>
              {#if project.schedule}
                <span class="schedule-detail">{scheduleSummary(project.schedule)}</span>
                {#if status?.next_run}<span class="next-run">Next push: {status.next_run}</span>{/if}
                {#if status?.running}<span class="state running">Running now</span>{:else if status?.pending}<span class="state pending">Waiting for project</span>{/if}
              {:else}
                <span class="schedule-detail off">Not scheduled</span>
              {/if}
            </div>
            <div class="row-actions">
              <div class="quick-actions" aria-label={`Apply quick schedule to ${project.name}`}>
                {#each quickSlots as slot}
                  <button class="quick-btn" disabled={busyProjectIds.has(project.id)} title={`Set ${project.name} to ${slot.label}`} onclick={() => applyQuick(project, slot)}>
                    {slot.label}
                  </button>
                {/each}
              </div>
              <button
                class="toggle"
                class:on={project.schedule}
                aria-pressed={!!project.schedule}
                disabled={busyProjectIds.has(project.id)}
                title={project.schedule ? "Turn schedule off" : "Choose a cadence to turn schedule on"}
                onclick={() => toggle(project)}>
                {project.schedule ? "On" : "Off"}
              </button>
              <button onclick={() => openEditor(project)}>{project.schedule ? "Edit" : "Set up"}</button>
            </div>
          </div>
        {/each}
      </div>
    {/if}

    <div class="manager-footer">
      <small>Push makes the remote match local, including remote-only removals.</small>
      <button class="cancel-btn" onclick={onclose}>Done</button>
    </div>
  </div>
</div>

{#if editorProject}
  <ScheduleEditor
    project={editorProject}
    schedule={editorProject.schedule ?? null}
    nextRun={statuses.get(editorProject.id)?.next_run ?? null}
    onclose={() => editorProject = null}
    onupdated={afterEditor}
  />
{/if}

<style>
  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 300;
  }

  .manager {
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 20px;
    width: min(900px, 92vw);
    max-width: 92vw;
    height: min(820px, calc(100vh - 48px));
    display: flex;
    flex-direction: column;
  }

  .manager-header { display: flex; justify-content: space-between; align-items: flex-start; gap: 16px; margin-bottom: 16px; }
  h2 { font-size: 16px; font-weight: 700; }
  .manager-header p { color: var(--text-muted); font-size: 11px; margin-top: 4px; }
  .legacy-notice { display: flex; justify-content: space-between; align-items: center; gap: 12px; margin-bottom: 10px; padding: 9px 10px; border: 1px solid var(--yellow); border-radius: var(--radius); background: var(--yellow-dim); }
  .legacy-notice div { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
  .legacy-notice strong { color: var(--yellow); font-size: 11px; }
  .legacy-notice small { color: var(--text-muted); font-size: 10px; line-height: 1.35; }
  .history-warning { margin-bottom: 10px; padding: 9px 10px; border: 1px solid var(--yellow); border-radius: var(--radius); background: var(--yellow-dim); color: var(--yellow); font-size: 11px; line-height: 1.35; }
  .close-btn { border: none; background: transparent; color: var(--text-muted); font-size: 22px; line-height: 1; padding: 0 4px; }
  .search-box { display: flex; align-items: center; gap: 8px; margin-bottom: 10px; }
  .search-box svg { width: 16px; height: 16px; flex-shrink: 0; fill: none; stroke: var(--text-muted); stroke-width: 2; stroke-linecap: round; stroke-linejoin: round; }
  .search-box input { flex: 1; width: auto; font-family: var(--font-sans); font-size: 12px; padding: 6px 10px; }
  .quick-panel { background: var(--bg-card); border: 1px solid var(--border); border-radius: var(--radius); padding: 9px 10px; margin-bottom: 10px; }
  .quick-header { display: flex; justify-content: space-between; align-items: center; gap: 8px; font-size: 11px; font-weight: 600; }
  .quick-header small { color: var(--text-muted); font-size: 10px; font-weight: 400; margin-left: 4px; }
  .text-btn { border: none; background: transparent; color: var(--accent); padding: 2px 4px; font-size: 11px; }
  .quick-list { display: flex; flex-wrap: wrap; align-items: center; gap: 6px; margin-top: 7px; }
  .quick-list span { color: var(--text); background: var(--bg-hover); border-radius: 4px; padding: 3px 6px; font-size: 10px; }
  .quick-list small { flex-basis: 100%; color: var(--text-muted); font-size: 10px; }
  .quick-editor { display: flex; flex-direction: column; gap: 6px; margin-top: 8px; }
  .quick-edit-row { display: grid; grid-template-columns: minmax(0, 1fr) 64px 88px; gap: 6px; }
  .quick-edit-row input, .quick-edit-row select { min-width: 0; padding: 5px 7px; font-size: 11px; }
  .save-quicks { align-self: flex-end; font-size: 11px; padding: 5px 9px; }
  .queue-toggle { display: flex; align-items: flex-start; gap: 8px; margin-bottom: 10px; color: var(--text); cursor: pointer; }
  .queue-toggle input { width: 14px; height: 14px; margin-top: 1px; flex-shrink: 0; cursor: pointer; }
  .queue-toggle span { display: flex; flex-direction: column; gap: 2px; font-size: 11px; }
  .queue-toggle small { color: var(--text-muted); font-size: 10px; line-height: 1.35; }
  .manager-summary { display: flex; gap: 8px; align-items: baseline; color: var(--text-muted); font-size: 11px; margin-bottom: 8px; }
  .manager-summary strong { color: var(--accent); font-size: 12px; }
  .schedule-list { flex: 1; min-height: 0; overflow-y: auto; border-top: 1px solid var(--border); }
  .schedule-row { display: flex; align-items: center; justify-content: space-between; gap: 14px; padding: 11px 0; border-bottom: 1px solid var(--border); }
  .schedule-info { min-width: 0; display: flex; flex-direction: column; gap: 3px; }
  .schedule-name { display: flex; align-items: center; gap: 8px; min-width: 0; }
  .schedule-name strong { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 13px; }
  .missing { color: var(--yellow); font-size: 10px; white-space: nowrap; }
  .schedule-detail, .next-run, .state { font-size: 11px; color: var(--text-muted); }
  .schedule-detail { color: var(--text); }
  .schedule-detail.off { color: var(--text-muted); font-style: italic; }
  .next-run { font-family: var(--font-mono); }
  .state.running { color: var(--accent); }
  .state.pending { color: var(--yellow); }
  .row-actions { display: flex; align-items: center; gap: 6px; flex-shrink: 0; }
  .quick-actions { display: flex; align-items: center; gap: 4px; }
  .quick-btn { white-space: nowrap; font-size: 10px !important; padding: 5px 8px !important; }
  .row-actions button { font-size: 11px; padding: 5px 10px; }
  .toggle { min-width: 42px; color: var(--text-muted); }
  .toggle.on { border-color: var(--green); color: var(--green); background: var(--green-dim); }
  .empty { color: var(--text-muted); font-size: 12px; padding: 24px 0; text-align: center; }
  .err { color: var(--red); font-size: 12px; margin: 14px 0; }
  .manager-footer { display: flex; justify-content: space-between; align-items: center; gap: 12px; padding-top: 14px; }
  .manager-footer small { color: var(--text-muted); font-size: 10px; line-height: 1.4; }
  .cancel-btn { font-weight: 600; flex-shrink: 0; }

  @media (max-width: 480px) {
    .manager { padding: 16px; }
    .schedule-row { align-items: flex-start; }
    .row-actions { flex-direction: column; align-items: stretch; }
    .quick-actions { justify-content: flex-end; }
    .manager-footer { align-items: flex-end; }
  }
</style>
