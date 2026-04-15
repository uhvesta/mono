import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const childPath = fileURLToPath(new URL("./checkleft_exec_child.mjs", import.meta.url));
const request = {
  changeset: {
    changed_files: [],
    file_line_deltas: {},
    file_diffs: {},
  },
  config: {
    hello: "world",
  },
};

const child = spawn(process.execPath, [childPath], {
  stdio: ["pipe", "pipe", "pipe"],
});

let stdout = "";
let stderr = "";
child.stdout.setEncoding("utf8");
child.stderr.setEncoding("utf8");
child.stdout.on("data", (chunk) => {
  stdout += chunk;
});
child.stderr.on("data", (chunk) => {
  stderr += chunk;
});

await new Promise((resolve) => setTimeout(resolve, 50));
child.stdin.end(JSON.stringify(request));

const { code } = await new Promise((resolve) => {
  child.on("exit", (exitCode) => resolve({ code: exitCode }));
});

assert.equal(code, 0, stderr);
assert.deepEqual(JSON.parse(stdout), request);
