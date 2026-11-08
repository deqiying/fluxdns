import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { ApiError } from "@/shared/api/errors";
import { PageState } from "./PageState";

it("繁忙查询保留读取语义、诊断、字段错误及 request ID，刷新失败不移除旧内容", () => {
  const error = new ApiError({ code: "OPERATION_BUSY", message: "unsafe", kind: "http", requestId: "query-request",
    diagnostic: { stage: "read", code: "OWNER_BUSY", message: "配置正在切换", resource: "dns" },
    fieldErrors: [{ path: "/query/filter", code: "INVALID_ARGUMENT" }] });
  render(<><PageState error={error} hasData /><div>旧内容</div></>);
  const alert = screen.getByRole("alert");
  expect(alert).toHaveTextContent("刷新失败，当前显示上次读取的内容");
  expect(alert).toHaveTextContent("配置读取暂时繁忙");
  expect(alert).toHaveTextContent("OWNER_BUSY");
  expect(alert).toHaveTextContent("query-request");
  expect(alert).toHaveTextContent("/query/filter");
  expect(alert).not.toHaveTextContent("unsafe");
  expect(screen.getByText("旧内容")).toBeInTheDocument();
});
