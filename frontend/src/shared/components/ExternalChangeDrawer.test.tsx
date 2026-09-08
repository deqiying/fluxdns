import { App } from "antd";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import type { ExternalWorkspaceState } from "@/shared/config/external-change";
import { ExternalChangeDrawer } from "./ExternalChangeDrawer";

const state: ExternalWorkspaceState = {
  issue: { key: "issue", kind: "files_changed", activeRevision: "active-1", fileRevision: "file-2" },
  dismissedKey: null,
  open: true,
  dirty: false,
  phase: "ready",
  diff: {
    expected: { active_revision: "active-1", observed_file_revision: "file-2" },
    editable: [],
    protected_changes: ["database"],
    parse_error: null,
  },
  operation: null,
  error: null,
};

it("还原操作明确确认只覆盖文件", async () => {
  const user = userEvent.setup();
  const onRestore = vi.fn();
  render(<App><ExternalChangeDrawer state={state} onClose={() => {}} onRestore={onRestore} /></App>);
  expect(screen.getByText("database")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: /还原文件/ }));
  expect(screen.getByText(/不会回滚正在运行的 DNS 配置/)).toBeInTheDocument();
  const buttons = screen.getAllByRole("button", { name: /还原文件/ });
  await user.click(buttons[buttons.length - 1]);
  expect(onRestore).toHaveBeenCalledOnce();
});

it("二次外改显示冲突而不渲染旧差异", () => {
  render(<App><ExternalChangeDrawer state={{ ...state, phase: "conflict" }} onClose={() => {}} onRestore={() => {}} /></App>);
  expect(screen.getByText("配置文件已再次变化，请重新读取差异。")).toBeInTheDocument();
  expect(screen.queryByText("database")).not.toBeInTheDocument();
});
