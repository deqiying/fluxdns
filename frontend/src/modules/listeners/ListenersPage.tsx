import { useEffect, useMemo, useState } from "react";
import { Button, Form, Input, InputNumber, Segmented, Select, Space, Table, Tag, Tooltip, Typography, type TableColumnsType } from "antd";
import { Pencil, Plus, Search, Trash2 } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import { ConfigFormModal } from "@/shared/components/ConfigFormModal";
import { ConfigStateSummary } from "@/shared/components/ConfigStateSummary";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { configStateEditable, useConfigChangeMutation, useConfigModule } from "@/shared/config/hooks";

type Schemas = components["schemas"];
type Listener = Schemas["Listener"];
type DohEndpoint = Schemas["DohEndpoint"];

interface EndpointFormValue {
  name: string;
  addresses: string[];
  port: number;
  tls_mode: "terminate" | "external";
  certificate_file?: string;
  private_key_file?: string;
  client_ip_source: "peer" | "forwarded_header" | "proxy_protocol";
  header?: "X-Forwarded-For" | "X-Real-IP" | "Forwarded";
  trusted_proxies?: string[];
  on_missing?: "reject" | "use_peer";
  on_invalid?: "reject" | "use_peer";
}

interface ListenerFormValues {
  name: string;
  type: "udp" | "tcp" | "doh";
  addresses?: string[];
  port?: number;
  strategy?: string;
  hosts?: string;
  routes?: Array<{ path: string; strategy: string }>;
  endpoints?: EndpointFormValue[];
}

export function ListenersPage() {
  const query = useConfigModule("listener");
  const strategies = useConfigModule("strategy");
  const hosts = useConfigModule("hosts");
  const mutation = useConfigChangeMutation("listener");
  const [form] = Form.useForm<ListenerFormValues>();
  const formType = Form.useWatch("type", form);
  const [editing, setEditing] = useState<Listener | "create" | null>(null);
  const [dirty, setDirty] = useState(false);
  const [search, setSearch] = useState("");
  const items = useMemo(() => query.data?.values.flatMap((item) => item.module === "listener" ? [item.value] : []) ?? [], [query.data]);
  const visible = items.filter((item) => item.name.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase()));
  const strategyOptions = strategies.data?.values.flatMap((item) => item.module === "strategy" ? [{ label: item.value.name, value: item.value.name }] : []) ?? [];
  const hostsOptions = hosts.data?.values.flatMap((item) => item.module === "hosts" ? [{ label: item.value.name, value: item.value.name }] : []) ?? [];

  useEffect(() => {
    if (!editing) return;
    mutation.reset();
    setDirty(false);
    if (editing === "create") {
      form.setFieldsValue({ name: "", type: "udp", addresses: ["127.0.0.1"], port: 53, strategy: "" });
    } else if (editing.type === "udp" || editing.type === "tcp") {
      form.setFieldsValue({ name: editing.name, type: editing.type, addresses: editing.addresses, port: editing.port, strategy: editing.strategy, hosts: editing.hosts });
    } else {
      form.setFieldsValue({
        name: editing.name,
        type: "doh",
        routes: editing.routes,
        endpoints: editing.endpoints.map(endpointToForm),
      });
    }
  }, [editing, form]);

  const columns: TableColumnsType<Listener> = [
    { title: "名称", dataIndex: "name", width: 210, render: (value: string) => <Typography.Text strong>{value}</Typography.Text> },
    { title: "协议", width: 90, render: (_, item) => <Tag color={item.type === "doh" ? "blue" : undefined}>{item.type.toUpperCase()}</Tag> },
    { title: "绑定", render: (_, item) => bindingSummary(item) },
    { title: "策略", width: 190, render: (_, item) => item.type === "doh" ? `${item.routes.length} 条路由` : item.strategy },
    {
      title: "Runtime", width: 140, render: (_, item) => {
        const runtime = query.data?.runtime.find((value) => value.module === "listener" && value.name === item.name);
        if (!runtime || runtime.module !== "listener") return <Tag>未知</Tag>;
        const accepting = runtime.bindings.filter((binding) => binding.accepting).length;
        return <Tag color={accepting === runtime.bindings.length ? "success" : "warning"}>{accepting}/{runtime.bindings.length} 接受中</Tag>;
      },
    },
    { title: "操作", width: 72, align: "center", render: (_, item) => <Tooltip title="编辑 Listener"><Button type="text" aria-label={`编辑 Listener ${item.name}`} icon={<Pencil size={17} />} onClick={() => setEditing(item)} /></Tooltip> },
  ];

  const submit = async () => {
    if (!query.data || !editing) return;
    const values = await form.validateFields();
    const value: Listener = values.type === "doh"
      ? { name: values.name, type: "doh", routes: values.routes ?? [], endpoints: (values.endpoints ?? []).map(endpointFromForm) }
      : { name: values.name, type: values.type, addresses: values.addresses ?? [], port: values.port ?? 53, strategy: values.strategy ?? "", ...(values.hosts ? { hosts: values.hosts } : {}) };
    const change: Schemas["ConfigChange"] = editing === "create"
      ? { module: "listener", change: { action: "create", value } }
      : { module: "listener", change: { action: "update", original_name: editing.name, value } };
    try {
      const operation = await mutation.mutateAsync({ change, state: query.data.state });
      if (operation) setEditing(null);
    } catch {
      // 绑定或 revision 失败时保留草稿。
    }
  };

  return (
    <PageFrame title="监听入口" description="管理 UDP、TCP 和 DoH 监听；只有发生变化的物理 endpoint 进入重绑。" meta={query.data ? <ConfigStateSummary state={query.data.state} /> : undefined} actions={<Button type="primary" icon={<Plus size={17} />} disabled={!query.data || !configStateEditable(query.data.state)} onClick={() => setEditing("create")}>添加 Listener</Button>}>
      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {query.data ? <div className="config-module-content"><div className="config-table-toolbar"><Input allowClear value={search} prefix={<Search size={16} />} placeholder="搜索 Listener 名称" onChange={(event) => setSearch(event.target.value)} /></div><Table rowKey="name" columns={columns} dataSource={visible} pagination={{ pageSize: 20, hideOnSinglePage: true }} scroll={{ x: 900 }} locale={{ emptyText: search ? "没有匹配的 Listener" : "尚未配置 Listener" }} /></div> : null}
      <ConfigFormModal open={editing !== null} title={editing === "create" ? "添加 Listener" : "编辑 Listener"} dirty={dirty} busy={mutation.isPending} error={mutation.error} onCancel={() => setEditing(null)} onSubmit={() => void submit()}>
        <Form form={form} layout="vertical" requiredMark="optional" onValuesChange={() => setDirty(true)}>
          <Form.Item name="name" label="名称" rules={[{ required: true }, { max: 128 }]}><Input /></Form.Item>
          <Form.Item name="type" label="协议" rules={[{ required: true }]}><Segmented block options={[{ label: "UDP", value: "udp" }, { label: "TCP", value: "tcp" }, { label: "DoH", value: "doh" }]} /></Form.Item>
          {formType === "doh" ? <DohFields strategyOptions={strategyOptions} /> : <SocketFields strategyOptions={strategyOptions} hostsOptions={hostsOptions} />}
        </Form>
      </ConfigFormModal>
    </PageFrame>
  );
}

function SocketFields({ strategyOptions, hostsOptions }: { strategyOptions: Array<{ label: string; value: string }>; hostsOptions: Array<{ label: string; value: string }> }) {
  return <><Form.Item name="addresses" label="监听地址" rules={[{ required: true }]}><Select mode="tags" tokenSeparators={[","]} /></Form.Item><Form.Item name="port" label="端口" rules={[{ required: true }]}><InputNumber min={1} max={65535} /></Form.Item><Form.Item name="strategy" label="策略" rules={[{ required: true }]}><Select showSearch options={strategyOptions} /></Form.Item><Form.Item name="hosts" label="Hosts"><Select allowClear showSearch options={hostsOptions} /></Form.Item></>;
}

function DohFields({ strategyOptions }: { strategyOptions: Array<{ label: string; value: string }> }) {
  return (
    <>
      <Form.List name="routes" rules={[{ validator: async (_, routes) => { if (!routes?.length) throw new Error("至少需要一条 DoH 路由"); } }]}>
        {(fields, { add, remove }, { errors }) => <div className="nested-form-list"><Typography.Text strong>路由</Typography.Text>{fields.map((field, index) => <Space key={field.key} className="doh-route-row" align="start"><Form.Item name={[field.name, "path"]} rules={[{ required: true }]}><Input placeholder="/dns-query" /></Form.Item><Form.Item name={[field.name, "strategy"]} rules={[{ required: true }]}><Select options={strategyOptions} placeholder="策略" /></Form.Item><Tooltip title="移除"><Button type="text" danger aria-label={`移除路由 ${index + 1}`} icon={<Trash2 size={16} />} onClick={() => remove(index)} /></Tooltip></Space>)}<Button icon={<Plus size={16} />} onClick={() => add({ path: "/dns-query", strategy: "" })}>添加路由</Button><Form.ErrorList errors={errors} /></div>}
      </Form.List>
      <Form.List name="endpoints" rules={[{ validator: async (_, endpoints) => { if (!endpoints?.length) throw new Error("至少需要一个 DoH endpoint"); } }]}>
        {(fields, { add, remove }, { errors }) => <div className="nested-form-list"><Typography.Text strong>Endpoints</Typography.Text>{fields.map((field, index) => <EndpointFields key={field.key} field={field} index={index} remove={remove} />)}<Button icon={<Plus size={16} />} onClick={() => add({ name: "", addresses: ["127.0.0.1"], port: 443, tls_mode: "external", client_ip_source: "peer" })}>添加 Endpoint</Button><Form.ErrorList errors={errors} /></div>}
      </Form.List>
    </>
  );
}

function EndpointFields({ field, index, remove }: { field: { key: number; name: number }; index: number; remove: (index: number) => void }) {
  return (
    <div className="doh-endpoint-fields">
      <div className="doh-endpoint-heading"><Typography.Text>Endpoint {index + 1}</Typography.Text><Tooltip title="移除"><Button type="text" danger aria-label={`移除 Endpoint ${index + 1}`} icon={<Trash2 size={16} />} onClick={() => remove(index)} /></Tooltip></div>
      <Form.Item name={[field.name, "name"]} label="名称" rules={[{ required: true }]}><Input /></Form.Item>
      <Form.Item name={[field.name, "addresses"]} label="地址" rules={[{ required: true }]}><Select mode="tags" tokenSeparators={[","]} /></Form.Item>
      <Form.Item name={[field.name, "port"]} label="端口" rules={[{ required: true }]}><InputNumber min={1} max={65535} /></Form.Item>
      <Form.Item name={[field.name, "tls_mode"]} label="TLS" rules={[{ required: true }]}><Select options={[{ label: "FluxDNS 终止 TLS", value: "terminate" }, { label: "外部终止 TLS", value: "external" }]} /></Form.Item>
      <Form.Item noStyle shouldUpdate={(previous, current) => previous.endpoints?.[index]?.tls_mode !== current.endpoints?.[index]?.tls_mode}>
        {({ getFieldValue }) => getFieldValue(["endpoints", index, "tls_mode"]) === "terminate" ? <Space className="paired-fields" align="start"><Form.Item name={[field.name, "certificate_file"]} label="证书文件" rules={[{ required: true }]}><Input /></Form.Item><Form.Item name={[field.name, "private_key_file"]} label="私钥文件" rules={[{ required: true }]}><Input /></Form.Item></Space> : null}
      </Form.Item>
      <Form.Item name={[field.name, "client_ip_source"]} label="客户端 IP 来源" rules={[{ required: true }]}><Select options={[{ label: "连接对端", value: "peer" }, { label: "转发 Header", value: "forwarded_header" }, { label: "PROXY protocol", value: "proxy_protocol" }]} /></Form.Item>
      <Form.Item noStyle shouldUpdate={(previous, current) => previous.endpoints?.[index]?.client_ip_source !== current.endpoints?.[index]?.client_ip_source}>
        {({ getFieldValue }) => getFieldValue(["endpoints", index, "client_ip_source"]) === "peer" ? null : <><Form.Item name={[field.name, "trusted_proxies"]} label="可信代理"><Select mode="tags" tokenSeparators={[","]} /></Form.Item>{getFieldValue(["endpoints", index, "client_ip_source"]) === "forwarded_header" ? <Form.Item name={[field.name, "header"]} label="Header" rules={[{ required: true }]}><Select options={["X-Forwarded-For", "X-Real-IP", "Forwarded"].map((value) => ({ label: value, value }))} /></Form.Item> : null}<Space className="paired-fields" align="start"><Form.Item name={[field.name, "on_missing"]} label="缺失处理"><Select options={[{ label: "拒绝", value: "reject" }, { label: "使用对端", value: "use_peer" }]} /></Form.Item><Form.Item name={[field.name, "on_invalid"]} label="无效处理"><Select options={[{ label: "拒绝", value: "reject" }, { label: "使用对端", value: "use_peer" }]} /></Form.Item></Space></>}
      </Form.Item>
    </div>
  );
}

function endpointToForm(value: DohEndpoint): EndpointFormValue {
  return { name: value.name, addresses: value.addresses, port: value.port, tls_mode: value.tls.mode, certificate_file: value.tls.certificate_file, private_key_file: value.tls.private_key_file, client_ip_source: value.client_ip.source, header: value.client_ip.header, trusted_proxies: value.client_ip.trusted_proxies, on_missing: value.client_ip.on_missing, on_invalid: value.client_ip.on_invalid };
}

function endpointFromForm(value: EndpointFormValue): DohEndpoint {
  return {
    name: value.name,
    addresses: value.addresses,
    port: value.port,
    tls: value.tls_mode === "terminate" ? { mode: "terminate", certificate_file: value.certificate_file, private_key_file: value.private_key_file } : { mode: "external" },
    client_ip: value.client_ip_source === "peer" ? { source: "peer" } : { source: value.client_ip_source, ...(value.client_ip_source === "forwarded_header" && value.header ? { header: value.header } : {}), ...(value.trusted_proxies?.length ? { trusted_proxies: value.trusted_proxies } : {}), ...(value.on_missing ? { on_missing: value.on_missing } : {}), ...(value.on_invalid ? { on_invalid: value.on_invalid } : {}) },
  };
}

function bindingSummary(value: Listener): string {
  if (value.type === "doh") return `${value.endpoints.length} endpoints`;
  return `${value.addresses.join(", ")}:${value.port}`;
}
