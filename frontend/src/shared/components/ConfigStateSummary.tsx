import type { ConfigState } from "@/shared/config/api";

/** 标题区只显示磁盘与运行配置是否一致。 */
export function ConfigSyncBadge({ state }: { state: ConfigState }) {
  const synced = state.disk.status === "in_sync";
  const label = synced ? "已同步" : "磁盘不一致";
  return (
    <span role="status" aria-label={`配置同步状态：${label}`} className={`config-sync-badge config-sync-badge-${synced ? "synced" : "pending"}`}>
      <i aria-hidden="true" />
      {label}
    </span>
  );
}
