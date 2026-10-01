import { useEffect, useMemo, useState } from "react";
import {
  Button,
  Form,
  Input,
  Segmented,
  Space,
  Table,
  Tag,
  Tooltip,
  Typography,
  type TableColumnsType,
} from "antd";
import { Pencil, Plus, Search } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import { ConfigFormModal } from "@/shared/components/ConfigFormModal";
import { ConfigSyncBadge } from "@/shared/components/ConfigStateSummary";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { configStateEditable, useConfigChangeMutation, useConfigModule, useConfigState } from "@/shared/config/hooks";

type Schemas = components["schemas"];
type Outbound = Schemas["Outbound"];

type SecretKind = "env" | "file" | "url";

interface ProxyFormValues {
  name: string;
  secretKind: SecretKind;
  secretValue: string;
}

/// 内联地址经服务端脱敏，密码位为占位符；用户不改动时后端会恢复原密码。
const REDACTED_PASSWORD = "FLUXDNS_REDACTED_SECRET";

/// 把秘密来源引用规整成表单可编辑的 kind/值；脱敏内联值原样回填。
function secretRefOf(proxyUrl: Outbound["proxy_url"]): { kind: SecretKind; value: string } {
  if (typeof proxyUrl === "string") return { kind: "url", value: proxyUrl };
  if ("env" in proxyUrl) return { kind: "env", value: proxyUrl.env };
  if ("file" in proxyUrl) return { kind: "file", value: proxyUrl.file };
  return { kind: "url", value: proxyUrl.url };
}

export function ProxiesPage() {
  const query = useConfigModule("outbound");
  const mutation = useConfigChangeMutation("outbound");
  const state = useConfigState();
  const [form] = Form.useForm<ProxyFormValues>();
  const [editing, setEditing] = useState<Outbound | "create" | null>(null);
  const [dirty, setDirty] = useState(false);
  const [search, setSearch] = useState("");
  const proxies = useMemo(() => query.data?.values.flatMap((item) =>
    item.module === "outbound" ? [item.value] : []) ?? [], [query.data]);
  const visible = proxies.filter((item) =>
    `${item.name} ${secretRefOf(item.proxy_url).value}`
      .toLocaleLowerCase()
      .includes(search.trim().toLocaleLowerCase()));

  useEffect(() => {
    if (!editing) return;
    if (editing === "create") {
      form.setFieldsValue({ name: "", secretKind: "env", secretValue: "" });
    } else {
      form.setFieldsValue({
        name: editing.name,
        secretKind: secretRefOf(editing.proxy_url).kind,
        secretValue: secretRefOf(editing.proxy_url).value,
      });
    }
    setDirty(false);
    mutation.reset();
  }, [editing, form]);

  const columns: TableColumnsType<Outbound> = [
    { title: "名称", dataIndex: "name", width: 240, render: (value: string) => <Typography.Text strong>{value}</Typography.Text> },
    { title: "类型", width: 120, render: () => <Tag>SOCKS5</Tag> },
    {
      title: "SecretRef",
      render: (_, item) => {
        const secret = secretRefOf(item.proxy_url);
        if (secret.kind === "url") {
          const redacted = secret.value.includes(REDACTED_PASSWORD)
            ? `${secret.value}（密码已脱敏；不改动则保留原密码，改动主机或端口时需重填完整 URL）`
            : secret.value;
          return <Typography.Text code>{redacted}</Typography.Text>;
        }
        return <Typography.Text code>{secret.kind}:{secret.value}</Typography.Text>;
      },
    },
    {
      title: "引用",
      width: 110,
      render: (_, item) => query.data?.references.filter((reference) => reference.to_name === item.name).length ?? 0,
    },
    {
      title: "操作",
      width: 80,
      align: "center",
      render: (_, item) => (
        <Tooltip title="编辑代理">
          <Button type="text" aria-label={`编辑代理 ${item.name}`} icon={<Pencil size={17} />} onClick={() => setEditing(item)} />
        </Tooltip>
      ),
    },
  ];

  const submit = async () => {
    if (!query.data || !editing) return;
    const values = await form.validateFields();
    const value: Outbound = {
      name: values.name,
      type: "socks5",
      proxy_url: values.secretKind === "env"
        ? { env: values.secretValue }
        : values.secretKind === "file"
          ? { file: values.secretValue }
          : values.secretValue,
    };
    const change: Schemas["ConfigChange"] = editing === "create"
      ? { module: "outbound", change: { action: "create", value } }
      : { module: "outbound", change: { action: "update", original_name: editing.name, value } };
    try {
      const operation = await mutation.mutateAsync({ change, state: query.data.state });
      if (operation) setEditing(null);
    } catch {
      // mutation.error 由表单容器呈现，草稿保持不变。
    }
  };

  return (
    <PageFrame
      title="代理配置"
      description="每一条代理，凭据自守。"
      actions={(
        <Space size={12}>
          {state.data ? <ConfigSyncBadge state={state.data} /> : null}
          <Button
            type="primary"
            icon={<Plus size={17} />}
            disabled={!query.data || !configStateEditable(query.data.state)}
            onClick={() => setEditing("create")}
          >
            添加代理
          </Button>
        </Space>
      )}
    >
      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {query.data ? (
        <div className="config-module-content">
          <div className="config-table-toolbar">
            <Input
              allowClear
              value={search}
              prefix={<Search size={16} aria-hidden="true" />}
              placeholder="搜索代理名称或 SecretRef"
              onChange={(event) => setSearch(event.target.value)}
            />
          </div>
          <Table
            rowKey="name"
            columns={columns}
            dataSource={visible}
            pagination={{ pageSize: 20, hideOnSinglePage: true }}
            scroll={{ x: 760 }}
            locale={{ emptyText: search ? "没有匹配的代理" : "尚未配置代理" }}
          />
        </div>
      ) : null}
      <ConfigFormModal
        open={editing !== null}
        title={editing === "create" ? "添加代理" : "编辑代理"}
        dirty={dirty}
        busy={mutation.isPending}
        error={mutation.error}
        onCancel={() => setEditing(null)}
        onSubmit={() => void submit()}
      >
        <Form form={form} layout="vertical" requiredMark="optional" onValuesChange={() => setDirty(true)}>
          <Form.Item name="name" label="名称" rules={[{ required: true }, { max: 128 }]}>
            <Input autoComplete="off" />
          </Form.Item>
          <Form.Item name="secretKind" label="SecretRef 来源" rules={[{ required: true }]}>
            <Segmented
              block
              options={[
                { label: "环境变量", value: "env" },
                { label: "文件", value: "file" },
                { label: "直接填写 URL", value: "url" },
              ]}
            />
          </Form.Item>
          <Form.Item noStyle shouldUpdate={(previous, current) => previous.secretKind !== current.secretKind}>
            {({ getFieldValue }) => {
              const kind = getFieldValue("secretKind") as SecretKind | undefined;
              const label = kind === "file" ? "引用文件" : kind === "url" ? "代理 URL" : "环境变量";
              const placeholder = kind === "file"
                ? "./secrets/proxy.txt"
                : kind === "url"
                  ? "socks5://user:password@host:1080"
                  : "PROXY_URL";
              return (
                <Form.Item
                  name="secretValue"
                  label={label}
                  rules={[{ required: true }, { max: 4096 }]}
                >
                  <Input autoComplete="off" placeholder={placeholder} />
                </Form.Item>
              );
            }}
          </Form.Item>
          <Space size={6}>
            <Typography.Text type="secondary">
              环境变量与文件只保存引用位置；直接填写的 URL 会脱敏显示密码，未修改时保留原密码。
            </Typography.Text>
          </Space>
        </Form>
      </ConfigFormModal>
    </PageFrame>
  );
}
