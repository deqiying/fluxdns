import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import { ExternalChangeBanner } from "./ExternalChangeBanner";

it("外部变化提示提供查看和仅关闭当前提示的动作", async () => {
  const user = userEvent.setup();
  const onOpen = vi.fn();
  const onDismiss = vi.fn();
  render(<ExternalChangeBanner
    issue={{ key: "issue", kind: "files_changed", activeRevision: "active-1", fileRevision: "file-2" }}
    onOpen={onOpen}
    onDismiss={onDismiss}
  />);
  expect(screen.getByText("配置文件已在外部修改")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "查看" }));
  await user.click(screen.getByRole("button", { name: "关闭提示" }));
  expect(onOpen).toHaveBeenCalledOnce();
  expect(onDismiss).toHaveBeenCalledOnce();
});
