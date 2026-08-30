/**
 * Operation-control identity helpers shared by Dashboard and its lightweight
 * regression test. Project names are display labels; project_id is the join
 * key that survives a rename.
 */
export function activeProjectIds(operations) {
  return new Set(operations.map((operation) => operation.project_id));
}

export function cancelTarget(project, runningProjectIds) {
  return runningProjectIds.has(project.id) ? project.id : null;
}

export function scheduleStatusMap(statuses) {
  return new Map(statuses.map((status) => [status.project_id, status]));
}

export function setProjectStatus(statuses, projectId, status) {
  const next = new Map(statuses);
  next.set(projectId, status);
  return next;
}

export function markProjectModified(statuses, projectId) {
  const current = statuses.get(projectId);
  if (!current || current.state !== "synced") return statuses;
  return setProjectStatus(statuses, projectId, { ...current, state: "modified", diffs: -1 });
}

export function toggleProjectId(ids, projectId) {
  const next = new Set(ids);
  if (next.has(projectId)) next.delete(projectId);
  else next.add(projectId);
  return next;
}

/**
 * Join backend snapshots and scheduled events by immutable project ID. The
 * event revision guard belongs here so the Dashboard and its regression test
 * exercise the same reload-race behavior.
 */
export function reconcileOperationSnapshots(statuses, operations, eventStates, revisionAtRequest) {
  const nextStatuses = scheduleStatusMap(statuses);
  for (const [projectId, event] of eventStates) {
    if (event.revision <= revisionAtRequest) continue;
    const current = nextStatuses.get(projectId);
    if (current) nextStatuses.set(projectId, { ...current, ...event });
  }

  const activeOperations = operations.filter((operation) => {
    const event = eventStates.get(operation.project_id);
    return !(event?.revision > revisionAtRequest && event.terminal);
  });
  return { statuses: nextStatuses, activeOperations };
}

export function applyScheduleEvent(statuses, event) {
  const current = statuses.get(event.project_id);
  if (!current) return statuses;
  const patch = event.phase === "started"
    ? { pending: false, running: true, scheduled_running: true, terminal: false }
    : event.phase === "deferred"
      ? { pending: true }
      : { pending: false, running: false, scheduled_running: false, terminal: true };
  return new Map([...statuses, [event.project_id, { ...current, ...patch }]]);
}

export function clearOperationState(runningProjects, runningDisplayNames, progress, projectId) {
  const nextRunning = new Map(runningProjects);
  nextRunning.delete(projectId);
  const nextDisplayNames = new Map(runningDisplayNames);
  nextDisplayNames.delete(projectId);
  const nextProgress = new Map(progress);
  nextProgress.delete(projectId);
  return {
    runningProjects: nextRunning,
    runningDisplayNames: nextDisplayNames,
    progress: nextProgress,
  };
}
