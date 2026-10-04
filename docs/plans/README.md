# 活动计划

> 文档状态：有效
>
> 适用范围：尚需实施、决策或验收的独立变更

| 计划 | 计划状态 | 内容 |
| --- | --- | --- |
| [Docker 镜像交付](docker-image.md) | 待验收 | 独立 workflow 响应 tag 推送，自行编译并推送 `linux/amd64` 镜像到 GHCR，不依赖 Release 流程；工作目录固定 `/etc/fluxdns`，附 `docker/` 部署示例、env 模板与镜像冒烟夹具 |
| [负缓存新鲜度收敛](negative-cache-freshness.md) | 待验收 | 代码与文档已完成：NODATA/NXDOMAIN 使用独立的短乐观窗口和 TTL 上限，失败类不再乐观返回，后台刷新记录响应类与 TTL；剩余本地可控上游的运行验收 |

解析记录页面与缓存标签已按最终设计完成，行为与验证边界见[前端页面实现](../implementation/frontend/pages.md)。

解析记录包含搜索、配置读取恢复和策略规则 ECS 编辑已完成，当前行为与验收证据见[Management 实现](../implementation/backend/management.md)和[前端页面实现](../implementation/frontend/pages.md)。

WebUI 的当前实现与平台边界见[实现入口](../implementation/README.md)和[联合验收](../implementation/webui-acceptance.md)；接受的职责与约束见[架构入口](../architecture/README.md)。

计划以问题、相对当前基线的变化、步骤、风险和退出条件为中心。长期设计和实现分别放入 [architecture](../architecture/README.md) 与 [implementation](../implementation/README.md)，不保留已完成的阶段清单或总体进度文档。

代码完成但必要验收未完成时保留“待验收”。方案执行完成后，新逻辑必须沉淀到对应 implementation；若改变原有设计，同步更新对应 architecture，然后删除方案文档与索引项，不建立 archive/history。取消方案只保留已实际发生的变更与有效决策，不把未实施内容写成现状。具体流程见[文档维护规则](../rules/documentation-maintenance.md)。
