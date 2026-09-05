# 系统设计

> 文档状态：有效
>
> 适用范围：FluxDNS 数据面、管理面、代码工程与交付边界
>
> 最后评审：2026-09-05（既有跨域边界拆分复核）

## 设计结论

FluxDNS 是单进程、异步事件驱动的策略 DNS 服务。后端使用一个 Rust binary crate；前端是独立 React SPA，发布时可以编译内嵌进同一 binary。没有独立复用或发布需求时，不拆多 crate 或增加额外管理进程。

```text
DNS client -> UDP / TCP / DoH adapter -> DNS / Policy -> Cache / Upstream
                                               |
                                  bounded resolution event
                                               |
                                    Stats / Detail / Cache worker

Browser -> SPA -> same-origin /api/v1 -> Management adapter
                                        | Runtime / Telemetry snapshot
                                        | ManagementStorageRead
                                        | initial-user ConfigStore
```

箭头表示请求或数据流，不是允许反向依赖具体 adapter 的许可。

## 不变量

- DNS 请求热路径不等待 SQLite、详情投影或 cache commit；后台失败和丢弃必须可观测，不能伪造成功统计。
- DoH parser 只处理 DNS HTTP envelope。管理端 JSON、Cookie、静态文件和 SPA fallback 使用独立 listener/router，不混入 DoH。
- Management 通过领域 DTO、snapshot 与 port 读数据，handler 不直连 SQLx，不直接控制 DNS transport。
- 后端是配置解析、路径、继承、策略和安全校验的事实来源。前端不复制 DNS 业务逻辑，不直连上游 DNS。
- 配置、已校验候选和活动运行时分离；prepare 或 bind 失败不能发布半成品。请求捕获稳定 runtime，资源按独立版本发布。
- Storage/Telemetry 等进程服务不随每个配置 revision 重建，避免统计、健康和写入生命周期被 reload 切断。

## 部署与安全

Management 只监听 HTTP。浏览器的唯一外部 origin 来自 `webui.public_origin`；常规部署在可信反向代理终止 TLS。HTTP 直连只适用于 loopback 或可信隔离管理网，不能宣称具有传输加密。

前端开发产物和后端构建物保持独立；内嵌发布物运行不依赖外部 `frontend/dist` 或 Node.js。`/api/*` 不能回退到 SPA。构建/发布的实际步骤见[交付实现](../implementation/delivery.md)，工具与本地产物规则见[环境规则](../rules/environment-usage.md)。

## 领域入口

- [后端总览](backend/overview.md)：依赖方向、runtime 和数据面契约。
- [Management](management.md)：初始化、会话、安全和 API 边界。
- [前端](frontend.md)：应用、页面与查询状态。
- [当前实现](../implementation/README.md)：正式入口与证据，不用本设计推断实现完成。

DoT、DoQ、主动上游健康检查、通用配置编辑和 runtime command 不由本设计自动授权；新增能力需要独立契约与方案。
