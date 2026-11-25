import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it } from "vitest";
import { App } from "antd";
import { ApiError } from "@/shared/api/errors";
import { ConfigFormModal } from "./ConfigFormModal";

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

it("顶部错误展示安全诊断和请求 ID", () => {
  const error = new ApiError({
    code: "VALIDATION_FAILED",
    message: "unsafe detail",
    kind: "http",
    requestId: "request-safe",
    fieldErrors: [{ path: "/changes/0/change/path", code: "INVALID_ARGUMENT" }],
    diagnostic: { stage: "validate", code: "INVALID_RESOURCE", message: "<img src=x onerror=alert(1)>", resource: "guard" },
  });
  render(
    <App>
      <ConfigFormModal open title="编辑日志" dirty={false} error={error} onCancel={() => {}} onSubmit={() => {}}>
        <div>form content</div>
      </ConfigFormModal>
    </App>,
  );
  const alert = screen.getByRole("alert");
  expect(alert).toHaveTextContent("配置校验失败");
  expect(alert).toHaveTextContent("请求 ID：request-safe");
  expect(alert).toHaveTextContent("字段 /changes/0/change/path：INVALID_ARGUMENT");
  expect(alert).toHaveTextContent("<img src=x onerror=alert(1)>");
  expect(alert.querySelector("img")).toBeNull();
  expect(screen.queryByText("unsafe detail")).not.toBeInTheDocument();
});
