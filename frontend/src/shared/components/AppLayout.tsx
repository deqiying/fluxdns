import { useState } from "react";
import type { MenuProps } from "antd";
import { Avatar, Breadcrumb, Button, Drawer, Flex, Layout, Menu, Tooltip, Typography, message } from "antd";
import {
  Activity,
  FileCode2,
  Gauge,
  Laptop,
  Layers3,
  ListFilter,
  LogOut,
  Menu as MenuIcon,
  Network,
  PanelLeftClose,
  PanelLeftOpen,
  Radio,
  Settings2,
  SlidersHorizontal,
  Waypoints,
  Workflow,
  type LucideIcon,
} from "lucide-react";
import { Outlet, useLocation, useNavigate } from "react-router-dom";
import { managementRoutes, type ManagementPath } from "@/app/route-contract";
import { getSafeErrorMessage } from "@/shared/api/errors";
import { useAuth } from "@/modules/auth/AuthProvider";
import { ConfigFileStatus } from "./ConfigFileStatus";

const { Header, Content, Sider } = Layout;

const navigationGroups = [
  { key: "monitor", label: "监控" },
  { key: "dns", label: "DNS 管理" },
  { key: "system", label: "系统" },
] as const;

const routeIcons: Record<ManagementPath, LucideIcon> = {
  "/dashboard": Activity,
  "/queries": ListFilter,
  "/listeners": Radio,
  "/upstreams": Network,
  "/dns-settings": SlidersHorizontal,
  "/strategies": Workflow,
  "/hosts": FileCode2,
  "/rule-sets": Layers3,
  "/clients": Laptop,
  "/proxies": Waypoints,
  "/system-settings": Settings2,
  "/system-runtime": Gauge,
};

const menuItems: MenuProps["items"] = navigationGroups.map((group) => ({
  key: group.key,
  type: "group",
  label: group.label,
  children: managementRoutes
    .filter((route) => route.group === group.key)
    .map((route) => {
      const Icon = routeIcons[route.path];
      return {
        key: route.path,
        icon: <Icon size={18} strokeWidth={1.8} aria-hidden="true" />,
        label: route.title,
      };
    }),
}));

export function AppLayout() {
  const [collapsed, setCollapsed] = useState(false);
  const [mobileNavigationOpen, setMobileNavigationOpen] = useState(false);
  const [messageApi, messageContext] = message.useMessage();
  const location = useLocation();
  const navigate = useNavigate();
  const auth = useAuth();
  const current = managementRoutes.find(
    (route) => location.pathname === route.path || location.pathname === `${route.path}/`,
  );
  const currentGroup = navigationGroups.find((group) => group.key === current?.group);
  const userName = auth.session?.user.name ?? "Administrator";
  const userInitial = userName.trim().charAt(0).toUpperCase() || "A";

  const handleLogout = async () => {
    try {
      await auth.logout();
    } catch (error) {
      messageApi.error(getSafeErrorMessage(error));
    }
  };

  const handleNavigate: MenuProps["onClick"] = ({ key }) => {
    navigate(key);
    setMobileNavigationOpen(false);
  };

  const navigationMenu = (className: string) => (
    <Menu
      aria-label="主导航"
      className={className}
      mode="inline"
      selectedKeys={current ? [current.path] : []}
      items={menuItems}
      onClick={handleNavigate}
    />
  );

  const accountPanel = (compact = false) => (
    <div className={`sidebar-account${compact ? " sidebar-account-compact" : ""}`}>
      <Avatar size={32}>{userInitial}</Avatar>
      {!compact ? (
        <div className="sidebar-account-copy">
          <Typography.Text ellipsis title={userName}>{userName}</Typography.Text>
          <Typography.Text type="secondary">管理员</Typography.Text>
        </div>
      ) : null}
      <Tooltip title="退出登录" placement="top">
        <Button
          aria-label="退出登录"
          type="text"
          icon={<LogOut size={18} aria-hidden="true" />}
          loading={auth.isLoggingOut}
          onClick={() => void handleLogout()}
        />
      </Tooltip>
    </div>
  );

  return (
    <Layout className="app-shell">
      {messageContext}
      <Sider className="app-sider" theme="light" width={244} collapsedWidth={72} collapsed={collapsed} trigger={null}>
        <div className="app-sidebar-content">
          <div className="brand">
            <span className="brand-mark"><Network size={20} strokeWidth={1.9} aria-hidden="true" /></span>
            {!collapsed ? <strong>FluxDNS</strong> : null}
          </div>
          <div className="sidebar-navigation">{navigationMenu("app-menu")}</div>
          <div className="sidebar-footer">
            {accountPanel(collapsed)}
            <Tooltip title={collapsed ? "展开导航" : "收起导航"} placement="right">
              <Button
                className="sidebar-collapse"
                aria-label={collapsed ? "展开导航" : "收起导航"}
                type="text"
                icon={collapsed ? <PanelLeftOpen size={18} aria-hidden="true" /> : <PanelLeftClose size={18} aria-hidden="true" />}
                onClick={() => setCollapsed((value) => !value)}
              />
            </Tooltip>
          </div>
        </div>
      </Sider>
      <Drawer
        className="mobile-navigation-drawer"
        title="FluxDNS"
        placement="left"
        size={280}
        open={mobileNavigationOpen}
        onClose={() => setMobileNavigationOpen(false)}
      >
        {navigationMenu("app-menu mobile-app-menu")}
        {accountPanel()}
      </Drawer>
      <Layout className="app-workspace">
        <Header className="app-header">
          <Flex align="center" justify="space-between" gap={16} style={{ height: "100%" }}>
            <Flex align="center" gap={12} className="header-location">
              <Button
                className="mobile-nav-trigger"
                aria-label="打开导航"
                type="text"
                icon={<MenuIcon size={20} aria-hidden="true" />}
                onClick={() => setMobileNavigationOpen(true)}
              />
              <Breadcrumb
                items={[
                  { title: currentGroup?.label ?? "Management" },
                  { title: current?.title ?? "页面不存在" },
                ]}
              />
            </Flex>
            <Flex align="center" gap={8} className="header-account">
              <Avatar size={30}>{userInitial}</Avatar>
              <Typography.Text ellipsis title={userName}>{userName}</Typography.Text>
            </Flex>
          </Flex>
        </Header>
        <Content>
          <ConfigFileStatus />
          <div className="content-wrap">
            <Outlet />
          </div>
        </Content>
      </Layout>
    </Layout>
  );
}
