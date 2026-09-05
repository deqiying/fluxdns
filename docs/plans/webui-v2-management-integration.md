# WebUI v2 集成验收计划

> 文档状态：有效
>
> 计划状态：待验收
>
> 适用范围：已实现 v2 管理面、认证/初始化与内嵌发布的剩余环境和故障验收
>
> 代码基线：`0f18d5b2ddf67625121fd7e0662e21723362565f`

## 问题与现状依据

Management listener、setup/auth/session、配置事务、七个只读 API、前端集成和发布脚本已接入代码。不能把剩余浏览器/平台验收写成代码未实现，也不能用 mock 或 workflow 文件存在关闭原验收要求。

正式接线见[管理端实现](../implementation/backend/management.md)、[前端实现](../implementation/frontend/README.md)和[交付实现](../implementation/delivery.md)。原方案 2026-09-04 的 Windows/HTTP/DOM/Console 报告已集中迁入交付文档并标记本轮未复核；没有可追溯基线的测试总数不作为当前证据。

## 目标与非目标

目标是补齐原 v2 验收证据，发现失败后先定位原因、确定修复范围，再复验。稳定契约已收敛至 [Management](../architecture/management.md)、[Config](../architecture/backend/modules/config.md) 与[前端设计](../architecture/frontend.md)，本计划不重复接口 schema 或改动这些契约。

不新增通用配置编辑、用户管理、DNS query、cache clear 或 runtime command；不因验收自动安装代理、触发收费服务、push tag 或发布 Release。外部/不可逆动作按现有审批边界执行。

## 剩余任务

| 任务 | 前置条件与执行范围 | 合格证据 | 状态 |
| --- | --- | --- | --- |
| 真实浏览器安全与同源 | 支持 Cookie/Network/Storage 观察的浏览器、本地隔离配置；HTTPS 需已有可信代理环境 | setup required/ready、初始化跳转/409、登录/恢复/退出/过期、Origin/Fetch Metadata、限流；Cookie HttpOnly/Strict/Path/Domain/Secure 和无 token 持久化 | 待执行 |
| 原生发布与真实 Actions | 对应 Windows/Linux x86_64、macOS ARM64 runner；另行批准真实发布动作 | main/tag/version gate、共享门禁、同一前端产物、三个可执行 binary、归档与 checksums、Release 汇总均有真实运行记录 | 待执行 |
| 无外部 WebUI 的部署验收 | 每个目标平台的内嵌 binary、受保护配置，无外部 dist/Node 依赖 | SPA 深链接、静态 MIME/HEAD/ETag/304/CSP/cache、未知 API JSON 错误，三阶段独立构建物保留 | Windows 仅有旧报告；其余待执行 |
| 配置事务与安全故障矩阵 | 临时源文件/snapshot、可控中断与权限环境，禁止使用真实配置 | 单/双路径、每个 crash point、journal 损坏、外部指纹冲突、权限和 symlink、并发 setup、watcher self-change；无秘密泄漏 | 有定向测试代码，完整矩阵待验收 |
| 生命周期与 DNS 回归 | 本地 UDP/TCP/DoH 和 Management 隔离端口 | enable false 不监听；enable true bind 冲突失败；运行期 fatal/accept 重试与 shutdown 有界，无悬挂任务，DNS contract/smoke 正常 | 待补可追溯运行记录 |

## 执行顺序

1. 冻结本次验收源码 commit、平台、工具版本和场景；核对相关测试当前覆盖，明确未覆盖故障。
2. 在临时目录执行后端配置/auth/session/router/query/assets 与前端路由/client/页面测试；记录实际命令，不预填通过。
3. 使用真实同源服务完成浏览器观察与 DNS/Management 回归，测试密码和 token 不进入受跟踪输出。
4. 在获准目标环境执行原生构建/部署；真实 Actions 或发布先满足外部动作授权。
5. 将新逻辑、证据和限制更新到对应 implementation；若获准的方案改变原有设计，同步更新对应 architecture。失败保留任务与最小重现，不通过更改设计掩盖问题。

## 风险与退出条件

主要风险是把 Windows 单平台、mock/handler 测试或静态 workflow 检查当成所有场景已验收。每项证据必须含日期、环境、源码 commit、命令和结果；不支持的环境明确为未执行。

退出需同时满足原 v2 的单 binary 无外部文件运行、独立管理面、OpenAPI/生成类型对齐、一次性 setup、受限配置写入与恢复、真实会话/同源保护、port-only 查询以及原生发布要求。所有剩余项完成，或由用户明确取消/调整对应验收范围后，才更新终态；确认新逻辑与证据已沉淀到 implementation、改变的设计已同步到 architecture，再删除本计划与索引项。文档结构迁移本身不关闭本计划。
