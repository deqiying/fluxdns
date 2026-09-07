# 活动计划

> 文档状态：有效
>
> 适用范围：尚需实施、决策或验收的独立变更

WebUI 重构的 D-01 至 D-12 已在[决策清单](webui-management-decisions.md)记录确认结果。2026-09-07 已追加授权 P1 公共底座、必要验证和中文 Conventional Commit 本地提交，不 push、不自动进入 P2。GC-01/BC-01 不重复实施；BC-02 活动源与候选内部能力已落实，生产接线仍待服务控制和新 owner，BC-26 未完成。[配置专项](webui-management-config-runtime-plan.md)统一热更新与文件处理语义。

| 计划 | 文档状态 | 计划状态 | 剩余范围 |
| --- | --- | --- | --- |
| [WebUI 重构决策清单](webui-management-decisions.md) | 有效 | 实施中 | 12 项决定已确认；P0 技术核定与剩余 T 项 |
| [WebUI 重构开发总计划](webui-management-development-plan.md) | 有效 | 实施中 | P0 内部契约已交付；P1 已授权，后端内部底座与 12 路由壳层已推进，P2-P5 未授权 |
| [WebUI 配置热更新专项](webui-management-config-runtime-plan.md) | 有效 | 实施中 | 活动源、服务队列消费者、文件事务/状态与差异投影内部能力及仅提示 watcher 已落实；正式 v2 应用/恢复、异步事务和 HTTP/UI 未闭合 |
| [WebUI 后端开发计划](webui-management-backend-development-plan.md) | 有效 | 实施中 | BC-02/03/29 内部能力已推进；完整应用/持久化接线未完成；BE-01 启动及 BC-26 待新 owner |
| [WebUI 前端开发计划](webui-management-frontend-development-plan.md) | 有效 | 实施中 | FC-01 Bearer、12 路由壳层、主题与响应式导航已接入；v2 切换、FC-02/16 和业务页面待实施 |
| [WebUI 管理后台重构需求](webui-management-requirements.md) | 草案 | 待评审 | 保留已审阅模块范围；已按决定校正 name、热配置和旧图标注解释 |
| [WebUI 配套后端重构方案](webui-management-backend-refactor.md) | 草案 | 待评审 | 身份、独立快照、统一保留与日分片；取消旧版迁移/兼容，接入热配置专项 |

计划以问题、相对当前基线的变化、步骤、风险和退出条件为中心。长期设计和实现分别放入 [architecture](../architecture/README.md) 与 [implementation](../implementation/README.md)，不保留已完成的阶段清单或总体进度文档。

代码完成但必要验收未完成时保留“待验收”。方案执行完成后，新逻辑必须沉淀到对应 implementation；若改变原有设计，同步更新对应 architecture，然后删除方案文档与索引项，不建立 archive/history。取消方案只保留已实际发生的变更与有效决策，不把未实施内容写成现状。具体流程见[文档维护规则](../rules/documentation-maintenance.md)。
