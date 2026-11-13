# 活动计划

> 文档状态：有效
>
> 适用范围：尚需实施、决策或验收的独立变更

| 计划 | 计划状态 | 内容 |
| --- | --- | --- |
| [Docker 镜像交付](docker-image.md) | 待验收 | 独立 workflow 响应 tag 推送，自行编译并推送 `linux/amd64` 镜像到 GHCR，不依赖 Release 流程；工作目录固定 `/etc/fluxdns`，附 `docker/` 部署示例、env 模板与镜像冒烟夹具 |
| [负缓存新鲜度收敛](negative-cache-freshness.md) | 待验收 | REFUSED/空 Answer 已调整为固定 10s 且不乐观续用，客户端 TTL 同步；非空 NXDOMAIN 保留原策略。代码与受控测试已更新，剩余按新口径执行本地可控上游运行验收及写入/刷新观测 |
| [系统运行状态页内存分析](runtime-memory-analysis.md) | 待验收 | 正式接线与前端检查已完成；Cargo 依赖阻塞，Rust 工程验证及 Linux proc/API/DNS 连续性仍待验收 |

解析记录页面重设计与请求级 ECS 观测已按本轮工程交付范围完成，行为见[前端页面实现](../implementation/frontend/pages.md)，工程验证及用户取消运行时验收的边界见 [Management 实现](../implementation/backend/management.md#ecs-工程验证与边界)。

解析记录包含搜索、配置读取恢复和策略规则 ECS 编辑已完成，当前行为与验收证据见[Management 实现](../implementation/backend/management.md)和[前端页面实现](../implementation/frontend/pages.md)。

WebUI 的当前实现与平台边界见[实现入口](../implementation/README.md)和[联合验收](../implementation/webui-acceptance.md)；接受的职责与约束见[架构入口](../architecture/README.md)。

计划以问题、相对当前基线的变化、步骤、风险和退出条件为中心。长期设计和实现分别放入 [architecture](../architecture/README.md) 与 [implementation](../implementation/README.md)，不保留已完成的阶段清单或总体进度文档。

代码完成但必要验收未完成时保留“待验收”。方案执行完成后，新逻辑必须沉淀到对应 implementation；若改变原有设计，同步更新对应 architecture，然后删除方案文档与索引项，不建立 archive/history。取消方案只保留已实际发生的变更与有效决策，不把未实施内容写成现状。具体流程见[文档维护规则](../rules/documentation-maintenance.md)。
