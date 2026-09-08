import { useEffect, useMemo, useState } from "react";
import { Button, Form, Input, Segmented, Select, Switch, Table, Tag, Tooltip, Typography, type TableColumnsType } from "antd";
import { Pencil, Plus, Search } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import { ConfigFormModal } from "@/shared/components/ConfigFormModal";
import { ConfigStateSummary } from "@/shared/components/ConfigStateSummary";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { configStateEditable, useConfigChangeMutation, useConfigModule } from "@/shared/config/hooks";

type Schemas = components["schemas"];
type RuleSet = Schemas["RuleSet"];

interface RuleSetFormValues {
  name: string;
  type: "const" | "file" | "remote";
  format: "json" | "clash" | "dat";
  content?: string;
  path?: string;
  url?: string;
  proxy?: string;
  auto_update?: boolean;
  update_interval?: string;
}

export function RuleSetsPage() {
  const query = useConfigModule("rule_set");
  const proxies = useConfigModule("outbound");
  const mutation = useConfigChangeMutation("rule_set");
  const [form] = Form.useForm<RuleSetFormValues>();
  const sourceType = Form.useWatch("type", form);
  const [editing, setEditing] = useState<RuleSet | "create" | null>(null);
  const [dirty, setDirty] = useState(false);
  const [search, setSearch] = useState("");
  const items = useMemo(() => query.data?.values.flatMap((item) => item.module === "rule_set" ? [item.value] : []) ?? [], [query.data]);
  const proxyOptions = proxies.data?.values.flatMap((item) => item.module === "outbound" ? [{ label: item.value.name, value: item.value.name }] : []) ?? [];
  const visible = items.filter((item) => `${item.name} ${sourceLabel(item)}`.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase()));

  useEffect(() => {
    if (!editing) return;
    mutation.reset();
    setDirty(false);
    if (editing === "create") {
      form.setFieldsValue({ name: "", type: "const", format: "json", content: "", auto_update: false });
    } else if (editing.type === "const") {
      form.setFieldsValue({ name: editing.name, type: "const", format: editing.format, content: editing.rule });
    } else if (editing.type === "file") {
      form.setFieldsValue({ name: editing.name, type: "file", format: editing.format, path: editing.path, auto_update: editing.auto_update, update_interval: editing.update_interval });
    } else {
      form.setFieldsValue({ name: editing.name, type: "remote", format: editing.format, url: editing.url, proxy: editing.proxy, auto_update: editing.auto_update, update_interval: editing.update_interval });
    }
  }, [editing, form]);

  const columns: TableColumnsType<RuleSet> = [
    { title: "名称", dataIndex: "name", width: 220, render: (value: string) => <Typography.Text strong>{value}</Typography.Text> },
    { title: "来源", width: 110, render: (_, item) => <Tag color={item.type === "remote" ? "blue" : undefined}>{sourceTypeLabel(item.type)}</Tag> },
    { title: "格式", dataIndex: "format", width: 100, render: (value: string) => <Tag>{value.toUpperCase()}</Tag> },
    { title: "位置", ellipsis: true, render: (_, item) => <Typography.Text code={item.type !== "const"}>{sourceLabel(item)}</Typography.Text> },
    { title: "刷新", width: 130, render: (_, item) => item.type !== "const" && item.auto_update ? item.update_interval ?? "默认周期" : "不自动刷新" },
    {
      title: "状态", width: 120, render: (_, item) => {
        const runtime = query.data?.runtime.find((value) => value.module === "rule_set" && value.name === item.name);
        return runtime && runtime.module === "rule_set" ? <Tag color={runtime.condition === "ready" ? "success" : runtime.condition === "failed" ? "error" : "warning"}>{runtime.condition}</Tag> : <Tag>未知</Tag>;
      },
    },
    {
      title: "操作", width: 72, align: "center", render: (_, item) => (
        <Tooltip title="编辑规则集"><Button type="text" aria-label={`编辑规则集 ${item.name}`} icon={<Pencil size={17} />} onClick={() => setEditing(item)} /></Tooltip>
      ),
    },
  ];

  const submit = async () => {
    if (!query.data || !editing) return;
    const values = await form.validateFields();
    const update = values.auto_update ?? false;
    const updateFields = update && values.update_interval ? { auto_update: true, update_interval: values.update_interval } : { auto_update: update };
    const value: RuleSet = values.type === "const"
      ? { name: values.name, type: "const", format: values.format, rule: values.content ?? "" }
      : values.type === "file"
        ? { name: values.name, type: "file", format: values.format, path: values.path ?? "", ...updateFields }
        : { name: values.name, type: "remote", format: values.format, url: values.url ?? "", ...(values.proxy ? { proxy: values.proxy } : {}), ...updateFields };
    const change: Schemas["ConfigChange"] = editing === "create"
      ? { module: "rule_set", change: { action: "create", value } }
      : { module: "rule_set", change: { action: "update", original_name: editing.name, value } };
    try {
      const operation = await mutation.mutateAsync({ change, state: query.data.state });
      if (operation) setEditing(null);
    } catch {
      // 保留草稿和字段错误。
    }
  };

  return (
    <PageFrame
      title="规则集"
      description="管理内联、本地和远程规则资源；刷新失败时继续显示后端报告的陈旧快照状态。"
      meta={query.data ? <ConfigStateSummary state={query.data.state} /> : undefined}
      actions={<Button type="primary" icon={<Plus size={17} />} disabled={!query.data || !configStateEditable(query.data.state)} onClick={() => setEditing("create")}>添加规则集</Button>}
    >
      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {query.data ? (
        <div className="config-module-content">
          <div className="config-table-toolbar"><Input allowClear value={search} prefix={<Search size={16} />} placeholder="搜索规则集名称或来源" onChange={(event) => setSearch(event.target.value)} /></div>
          <Table rowKey="name" columns={columns} dataSource={visible} pagination={{ pageSize: 20, hideOnSinglePage: true }} scroll={{ x: 1050 }} locale={{ emptyText: search ? "没有匹配的规则集" : "尚未配置规则集" }} />
        </div>
      ) : null}
      <ConfigFormModal open={editing !== null} title={editing === "create" ? "添加规则集" : "编辑规则集"} dirty={dirty} busy={mutation.isPending} error={mutation.error} onCancel={() => setEditing(null)} onSubmit={() => void submit()}>
        <Form form={form} layout="vertical" requiredMark="optional" onValuesChange={() => setDirty(true)}>
          <Form.Item name="name" label="名称" rules={[{ required: true }, { max: 128 }]}><Input autoComplete="off" /></Form.Item>
          <Form.Item name="type" label="来源" rules={[{ required: true }]}><Segmented block options={[{ label: "内联", value: "const" }, { label: "本地文件", value: "file" }, { label: "远程 URL", value: "remote" }]} /></Form.Item>
          <Form.Item name="format" label="格式" rules={[{ required: true }]}><Select options={[{ label: "JSON", value: "json" }, { label: "Clash 行格式", value: "clash" }, { label: "DAT 二进制", value: "dat" }]} /></Form.Item>
          {sourceType === "const" ? (
            <Form.Item name="content" label="内联规则" rules={[{ required: true }, { max: 262144 }]}><Input.TextArea autoSize={{ minRows: 8, maxRows: 18 }} /></Form.Item>
          ) : (
            <>
              {sourceType === "remote" ? (
                <>
                  <Form.Item name="url" label="远程 URL" rules={[{ required: true }, { type: "url" }, { max: 4096 }]}><Input autoComplete="off" /></Form.Item>
                  <Form.Item name="proxy" label="下载代理"><Select allowClear options={proxyOptions} placeholder="直连" /></Form.Item>
                </>
              ) : <Form.Item name="path" label="文件路径" rules={[{ required: true }, { max: 4096 }]}><Input autoComplete="off" /></Form.Item>}
              <Form.Item name="auto_update" label="自动更新" valuePropName="checked"><Switch /></Form.Item>
              <Form.Item noStyle shouldUpdate={(previous, current) => previous.auto_update !== current.auto_update}>
                {({ getFieldValue }) => getFieldValue("auto_update") ? <Form.Item name="update_interval" label="更新周期" rules={[{ required: true }]}><Input placeholder="24h" /></Form.Item> : null}
              </Form.Item>
            </>
          )}
        </Form>
      </ConfigFormModal>
    </PageFrame>
  );
}

function sourceTypeLabel(type: RuleSet["type"]): string {
  return type === "const" ? "内联" : type === "file" ? "本地文件" : "远程";
}

function sourceLabel(value: RuleSet): string {
  if (value.type === "const") return "内联内容";
  return value.type === "file" ? value.path : value.url;
}
