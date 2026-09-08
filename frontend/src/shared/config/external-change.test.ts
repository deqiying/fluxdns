import { describe, expect, it } from "vitest";
import { ApiError } from "@/shared/api/errors";
import type { ConfigState, ExternalDiff } from "./api";
import {
  externalWorkspaceReducer,
  initialExternalWorkspaceState,
  isExternalBannerVisible,
} from "./external-change";

function configState(fileRevision = "file-2", source: ConfigState["files"]["source"] = "changed"): ConfigState {
  return {
    active_revision: "active-1",
    runtime_revision: "runtime-1",
    persisted_revision: "active-1",
    observed_file_revision: fileRevision,
    files: { source, derived: "unchanged" },
    synchronization: "synced",
    operation_id: null,
  };
}

function diff(fileRevision = "file-2"): ExternalDiff {
  return {
    expected: { active_revision: "active-1", observed_file_revision: fileRevision },
    editable: [],
    protected_changes: [],
    parse_error: null,
  };
}

describe("外部配置变化状态机", () => {
  it("关闭提示不解决问题，同一事实保持关闭，新版本重新提示", () => {
    const observed = externalWorkspaceReducer(initialExternalWorkspaceState, { type: "snapshot", state: configState() });
    expect(isExternalBannerVisible(observed)).toBe(true);
    const dismissed = externalWorkspaceReducer(observed, { type: "dismiss" });
    expect(dismissed.issue).not.toBeNull();
    expect(isExternalBannerVisible(dismissed)).toBe(false);
    const same = externalWorkspaceReducer(dismissed, { type: "snapshot", state: configState() });
    expect(isExternalBannerVisible(same)).toBe(false);
    const changedAgain = externalWorkspaceReducer(same, { type: "snapshot", state: configState("file-3") });
    expect(isExternalBannerVisible(changedAgain)).toBe(true);
  });

  it("差异和还原绑定观察版本，操作成功后仍等待权威状态确认", () => {
    let state = externalWorkspaceReducer(initialExternalWorkspaceState, { type: "snapshot", state: configState() });
    state = externalWorkspaceReducer(state, { type: "open" });
    expect(externalWorkspaceReducer(state, { type: "loaded", diff: diff("stale") }).phase).toBe("conflict");
    state = externalWorkspaceReducer(state, { type: "loaded", diff: diff() });
    state = externalWorkspaceReducer(state, { type: "restore" });
    state = externalWorkspaceReducer(state, {
      type: "operation",
      operation: { operation_id: "restore-1", status: { state: "applied_synced", active_revision: "active-1", persisted_revision: "active-1" } },
    });
    expect(state.phase).toBe("awaiting_state");
    expect(state.issue).not.toBeNull();
    const resolved = externalWorkspaceReducer(state, { type: "snapshot", state: configState("file-4", "unchanged") });
    expect(resolved).toEqual(initialExternalWorkspaceState);
  });

  it("二次外改冲突保留差异与脏草稿", () => {
    let state = externalWorkspaceReducer(initialExternalWorkspaceState, { type: "snapshot", state: configState() });
    state = externalWorkspaceReducer(state, { type: "loaded", diff: diff() });
    state = externalWorkspaceReducer(state, { type: "dirty", value: true });
    const error = new ApiError({ code: "FILE_REVISION_CONFLICT", message: "changed", kind: "http", status: 409 });
    state = externalWorkspaceReducer(state, { type: "failure", error });
    expect(state.phase).toBe("conflict");
    expect(state.dirty).toBe(true);
    expect(state.diff).toEqual(diff());
  });
});
