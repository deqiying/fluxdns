import { useEffect } from "react";
import { Alert, Button, Space } from "antd";
import { useConfigDiskAction, useConfigState } from "@/shared/config/hooks";

const titles = {
  modified: "配置文件已在磁盘上被修改（或上次写入失败），与当前运行配置不一致",
  missing: "配置文件不存在",
  unreadable: "配置文件无法读取",
};

/** 全局提示只反映磁盘状态；编辑器不以此限制保存。 */
export function DiskStatusBanner() {
  const { data, refetch } = useConfigState();
  const action = useConfigDiskAction();
  const status = data?.disk.status;

  useEffect(() => {
    const refresh = () => { void refetch({ cancelRefetch: false }); };
    window.addEventListener("focus", refresh);
    return () => window.removeEventListener("focus", refresh);
  }, [refetch]);

  if (!status || status === "in_sync") return null;
  return (
    <Alert
      className="disk-status-banner"
      type="warning"
      showIcon
      title={titles[status]}
      action={(
        <Space wrap>
          {status === "modified" ? (
            <Button size="small" disabled={action.isPending} loading={action.isPending && action.variables === "reload"} onClick={() => action.mutate("reload")}>重新加载</Button>
          ) : null}
          <Button size="small" disabled={action.isPending} loading={action.isPending && action.variables === "persist"} onClick={() => action.mutate("persist")}>写入磁盘</Button>
        </Space>
      )}
    />
  );
}
