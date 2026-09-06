# WebUI 管理后台视觉草案

> 文档状态：草案
>
> 适用范围：WebUI 管理后台重构计划的视觉评审图稿、用途与生命周期

本目录集中存放 [WebUI 管理后台重构需求](../webui-management-requirements.md)的评审图稿。图稿用于确认视觉方向，不代表已接受的详细设计、当前实现或运行验收结果。

共 29 张桌面 SVG，覆盖全部 12 个一级模块。打开[本地评审总览](review.html)可按模块浏览、点击缩略图查看原图；不依赖网络、脚本或开发服务器。SVG 为可编辑源文件，本地 PNG 预览与检查产物不纳入版本控制。

## 本轮调整

根据[配套后端方案](../webui-management-backend-refactor.md)，重画客户端列表/编辑、解析列表/详情、DNS 概览与缓存编辑；新增 IP 匹配详情，以统计保留编辑替代旧的按条数记录配置。TTL/ECS 弹窗背景及系统存储只读信息同步更新，其他图稿保持原样。

- 客户端 ID 唯一，名称可重复；IP/CIDR 继续参与策略匹配。
- 解析详情分开显示原始 ID/IP、当时匹配 ID/来源和当前名称；UDP 示例的原始 ID 为“未传入”，缓存第二行仍为生产上游。
- 缓存使用可选 `.db` 快照及覆盖周期，无持久化配额；统计统一管理天数、宽限期和参考大小，解析详情只保留开关。

详细语义和未展开状态以[需求正文](../webui-management-requirements.md#7-视觉草案)为准。新增配置与查询字段均为提案，不代表现有 API 已支持。

## 配置与状态

| 模块 | 一级页面 | 配套编辑视图 |
| --- | --- | --- |
| 监听入口 | [入口列表](webui-listeners.svg)：协议、绑定地址、策略及 ECS 继承 | [UDP/TCP 编辑](webui-listener-editor.svg)、[DoH 编辑](webui-doh-listener-editor.svg) |
| DNS 配置 | [全局概览](webui-dns-settings.svg)：缓存、TTL/ECS、统计与详情开关 | [缓存快照](webui-dns-cache-editor.svg)、[TTL/ECS](webui-dns-policy-editor.svg)、[统计与数据保留](webui-statistics-editor.svg) |
| DNS 分流策略 | [策略列表](webui-dns-strategies.svg)：规则数、默认上游及覆盖关系 | [策略编辑](webui-strategy-editor.svg)：有序匹配表与独立覆盖项 |
| Hosts 配置 | [资源列表](webui-hosts.svg)：内联/文件来源及重载状态 | [内联编辑](webui-hosts-editor.svg)：结构化主机映射 |
| 规则集 | [规则集列表](webui-rule-sets.svg)：来源、格式、刷新计划与陈旧快照 | [远程规则编辑](webui-rule-set-editor.svg)：URL、代理与定时更新 |
| 客户端配置 | [客户端列表](webui-clients.svg)：唯一 ID、名称、IP/CIDR 及策略 | [客户端编辑](webui-client-editor.svg)：ID 只读、可选 IP 匹配与继承/覆盖 |
| 代理配置 | [代理列表](webui-proxies.svg)：协议族、SecretRef 来源和引用 | [代理编辑](webui-proxy-editor.svg)：仅编辑环境变量或文件引用 |
| 系统配置 | [系统概览](webui-system-settings.svg)：日志可编辑，统计库/详情目录等只读 | [日志编辑](webui-logs-editor.svg)：开关、级别、路径及重启提示 |
| 系统运行状态 | [进程概览](webui-system-runtime.svg)：时长、内存、线程与基础信息 | 无配置操作 |
| DNS 上游 | [上游组标签页](webui-upstream-groups.svg)：主成员、选择模式和回退 | [上游组编辑](webui-upstream-group-editor.svg)：保留旧名称及模式约束 |

## 服务与解析

| 图稿 | 用途 |
| --- | --- |
| [服务状态](webui-service-status.svg) | 浅色模式：四项关键指标、轻面积填充的 QPS 主线与 RPM 辅助曲线、选中时刻读数；不含 WebSocket 状态或监听入口摘要 |
| [服务状态 · 深色模式](webui-service-status-dark.svg) | 相同布局与数据，评审中性深色背景、文字对比度与曲线配色 |
| [DNS 上游](webui-dns-upstreams.svg) | 展示上游和上游组的浏览、筛选、配置列表与编辑入口 |
| [DNS 上游编辑](webui-doh-upstream-editor.svg) | 以 DoH 为当前类型，展示名称修改、类型选择及连接、代理出口和 ECS 配置；改名保存必须携带旧名称，约束见需求正文 |
| [解析记录列表](webui-query-records.svg) | 倒序列表，客户端区分当时匹配与当前名称，原始身份单独筛选；覆盖异常、截断与历史未保留 |
| [ID 匹配悬浮详情](webui-query-record-details.svg) | DoH 原始 ID 与当时匹配 ID 分列，当前名称单独显示；保持稳定记录及 Answer 明细 |
| [IP 匹配悬浮详情](webui-query-record-ip-details.svg) | UDP 原始 ID 未传入、IP 策略匹配与缓存生产上游；不回填原始 ID |

## 评审边界

全部画布为 1600 × 1040，统一字体、导航、状态色、分区、列表与弹窗风格。浅色图是本轮基准；深色风格仅提供既有服务状态样例，不暗示其余页面已完成深色适配。

图中使用演示地址、路径、变量名和数值。配置来源切换、展开字段、空态、错误态、窄屏和真实交互仍需后续详细设计；图中按钮不执行配置操作。本轮验证范围为 SVG 结构、栅格化视觉检查与文档链接，不包含前后端实现或运行验收。

本目录与对应活动计划保持相同生命周期。计划完成后删除仅用于评审的图稿；仍有长期设计价值的图稿迁入对应 `docs/architecture/` 文档范围并同步引用。
