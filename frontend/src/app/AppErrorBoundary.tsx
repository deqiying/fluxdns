import { Component, type ErrorInfo, type ReactNode } from "react";
import { Button, Result } from "antd";

interface Props {
  children: ReactNode;
}

interface State {
  failed: boolean;
}

/** 捕获渲染期异常并给出可恢复入口；错误细节不写入页面或持久化存储。 */
export class AppErrorBoundary extends Component<Props, State> {
  state: State = { failed: false };

  static getDerivedStateFromError(): State {
    return { failed: true };
  }

  componentDidCatch(_error: Error, _info: ErrorInfo) {
    // 后续接入 Telemetry 时仅提交稳定错误分类，不上传组件 props 或接口数据。
  }

  render() {
    if (this.state.failed) {
      return (
        <Result
          status="error"
          title="页面无法继续渲染"
          subTitle="请刷新页面重新建立只读会话。"
          extra={<Button onClick={() => window.location.reload()}>刷新页面</Button>}
        />
      );
    }
    return this.props.children;
  }
}
