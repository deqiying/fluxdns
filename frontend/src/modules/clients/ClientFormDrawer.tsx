import { useEffect, useState, type ReactNode } from "react";
import { Alert, App, Button, Drawer, Form, Input, Segmented, Select, Space, Typography, type FormProps } from "antd";
import type { Rule } from "antd/es/form";
import { Check, Lock, Pencil, RefreshCw, Save, Undo2 } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import { ApiError, getSafeErrorMessage } from "@/shared/api/errors";
import { DurationInput, durationOptionalRules } from "@/shared/components/DurationInput";
import { clientEditValue } from "@/shared/config/contract";
import { normalizeDuration } from "@/shared/config/form-values";
import { useConfigChangeMutation } from "@/shared/config/hooks";
import { ClientIdConfirmModal } from "./ClientIdConfirmModal";
import { CopyButton } from "./CopyButton";
import { abbreviateId, clientDohPaths, generateClientId, type Client } from "./client-view";

type Schemas = components["schemas"];

export type ClientEditorTarget = Client | "create";

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

interface ClientFormDrawerProps {
  target: ClientEditorTarget | null;
  /** 当前活动目录，用于 ID 唯一性本地预检。 */
  items: Client[];
  strategyOptions: { label: string; value: string }[];
  /** 带 `{client_id}` 占位符的 DoH 路由模板，用于生成新地址提示。 */
  routeTemplates: string[];
  configState: Schemas["ConfigState"] | undefined;
  onClose: () => void;
  /** 保存成功后回传最终名称，由列表负责关闭抽屉与短暂高亮。 */
  onSaved: (name: string) => void;
}

/** 与后端 valid_client_id 一致：1-128 个 URL unreserved ASCII，大小写敏感。 */
const CLIENT_ID_PATTERN = /^[A-Za-z0-9._~-]{1,128}$/;
const CACHE_OPTIONS = [{ label: "继承", value: "inherit" }, { label: "启用", value: "enabled" }, { label: "禁用", value: "disabled" }];
const TTL_OPTIONS = [{ label: "继承", value: "inherit" }, { label: "启用覆盖", value: "enabled" }, { label: "禁用覆盖", value: "disabled" }];
const ECS_OPTIONS = [{ label: "继承", value: "inherit" }, { label: "禁用", value: "disabled" }, { label: "客户端地址", value: "client" }, { label: "自定义", value: "custom" }];

interface IdMeta {
  errors: number;
  validating: boolean;
}

interface PendingConfirm {
  previous: string;
  next: string;
  resolve: (confirmed: boolean) => void;
}

/** 必填标记放在标签后，用 CSS 绘制星号，避免污染标签的可访问名称。 */
const requiredMark: FormProps["requiredMark"] = (label, { required }) => (
  <>
    {label}
    {required ? <span className="client-required-mark" aria-hidden="true" /> : null}
  </>
);

export function ClientFormDrawer({ target, items, strategyOptions, routeTemplates, configState, onClose, onSaved }: ClientFormDrawerProps) {
  const { modal, message } = App.useApp();
  const mutation = useConfigChangeMutation("clients");
  const [form] = Form.useForm<ClientFormValues>();
  const ttlMode = Form.useWatch("ttl_mode", form);
  const ecsMode = Form.useWatch("ecs_mode", form);
  const clientId = Form.useWatch("client_id", form);
  const [dirty, setDirty] = useState(false);
  // 编辑已有客户端时 ID 默认锁定，显式解锁后才允许修改请求身份。
  const [idUnlocked, setIdUnlocked] = useState(false);
  const [idMeta, setIdMeta] = useState<IdMeta>({ errors: 0, validating: false });
  const [pendingConfirm, setPendingConfirm] = useState<PendingConfirm | null>(null);
  const busy = mutation.isPending;
  const creating = target === "create";
  const originalId = target && target !== "create" ? target.client_id : null;
  const idEditable = creating || idUnlocked;
  const idChanged = originalId !== null && typeof clientId === "string" && clientId !== originalId;
  const idValid = typeof clientId === "string" && clientId !== "" && idMeta.errors === 0 && !idMeta.validating;
  const impactPaths = idChanged && idValid ? clientDohPaths(routeTemplates, clientId) : [];

  // 唯一性只做本地预检，最终以后端整份候选校验为准。
  const clientIdRules: Rule[] = [
    { required: true, message: "客户端 ID 不能为空" },
    { pattern: CLIENT_ID_PATTERN, message: "仅支持 A-Z a-z 0-9 . _ ~ -，长度 1-128，区分大小写" },
    {
      validator: async (_, value?: string) => {
        if (!value || value === originalId) return;
        const owner = items.find((item) => item.client_id === value);
        if (owner) throw new Error(`已被客户端 “${owner.name}” 使用`);
      },
    },
  ];

  useEffect(() => {
    if (!target) return;
    mutation.reset();
    setDirty(false);
    setIdUnlocked(false);
    setIdMeta({ errors: 0, validating: false });
    setPendingConfirm(null);
    if (target === "create") {
      form.setFieldsValue({ name: "", client_id: "", ips: [], cache_mode: "inherit", ttl_mode: "inherit", ecs_mode: "inherit" });
    } else {
      form.setFieldsValue({
        name: target.name,
        client_id: target.client_id,
        ips: target.match?.ips ?? [],
        strategy: target.strategy,
        cache_mode: target.cache ? (target.cache.enabled ? "enabled" : "disabled") : "inherit",
        ttl_mode: target.ttl_override ? (target.ttl_override.enabled === false ? "disabled" : "enabled") : "inherit",
        ttl_min: normalizeDuration(target.ttl_override?.min),
        ttl_max: normalizeDuration(target.ttl_override?.max),
        ecs_mode: target.edns_client_subnet?.mode ?? "inherit",
        ecs_custom_ip: target.edns_client_subnet?.custom_ip,
      });
    }
  }, [target, form]);

  const requestClose = () => {
    if (busy) return;
    if (!dirty) {
      onClose();
      return;
    }
    modal.confirm({
      title: "放弃未保存的修改？",
      content: "当前草稿尚未保存。",
      okText: "放弃修改",
      cancelText: "继续编辑",
      okButtonProps: { danger: true },
      onOk: onClose,
    });
  };

  const trackIdMeta: FormProps<ClientFormValues>["onFieldsChange"] = (changed) => {
    const field = changed.find((item) => [item.name].flat()[0] === "client_id");
    if (field) setIdMeta({ errors: field.errors?.length ?? 0, validating: Boolean(field.validating) });
  };

  const revalidateClientId = () => {
    // 校验结果直接显示在字段下方，这里无需再处理拒绝。
    void form.validateFields(["client_id"]).catch(() => undefined);
  };

  const resetClientId = () => {
    if (originalId === null) return;
    form.setFieldValue("client_id", originalId);
    revalidateClientId();
    setIdUnlocked(false);
  };

  const fillGeneratedId = () => {
    form.setFieldValue("client_id", generateClientId());
    setDirty(true);
    revalidateClientId();
  };

  /** 修改 ID 会让旧身份的请求不再命中本客户端，保存前需要单独确认；历史记录不迁移。 */
  const confirmClientIdChange = (previous: string, next: string) =>
    new Promise<boolean>((resolve) => setPendingConfirm({ previous, next, resolve }));

  const settleConfirm = (confirmed: boolean) => {
    pendingConfirm?.resolve(confirmed);
    setPendingConfirm(null);
  };

  const submit = async () => {
    if (!configState || !target) return;
    let values: ClientFormValues;
    try {
      values = await form.validateFields();
    } catch {
      // 字段错误已内联展示，保留草稿等待修正。
      return;
    }
    const common = {
      name: values.name,
      match: { ips: values.ips ?? [] },
      ...(values.strategy ? { strategy: values.strategy } : {}),
      ...(values.cache_mode === "inherit" ? {} : { cache: { enabled: values.cache_mode === "enabled" } }),
      ...(values.ttl_mode === "inherit" ? {} : { ttl_override: values.ttl_mode === "disabled" ? { enabled: false } : { enabled: true, ...(values.ttl_min ? { min: values.ttl_min } : {}), ...(values.ttl_max ? { max: values.ttl_max } : {}) } }),
      ...(values.ecs_mode === "inherit" ? {} : { edns_client_subnet: { mode: values.ecs_mode, ...(values.ecs_mode === "custom" && values.ecs_custom_ip ? { custom_ip: values.ecs_custom_ip } : {}) } }),
    };
    let change: Schemas["ConfigChange"];
    let nextId: string | undefined;
    if (target === "create") {
      change = { module: "clients", change: { action: "create", value: { ...common, client_id: values.client_id } } };
    } else {
      // 只有 ID 实际变化时才提交 client_id；未变化的编辑保持原有白名单形状。
      nextId = values.client_id !== target.client_id ? values.client_id : undefined;
      if (nextId && !(await confirmClientIdChange(target.client_id, nextId))) return;
      const value: Schemas["ClientEdit"] = { ...clientEditValue({ ...common, client_id: target.client_id }), ...(nextId ? { client_id: nextId } : {}) };
      change = { module: "clients", change: { action: "update", original_name: target.name, value } };
    }
    try {
      const operation = await mutation.mutateAsync({ change, state: configState });
      if (!operation) return;
      if (nextId) void message.success(`客户端 ID 已更新为 ${nextId}`);
      onSaved(values.name);
    } catch (error) {
      // 冲突与校验失败时保留草稿，错误同时显示在抽屉顶部。
      void message.error(`保存失败：${getSafeErrorMessage(error)}`);
    }
  };

  const idExtra: ReactNode = idChanged || (creating && clientId)
    ? (
      <span className="client-id-extra">
        {idValid ? <span className="client-id-valid"><Check size={14} aria-hidden="true" />格式有效，且未被其他客户端使用</span> : <span />}
        {idChanged && originalId ? <span title={originalId}>原 ID：<code>{abbreviateId(originalId)}</code></span> : null}
      </span>
    )
    : "终端的请求身份：DoH 路径中的 {client_id} 段或精确 ID 匹配";

  const footerStatus = dirty ? (idChanged ? "客户端 ID 将被修改，保存时需确认" : "有未保存的修改") : "";

  return (
    <Drawer
      className="client-drawer"
      open={target !== null}
      size={560}
      destroyOnHidden
      keyboard={!busy}
      mask={{ closable: !busy }}
      closable={{ placement: "end", disabled: busy }}
      onClose={requestClose}
      title={(
        <div className="client-drawer-title">
          <span>{creating ? "添加客户端" : "编辑客户端"}</span>
          <Typography.Text type="secondary">修改先应用到运行时，再写入配置文件</Typography.Text>
        </div>
      )}
      footer={(
        <div className="client-drawer-footer">
          <span className={`client-drawer-status${dirty ? " client-drawer-status-dirty" : ""}`}>{footerStatus}</span>
          <Space>
            <Button disabled={busy} onClick={requestClose}>取消</Button>
            <Button type="primary" icon={<Save size={16} aria-hidden="true" />} loading={busy} onClick={() => void submit()}>保存</Button>
          </Space>
        </div>
      )}
    >
      {mutation.error ? (
        <Alert
          className="config-form-error"
          type="error"
          showIcon
          title={getSafeErrorMessage(mutation.error)}
          description={mutation.error instanceof ApiError && mutation.error.requestId
            ? <Typography.Text type="secondary">请求 ID：{mutation.error.requestId}</Typography.Text>
            : undefined}
        />
      ) : null}
      <Form form={form} layout="vertical" requiredMark={requiredMark} onValuesChange={() => setDirty(true)} onFieldsChange={trackIdMeta}>
        <div className="client-form-section">基本信息</div>
        <Form.Item name="name" label="管理名称" rules={[{ required: true, message: "管理名称不能为空" }, { max: 128, message: "最多 128 个字符" }]} extra="界面展示与策略引用使用的唯一名称，最多 128 字符">
          <Input />
        </Form.Item>

        {/* ID 标签行自绘：右侧放修改/撤销操作，标签本身只保留纯文本以维持输入框的可访问名称。 */}
        <div className="client-id-head">
          <span className="client-id-head-label">
            <label htmlFor="client_id">客户端 ID</label>
            <span className="client-required-mark" aria-hidden="true" />
            {idChanged ? <span className="client-id-changed">已修改</span> : null}
          </span>
          {creating ? null : idUnlocked
            ? <Button type="link" size="small" icon={<Undo2 size={14} aria-hidden="true" />} aria-label="撤销客户端 ID 修改" onClick={resetClientId}>撤销修改</Button>
            : <Button type="link" size="small" icon={<Pencil size={14} aria-hidden="true" />} aria-label="修改客户端 ID" onClick={() => setIdUnlocked(true)}>修改</Button>}
        </div>
        <Form.Item name="client_id" rules={clientIdRules} className="client-id-item" extra={idExtra}>
          {idEditable ? (
            <Input
              className="client-id-input"
              placeholder="例如 phone-xiao"
              showCount
              maxLength={128}
              autoFocus={!creating}
              suffix={<Button type="link" size="small" className="client-id-generate" icon={<RefreshCw size={13} aria-hidden="true" />} aria-label="生成随机客户端 ID" onClick={fillGeneratedId}>生成</Button>}
            />
          ) : (
            <Input
              className="client-id-input client-id-locked"
              readOnly
              prefix={<Lock size={14} aria-hidden="true" />}
              suffix={originalId ? <CopyButton text={originalId} label="复制客户端 ID" /> : null}
            />
          )}
        </Form.Item>
        {idChanged ? (
          <Alert
            className="client-id-impact"
            type="warning"
            showIcon
            title="修改客户端 ID 会改变该终端的请求身份"
            description={(
              <ul>
                <li>
                  终端需改用新的 DoH 地址；使用旧 ID 的请求将不再匹配本客户端
                  {impactPaths.length > 0 ? <span className="client-id-impact-paths">{impactPaths.map((path) => <code key={path}>{path}</code>)}</span> : null}
                </li>
                <li>历史查询与统计仍归属旧 ID，不会迁移或重写</li>
                <li>该客户端的缓存池将按新身份重新建立</li>
              </ul>
            )}
          />
        ) : null}

        <div className="client-form-section">匹配规则</div>
        <Form.Item name="ips" label={<>IP / CIDR<span className="client-optional-mark">（可选）</span></>} extra="优先按客户端 ID 匹配；未命中时按最长 CIDR 前缀匹配">
          <Select mode="tags" tokenSeparators={[","]} placeholder="输入地址后回车" className="client-ip-select" />
        </Form.Item>

        <div className="client-form-section">策略与覆盖</div>
        <Form.Item name="strategy" label={<>策略<span className="client-optional-mark">（可选）</span></>}>
          <Select allowClear showSearch options={strategyOptions} placeholder="继承默认" />
        </Form.Item>
        <div className="client-override-panel">
          <OverrideRow label="缓存" name="cache_mode" options={CACHE_OPTIONS} />
          <OverrideRow label="TTL" name="ttl_mode" options={TTL_OPTIONS}>
            {ttlMode === "enabled" ? (
              <div className="client-override-pair">
                <Form.Item name="ttl_min" label="最小 TTL" rules={durationOptionalRules}><DurationInput label="最小 TTL" /></Form.Item>
                <Form.Item name="ttl_max" label="最大 TTL" rules={durationOptionalRules}><DurationInput label="最大 TTL" /></Form.Item>
              </div>
            ) : null}
          </OverrideRow>
          <OverrideRow label="ECS" name="ecs_mode" options={ECS_OPTIONS}>
            {ecsMode === "custom" ? (
              <Form.Item name="ecs_custom_ip" label="自定义 ECS 地址" rules={[{ required: true, message: "请填写 IP 或 CIDR" }]}>
                <Input placeholder="例如 192.0.2.0/24" />
              </Form.Item>
            ) : null}
          </OverrideRow>
        </div>
        <Typography.Paragraph type="secondary" className="client-override-note">
          “继承”表示沿用策略或全局配置；留空 TTL 边界表示不设置，0s 表示不设限
        </Typography.Paragraph>
      </Form>
      <ClientIdConfirmModal
        open={pendingConfirm !== null}
        previous={pendingConfirm?.previous ?? ""}
        next={pendingConfirm?.next ?? ""}
        dohPaths={pendingConfirm ? clientDohPaths(routeTemplates, pendingConfirm.next) : []}
        onConfirm={() => settleConfirm(true)}
        onCancel={() => settleConfirm(false)}
      />
    </Drawer>
  );
}

interface OverrideRowProps {
  label: string;
  name: "cache_mode" | "ttl_mode" | "ecs_mode";
  options: { label: string; value: string }[];
  children?: ReactNode;
}

/** 覆盖项一行一个分段控件，一眼看到当前值与可选项；启用后的补充字段在行内展开。 */
function OverrideRow({ label, name, options, children }: OverrideRowProps) {
  const labelId = `client-${name}-label`;
  return (
    <div className="client-override-row">
      <div className="client-override-head">
        <span id={labelId}>{label}</span>
        <Form.Item name={name} noStyle>
          <Segmented size="small" options={options} aria-labelledby={labelId} />
        </Form.Item>
      </div>
      {children ? <div className="client-override-body">{children}</div> : null}
    </div>
  );
}
