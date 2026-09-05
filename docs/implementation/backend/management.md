# Management 实现

> 文档状态：有效
>
> 适用范围：正式 Management listener、认证、配置写入、只读查询与内嵌资源接线
>
> 最后核对：2026-09-05（构造、路由、配置事务和查询边界静态核对）
>
> 核对基线：`0f18d5b2ddf67625121fd7e0662e21723362565f`

## 入口与生命周期

[`app::run_command`](../../../backend/src/app.rs) 在 DNS candidate 绑定、coordinator 创建后调用 [`ManagementService::bind`](../../../backend/src/management/server.rs)。后者调用 feature-aware 资源检查并要求 origin，创建 AuthState、SessionStore、ConfigStore、只读 SQLite adapter 与 query service，最后绑定独立 HTTP listener。

`DnsService::attach_management` 持有管理状态并注册受监督 task。不是 DoH listener 的附加路由；`webui.enable: false` 不创建此链。`ManagementRuntime::reconcile_users` 识别内部写入指纹，外部 reload 撤销所有 session；`shutdown` 撤销会话。

## 路由与保护

[`router.rs`](../../../backend/src/management/router.rs) 的 `build_router` 组装公开 setup/login/logout、受保护 session 与 [`query.rs`](../../../backend/src/management/query.rs) 的七个查询端点；未知 API 与 SPA fallback 隔离。字段/状态码以 [OpenAPI](../../../frontend/openapi/management-api-v1.yaml) 为准，不在本文复制完整响应模型。

router 固定保护包括 JSON body 16 KiB、URI 4 KiB、64 个 header/16 KiB header bytes、256 个并发请求和 15 秒总请求 timeout。另有 setup/login 限流、Origin/Fetch Metadata、request ID 和统一错误处理。这些是实现常量，不是额外 YAML 字段。

[`auth.rs`](../../../backend/src/management/auth.rs) 的 `validate_setup_credentials` 与 `hash_password` 使用 12 至 1024 bytes 密码、Argon2id 19 MiB/2 iterations/parallelism 1；登录兼容 bcrypt。密码不 trim，用户名使用配置层共享规范。

[`session.rs`](../../../backend/src/management/session.rs) 使用 24 小时绝对期限、30 分钟空闲期限、全局 4096/单用户 16 的容量保护。`issue/lookup` 清理过期记录并控制容量，token 只通过 Cookie；HTTP/HTTPS 两种名称与 Secure 策略见 [Management 设计](../../architecture/management.md)。没有独立持久化 session 数据库。

## 首次用户写入

`post_setup` 完成凭据检查与 hash 后，通过 [`ConfigStore::create_initial_user`](../../../backend/src/config/store.rs) 写入，成功提交后才发布用户快照与 session。实际链为：

```text
try_lock -> ConfigFileLock -> reread source / fingerprint check
 -> source_edit::create_initial_webui_user
 -> ConfigLoader::load_candidate_bytes without snapshot
 -> commit_candidate: stages / journal / two target replacements
 -> publish expected + self-written fingerprint -> update auth / session
```

[`source_edit.rs`](../../../backend/src/config/source_edit.rs) 使用 source-preserving YAML 编辑，仅修改 users；不支持的语法明确失败。writer 输入上限为 4 MiB，普通 loader 上限为 8 MiB，因此“可加载”不等于“可首次初始化写回”。源/快照冲突和恢复约束见 [Config 设计](../../architecture/backend/modules/config.md)。

`recover_pending_transaction` 仅接在正式 run 的加载前；`validate` 不执行事务恢复。两文件替换有 journal，但不是整体原子 rename；跨平台权限、真实中断和各 crash point 仍需验收，不能仅凭函数存在宣称完整恢复矩阵通过。

## 查询数据流

| API 主题 | 实际数据来源 | 限制 |
| --- | --- | --- |
| overview | coordinator summary、resolution metrics、只读数据库计数 | 详情关闭或无数据时按响应原因区分不可用，不把缺数当零 |
| runtime | 当前 RuntimeSnapshot、listener 与摘要 | 不是直接操作 listener 的命令入口 |
| health | telemetry health snapshot | 缺少观测来源时不能推断健康 |
| statistics | `ManagementStorageRead` 的聚合查询 | 时间范围与维度校验 |
| queries | `ManagementStorageRead` 的详情查询 | 有界分页/过滤/排序，历史脱敏行返回 `legacy_redacted` |
| resources | runtime 资源 snapshot 元数据 | 只读，不触发刷新 |
| system | 版本、进程/构建与功能元数据 | 不提供配置秘密或绝对路径 |

[`ports/management.rs`](../../../backend/src/ports/management.rs) 定义领域读口；[`SqliteManagementReadModel`](../../../backend/src/storage/management_read.rs) 使用独立只读 pool、绑定参数和固定 SQL。query service 固定 5 秒 deadline、默认 20/最大 100 行分页和最长 31 天统计窗口。

查询详情可供所有已认证用户读取 qname、有效 client IP、配置标识、upstream provenance 与有界 answer；DNS wire、request digest、route 文本和 SecretRef 不进入 API。core duration 的历史缺失值不会补造。

## 静态资源与证据

[`assets.rs`](../../../backend/src/management/assets.rs) 在 `webui-embed` 下通过 `RustEmbed` 派生的 `WebAssets` 读取 `frontend/dist`，排除 map，处理 MIME、ETag、HEAD、条件请求和 fallback；打包脚本另行检查 dist/index.html，仓库没有自定义 `backend/build.rs`。

启用 embed feature 时，`ensure_available` 检查内嵌 index。**未启用该 feature 时检查返回 Ok，Management API 仍可启动，但没有 SPA 资源**：接受 HTML 的前端 fallback 返回 503，普通资源缺失返回 404。不能写成默认 binary 必须有 SPA 才能启动管理端。打包行为与历史 Windows 证据见[交付实现](../delivery.md)。

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| setup/auth/session | router、AuthState、SessionStore | ManagementService -> DnsService | 本轮静态；源码含 auth/session/router 测试 | 真实浏览器 Cookie/Storage 未在本轮验证 |
| users 事务 | source_edit、ConfigStore、journal recovery | setup 写入，run 启动恢复，watcher 对账 | 本轮核对；存在双路径恢复与 Busy 竞争测试 | 完整跨平台 crash/权限矩阵待验收 |
| 七个只读 API | ManagementQueryService + StorageRead port | app 注入真实 coordinator/DB/telemetry | 本轮核对 handler 不持有 SQLx | 未执行全端点真实 HTTP 与浏览器 smoke |
| 内嵌 SPA | assets + build feature | bind 前 ensure_available | 静态；历史证据单独标注于交付文档 | Actions/Linux/macOS 发布未由静态代码证明 |

本轮未运行后端测试、服务或浏览器；剩余工作统一由[v2 验收计划](../../plans/webui-v2-management-integration.md)管理。
