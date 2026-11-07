import { Component, type ErrorInfo, type ReactNode } from "react";
import { Button, Result } from "antd";
import { PageLoadError } from "./lazy-page";

interface Props {
  children: ReactNode;
}

interface State {
  failure: "page-load" | "render" | null;
}

/** 区分模块加载与渲染异常，保留手动刷新入口，避免服务重启期间自动刷新循环。 */
export class AppErrorBoundary extends Component<Props, State> {
  state: State = { failure: null };

  static getDerivedStateFromError(error: unknown): State {
    return { failure: error instanceof PageLoadError ? "page-load" : "render" };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // 仅输出稳定分类和组件栈；不主动记录 cause、接口数据、资源 URL 或组件 props。
    console.error("[FluxDNS] 页面异常", {
      kind: error instanceof PageLoadError ? "PAGE_LOAD_FAILED" : "RENDER_FAILED",
      componentStack: info.componentStack,
    });
  }

  render() {
    if (this.state.failure) {
      const pageLoadFailed = this.state.failure === "page-load";
      return (
        <div className="fullscreen-state" role="alert">
          <Result
            status={pageLoadFailed ? "warning" : "error"}
            title={pageLoadFailed ? "页面资源加载失败" : "页面无法继续渲染"}
            subTitle={pageLoadFailed
              ? "服务可能已更新，或网络连接暂时中断。请确认服务可用后刷新页面，以加载最新界面。刷新会丢弃未保存的修改。"
              : "页面发生了意外错误，请刷新后重试。刷新会丢弃未保存的修改；若问题持续，请查看浏览器控制台。"}
            extra={<Button type="primary" onClick={() => window.location.reload()}>刷新页面</Button>}
          />
        </div>
      );
    }
    return this.props.children;
  }
}
