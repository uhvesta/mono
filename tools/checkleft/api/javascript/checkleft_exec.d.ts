export type Severity = "error" | "warning" | "info";

export type ChangeKind = "added" | "modified" | "deleted" | "renamed";

export interface FileLineDelta {
  added_lines: number;
  removed_lines: number;
}

export interface DiffHunk {
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
  added_lines: number;
  removed_lines: number;
}

export interface FileDiff {
  hunks: DiffHunk[];
}

export interface ChangedFile {
  path: string;
  kind: ChangeKind;
  old_path: string | null;
}

export interface ChangeSet {
  changed_files: ChangedFile[];
  file_line_deltas: Record<string, FileLineDelta>;
  file_diffs: Record<string, FileDiff>;
  commit_description?: string | null;
  pr_description?: string | null;
  change_id?: string | null;
  repository?: string | null;
}

export interface ExecCheckRequest {
  changeset: ChangeSet;
  config: unknown;
}

export interface Location {
  path: string;
  line: number | null;
  column: number | null;
}

export interface FileEdit {
  path: string;
  old_text: string;
  new_text: string;
}

export interface SuggestedFix {
  description: string;
  edits: FileEdit[];
}

export interface Finding {
  severity: Severity;
  message: string;
  location: Location | null;
  remediation: string | null;
  suggested_fix: SuggestedFix | null;
}

export interface ExecCheckResponse {
  findings: Finding[];
}

export function readRequest(): ExecCheckRequest;

export function writeResponse(findings: Iterable<Finding>): void;
