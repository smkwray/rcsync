import assert from "node:assert/strict";
import { mergeSettingsProjects } from "./lib/settingsState.js";

const first = { id: "a", name: "same", local_path: "~/one" };
const second = { id: "b", name: "same", local_path: "~/two" };

const retained = mergeSettingsProjects(
  [first, second],
  [first, second],
  [second],
);
assert.deepEqual(retained.map((project) => project.id), ["b"]);

const exactIdWins = mergeSettingsProjects(
  [first, second],
  [first, second],
  [{ ...second, name: "same" }],
);
assert.deepEqual(exactIdWins.map((project) => project.id), ["b"]);

const added = { id: "", name: "same", local_path: "~/three" };
const withNew = mergeSettingsProjects([first], [first], [first, added]);
assert.deepEqual(withNew.map((project) => project.id), ["a", ""]);
console.log("settings-project-identity: passed");
