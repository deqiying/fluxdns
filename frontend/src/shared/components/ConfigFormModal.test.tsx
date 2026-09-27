import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it } from "vitest";
import { App } from "antd";
import { ApiError } from "@/shared/api/errors";
import { ConfigFormModal, formFieldErrors } from "./ConfigFormModal";

it("脏表单关闭需要显式确认", async () => {
  const user = userEvent.setup();
  let closed = false;
  render(
    <App>
      <ConfigFormModal
        open
        title="编辑日志"
        dirty
        onCancel={() => { closed = true; }}
        onSubmit={() => {}}
      >
        <div>form content</div>
      </ConfigFormModal>
    </App>,
  );

  await user.click(screen.getByRole("button", { name: /取\s*消/ }));
  expect(closed).toBe(false);
  await user.click(await screen.findByRole("button", { name: "放弃修改" }));
  expect(closed).toBe(true);
});

it("错误展示安全文案和 request ID，并提供字段定位", () => {
  const error = new ApiError({
    code: "VALIDATION_FAILED",
    message: "unsafe detail",
    kind: "http",
    requestId: "request-safe",
    fieldErrors: [{ path: "/changes/0/change/path", code: "INVALID_ARGUMENT" }],
  });
  render(
    <App>
      <ConfigFormModal open title="编辑日志" dirty={false} error={error} onCancel={() => {}} onSubmit={() => {}}>
        <div>form content</div>
      </ConfigFormModal>
    </App>,
  );
  expect(screen.getByText("请求失败，请稍后重试。")).toBeInTheDocument();
  expect(screen.getByText("请求 ID：request-safe")).toBeInTheDocument();
  expect(screen.queryByText("unsafe detail")).not.toBeInTheDocument();
  expect(formFieldErrors(error)).toEqual([{ name: ["changes", "0", "change", "path"], errors: ["INVALID_ARGUMENT"] }]);
});
