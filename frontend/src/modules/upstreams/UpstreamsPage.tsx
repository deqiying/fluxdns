import { useEffect, useMemo, useState } from "react";
import { Button, Form, Input, InputNumber, Segmented, Select, Space, Switch, Table, Tabs, Tag, Tooltip, Typography, type TableColumnsType } from "antd";
import { ArrowDown, ArrowUp, Pencil, Plus, Search, Trash2 } from "lucide-react";
import { useSearchParams } from "react-router-dom";
import type { components } from "@/shared/api/generated-v2";
import { upstreamTabs } from "@/app/route-contract";
import { ConfigFormModal } from "@/shared/components/ConfigFormModal";
import { ConfigStateSummary } from "@/shared/components/ConfigStateSummary";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { configStateEditable, useConfigChangeMutation, useConfigModule } from "@/shared/config/hooks";

type Schemas = components["schemas"];
type Upstream = Schemas["Upstream"];
type Member = Schemas["UpstreamMember"];

interface UpstreamFormValues {
  name: string;
  type: "hosts" | "doh" | "group";
  format?: "json" | "hosts";
  hosts?: string;
  address?: string;
  bootstrap?: string;
  connect_ip?: string;
  proxy?: string;
  ecs_mode?: "disabled" | "client" | "custom";
  ecs_custom_ip?: string;
  upstreams?: Member[];
  upstream_mode?: Schemas["UpstreamMode"];
  timeout?: string;
  fallback_enabled?: boolean;
  fallbacks?: Member[];
  fallback_upstream_mode?: Schemas["UpstreamMode"];
  fallback_timeout?: string;
}

const modeOptions = [
  { label: "并行", value: "parallel" },
  { label: "轮询", value: "round-robin" },
  { label: "负载均衡", value: "load-balance" },
  { label: "故障切换", value: "failover" },
];

export function UpstreamsPage() {
  const query = useConfigModule("upstreams");
  const proxies = useConfigModule("outbound");
  const mutation = useConfigChangeMutation("upstreams");
  const [searchParams, setSearchParams] = useSearchParams();
  const [form] = Form.useForm<UpstreamFormValues>();
  const formType = Form.useWatch("type", form);
  const ecsMode = Form.useWatch("ecs_mode", form);
  const fallbackEnabled = Form.useWatch("fallback_enabled", form);
  const [editing, setEditing] = useState<Upstream | "create" | null>(null);
  const [dirty, setDirty] = useState(false);
  const [search, setSearch] = useState("");
  const requestedTab = searchParams.get("tab");
  const activeTab = upstreamTabs.find((tab) => tab === requestedTab) ?? upstreamTabs[0];
  const items = useMemo(() => query.data?.values.flatMap((item) => item.module === "upstreams" ? [item.value] : []) ?? [], [query.data]);
  const upstreamOptions = items.map((item) => ({ label: item.name, value: item.name }));
  const proxyOptions = proxies.data?.values.flatMap((item) => item.module === "outbound" ? [{ label: item.value.name, value: item.value.name }] : []) ?? [];
  const visible = items.filter((item) => (activeTab === "groups" ? item.type === "group" : item.type !== "group") && item.name.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase()));

  useEffect(() => {
    if (!editing) return;
    mutation.reset();
    setDirty(false);
    if (editing === "create") {
      form.setFieldsValue(activeTab === "groups"
        ? { name: "", type: "group", upstreams: [{ name: "", weight: 1 }], upstream_mode: "parallel", timeout: "5s", fallback_enabled: false }
        : { name: "", type: "doh", ecs_mode: "disabled" });
    } else if (editing.type === "hosts") {
      form.setFieldsValue({ name: editing.name, type: "hosts", format: editing.format, hosts: editing.hosts });
    } else if (editing.type === "doh") {
      form.setFieldsValue({
        name: editing.name,
        type: "doh",
        address: editing.address,
        bootstrap: editing.bootstrap,
        connect_ip: editing.connect_ip,
        proxy: editing.proxy,
        ecs_mode: editing.edns_client_subnet?.mode ?? "disabled",
        ecs_custom_ip: editing.edns_client_subnet?.custom_ip,
      });
    } else {
      form.setFieldsValue({
        name: editing.name,
        type: "group",
        upstreams: editing.upstreams,
        upstream_mode: editing.upstream_mode,
        timeout: editing.timeout,
        fallback_enabled: Boolean(editing.fallbacks),
        fallbacks: editing.fallbacks,
        fallback_upstream_mode: editing.fallback_upstream_mode,
        fallback_timeout: editing.fallback_timeout,
      });
    }
  }, [activeTab, editing, form]);

  const columns: TableColumnsType<Upstream> = [
    { title: "名称", dataIndex: "name", width: 220, render: (value: string) => <Typography.Text strong>{value}</Typography.Text> },
    { title: "类型", width: 110, render: (_, item) => <Tag color={item.type === "doh" ? "blue" : item.type === "group" ? "cyan" : undefined}>{item.type.toUpperCase()}</Tag> },
    { title: "目标", ellipsis: true, render: (_, item) => <Typography.Text>{upstreamSummary(item)}</Typography.Text> },
    { title: "代理", width: 150, render: (_, item) => item.type === "doh" ? item.proxy ?? "直连" : "不适用" },
    { title: "引用", width: 80, render: (_, item) => query.data?.references.filter((reference) => reference.to_name === item.name).length ?? 0 },
    {
      title: "操作", width: 72, align: "center", render: (_, item) => (
        <Tooltip title="编辑上游"><Button type="text" aria-label={`编辑上游 ${item.name}`} icon={<Pencil size={17} />} onClick={() => setEditing(item)} /></Tooltip>
      ),
    },
  ];

  const submit = async () => {
    if (!query.data || !editing) return;
    const values = await form.validateFields();
    const value: Upstream = values.type === "hosts"
      ? { name: values.name, type: "hosts", format: values.format ?? "hosts", hosts: values.hosts ?? "" }
      : values.type === "doh"
        ? {
            name: values.name,
            type: "doh",
            address: values.address ?? "",
            ...(values.bootstrap ? { bootstrap: values.bootstrap } : {}),
            ...(values.connect_ip ? { connect_ip: values.connect_ip } : {}),
            ...(values.proxy ? { proxy: values.proxy } : {}),
            ...(values.ecs_mode ? { edns_client_subnet: { mode: values.ecs_mode, ...(values.ecs_mode === "custom" && values.ecs_custom_ip ? { custom_ip: values.ecs_custom_ip } : {}) } } : {}),
          }
        : {
            name: values.name,
            type: "group",
            upstreams: values.upstreams ?? [],
            upstream_mode: values.upstream_mode ?? "parallel",
            timeout: values.timeout ?? "5s",
            ...(values.fallback_enabled ? {
              fallbacks: values.fallbacks ?? [],
              fallback_upstream_mode: values.fallback_upstream_mode ?? "parallel",
              fallback_timeout: values.fallback_timeout ?? "5s",
            } : {}),
          };
    const change: Schemas["ConfigChange"] = editing === "create"
      ? { module: "upstreams", change: { action: "create", value } }
      : { module: "upstreams", change: { action: "update", original_name: editing.name, value } };
    try {
      const operation = await mutation.mutateAsync({ change, state: query.data.state });
      if (operation) setEditing(null);
    } catch {
      // 保留草稿和后端字段错误。
    }
  };

  return (
    <PageFrame
      title="DNS 上游"
      description="配置 Hosts、DoH 上游及嵌套上游组；名称变化由后端统一维护类型化引用。"
      meta={query.data ? <ConfigStateSummary state={query.data.state} /> : undefined}
      actions={<Button type="primary" icon={<Plus size={17} />} disabled={!query.data || !configStateEditable(query.data.state)} onClick={() => setEditing("create")}>{activeTab === "groups" ? "添加上游组" : "添加上游"}</Button>}
    >
      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {query.data ? (
        <div className="config-module-content">
          <Tabs activeKey={activeTab} items={[{ key: "upstreams", label: "上游" }, { key: "groups", label: "上游组" }]} onChange={(tab) => setSearchParams(tab === "upstreams" ? {} : { tab })} />
          <div className="config-table-toolbar"><Input allowClear value={search} prefix={<Search size={16} />} placeholder="搜索上游名称" onChange={(event) => setSearch(event.target.value)} /></div>
          <Table rowKey="name" columns={columns} dataSource={visible} pagination={{ pageSize: 20, hideOnSinglePage: true }} scroll={{ x: 920 }} locale={{ emptyText: search ? "没有匹配的上游" : activeTab === "groups" ? "尚未配置上游组" : "尚未配置上游" }} />
        </div>
      ) : null}
      <ConfigFormModal open={editing !== null} title={editing === "create" ? (activeTab === "groups" ? "添加上游组" : "添加上游") : "编辑上游"} dirty={dirty} busy={mutation.isPending} error={mutation.error} onCancel={() => setEditing(null)} onSubmit={() => void submit()}>
        <Form form={form} layout="vertical" requiredMark="optional" onValuesChange={() => setDirty(true)}>
          <Form.Item name="name" label="名称" rules={[{ required: true }, { max: 128 }]}><Input autoComplete="off" /></Form.Item>
          <Form.Item name="type" label="类型" rules={[{ required: true }]}><Segmented block options={[{ label: "Hosts", value: "hosts" }, { label: "DoH", value: "doh" }, { label: "上游组", value: "group" }]} /></Form.Item>
          {formType === "hosts" ? <HostsFields /> : null}
          {formType === "doh" ? (
            <>
              <Form.Item name="address" label="DoH 地址" rules={[{ required: true }, { type: "url" }, { max: 4096 }]}><Input placeholder="https://dns.example/dns-query" /></Form.Item>
              <Form.Item name="bootstrap" label="Bootstrap 上游"><Select allowClear showSearch options={upstreamOptions} /></Form.Item>
              <Form.Item name="connect_ip" label="连接 IP"><Input /></Form.Item>
              <Form.Item name="proxy" label="代理"><Select allowClear options={proxyOptions} placeholder="直连" /></Form.Item>
              <Form.Item name="ecs_mode" label="ECS"><Select options={[{ label: "禁用", value: "disabled" }, { label: "客户端地址", value: "client" }, { label: "自定义", value: "custom" }]} /></Form.Item>
              {ecsMode === "custom" ? <Form.Item name="ecs_custom_ip" label="自定义 ECS" rules={[{ required: true }]}><Input /></Form.Item> : null}
            </>
          ) : null}
          {formType === "group" ? (
            <>
              <MemberList name="upstreams" label="主要成员" options={upstreamOptions} form={form} />
              <Form.Item name="upstream_mode" label="主要模式" rules={[{ required: true }]}><Select options={modeOptions} /></Form.Item>
              <Form.Item name="timeout" label="主要超时" rules={[{ required: true }]}><Input placeholder="5s" /></Form.Item>
              <Form.Item name="fallback_enabled" label="启用 fallback" valuePropName="checked"><Switch /></Form.Item>
              {fallbackEnabled ? (
                <>
                  <MemberList name="fallbacks" label="Fallback 成员" options={upstreamOptions} form={form} />
                  <Form.Item name="fallback_upstream_mode" label="Fallback 模式" rules={[{ required: true }]}><Select options={modeOptions} /></Form.Item>
                  <Form.Item name="fallback_timeout" label="Fallback 超时" rules={[{ required: true }]}><Input placeholder="5s" /></Form.Item>
                </>
              ) : null}
            </>
          ) : null}
        </Form>
      </ConfigFormModal>
    </PageFrame>
  );
}

function HostsFields() {
  return (
    <>
      <Form.Item name="format" label="Hosts 格式" rules={[{ required: true }]}><Select options={[{ label: "Hosts 行格式", value: "hosts" }, { label: "JSON", value: "json" }]} /></Form.Item>
      <Form.Item name="hosts" label="内联映射" rules={[{ required: true }, { max: 262144 }]}><Input.TextArea autoSize={{ minRows: 7, maxRows: 16 }} /></Form.Item>
    </>
  );
}

function MemberList({ name, label, options, form }: { name: "upstreams" | "fallbacks"; label: string; options: Array<{ label: string; value: string }>; form: ReturnType<typeof Form.useForm<UpstreamFormValues>>[0] }) {
  return (
    <Form.List name={name} rules={[{ validator: async (_, members) => { if (!members?.length) throw new Error(`至少需要一个${label}`); } }]}>
      {(fields, { add, remove, move }, { errors }) => (
        <div className="nested-form-list">
          <Typography.Text strong>{label}</Typography.Text>
          {fields.map((field, index) => (
            <Space key={field.key} className="nested-form-row" align="start">
              <Form.Item {...field} name={[field.name, "name"]} rules={[{ required: true }]}><Select showSearch options={options} placeholder="选择上游" /></Form.Item>
              <Form.Item {...field} name={[field.name, "weight"]} initialValue={1} rules={[{ required: true }]}><InputNumber min={1} max={4294967295} aria-label={`${label}权重 ${index + 1}`} /></Form.Item>
              <Tooltip title="上移"><Button type="text" icon={<ArrowUp size={16} />} aria-label={`${label}上移 ${index + 1}`} disabled={index === 0} onClick={() => move(index, index - 1)} /></Tooltip>
              <Tooltip title="下移"><Button type="text" icon={<ArrowDown size={16} />} aria-label={`${label}下移 ${index + 1}`} disabled={index === fields.length - 1} onClick={() => move(index, index + 1)} /></Tooltip>
              <Tooltip title="移除"><Button type="text" danger icon={<Trash2 size={16} />} aria-label={`${label}移除 ${index + 1}`} onClick={() => remove(index)} /></Tooltip>
            </Space>
          ))}
          <Button icon={<Plus size={16} />} onClick={() => add({ name: "", weight: 1 })}>添加成员</Button>
          <Form.ErrorList errors={errors} />
        </div>
      )}
    </Form.List>
  );
}

function upstreamSummary(value: Upstream): string {
  if (value.type === "hosts") return `${value.format.toUpperCase()} · ${value.hosts.split(/\r?\n/).filter(Boolean).length} 行`;
  if (value.type === "doh") return value.address;
  return `${value.upstreams.length} 个成员 · ${value.upstream_mode}`;
}
