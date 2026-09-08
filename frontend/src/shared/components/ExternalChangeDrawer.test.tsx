import { App } from "antd";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import type { ExternalWorkspaceState } from "@/shared/config/external-change";
import { ExternalChangeDrawer } from "./ExternalChangeDrawer";

const state: ExternalWorkspaceState = {
  issue: { key: "issue", kind: "files_changed", activeRevision: "active-1", fileRevision: "file-2", operationId: null },
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

it("可勾选多个类型化差异并排除不授权删除的资源", async () => {
  const user = userEvent.setup();
  const onAdopt = vi.fn();
  const onDirty = vi.fn();
  const adoptionState: ExternalWorkspaceState = {
    ...state,
    diff: {
      expected: { active_revision: "active-1", observed_file_revision: "file-2" },
      editable: [
        {
          active: { module: "logs", value: { enable: true, level: "info", path: "old.log" } },
          external: { module: "logs", value: { enable: true, level: "debug", path: "new.log" } },
        },
        {
          active: { module: "hosts", value: { name: "removed", type: "const", format: "hosts", hosts: "127.0.0.1 old" } },
          external: null,
        },
      ],
      protected_changes: [],
      parse_error: null,
    },
  };
  render(<App><ExternalChangeDrawer state={adoptionState} onClose={() => {}} onRestore={() => {}} onAdopt={onAdopt} onDirty={onDirty} /></App>);
  expect(await screen.findByRole("button", { name: "组合采用 1 项" })).toBeEnabled();
  expect(screen.getByRole("checkbox", { name: "采用 Hosts / removed" })).toBeDisabled();
  expect(screen.getByText(/不授权删除/)).toBeInTheDocument();
  await user.click(screen.getByRole("checkbox", { name: "采用 日志" }));
  expect(screen.getByRole("button", { name: "组合采用 0 项" })).toBeDisabled();
  await user.click(screen.getByRole("checkbox", { name: "采用 日志" }));
  await user.click(screen.getByRole("button", { name: "组合采用 1 项" }));
  expect(onDirty).toHaveBeenCalledWith(true);
  expect(onAdopt).toHaveBeenCalledWith([{ module: "logs", change: { enable: true, level: "debug", path: "new.log" } }]);
});
