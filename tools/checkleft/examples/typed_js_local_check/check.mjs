// @ts-check

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import {
  readRequest,
  writeResponse,
} from "@checkleft/exec";

/** @import { ExecCheckRequest, Finding } from "@checkleft/exec" */

/**
 * @param {unknown} raw
 * @returns {{ forbiddenCalls: string[]; includeExtensions: string[] }}
 */
function normalizeConfig(raw) {
  const table = raw && typeof raw === "object" && !Array.isArray(raw) ? raw : {};
  const forbiddenCalls = Array.isArray(table.forbidden_calls)
    ? table.forbidden_calls.filter(
        (value) => typeof value === "string" && value.trim() !== "",
      )
    : ["console.log"];
  const includeExtensions = Array.isArray(table.include_extensions)
    ? table.include_extensions.filter(
        (value) => typeof value === "string" && value.trim() !== "",
      )
    : [".js", ".jsx", ".ts", ".tsx"];

  return {
    forbiddenCalls: forbiddenCalls.length > 0 ? forbiddenCalls : ["console.log"],
    includeExtensions:
      includeExtensions.length > 0
        ? includeExtensions
        : [".js", ".jsx", ".ts", ".tsx"],
  };
}

/**
 * @param {string} text
 * @param {string} needle
 * @returns {{ line: number; column: number } | null}
 */
function findLocation(text, needle) {
  const lines = text.split("\n");
  for (let index = 0; index < lines.length; index += 1) {
    const column = lines[index].indexOf(needle);
    if (column >= 0) {
      return { line: index + 1, column: column + 1 };
    }
  }
  return null;
}

/**
 * @param {ExecCheckRequest} request
 * @param {{
 *   repoRoot?: string;
 *   existsSync?: typeof fs.existsSync;
 *   readFileSync?: (absolutePath: string) => string;
 * }} [options]
 * @returns {Finding[]}
 */
export function runCheck(request, options = {}) {
  const repoRoot = options.repoRoot || process.cwd();
  const existsSync = options.existsSync || fs.existsSync;
  const readFileSync =
    options.readFileSync ||
    ((absolutePath) => fs.readFileSync(absolutePath, "utf8"));
  const config = normalizeConfig(request.config);

  /** @type {Finding[]} */
  const findings = [];

  for (const changedFile of request.changeset.changed_files) {
    if (changedFile.kind === "deleted") {
      continue;
    }

    if (!config.includeExtensions.some((ext) => changedFile.path.endsWith(ext))) {
      continue;
    }

    const absolutePath = path.join(repoRoot, changedFile.path);
    if (!existsSync(absolutePath)) {
      continue;
    }

    const contents = readFileSync(absolutePath);
    for (const forbiddenCall of config.forbiddenCalls) {
      const location = findLocation(contents, forbiddenCall);
      if (!location) {
        continue;
      }

      findings.push({
        severity: "warning",
        message: `debug logging via \`${forbiddenCall}\` is not allowed`,
        location: {
          path: changedFile.path,
          line: location.line,
          column: location.column,
        },
        remediation:
          "Remove the debug call or replace it with structured logging that is allowed in this codepath.",
        suggested_fix: null,
      });
    }
  }

  return findings;
}

function isEntrypoint() {
  if (!process.argv[1]) {
    return false;
  }
  return path.resolve(process.argv[1]) === path.resolve(fileURLToPath(import.meta.url));
}

if (isEntrypoint()) {
  const repoRoot = process.env.CHECKLEFT_REPO_ROOT || process.cwd();
  writeResponse(runCheck(readRequest(), { repoRoot }));
}
