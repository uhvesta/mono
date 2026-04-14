// @ts-check

import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { runCheck } from "./check.mjs";

/**
 * @returns {import("@checkleft/exec").ExecCheckRequest}
 */
function baseRequest() {
  return {
    changeset: {
      changed_files: [
        {
          path: "tools/checkleft/examples/typed_js_local_check/fixture.ts",
          kind: "modified",
          old_path: null,
        },
      ],
      file_line_deltas: {},
      file_diffs: {},
      commit_description: null,
      pr_description: null,
      change_id: null,
      repository: null,
    },
    config: {
      forbidden_calls: ["console.log"],
      include_extensions: [".ts"],
    },
  };
}

const fixturePath = fileURLToPath(new URL("./fixture.ts", import.meta.url));
const fixtureText = fs.readFileSync(fixturePath, "utf8");
const repoRoot = "/example-repo";
const expectedPath = path.join(
  repoRoot,
  "tools/checkleft/examples/typed_js_local_check/fixture.ts",
);

const findings = runCheck(baseRequest(), {
  repoRoot,
  existsSync(absolutePath) {
    assert.equal(absolutePath, expectedPath);
    return true;
  },
  readFileSync(absolutePath) {
    assert.equal(absolutePath, expectedPath);
    return fixtureText;
  },
});

assert.equal(findings.length, 1);
assert.deepEqual(findings[0], {
  severity: "warning",
  message: "debug logging via `console.log` is not allowed",
  location: {
    path: "tools/checkleft/examples/typed_js_local_check/fixture.ts",
    line: 2,
    column: 3,
  },
  remediation:
    "Remove the debug call or replace it with structured logging that is allowed in this codepath.",
  suggested_fix: null,
});

const excludedFindings = runCheck(
  {
    ...baseRequest(),
    config: {
      forbidden_calls: ["console.log"],
      include_extensions: [".js"],
    },
  },
  {
    repoRoot,
    existsSync() {
      throw new Error("extension-filtered files should not be read");
    },
    readFileSync() {
      throw new Error("extension-filtered files should not be read");
    },
  },
);

assert.deepEqual(excludedFindings, []);
