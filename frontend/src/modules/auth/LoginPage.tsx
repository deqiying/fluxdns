import { useRef, useState } from "react";
import { Alert, Button, Form, Input, Space, Spin, Typography } from "antd";
import { Activity, RefreshCw, ShieldCheck, type LucideIcon } from "lucide-react";
import { Navigate, useLocation, useNavigate } from "react-router-dom";
import brandIcon from "@/assets/fluxdns-icon.svg?no-inline";
import { getSafeErrorMessage } from "@/shared/api/errors";
import type { LoginRequest } from "@/shared/api/types";
import { useAuth } from "./AuthProvider";

interface LoginLocationState {
  from?: string;
  sessionExpired?: boolean;
}

/** 左栏三条能力都与当前实现一致，不做未接线的承诺。 */
const loginHighlights: { icon: LucideIcon; title: string; description: string }[] = [
  { icon: Activity, title: "实时运行状态", description: "查询速率、缓存命中与上游延迟在同一视图集中呈现。" },
  { icon: ShieldCheck, title: "配置变更可追溯", description: "改动经过校验后应用，页面持续显示运行与文件同步状态。" },
  { icon: RefreshCw, title: "会话自动续期", description: "访问凭据在到期前静默刷新，长时间停留不会被打断。" },
];

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
        <div className="login-brand">
          <img className="login-brand-mark" src={brandIcon} alt="" width={40} height={40} />
          <strong>FluxDNS</strong>
        </div>

        <div>
          <span className="login-kicker">Secure DNS observability</span>
          <h1>清晰掌握每一次运行状态。</h1>
          <p>
            查看服务状态与解析记录，管理 DNS 配置。配置变更经过校验后应用，页面持续显示运行与文件同步状态。
          </p>
          <ul className="login-highlights">
            {loginHighlights.map(({ icon: Icon, title, description }) => (
              <li key={title}>
                <span className="login-highlight-icon">
                  <Icon size={18} strokeWidth={1.8} aria-hidden="true" />
                </span>
                <div className="login-highlight-copy">
                  <strong>{title}</strong>
                  <span className="login-highlight-text">{description}</span>
                </div>
              </li>
            ))}
          </ul>
        </div>

        <span className="login-footnote">FluxDNS Management Console</span>
      </section>

      <section className="login-panel">
        <div className="login-brand login-brand-mobile">
          <img className="login-brand-mark" src={brandIcon} alt="" width={32} height={32} />
          <strong>FluxDNS</strong>
        </div>

        <div className="login-card">
          <Space orientation="vertical" size={6} style={{ width: "100%", marginBottom: 28 }}>
            <Typography.Text type="secondary">DNS 管理界面</Typography.Text>
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

          <p className="login-card-note">会话在有效期内自动续期，长时间操作不会中断。</p>
        </div>
      </section>
    </main>
  );
}
