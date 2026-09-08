import { useEffect, useMemo, useState } from "react";
import { Button, Form, Input, Select, Space, Table, Tag, Tooltip, Typography, type TableColumnsType } from "antd";
import { Pencil, Plus, Search } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import { ConfigFormModal } from "@/shared/components/ConfigFormModal";
import { ConfigStateSummary } from "@/shared/components/ConfigStateSummary";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { clientEditValue } from "@/shared/config/contract";
import { configStateEditable, useConfigChangeMutation, useConfigModule } from "@/shared/config/hooks";

type Schemas = components["schemas"];
type Client = Schemas["Client"];

interface ClientFormValues {
  name: string;
  client_id: string;
  ips: string[];
  strategy?: string;
  cache_mode: "inherit" | "enabled" | "disabled";
  ttl_mode: "inherit" | "enabled" | "disabled";
  ttl_min?: string;
  ttl_max?: string;
  ecs_mode: "inherit" | "disabled" | "client" | "custom";
  ecs_custom_ip?: string;
}

export function ClientsPage() {
  const query = useConfigModule("clients");
  const strategies = useConfigModule("strategy");
  const mutation = useConfigChangeMutation("clients");
  const [form] = Form.useForm<ClientFormValues>();
  const ttlMode = Form.useWatch("ttl_mode", form);
  const ecsMode = Form.useWatch("ecs_mode", form);
  const [editing, setEditing] = useState<Client | "create" | null>(null);
  const [dirty, setDirty] = useState(false);
  const [search, setSearch] = useState("");
  const items = useMemo(() => query.data?.values.flatMap((item) => item.module === "clients" ? [item.value] : []) ?? [], [query.data]);
  const visible = items.filter((item) => `${item.name} ${item.client_id} ${item.match?.ips?.join(" ") ?? ""}`.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase()));
  const strategyOptions = strategies.data?.values.flatMap((item) => item.module === "strategy" ? [{ label: item.value.name, value: item.value.name }] : []) ?? [];

  useEffect(() => {
    if (!editing) return;
    mutation.reset();
    setDirty(false);
    if (editing === "create") {
      form.setFieldsValue({ name: "", client_id: "", ips: [], cache_mode: "inherit", ttl_mode: "inherit", ecs_mode: "inherit" });
    } else {
      form.setFieldsValue({
        name: editing.name,
        client_id: editing.client_id,
        ips: editing.match?.ips ?? [],
        strategy: editing.strategy,
        cache_mode: editing.cache ? (editing.cache.enabled ? "enabled" : "disabled") : "inherit",
        ttl_mode: editing.ttl_override ? (editing.ttl_override.enabled === false ? "disabled" : "enabled") : "inherit",
        ttl_min: editing.ttl_override?.min,
        ttl_max: editing.ttl_override?.max,
        ecs_mode: editing.edns_client_subnet?.mode ?? "inherit",
        ecs_custom_ip: editing.edns_client_subnet?.custom_ip,
      });
    }
  }, [editing, form]);

  const columns: TableColumnsType<Client> = [
    { title: "客户端", width: 260, render: (_, item) => <div className="query-cell"><Typography.Text strong>{item.name}</Typography.Text><Typography.Text type="secondary" code>{item.client_id}</Typography.Text></div> },
    { title: "IP / CIDR", render: (_, item) => item.match?.ips?.length ? <Space wrap>{item.match.ips.map((ip) => <Tag key={ip}>{ip}</Tag>)}</Space> : <Typography.Text type="secondary">未配置</Typography.Text> },
    { title: "策略", dataIndex: "strategy", width: 180, render: (value?: string) => value ?? "继承默认" },
    { title: "缓存", width: 100, render: (_, item) => item.cache ? (item.cache.enabled ? "启用" : "禁用") : "继承" },
    { title: "操作", width: 72, align: "center", render: (_, item) => <Tooltip title="编辑客户端"><Button type="text" aria-label={`编辑客户端 ${item.name}`} icon={<Pencil size={17} />} onClick={() => setEditing(item)} /></Tooltip> },
  ];

  const submit = async () => {
    if (!query.data || !editing) return;
    const values = await form.validateFields();
    const common = {
      name: values.name,
      match: { ips: values.ips ?? [] },
      ...(values.strategy ? { strategy: values.strategy } : {}),
      ...(values.cache_mode === "inherit" ? {} : { cache: { enabled: values.cache_mode === "enabled" } }),
      ...(values.ttl_mode === "inherit" ? {} : { ttl_override: values.ttl_mode === "disabled" ? { enabled: false } : { enabled: true, ...(values.ttl_min ? { min: values.ttl_min } : {}), ...(values.ttl_max ? { max: values.ttl_max } : {}) } }),
      ...(values.ecs_mode === "inherit" ? {} : { edns_client_subnet: { mode: values.ecs_mode, ...(values.ecs_mode === "custom" && values.ecs_custom_ip ? { custom_ip: values.ecs_custom_ip } : {}) } }),
    };
    const change: Schemas["ConfigChange"] = editing === "create"
      ? { module: "clients", change: { action: "create", value: { ...common, client_id: values.client_id } } }
      : { module: "clients", change: { action: "update", original_name: editing.name, value: clientEditValue({ ...common, client_id: editing.client_id }) } };
    try {
      const operation = await mutation.mutateAsync({ change, state: query.data.state });
      if (operation) setEditing(null);
    } catch {
      // 冲突与校验失败时保留草稿。
    }
  };

  return (
    <PageFrame title="客户端配置" description="客户端 name 用于管理，client_id 用于请求匹配且普通编辑不可修改。" meta={query.data ? <ConfigStateSummary state={query.data.state} /> : undefined} actions={<Button type="primary" icon={<Plus size={17} />} disabled={!query.data || !configStateEditable(query.data.state)} onClick={() => setEditing("create")}>添加客户端</Button>}>
      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {query.data ? <div className="config-module-content"><div className="config-table-toolbar"><Input allowClear value={search} prefix={<Search size={16} />} placeholder="搜索 name、ID 或 IP" onChange={(event) => setSearch(event.target.value)} /></div><Table rowKey="name" columns={columns} dataSource={visible} pagination={{ pageSize: 20, hideOnSinglePage: true }} scroll={{ x: 920 }} locale={{ emptyText: search ? "没有匹配的客户端" : "尚未配置客户端" }} /></div> : null}
      <ConfigFormModal open={editing !== null} title={editing === "create" ? "添加客户端" : "编辑客户端"} dirty={dirty} busy={mutation.isPending} error={mutation.error} onCancel={() => setEditing(null)} onSubmit={() => void submit()}>
        <Form form={form} layout="vertical" requiredMark="optional" onValuesChange={() => setDirty(true)}>
          <Form.Item name="name" label="管理名称" rules={[{ required: true }, { max: 128 }]}><Input /></Form.Item>
          <Form.Item name="client_id" label="客户端 ID" rules={[{ required: true }, { max: 512 }]}><Input disabled={editing !== "create"} /></Form.Item>
          <Form.Item name="ips" label="IP / CIDR"><Select mode="tags" tokenSeparators={[","]} /></Form.Item>
          <Form.Item name="strategy" label="策略"><Select allowClear showSearch options={strategyOptions} placeholder="继承默认" /></Form.Item>
          <Form.Item name="cache_mode" label="缓存覆盖" rules={[{ required: true }]}><Select options={[{ label: "继承", value: "inherit" }, { label: "启用", value: "enabled" }, { label: "禁用", value: "disabled" }]} /></Form.Item>
          <Form.Item name="ttl_mode" label="TTL 覆盖" rules={[{ required: true }]}><Select options={[{ label: "继承", value: "inherit" }, { label: "启用覆盖", value: "enabled" }, { label: "禁用覆盖", value: "disabled" }]} /></Form.Item>
          {ttlMode === "enabled" ? <Space className="paired-fields" align="start"><Form.Item name="ttl_min" label="最小 TTL"><Input placeholder="30s" /></Form.Item><Form.Item name="ttl_max" label="最大 TTL"><Input placeholder="1h" /></Form.Item></Space> : null}
          <Form.Item name="ecs_mode" label="ECS 覆盖" rules={[{ required: true }]}><Select options={[{ label: "继承", value: "inherit" }, { label: "禁用", value: "disabled" }, { label: "客户端地址", value: "client" }, { label: "自定义", value: "custom" }]} /></Form.Item>
          {ecsMode === "custom" ? <Form.Item name="ecs_custom_ip" label="自定义 ECS" rules={[{ required: true }]}><Input /></Form.Item> : null}
        </Form>
      </ConfigFormModal>
    </PageFrame>
  );
}
