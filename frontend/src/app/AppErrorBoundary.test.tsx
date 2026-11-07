import { Suspense } from "react";
import { act, render, screen } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
import { AppErrorBoundary } from "./AppErrorBoundary";
import { lazyPage, PageLoadError } from "./lazy-page";

beforeEach(() => {
  // React 自身也会输出捕获到的异常，预期失败用例只核对本应用的安全分类日志。
  vi.spyOn(console, "error").mockImplementation(() => {});
});

it("正常子页面保持可见", () => {
  render(<AppErrorBoundary><div>正常页面</div></AppErrorBoundary>);
  expect(screen.getByText("正常页面")).toBeInTheDocument();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it("加载期间保留 Suspense，模块就绪后正常渲染", async () => {
  let complete!: (value: { default: () => React.JSX.Element }) => void;
  const Page = lazyPage(() => new Promise((resolve) => { complete = resolve; }));
  render(<AppErrorBoundary><Suspense fallback="正在加载"><Page /></Suspense></AppErrorBoundary>);
  expect(screen.getByText("正在加载")).toBeInTheDocument();

  await act(async () => { complete({ default: () => <div>加载成功</div> }); });
  expect(screen.getByText("加载成功")).toBeInTheDocument();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it.each([
  new TypeError("Failed to fetch dynamically imported module: /assets/private-old.js"),
  new Error("Unable to preload CSS for /assets/private-old.css"),
  "浏览器未提供标准 Error 对象",
])("模块加载失败统一显示手动恢复入口，不泄漏原因或自动重试：%s", async (cause) => {
  const load = vi.fn().mockRejectedValue(cause);
  const Page = lazyPage(load);
  const tree = <AppErrorBoundary><Suspense fallback="正在加载"><Page /></Suspense></AppErrorBoundary>;
  const { rerender } = render(tree);

  expect(await screen.findByText("页面资源加载失败")).toBeInTheDocument();
  expect(screen.getByText(/服务可能已更新，或网络连接暂时中断/)).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "刷新页面" })).toBeInTheDocument();
  expect(screen.queryByText("页面无法继续渲染")).not.toBeInTheDocument();
  expect(screen.queryByText(/private-old|只读会话|浏览器未提供标准/)).not.toBeInTheDocument();
  expect(console.error).toHaveBeenCalledWith("[FluxDNS] 页面异常", {
    kind: "PAGE_LOAD_FAILED", componentStack: expect.any(String),
  });

  rerender(tree);
  expect(load).toHaveBeenCalledTimes(1);
  expect(window.localStorage.length).toBe(0);
  expect(window.sessionStorage.length).toBe(0);
});

it("模块成功加载后的渲染错误不误报为服务更新", async () => {
  function BrokenPage(): never { throw new Error("private-render-value"); }
  const Page = lazyPage(async () => ({ default: BrokenPage }));
  render(<AppErrorBoundary><Suspense fallback="正在加载"><Page /></Suspense></AppErrorBoundary>);

  expect(await screen.findByText("页面无法继续渲染")).toBeInTheDocument();
  expect(screen.queryByText("页面资源加载失败")).not.toBeInTheDocument();
  expect(screen.queryByText(/private-render-value|只读会话/)).not.toBeInTheDocument();
  expect(console.error).toHaveBeenCalledWith("[FluxDNS] 页面异常", {
    kind: "RENDER_FAILED", componentStack: expect.any(String),
  });
});

it("加载错误保留 cause 供本地诊断而不复制进安全错误正文", () => {
  const cause = new Error("private-module-diagnostic");
  const error = new PageLoadError(cause);
  expect(error.cause).toBe(cause);
  expect(error.name).toBe("PageLoadError");
  expect(error.message).toBe("页面模块加载失败");
});
