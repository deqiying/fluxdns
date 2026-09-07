import { Result, Tabs } from "antd";
import { useSearchParams } from "react-router-dom";
import { upstreamTabs } from "./route-contract";

export function PendingModulePage({ title }: { title: string }) {
  return <Result status="info" title={<h2>{title}</h2>} subTitle="当前版本暂不可用。" />;
}

const upstreamTabLabels = {
  upstreams: "上游",
  groups: "上游组",
} as const;

export function PendingUpstreamsPage() {
  const [searchParams, setSearchParams] = useSearchParams();
  const requestedTab = searchParams.get("tab");
  const activeTab = upstreamTabs.find((tab) => tab === requestedTab) ?? upstreamTabs[0];

  return (
    <section className="pending-upstreams-page" aria-labelledby="pending-upstreams-title">
      <h2 id="pending-upstreams-title">DNS 上游</h2>
      <Tabs
        activeKey={activeTab}
        items={upstreamTabs.map((tab) => ({
          key: tab,
          label: upstreamTabLabels[tab],
          children: <Result status="info" title={<h3>{upstreamTabLabels[tab]}</h3>} subTitle="当前版本暂不可用。" />,
        }))}
        onChange={(tab) => setSearchParams(tab === upstreamTabs[0] ? {} : { tab })}
      />
    </section>
  );
}
