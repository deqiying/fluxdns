# WebUI 管理后台视觉草案

> 文档状态：草案
>
> 适用范围：WebUI 管理后台重构计划的视觉评审图稿、用途与生命周期

本目录集中存放 [WebUI 管理后台重构需求](../webui-management-requirements.md)的评审图稿。图稿用于确认视觉方向，不代表已接受的详细设计、当前实现或运行验收结果。

| 图稿 | 用途 |
| --- | --- |
| [服务状态](webui-service-status.svg) | 浅色模式：四项关键指标、轻面积填充的 QPS 主线与 RPM 辅助曲线、选中时刻读数；不含 WebSocket 状态或监听入口摘要 |
| [服务状态 · 深色模式](webui-service-status-dark.svg) | 相同布局与数据，评审中性深色背景、文字对比度与曲线配色 |
| [DNS 上游](webui-dns-upstreams.svg) | 展示上游和上游组的浏览、筛选、配置列表与编辑入口 |
| [DNS 上游编辑](webui-doh-upstream-editor.svg) | 以 DoH 为当前类型，展示名称修改、类型选择及连接、代理出口和 ECS 配置；改名保存必须携带旧名称，约束见需求正文 |
| [解析记录列表](webui-query-records.svg) | 倒序列表、查询筛选、结果摘要、来源和耗时，覆盖正常、异常、截断与历史未保留状态 |
| [解析记录悬浮详情](webui-query-record-details.svg) | 结果单元格锚定的非模态浮层，展示请求上下文及 Answer 明细；查看期间保持记录稳定 |

本目录与对应活动计划保持相同生命周期。计划完成后删除仅用于评审的图稿；仍有长期设计价值的图稿迁入对应 `docs/architecture/` 文档范围并同步引用。
