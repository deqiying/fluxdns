import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Button, Descriptions, Form, Input, Select, Switch, Typography } from "antd";
import { Pencil } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import { ConfigFormModal } from "@/shared/components/ConfigFormModal";
import { ConfigStateSummary } from "@/shared/components/ConfigStateSummary";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { configStateEditable, useConfigChangeMutation, useConfigModule } from "@/shared/config/hooks";
import { getSystemConfig } from "./api";

type Schemas = components["schemas"];
type Logs = Schemas["Logs"];

const systemConfigKey = ["api", "v2", "config", "system"] as const;

export function SystemSettingsPage() {
  const systemQuery = useQuery({ queryKey: systemConfigKey, queryFn: ({ signal }) => getSystemConfig(signal) });
  const logsQuery = useConfigModule("logs");
  const mutation = useConfigChangeMutation("logs");
  const [form] = Form.useForm<Logs>();
  const [editing, setEditing] = useState(false);
  const [dirty, setDirty] = useState(false);
  const logs = logsQuery.data?.values.find((item) => item.module === "logs")?.value;

  useEffect(() => {
    if (!editing || !logs) return;
    mutation.reset();
    setDirty(false);
    form.setFieldsValue(logs);
  }, [editing, form, logs]);

  const submit = async () => {
    if (!logsQuery.data) return;
    const value = await form.validateFields();
    try {
      const operation = await mutation.mutateAsync({
        change: { module: "logs", change: value },
        state: logsQuery.data.state,
      });
      if (operation) setEditing(false);
    } catch {
      // 冲突与热应用失败时保留草稿。
    }
  };

  const error = systemQuery.error ?? logsQuery.error;
  return (
    <PageFrame
      title="系统配置"
      description="查看启动路径与 WebUI 监听；日志级别和文件支持类型化热更新。"
      meta={logsQuery.data ? <ConfigStateSummary state={logsQuery.data.state} /> : undefined}
    >
      <PageState
        loading={systemQuery.isLoading || logsQuery.isLoading}
        error={error}
        onRetry={() => { void systemQuery.refetch(); void logsQuery.refetch(); }}
      />
      {systemQuery.data && logs && logsQuery.data ? (
        <div className="settings-sections">
          <section className="settings-section">
            <div>
              <Typography.Title level={4}>工作路径</Typography.Title>
              <Typography.Text type="secondary">活动源配置中的只读路径表达</Typography.Text>
            </div>
            <Descriptions column={{ xs: 1, md: 2 }}>
              <Descriptions.Item label="work.path"><Typography.Text code>{systemQuery.data.work_path}</Typography.Text></Descriptions.Item>
              <Descriptions.Item label="rules.path"><Typography.Text code>{systemQuery.data.rules_path}</Typography.Text></Descriptions.Item>
              <Descriptions.Item label="database.path"><Typography.Text code>{systemQuery.data.database_path}</Typography.Text></Descriptions.Item>
              <Descriptions.Item label="records.path"><Typography.Text code>{systemQuery.data.records_path}</Typography.Text></Descriptions.Item>
            </Descriptions>
          </section>
          <section className="settings-section">
            <div>
              <Typography.Title level={4}>WebUI 监听</Typography.Title>
              <Typography.Text type="secondary">启动配置中的只读管理面入口</Typography.Text>
            </div>
            <Descriptions column={{ xs: 1, sm: 2, md: 4 }}>
              <Descriptions.Item label="状态">{systemQuery.data.webui_enabled ? "启用" : "禁用"}</Descriptions.Item>
              <Descriptions.Item label="地址">{systemQuery.data.webui_address}</Descriptions.Item>
              <Descriptions.Item label="端口">{systemQuery.data.webui_port}</Descriptions.Item>
              <Descriptions.Item label="公开来源">{systemQuery.data.public_origin ?? "未配置"}</Descriptions.Item>
            </Descriptions>
          </section>
          <section className="settings-section">
            <div>
              <Typography.Title level={4}>日志</Typography.Title>
              <Typography.Text type="secondary">变更会重新加载日志输出，不修改系统路径或 WebUI 监听</Typography.Text>
            </div>
            <Descriptions column={{ xs: 1, sm: 3 }}>
              <Descriptions.Item label="状态">{logs.enable ? "启用" : "禁用"}</Descriptions.Item>
              <Descriptions.Item label="级别">{logs.level}</Descriptions.Item>
              <Descriptions.Item label="文件"><Typography.Text code>{logs.path}</Typography.Text></Descriptions.Item>
            </Descriptions>
            <Button icon={<Pencil size={16} />} disabled={!configStateEditable(logsQuery.data.state)} onClick={() => setEditing(true)}>编辑日志</Button>
          </section>
        </div>
      ) : null}
      <ConfigFormModal open={editing} title="编辑日志" dirty={dirty} busy={mutation.isPending} error={mutation.error} onCancel={() => setEditing(false)} onSubmit={() => void submit()}>
        <Form form={form} layout="vertical" requiredMark="optional" onValuesChange={() => setDirty(true)}>
          <Form.Item name="enable" label="启用日志" valuePropName="checked"><Switch /></Form.Item>
          <Form.Item name="level" label="日志级别" rules={[{ required: true }]}><Select options={["trace", "debug", "info", "warn", "error"].map((value) => ({ label: value, value }))} /></Form.Item>
          <Form.Item name="path" label="日志文件" rules={[{ required: true }, { max: 4096 }]}><Input /></Form.Item>
        </Form>
      </ConfigFormModal>
    </PageFrame>
  );
}
