<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import type { IntervalUnit, Project, Schedule, WeeklySchedule } from "./types";

  let {
    project,
    schedule = null,
    nextRun = null,
    onclose,
    onupdated,
  }: {
    project: Project;
    schedule: Schedule | null;
    nextRun: string | null;
    onclose: () => void;
    onupdated: (projectId?: string) => void;
  } = $props();

  let kind = $state<"interval" | "weekly">("interval");
  let every = $state(24);
  let unit = $state<IntervalUnit>("hours");
  let weekdays = $state<number[]>([1, 2, 3, 4, 5]);
  let time = $state("18:00");
  let initialized = $state(false);
  let saving = $state(false);
  let error = $state("");

  const dayNames = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

  $effect(() => {
    if (initialized) return;
    initialized = true;
    kind = schedule?.kind ?? "interval";
    every = schedule?.kind === "interval" ? schedule.every : 24;
    unit = schedule?.kind === "interval" ? schedule.unit : "hours";
    weekdays = schedule?.kind === "weekly" ? [...schedule.weekdays] : [1, 2, 3, 4, 5];
    time = schedule?.kind === "weekly" ? minuteToTime(schedule.minute) : "18:00";
  });

  function minuteToTime(minute: number): string {
    return `${String(Math.floor(minute / 60)).padStart(2, "0")}:${String(minute % 60).padStart(2, "0")}`;
  }

  function toggleDay(day: number) {
    weekdays = weekdays.includes(day)
      ? weekdays.filter((value) => value !== day)
      : [...weekdays, day].sort((a, b) => a - b);
  }

  function summary(): string {
    if (kind === "interval") {
      return `Every ${every} ${unit}${every === 1 ? "" : "s"}`;
    }
    return `${weekdays.map((day) => dayNames[day]).join(", ")} at ${time}`;
  }

  function scheduleValue(): Schedule | null {
    if (kind === "interval") {
      const old = schedule?.kind === "interval" ? schedule : null;
      return { kind, every: Number(every), unit, origin_ms: old && old.every === Number(every) && old.unit === unit ? old.origin_ms : 0 };
    }
    const [hours, minutes] = time.split(":").map(Number);
    return { kind, weekdays: [...weekdays], minute: hours * 60 + minutes } as WeeklySchedule;
  }

  function scheduleChanged(): boolean {
    return JSON.stringify(scheduleValue()) !== JSON.stringify(schedule);
  }

  async function save() {
    error = "";
    const value = scheduleValue();
    if (value?.kind === "interval" && (!Number.isInteger(value.every) || value.every < 1)) {
      error = "Choose an interval of at least 1.";
      return;
    }
    if (value?.kind === "weekly" && value.weekdays.length === 0) {
      error = "Choose at least one day.";
      return;
    }
    saving = true;
    try {
      const durableProjectId = await invoke<string>("set_project_schedule", {
        projectName: project.name,
        projectId: project.id,
        schedule: value,
      });
      onupdated(durableProjectId);
      onclose();
    } catch (e) {
      error = `${e}`;
    } finally {
      saving = false;
    }
  }

  function handleKey(e: KeyboardEvent) {
    if (e.key === "Escape") { e.preventDefault(); onclose(); }
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") { e.preventDefault(); save(); }
  }
</script>

<div class="overlay" role="dialog" aria-modal="true" tabindex="-1" onclick={onclose} onkeydown={handleKey}>
  <div class="schedule-dialog" onclick={(event) => event.stopPropagation()}>
    <div class="dialog-header">
      <h3>Schedule Push — {project.name}</h3>
      <button class="close-btn" onclick={onclose} aria-label="Close">×</button>
    </div>

    <div class="segmented">
      <button class:chosen={kind === "interval"} onclick={() => kind = "interval"}>Every</button>
      <button class:chosen={kind === "weekly"} onclick={() => kind = "weekly"}>On days</button>
    </div>

    {#if kind === "interval"}
      <div class="interval-row">
        <span>Every</span>
        <div class="number-field">
          <input type="number" min="1" max="365" bind:value={every} aria-label="Interval value" />
          <div class="stepper" aria-label="Adjust interval">
            <button type="button" aria-label="Increase interval" disabled={Number(every) >= 365}
              onclick={() => every = Math.min(365, Number(every) + 1)}>
              <svg viewBox="0 0 12 8" aria-hidden="true"><path d="M2 6 6 2l4 4" /></svg>
            </button>
            <button type="button" aria-label="Decrease interval" disabled={Number(every) <= 1}
              onclick={() => every = Math.max(1, Number(every) - 1)}>
              <svg viewBox="0 0 12 8" aria-hidden="true"><path d="m2 2 4 4 4-4" /></svg>
            </button>
          </div>
        </div>
        <select bind:value={unit}>
          <option value="hours">Hours</option>
          <option value="days">Days</option>
        </select>
      </div>
    {:else}
      <div class="day-row">
        {#each dayNames as name, day}
          <button class:chosen={weekdays.includes(day)} onclick={() => toggleDay(day)}>{name}</button>
        {/each}
      </div>
      <label class="time-row">
        <span>Local time</span>
        <input type="time" bind:value={time} />
      </label>
    {/if}

    <div class="summary">
      <strong>{summary()}</strong>
      {#if nextRun && !scheduleChanged()}<span>Next push: {nextRun}</span>{/if}
      {#if scheduleChanged()}<span>Next push is recalculated after saving.</span>{/if}
      <small>Runs only while rcsync is open. Push makes the remote match local, including remote-only removals.</small>
    </div>
    {#if error}<p class="err">{error}</p>{/if}
    <div class="dialog-actions">
      <button class="cancel-btn" onclick={onclose} disabled={saving}>Cancel</button>
      <button class="primary" onclick={save} disabled={saving}>{saving ? "Saving…" : "Save"}</button>
    </div>
  </div>
</div>

<style>
  .overlay { position: fixed; inset: 0; background: rgba(0, 0, 0, 0.6); display: flex; align-items: center; justify-content: center; z-index: 300; }
  .schedule-dialog { background: var(--bg); border: 1px solid var(--border); border-radius: 12px; padding: 20px; width: 390px; max-width: 90vw; }
  .dialog-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 16px; }
  h3 { font-size: 15px; font-weight: 700; }
  .close-btn { border: none; background: transparent; color: var(--text-muted); font-size: 22px; padding: 0 4px; }
  .segmented { display: flex; gap: 4px; margin: 16px 0 12px; }
  .segmented button { flex: 1; }
  .segmented button.chosen, .day-row button.chosen { border-color: var(--accent); color: var(--accent); background: var(--bg-hover); }
  .interval-row, .time-row { display: flex; align-items: center; gap: 8px; font-size: 12px; }
  .number-field { position: relative; width: 82px; flex-shrink: 0; }
  .number-field input { width: 100%; padding-right: 28px; -moz-appearance: textfield; }
  .number-field input::-webkit-inner-spin-button,
  .number-field input::-webkit-outer-spin-button { -webkit-appearance: none; margin: 0; }
  .stepper { position: absolute; top: 1px; right: 1px; bottom: 1px; width: 24px; display: flex; flex-direction: column; border-left: 1px solid var(--border); }
  .stepper button { flex: 1; display: flex; align-items: center; justify-content: center; padding: 0; border: none; border-radius: 0; background: var(--bg-hover); color: var(--text); }
  .stepper button:first-child { border-bottom: 1px solid var(--border); border-radius: 0 5px 0 0; }
  .stepper button:last-child { border-radius: 0 0 5px 0; }
  .stepper button:hover:not(:disabled) { background: var(--accent); color: #fff; }
  .stepper button:disabled { opacity: 0.3; }
  .stepper svg { width: 10px; height: 7px; fill: none; stroke: currentColor; stroke-width: 2; stroke-linecap: round; stroke-linejoin: round; }
  .interval-row select { flex: 1; }
  .day-row { display: flex; gap: 4px; margin-bottom: 14px; }
  .day-row button { flex: 1; padding: 5px 2px; font-size: 11px; }
  .time-row { justify-content: space-between; }
  .time-row input { font-family: var(--font-mono); }
  .summary { display: flex; flex-direction: column; gap: 4px; margin-top: 18px; padding: 10px; border: 1px solid var(--border); border-radius: 6px; color: var(--text-muted); font-size: 11px; line-height: 1.4; }
  .summary strong { color: var(--text); font-size: 12px; }
  .summary small { font-size: 10px; }
  .err { color: var(--red); font-size: 12px; margin-top: 8px; }
  .dialog-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 16px; }
</style>
