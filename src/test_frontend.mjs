const tests = {
  "operation-id-rename": "./test_frontend_operation_id_rename.mjs",
  "settings-project-identity": "./test_frontend_settings_project_identity.mjs",
};
const requested = process.argv[2] || "operation-id-rename";
const test = tests[requested];
if (!test) throw new Error(`Unknown frontend test: ${requested}`);
await import(test);
