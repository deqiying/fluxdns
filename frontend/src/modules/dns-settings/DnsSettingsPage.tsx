import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Button, Descriptions, Form, Input, InputNumber, Select, Space, Switch, Tag, Typography } from "antd";
import { Pencil } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import { ConfigFormModal } from "@/shared/components/ConfigFormModal";
import { ConfigStateSummary } from "@/shared/components/ConfigStateSummary";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { configStateEditable, useConfigChangeMutation, useConfigModule } from "@/shared/config/hooks";
import { getRetentionStatus, previewRetention, retentionStatusKey } from "./api";

type Schemas = components["schemas"];
type Dns = Schemas["Dns"];
type Statistics = Schemas["Statistics"];

interface DnsFormValues {
  cache_enabled: boolean;
  cache_size_bytes: number;
  failure_ttl: string;
  optimistic_enabled: boolean;
  optimistic_answer_ttl: string;
  optimistic_max_age: string;
  snapshot_enabled: boolean;
  snapshot_path: string;
  snapshot_interval: string;
  ttl_mode: "inherit" | "enabled" | "disabled";
  ttl_min?: string;
  ttl_max?: string;
  ecs_mode: "disabled" | "client" | "custom";
  ecs_custom_ip?: string;
  resolve_log_enable: boolean;
}

interface RetentionFormValues {
  days: number;
  grace_days: number;
  reference_size_bytes: number;
}

export function DnsSettingsPage() {
  const dnsQuery = useConfigModule("dns");
  const statisticsQuery = useConfigModule("statistics");
  const retentionQuery = useQuery({ queryKey: retentionStatusKey, queryFn: ({ signal }) => getRetentionStatus(signal) });
  const dnsMutation = useConfigChangeMutation("dns");
  const statisticsMutation = useConfigChangeMutation("statistics");
  const [dnsForm] = Form.useForm<DnsFormValues>();
  const [retentionForm] = Form.useForm<RetentionFormValues>();
  const ttlMode = Form.useWatch("ttl_mode", dnsForm);
  const ecsMode = Form.useWatch("ecs_mode", dnsForm);
  const [editor, setEditor] = useState<"dns" | "retention" | null>(null);
  const [dirty, setDirty] = useState(false);
  const dns = dnsQuery.data?.values.find((item) => item.module === "dns")?.value;
  const statistics = statisticsQuery.data?.values.find((item) => item.module === "statistics")?.value;

  useEffect(() => {
    if (!editor) return;
    setDirty(false);
    if (editor === "dns") {
      dnsMutation.reset();
      const cache = dns?.cache;
      dnsForm.setFieldsValue({
        cache_enabled: cache?.enabled ?? false,
        cache_size_bytes: cache?.memory.max_size_bytes ?? 67_108_864,
        failure_ttl: cache?.failure_ttl ?? "5s",
        optimistic_enabled: cache?.optimistic.enabled ?? false,
        optimistic_answer_ttl: cache?.optimistic.answer_ttl ?? "10s",
        optimistic_max_age: cache?.optimistic.max_age ?? "24h",
        snapshot_enabled: cache?.persistence?.enabled ?? false,
        snapshot_path: cache?.persistence?.path ?? "./data/dns-cache.fdcs",
        snapshot_interval: cache?.persistence?.snapshot_interval ?? "5m",
        ttl_mode: dns?.ttl_override ? (dns.ttl_override.enabled === false ? "disabled" : "enabled") : "inherit",
        ttl_min: dns?.ttl_override?.min,
        ttl_max: dns?.ttl_override?.max,
        ecs_mode: dns?.edns_client_subnet?.mode ?? "disabled",
        ecs_custom_ip: dns?.edns_client_subnet?.custom_ip,
        resolve_log_enable: dns?.resolve_log?.enable ?? false,
      });
    } else {
      statisticsMutation.reset();
      retentionForm.setFieldsValue({
        days: statistics?.retention?.days ?? 7,
        grace_days: statistics?.retention?.grace_days ?? 3,
        reference_size_bytes: statistics?.retention?.reference_size_bytes ?? 1_073_741_824,
      });
    }
  }, [dns, editor, dnsForm, retentionForm, statistics]);

  const saveDns = async () => {
    if (!dnsQuery.data) return;
    const values = await dnsForm.validateFields();
    const value: Dns = {
      cache: {
        enabled: values.cache_enabled,
        memory: { max_size_bytes: values.cache_size_bytes },
        failure_ttl: values.failure_ttl,
        optimistic: { enabled: values.optimistic_enabled, answer_ttl: values.optimistic_answer_ttl, max_age: values.optimistic_max_age },
        persistence: { enabled: values.snapshot_enabled, path: values.snapshot_path, snapshot_interval: values.snapshot_interval },
      },
      ...(values.ttl_mode === "inherit" ? {} : { ttl_override: values.ttl_mode === "disabled" ? { enabled: false } : { enabled: true, ...(values.ttl_min ? { min: values.ttl_min } : {}), ...(values.ttl_max ? { max: values.ttl_max } : {}) } }),
      edns_client_subnet: { mode: values.ecs_mode, ...(values.ecs_mode === "custom" && values.ecs_custom_ip ? { custom_ip: values.ecs_custom_ip } : {}) },
      resolve_log: { enable: values.resolve_log_enable },
    };
    try {
      const operation = await dnsMutation.mutateAsync({ change: { module: "dns", change: value }, state: dnsQuery.data.state });
      if (operation) setEditor(null);
    } catch {
      // 失败由表单容器显示。
    }
  };

  const saveRetention = async () => {
    if (!statisticsQuery.data) return;
    const values = await retentionForm.validateFields();
    const value: Statistics = { retention: values };
    try {
      const preview = await previewRetention({
        expected: { active_revision: statisticsQuery.data.state.active_revision, observed_file_revision: statisticsQuery.data.state.observed_file_revision },
        policy: value,
      });
      const detail = `候选截止日期 ${preview.proposed_cutoff_utc_date}，当前详情文件 ${preview.detail_bytes} bytes。保存不会立即删除数据。`;
      const operation = await statisticsMutation.mutateAsync({
        change: { module: "statistics", change: value },
        state: statisticsQuery.data.state,
        confirmationDetails: preview.shortens_history ? { retention_shortening: detail } : undefined,
      });
      if (operation) setEditor(null);
    } catch {
      // 预览、revision 或应用错误统一保留草稿。
    }
  };

  const loading = dnsQuery.isLoading || statisticsQuery.isLoading;
  const error = dnsQuery.error ?? statisticsQuery.error;
  return (
    <PageFrame title="DNS 配置" description="管理缓存、TTL、ECS、详情记录和统计保留；系统路径仍保持只读。" meta={dnsQuery.data ? <ConfigStateSummary state={dnsQuery.data.state} /> : undefined}>
      <PageState loading={loading} error={error} onRetry={() => { void dnsQuery.refetch(); void statisticsQuery.refetch(); }} />
      {dns && statistics && dnsQuery.data && statisticsQuery.data ? (
        <div className="settings-sections">
          <section className="settings-section"><div><Typography.Title level={4}>缓存与解析</Typography.Title><Typography.Text type="secondary">缓存容量、失败 TTL、乐观缓存及持久化快照</Typography.Text></div><Descriptions column={{ xs: 1, sm: 2, md: 4 }}><Descriptions.Item label="缓存">{dns.cache?.enabled ? "启用" : "禁用"}</Descriptions.Item><Descriptions.Item label="内存上限">{dns.cache?.memory.max_size_bytes ?? "默认"} bytes</Descriptions.Item><Descriptions.Item label="快照">{dns.cache?.persistence?.enabled ? dns.cache.persistence.path : "禁用"}</Descriptions.Item><Descriptions.Item label="详情记录">{dns.resolve_log?.enable ? "启用" : "禁用"}</Descriptions.Item></Descriptions><Button icon={<Pencil size={16} />} disabled={!configStateEditable(dnsQuery.data.state)} onClick={() => setEditor("dns")}>编辑 DNS</Button></section>
          <section className="settings-section"><div><Typography.Title level={4}>数据保留</Typography.Title><Typography.Text type="secondary">R/G/T 使用服务端真实 SQLite 与 WAL 长度预览</Typography.Text></div><Descriptions column={{ xs: 1, sm: 2, md: 4 }}><Descriptions.Item label="R">{statistics.retention?.days ?? 7} 天</Descriptions.Item><Descriptions.Item label="G">{statistics.retention?.grace_days ?? 3} 天</Descriptions.Item><Descriptions.Item label="T">{statistics.retention?.reference_size_bytes ?? 1_073_741_824} bytes</Descriptions.Item><Descriptions.Item label="已发布截止">{retentionQuery.data?.cutoff_utc_date ?? "尚未发布"}</Descriptions.Item></Descriptions><Space wrap>{retentionQuery.data ? <Tag>详情 {retentionQuery.data.detail_bytes} bytes</Tag> : null}<Button icon={<Pencil size={16} />} disabled={!configStateEditable(statisticsQuery.data.state)} onClick={() => setEditor("retention")}>编辑保留策略</Button></Space></section>
        </div>
      ) : null}
      <ConfigFormModal open={editor === "dns"} title="编辑 DNS 配置" dirty={dirty} busy={dnsMutation.isPending} error={dnsMutation.error} onCancel={() => setEditor(null)} onSubmit={() => void saveDns()}>
        <Form form={dnsForm} layout="vertical" requiredMark="optional" onValuesChange={() => setDirty(true)}>
          <Form.Item name="cache_enabled" label="启用缓存" valuePropName="checked"><Switch /></Form.Item><Form.Item name="cache_size_bytes" label="内存上限（bytes）" rules={[{ required: true }]}><InputNumber min={1} max={1_099_511_627_776} /></Form.Item><Form.Item name="failure_ttl" label="失败 TTL" rules={[{ required: true }]}><Input /></Form.Item>
          <Form.Item name="optimistic_enabled" label="乐观缓存" valuePropName="checked"><Switch /></Form.Item><Space className="paired-fields" align="start"><Form.Item name="optimistic_answer_ttl" label="回答 TTL" rules={[{ required: true }]}><Input /></Form.Item><Form.Item name="optimistic_max_age" label="最大陈旧时间" rules={[{ required: true }]}><Input /></Form.Item></Space>
          <Form.Item name="snapshot_enabled" label="缓存快照" valuePropName="checked"><Switch /></Form.Item><Form.Item name="snapshot_path" label="快照路径" rules={[{ required: true }]}><Input /></Form.Item><Form.Item name="snapshot_interval" label="快照周期" rules={[{ required: true }]}><Input /></Form.Item>
          <Form.Item name="ttl_mode" label="TTL 覆盖"><Select options={[{ label: "使用默认", value: "inherit" }, { label: "启用", value: "enabled" }, { label: "禁用", value: "disabled" }]} /></Form.Item>{ttlMode === "enabled" ? <Space className="paired-fields" align="start"><Form.Item name="ttl_min" label="最小 TTL"><Input /></Form.Item><Form.Item name="ttl_max" label="最大 TTL"><Input /></Form.Item></Space> : null}
          <Form.Item name="ecs_mode" label="ECS"><Select options={[{ label: "禁用", value: "disabled" }, { label: "客户端地址", value: "client" }, { label: "自定义", value: "custom" }]} /></Form.Item>{ecsMode === "custom" ? <Form.Item name="ecs_custom_ip" label="自定义 ECS" rules={[{ required: true }]}><Input /></Form.Item> : null}<Form.Item name="resolve_log_enable" label="记录解析详情" valuePropName="checked"><Switch /></Form.Item>
        </Form>
      </ConfigFormModal>
      <ConfigFormModal open={editor === "retention"} title="编辑数据保留" dirty={dirty} busy={statisticsMutation.isPending} error={statisticsMutation.error} onCancel={() => setEditor(null)} onSubmit={() => void saveRetention()}>
        <Form form={retentionForm} layout="vertical" requiredMark="optional" onValuesChange={() => setDirty(true)}><Form.Item name="days" label="R：基础保留天数" rules={[{ required: true }]}><InputNumber min={1} max={3650} /></Form.Item><Form.Item name="grace_days" label="G：宽限天数" rules={[{ required: true }]}><InputNumber min={0} max={3649} /></Form.Item><Form.Item name="reference_size_bytes" label="T：参考大小（bytes）" rules={[{ required: true }]}><InputNumber min={1} max={1_099_511_627_776} /></Form.Item><Typography.Text type="secondary">保存前由服务端重新采样详情数据库与 WAL；保存只更新策略，不立即清理。</Typography.Text></Form>
      </ConfigFormModal>
    </PageFrame>
  );
}
