import { useEffect, useState } from "react";
import { Button, Checkbox, Modal, Typography } from "antd";
import { TriangleAlert } from "lucide-react";
import { CopyButton } from "./CopyButton";

interface ClientIdConfirmModalProps {
  open: boolean;
  previous: string;
  next: string;
  /** 按已配置 DoH 路由代入新 ID 的路径；为空时隐藏地址区块。 */
  dohPaths: string[];
  onConfirm: () => void;
  onCancel: () => void;
}

/**
 * 修改客户端 ID 的二次确认。必须勾选知情项才能确认：
 * 旧 ID 的请求将不再匹配本客户端，历史记录仍归属旧 ID 且不迁移。
 */
export function ClientIdConfirmModal({ open, previous, next, dohPaths, onConfirm, onCancel }: ClientIdConfirmModalProps) {
  const [acknowledged, setAcknowledged] = useState(false);
  useEffect(() => {
    if (open) setAcknowledged(false);
  }, [open]);

  return (
    <Modal
      className="client-id-confirm"
      open={open}
      width={480}
      centered
      destroyOnHidden
      title={(
        <div className="client-id-confirm-title">
          <span className="client-id-confirm-icon"><TriangleAlert size={18} aria-hidden="true" /></span>
          <div>
            <div>确认修改客户端 ID？</div>
            <Typography.Text type="secondary">保存后立即应用到运行时，并写入配置文件。</Typography.Text>
          </div>
        </div>
      )}
      onCancel={onCancel}
      footer={(
        <>
          <Button onClick={onCancel}>取消</Button>
          <Button type="primary" disabled={!acknowledged} onClick={onConfirm}>确认修改</Button>
        </>
      )}
    >
      <dl className="client-id-confirm-ids">
        <dt>原 ID</dt>
        <dd><span className="client-id-chip client-id-chip-old">{previous}</span></dd>
        <dt>新 ID</dt>
        <dd><span className="client-id-chip client-id-chip-new">{next}</span></dd>
      </dl>
      {dohPaths.length > 0 ? (
        <section className="client-id-confirm-doh">
          <div className="client-id-confirm-doh-head">
            <Typography.Text strong>请同步更新终端上的 DoH 地址</Typography.Text>
            <Typography.Text type="secondary">按已配置路由生成</Typography.Text>
          </div>
          <ul>
            {dohPaths.map((path) => (
              <li key={path}>
                <code>{path}</code>
                <CopyButton text={path} label={`复制 DoH 路径 ${path}`} />
              </li>
            ))}
          </ul>
        </section>
      ) : null}
      <Checkbox checked={acknowledged} onChange={(event) => setAcknowledged(event.target.checked)}>
        我已了解：旧 ID 的请求将不再匹配本客户端，历史记录保持不变
      </Checkbox>
    </Modal>
  );
}
