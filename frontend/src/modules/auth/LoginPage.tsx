import { useRef, useState } from "react";
import { Alert, Button, Card, Form, Input, Space, Spin, Typography } from "antd";
import { Navigate, useLocation, useNavigate } from "react-router-dom";
import type { LoginRequest } from "@/shared/api/types";
import { getSafeErrorMessage } from "@/shared/api/errors";
import { useAuth } from "./AuthProvider";

interface LoginLocationState {
  from?: string;
  sessionExpired?: boolean;
}

export function LoginPage() {
  const [form] = Form.useForm<LoginRequest>();
  const [submitError, setSubmitError] = useState<unknown>();
  const loginInProgress = useRef(false);
  const auth = useAuth();
  const navigate = useNavigate();
  const location = useLocation();
  const state = location.state as LoginLocationState | null;

  if (auth.isLoading) {
    return (
      <div className="fullscreen-state" aria-label="正在读取 WebUI 状态">
        <Spin size="large" />
      </div>
    );
  }

  if (auth.error) {
    return (
      <div className="fullscreen-state">
        <Alert type="error" showIcon message={getSafeErrorMessage(auth.error)} />
      </div>
    );
  }

  if (auth.setupRequired) {
    return <Navigate to="/initialize" replace />;
  }

  if (auth.session && !loginInProgress.current) {
    return <Navigate to="/dashboard" replace />;
  }

  const handleSubmit = async (values: LoginRequest) => {
    setSubmitError(undefined);
    loginInProgress.current = true;
    try {
      await auth.login(values);
      form.setFieldValue("password", "");
      navigate(state?.from && state.from !== "/login" ? state.from : "/dashboard", { replace: true });
    } catch (error) {
      loginInProgress.current = false;
      form.setFieldValue("password", "");
      setSubmitError(error);
    }
  };

  return (
    <main className="login-page">
      <section className="login-visual" aria-label="FluxDNS 介绍">
        <div>
          <div className="brand-mark">FD</div>
        </div>
        <div>
          <span className="login-kicker">Secure DNS observability</span>
          <h1>清晰掌握每一次运行状态。</h1>
          <p>
            FluxDNS WebUI 提供受控、只读的运行时视图。所有管理请求保持同源，查询数据由服务端聚合并完成安全投影。
          </p>
        </div>
        <Typography.Text style={{ color: "#62758c" }}>FluxDNS Management Console</Typography.Text>
      </section>

      <section className="login-panel">
        <Card className="login-card">
          <Space orientation="vertical" size={6} style={{ width: "100%", marginBottom: 28 }}>
            <Typography.Text type="secondary">只读管理界面</Typography.Text>
            <Typography.Title level={2} style={{ margin: 0 }}>
              登录 FluxDNS
            </Typography.Title>
            <Typography.Paragraph type="secondary" style={{ margin: 0 }}>
              使用服务端配置的管理账号继续。
            </Typography.Paragraph>
          </Space>

          {state?.sessionExpired || auth.sessionExpired ? (
            <Alert type="warning" showIcon message="登录状态已过期，请重新登录。" style={{ marginBottom: 20 }} />
          ) : null}
          {submitError ? (
            <Alert type="error" showIcon message={getSafeErrorMessage(submitError)} style={{ marginBottom: 20 }} />
          ) : null}

          <Form form={form} layout="vertical" requiredMark={false} onFinish={handleSubmit}>
            <Form.Item
              label="用户名"
              name="username"
              rules={[{ required: true, message: "请输入用户名" }, { max: 128, message: "用户名过长" }]}
            >
              <Input autoComplete="username" size="large" placeholder="管理账号" />
            </Form.Item>
            <Form.Item
              label="密码"
              name="password"
              rules={[{ required: true, message: "请输入密码" }, { max: 1024, message: "密码过长" }]}
            >
              <Input.Password autoComplete="current-password" size="large" placeholder="密码" />
            </Form.Item>
            <Button className="login-submit" type="primary" htmlType="submit" block loading={auth.isLoggingIn}>
              登录
            </Button>
          </Form>
        </Card>
      </section>
    </main>
  );
}
