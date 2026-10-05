import { App, Button, Tooltip } from "antd";
import { Copy } from "lucide-react";

interface CopyButtonProps {
  text: string;
  /** 无障碍名称，需包含被复制对象，例如「复制客户端 ID desktop」。 */
  label: string;
}

/**
 * 小号复制按钮。非安全上下文（局域网 HTTP）没有 navigator.clipboard，
 * 此时明确提示失败而不是静默无反应。
 */
export function CopyButton({ text, label }: CopyButtonProps) {
  const { message } = App.useApp();
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      void message.success("已复制");
    } catch {
      void message.error("复制失败，请手动选择文本复制");
    }
  };
  return (
    <Tooltip title="复制">
      <Button
        type="text"
        size="small"
        className="client-copy-button"
        aria-label={label}
        icon={<Copy size={13} aria-hidden="true" />}
        onClick={(event) => {
          event.stopPropagation();
          void copy();
        }}
      />
    </Tooltip>
  );
}
