import { ApiError } from "@/shared/api/errors";
import type { ConfigState, ExternalDiff, OperationResult } from "./api";

export type ExternalIssueKind =
  | "files_changed"
  | "files_missing"
  | "files_unreadable"
  | "files_oversized"
  | "applied_unpersisted"
  | "blocked";

export interface ExternalIssue {
  key: string;
  kind: ExternalIssueKind;
  activeRevision: string;
  fileRevision: string;
  operationId: string | null;
}

export interface ExternalWorkspaceState {
  issue: ExternalIssue | null;
  dismissedKey: string | null;
  open: boolean;
  dirty: boolean;
  phase: "idle" | "loading" | "ready" | "restoring" | "applying" | "awaiting_state" | "conflict" | "error";
  diff: ExternalDiff | null;
  operation: OperationResult | null;
  error: unknown;
}

export type ExternalWorkspaceAction =
  | { type: "snapshot"; state: ConfigState }
  | { type: "dismiss" }
  | { type: "open" }
  | { type: "close" }
  | { type: "load" }
  | { type: "loaded"; diff: ExternalDiff }
  | { type: "dirty"; value: boolean }
  | { type: "restore" }
  | { type: "apply" }
  | { type: "operation"; operation: OperationResult }
  | { type: "failure"; error: unknown };

export const initialExternalWorkspaceState: ExternalWorkspaceState = {
  issue: null,
  dismissedKey: null,
  open: false,
  dirty: false,
  phase: "idle",
  diff: null,
  operation: null,
  error: null,
};

export function externalWorkspaceReducer(
  current: ExternalWorkspaceState,
  action: ExternalWorkspaceAction,
): ExternalWorkspaceState {
  switch (action.type) {
    case "snapshot": {
      const issue = externalIssueFromState(action.state);
      if (!issue) return initialExternalWorkspaceState;
      if (issue.key === current.issue?.key) return { ...current, issue };
      return {
        ...initialExternalWorkspaceState,
        issue,
        open: current.open,
        phase: current.open ? "loading" : "idle",
      };
    }
    case "dismiss":
      return { ...current, dismissedKey: current.issue?.key ?? null };
    case "open":
      return { ...current, open: true, phase: current.diff ? current.phase : "loading" };
    case "close":
      return { ...current, open: false };
    case "load":
      return { ...current, phase: "loading", error: null };
    case "loaded":
      if (!current.issue
        || action.diff.expected.active_revision !== current.issue.activeRevision
        || action.diff.expected.observed_file_revision !== current.issue.fileRevision) {
        return { ...current, phase: "conflict", error: null };
      }
      return { ...current, phase: "ready", diff: action.diff, dirty: false, error: null };
    case "dirty":
      return { ...current, dirty: action.value };
    case "restore":
      return { ...current, phase: "restoring", error: null };
    case "apply":
      return { ...current, phase: "applying", error: null };
    case "operation":
      return { ...current, phase: "awaiting_state", operation: action.operation, error: null };
    case "failure":
      return {
        ...current,
        phase: action.error instanceof ApiError && action.error.code === "FILE_REVISION_CONFLICT"
          ? "conflict"
          : "error",
        error: action.error,
      };
  }
}

export function externalIssueFromState(state: ConfigState): ExternalIssue | null {
  const kind = issueKind(state);
  if (!kind) return null;
  return {
    key: JSON.stringify([
      state.active_revision,
      state.observed_file_revision,
      state.synchronization,
      state.files.source,
      state.files.derived,
      state.operation_id,
    ]),
    kind,
    activeRevision: state.active_revision,
    fileRevision: state.observed_file_revision,
    operationId: state.operation_id,
  };
}

export function isExternalBannerVisible(state: ExternalWorkspaceState): boolean {
  return state.issue !== null && state.issue.key !== state.dismissedKey;
}

function issueKind(state: ConfigState): ExternalIssueKind | null {
  if (state.synchronization === "blocked") return "blocked";
  if (state.synchronization === "applied_unpersisted") return "applied_unpersisted";
  const conditions = [state.files.source, state.files.derived].filter((value) => value !== null);
  if (conditions.includes("oversized")) return "files_oversized";
  if (conditions.includes("unreadable")) return "files_unreadable";
  if (conditions.includes("missing")) return "files_missing";
  if (conditions.includes("changed")) return "files_changed";
  return null;
}
