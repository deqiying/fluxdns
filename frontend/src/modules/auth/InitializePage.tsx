import { useState } from "react";
import { Alert, Button, Card, Form, Input, Space, Spin, Typography } from "antd";
import { Navigate, useNavigate } from "react-router-dom";
import type { SetupRequest } from "@/shared/api/types";
import { ApiError, getSafeErrorMessage } from "@/shared/api/errors";
import { useAuth } from "./AuthProvider";

type InitializeFormValues = SetupRequest & { confirmPassword: string };

export function InitializePage() {
  const [form] = Form.useForm<InitializeFormValues>();
  const [submitError, setSubmitError] = useState<unknown>();
  const auth = useAuth();
  const navigate = useNavigate();

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

  if (!auth.setupRequired) {
    return <Navigate to={auth.session ? "/dashboard" : "/login"} replace />;
  }

  const handleSubmit = async (values: InitializeFormValues) => {
    setSubmitError(undefined);
    try {
      await auth.initialize({ username: values.username, password: values.password });
      form.setFieldsValue({ password: "", confirmPassword: "" });
      navigate("/dashboard", { replace: true });
    } catch (error) {
      form.setFieldsValue({ password: "", confirmPassword: "" });
      setSubmitError(error);
      if (error instanceof ApiError && error.status === 409) {
        try {
          const status = await auth.refreshSetup();
          if (status?.state === "ready") {
            navigate("/login", { replace: true });
          }
        } catch {
          // 保留原始 409 文案；刷新失败不会把竞争结果伪装成成功。
        }
      }
    }
  };

  return (
    <main className="login-page">
      <section className="login-visual" aria-label="FluxDNS 初始化介绍">
        <div>
          <div className="brand-mark">FD</div>
        </div>
        <div>
          <span className="login-kicker">Secure DNS observability</span>
          <h1>先创建唯一的管理账号。</h1>
          <p>首次初始化只会写入服务端配置中的 Argon2id hash，明文密码不会被保存或展示。</p>
        </div>
        <Typography.Text style={{ color: "#62758c" }}>FluxDNS Management Console</Typography.Text>
      </section>

      <section className="login-panel">
        <Card className="login-card">
          <Space orientation="vertical" size={6} style={{ width: "100%", marginBottom: 28 }}>
            <Typography.Text type="secondary">首次初始化</Typography.Text>
            <Typography.Title level={2} style={{ margin: 0 }}>
              初始化 FluxDNS
            </Typography.Title>
            <Typography.Paragraph type="secondary" style={{ margin: 0 }}>
              创建完成后会自动建立当前浏览器的管理 session。
            </Typography.Paragraph>
          </Space>

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
              rules={[
                { required: true, message: "请输入密码" },
                { min: 12, message: "密码至少需要 12 个字符" },
                { max: 1024, message: "密码过长" },
              ]}
            >
              <Input.Password autoComplete="new-password" size="large" placeholder="至少 12 个字符" />
            </Form.Item>
            <Form.Item
              label="确认密码"
              name="confirmPassword"
              dependencies={["password"]}
              rules={[
                { required: true, message: "请再次输入密码" },
                ({ getFieldValue }) => ({
                  validator(_, value: string) {
                    return !value || getFieldValue("password") === value
                      ? Promise.resolve()
                      : Promise.reject(new Error("两次输入的密码不一致"));
                  },
                }),
              ]}
            >
              <Input.Password autoComplete="new-password" size="large" placeholder="再次输入密码" />
            </Form.Item>
            <Button className="login-submit" type="primary" htmlType="submit" block loading={auth.isInitializing}>
              创建管理账号
            </Button>
          </Form>
        </Card>
      </section>
    </main>
  );
}
