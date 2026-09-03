import { useMemo, useState } from "react";
import { Button, Flex, Layout, Menu, Space, Typography, message } from "antd";
import { Outlet, useLocation, useNavigate } from "react-router-dom";
import { getSafeErrorMessage } from "@/shared/api/errors";
import { useAuth } from "@/modules/auth/AuthProvider";

const { Header, Content, Sider } = Layout;

const navigation = [
  { path: "/dashboard", label: "总览", short: "OV" },
  { path: "/runtime", label: "运行时", short: "RT" },
  { path: "/health", label: "健康状态", short: "HL" },
  { path: "/statistics", label: "统计", short: "ST" },
  { path: "/queries", label: "解析详情", short: "QY" },
  { path: "/resources", label: "资源", short: "RS" },
  { path: "/system", label: "系统", short: "SY" },
] as const;

export function AppLayout() {
  const [collapsed, setCollapsed] = useState(false);
  const [messageApi, messageContext] = message.useMessage();
  const location = useLocation();
  const navigate = useNavigate();
  const auth = useAuth();
  const current = navigation.find((item) => location.pathname.startsWith(item.path)) ?? navigation[0];

  const menuItems = useMemo(
    () =>
      navigation.map((item) => ({
        key: item.path,
        icon: <span className="nav-glyph">{item.short}</span>,
        label: item.label,
      })),
    [],
  );

  const handleLogout = async () => {
    try {
      await auth.logout();
    } catch (error) {
      messageApi.error(getSafeErrorMessage(error));
    }
  };

  return (
    <Layout className="app-shell">
      {messageContext}
      <Sider
        className="app-sider"
        width={238}
        collapsedWidth={72}
        collapsible
        collapsed={collapsed}
        onCollapse={setCollapsed}
      >
        <div className="brand">
          <div className="brand-mark">FD</div>
          {!collapsed ? (
            <div className="brand-copy">
              <strong>FluxDNS</strong>
              <span>READ-ONLY CONSOLE</span>
            </div>
          ) : null}
        </div>
        <Menu
          className="app-menu"
          theme="dark"
          mode="inline"
          selectedKeys={[current.path]}
          items={menuItems}
          onClick={({ key }) => navigate(key)}
        />
      </Sider>
      <Layout>
        <Header className="app-header">
          <Flex align="center" justify="space-between" style={{ height: "100%" }}>
            <Space orientation="vertical" size={0}>
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                Management / {current.label}
              </Typography.Text>
              <Typography.Title level={4} className="header-title">
                {current.label}
              </Typography.Title>
            </Space>
            <Space size="middle">
              <Typography.Text>{auth.session?.user.name}</Typography.Text>
              <Button onClick={handleLogout} loading={auth.isLoggingOut}>
                退出
              </Button>
            </Space>
          </Flex>
        </Header>
        <Content>
          <div className="content-wrap">
            <Outlet />
          </div>
        </Content>
      </Layout>
    </Layout>
  );
}
