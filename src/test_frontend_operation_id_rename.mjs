import assert from "node:assert/strict";
import {
  activeProjectIds,
  applyScheduleEvent,
  cancelTarget,
  clearOperationState,
  markProjectModified,
  reconcileOperationSnapshots,
  setProjectStatus,
  toggleProjectId,
} from "./lib/operationState.js";

const currentProject = { id: "p", name: "new" };
const activeSnapshot = [{ project_id: "p", project: "old", mode: "push" }];
const runningIds = activeProjectIds(activeSnapshot);

assert.equal(runningIds.has(currentProject.id), true);
assert.equal(cancelTarget(currentProject, runningIds), "p");
assert.equal(cancelTarget({ id: "other", name: "other" }, runningIds), null);

const scheduleStatuses = new Map([[
  "p",
  { project_id: "p", project: "new", pending: false, running: true, scheduled_running: true, terminal: false },
]]);
const afterTerminal = applyScheduleEvent(scheduleStatuses, {
  project_id: "p",
  project: "old",
  phase: "succeeded",
});
assert.equal(afterTerminal.get("p").running, false);
assert.equal(afterTerminal.get("p").terminal, true);

const reloaded = reconcileOperationSnapshots(
  [{ project_id: "p", project: "new", pending: false, running: false, scheduled_running: false }],
  [{ project_id: "p", project: "old", mode: "push", scheduled: true }],
  new Map([["p", { revision: 2, terminal: true, pending: false, running: false }]]),
  1,
);
assert.equal(reloaded.statuses.get("p").running, false);
assert.equal(reloaded.activeOperations.length, 0);

const duplicateNames = reconcileOperationSnapshots(
  [
    { project_id: "p1", project: "same", pending: false, running: true },
    { project_id: "p2", project: "same", pending: true, running: false },
  ],
  [{ project_id: "p1", project: "same", mode: "push", scheduled: true }],
  new Map(),
  0,
);
assert.equal(duplicateNames.statuses.get("p1").project_id, "p1");
assert.equal(duplicateNames.statuses.get("p2").project_id, "p2");
assert.equal(duplicateNames.activeOperations[0].project_id, "p1");

const independentStatuses = new Map([
  ["p1", { state: "synced", diffs: 0 }],
  ["p2", { state: "synced", diffs: 0 }],
]);
const modifiedFirst = markProjectModified(independentStatuses, "p1");
assert.equal(modifiedFirst.get("p1").state, "modified");
assert.equal(modifiedFirst.get("p2").state, "synced");
const updatedSecond = setProjectStatus(modifiedFirst, "p2", { state: "diffs", diffs: 3 });
assert.equal(updatedSecond.get("p1").state, "modified");
assert.equal(updatedSecond.get("p2").diffs, 3);

const pinned = toggleProjectId(new Set(), "p1");
assert.equal(pinned.has("p1"), true);
assert.equal(toggleProjectId(pinned, "p2").has("p1"), true);
assert.equal(toggleProjectId(pinned, "p1").has("p1"), false);

const cleared = clearOperationState(
  new Map([["p", "push"]]),
  new Map([["p", "old"]]),
  new Map([["p", "bytes"]]),
  "p",
);
assert.equal(cleared.runningProjects.has("p"), false);
assert.equal(cleared.progress.has("p"), false);
console.log("operation-id-rename: passed");
