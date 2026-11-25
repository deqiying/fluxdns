# FluxDNS 配置字段参考

> 文档状态：有效
>
> 适用范围：本文与当前模板同步，描述配置契约、校验和已实现运行时边界；未定义行为不应视为已支持。
>
> 最后核对：2026-09-21（DNS 缓存 `expired` 状态与过期保留期定向核对，基于本次工作树；其余正文沿用原核对范围）
>
> 核对基线：`d7296fd`；本轮核对 P5 变更与联合验收，分批历史结果按原日期和基线解释
>
> 2026-09-21 增量核对（本机日期）：仅核对 DNS 缓存的 group ECS、Resolved fingerprint、stale refresh、late-result 语义与条目过期状态 `expired`；其余正文保留原历史核对范围
>
> 增量核对基线：`e50b948edb4699f58a1e57039fb56442ab228687` 加本次缓存修复工作树；静态编译与前期定向测试证据见[DNS 管线](backend/dns-pipeline.md#缓存修复验证)，未部署或实测 OpenWrt
>
> 2026-10-04 增量核对：基线 `eb428f92cdc3acda836547107ef2f56f6d988249` 加本次工作树，仅更新快照读取等待和策略表单覆盖保真；其他配置契约不变
>
> 2026-10-04 负缓存增量核对：基线 `e811b713292467ce964534c1cb7e2ef44002cdf6` 加本次工作树，仅核对 `dns.cache.negative_ttl_max`、`optimistic.negative_max_age`、按响应质量区分的乐观窗口与缓存活动响应类观测；单元/策略层测试已覆盖，本地运行验收仍由[负缓存新鲜度计划](../plans/negative-cache-freshness.md)跟踪
>
> 2026-11-05 增量核对：基线 `0b93d3816b9ffe95a89966aefa88d77372ba60df` 加本次工作树，仅核对管理候选对顶格列表后续配置项的兼容编辑；验证范围见下文活动源与候选内部底座
>
> 2026-11-06 增量核对：基线 `3d44dd4` 加本次工作树，仅核对配置单次保存、无状态预览与文件确认边界；当时验证记录在 Management 实现的“单次保存与写接口简化（2026-11-06）”一节，该节已于 2026-11-26 随实现删除。
>
> 2026-11-07 增量核对：基线 `aa0fedf` 加本次工作树，仅核对 DoH 显式裸路径、必填 `{client_id}`、固定段优先及旧配置迁移；验证范围见 [DNS 管线](backend/dns-pipeline.md#能力与证据)。
>
> 2026-11-08 增量核对：基线 `bfece26` 加本次工作树，仅核对 DoH 裸路径兼容隐式 ID 后缀、必填显式模板及确定性优先级；本次测试证据见 [DNS 管线](backend/dns-pipeline.md#能力与证据)，其余历史范围未重新核对。
>
> 2026-11-08 写回风格增量核对：基线 `96daf34` 加本次工作树，仅核对管理候选的块式片段渲染、空集合展开与原文保留；验证与未覆盖范围见下文活动源与候选内部底座。
>
> 2026-11-08 短响应缓存增量核对：基线 `96daf34` 加本次工作树，仅更新 REFUSED/空 Answer 的固定 10s 缓存、客户端 TTL、禁止 stale 与 entry v3 恢复兼容；验证见 [DNS 管线](backend/dns-pipeline.md#短响应缓存验证)。
>
> 2026-11-08 duration 写回增量核对：基线 `5e8b4e1` 加本次工作树，仅核对 duration 字段的写回与 API 回显规范化；验证与未覆盖范围见下文活动源与候选内部底座。
>
> 2026-11-09 缓存 key/ECS 增量核对：基线 `08a774724e2304f72dd7b8d0a9d0299983f9b0e5` 加工作树，仅静态核对 v3 AnswerKey、成员 ECS、scope、刷新和兼容性；本轮未执行产品测试或构建，运行证据待主代理补充，其余历史结果不变。
>
> 2026-11-10 ECS 自定义兜底增量核对：基线 `f73514297f7df3925ff0f912840210e4acdd1046` 加工作树，仅扩展 `client_or_custom`、本级回退值校验与五层 WebUI 配置；验证边界见 [DNS 管线](backend/dns-pipeline.md#请求级-ecs-执行事实)。
>
> 2026-11-26 配置读写增量核对：基线 `be19359` 加工作树，重写配置读写、条目级写回、磁盘同步、派生副本与启动级配置各节；其余正文沿用原核对范围。验证见下文[配置读写与磁盘同步](#配置读写与磁盘同步2026-11-26)。上方 2026-11-05 与两条 2026-11-08 写回记录所指的“活动源与候选内部底座”一节已随 CST 修补一并删除。
>
> 依据：[config-example.yaml](../../config-example.yaml)
>
> 关联文档：[后端架构](../architecture/backend/overview.md)

## 加载与支持边界

### v2 契约与生产基线（2026-09-08）

[`config/contract.rs`](../../backend/src/config/contract.rs) 提供 `ConfigV2::parse/validate/resolve_paths`，[`ConfigV2Loader`](../../backend/src/config/load.rs) 已由 `app::run_command` 正式使用并直接生成 `ResolvedConfig`，不经过 v1 DTO 或 migration。根[配置模板](../../config-example.yaml)和[启动夹具](../../backend/tests/fixtures/config-v2.yaml)均为 v2；生产不提供 v1/v2 切换开关，不接受旧配置，也不转换旧配置或数据库。

| v2 字段/边界 | 契约与默认值 |
| --- | --- |
| `version` | 只接受 `2`；其他版本在完整解析前拒绝，无转换 |
| `name` | 各命名空间内唯一，1-128 个 ASCII 字符 `[A-Za-z0-9_.!-]`；上游和组共享空间；不同空间可同名，DoH endpoint 名仅在所属 listener 内唯一 |
| `clients[].client_id` | 必填、全体客户端唯一、大小写敏感，1-128 个 URL unreserved ASCII `[A-Za-z0-9._~-]`；普通编辑可显式修改（缺省沿用原值），name 不派生 ID；修改后历史记录仍归属旧 ID |
| `clients[].match.ips` | 缺省空数组；最多 256 项；地址转 /32 或 /128，CIDR 清除主机位，mapped IPv6 转 IPv4；规范化重复拒绝，包含关系允许 |
| `dns.cache` | 缺失时全局池关闭，内存 64 MiB，failure TTL 5s，optimistic 关闭/answer TTL 10s/max age 1d；显式对象沿用必填字段规则 |
| `dns.cache.persistence` | 缺失或空对象为 `enabled: false`、`path: ./data/dns-cache.db`、`snapshot_interval: 5m`；周期 1s-1d；删除旧 `max_size_bytes` |
| `dns.resolve_log` | 缺失为关闭；显式对象只接受必填 `enable`，删除旧条数/年龄配额 |
| `statistics.retention` | 缺失字段分别采用 `days: 7`、`grace_days: 3`、`reference_size_bytes: 1073741824`；days 为 1-3650，grace 非负且总和不超过 3650 |
| 字节预算 | 内存预算、参考大小为 1-1099511627776 bytes（1 TiB），可由 JavaScript 安全整数无损表达；参考大小不是磁盘硬配额 |
| `database.records_path` | 必填；与统计库、快照一起沿两级路径基准解析，不增加在线搬库 |
| 解析/集合预算 | 配置最多 4 MiB；每命名空间最多 1024 项；每份内联 Hosts/规则文本最多 256 KiB（UTF-8 字节） |
| 覆盖/类型分支 | cache/TTL/ECS 缺失仍表示继承，显式禁用不等于缺失；显式 null、未知字段、旧 `match.ids` 和 variant 残留字段均拒绝 |

共享 listener、上游、策略、Hosts、规则集、代理、SecretRef 和认证字段继续复用 [model](../../backend/src/config/model.rs)；共享资源引用、循环、socket 冲突和语义复用 [validate](../../backend/src/config/validate.rs)，没有另造旧版本转换层。名称和 ID 的请求期索引/历史事实属于 BC-04/05，P0 仅验证配置契约。

`resolve_paths` 是无 I/O 的词法检查：拒绝统计/快照相互碰撞、与详情目录及受保护配置/日志路径重叠，包括逻辑文件成为另一目标父路径的反向冲突，Windows 比较不区分大小写。物理路径由对应 owner 再检查；统计库要求 `fluxdns_layout(kind=statistics-v2, layout_version=1)`，空文件可在首次事务初始化，任何已有未标记 schema 或错误标记均拒绝。详情分片继续由每个文件的 `detail_meta` 标记归属。

### P1 客户端匹配索引内部能力（2026-09-08）

[`ResolvedClient`](../../backend/src/config/resolve.rs) 与 [`ClientRule`](../../backend/src/policy/client.rs) 已把配置管理 `name` 和请求身份 `client_ids` 明确分离，`ClientIndex` 同时构建 name/exact ID 索引并拒绝重复 name、重复 ID、重复规范化 CIDR 和空 matcher。请求匹配仍固定为大小写敏感的 ID 优先、最长 CIDR 回退；IPv4-mapped IPv6 地址在 CIDR 匹配及客户端 cache digest 前归一化为 IPv4，因此与对应 IPv4 使用同一客户端池。

v2 配置边界只允许单个唯一 `clients[].client_id`，生产 resolver 将其直接编译为唯一请求身份；`ResolvedClient` 的复数容器是运行时内部形态，不提供 `match.ids` 配置入口。历史记录仍只保留请求发生时已经冻结的匹配事实，不补造或重匹配旧身份。

### 配置旧路径退出（2026-09-09）

旧 `ConfigLoader`、v1 顶层/客户端/缓存/详情 DTO、迁移注册表和独立 v1 fixture 已删除。`run`、`validate`、显式内部 reload、首次用户候选和配置回归测试统一使用 `ConfigV2Loader`；通用资源 DTO、严格字段 parser 和安全快照继续复用。无物理来源的测试输入要求绝对 `work.path`，实际加载仍以源文件目录解析两级路径。内容比较使用的非加密 FNV-1a 移至 [`hash.rs`](../../backend/src/config/hash.rs)，认证和受管文件身份继续使用 SHA-256。

Windows Rust 1.98.0 执行 `cargo test --manifest-path backend/Cargo.toml -- --nocapture`：838 项通过、3 项显式忽略。覆盖 v2 拒绝旧配置、源路径/快照、真实双文件 journal、启动/reload 和认证候选；该次结果不代表 P5 的完整性能或浏览器验收。

### 配置读写与磁盘同步（2026-11-26）

配置读写以 [`ConfigStore`](../../backend/src/config/store.rs) 为唯一 owner。启动时 `app::run_command` 用 loader 实际读取的正文构造 [`ConfigDocument`](../../backend/src/config/document.rs)（正文、SHA-256 `config_digest`、`ConfigV2`、`runtime_revision = 1`、`restart_required`），以 `ArcSwap` 持有；读接口经 `current()` 无锁投影，不读磁盘。设计不变量见 [Config 设计 §12–§14](../architecture/backend/modules/config.md#12-配置文档与写锁)；HTTP 接线见 [Management 实现](backend/management.md#配置保存与磁盘操作2026-11-26)。

保存调用链：`ConfigStore::save` 取写锁（竞争即 `Busy`）→ 读源文件摘要，与当前文档不同且未设 `overwrite_disk` 时返回 `FileChanged` → [`change::apply_changes`](../../backend/src/config/change.rs) 在类型化配置上替换/追加条目、改写引用、恢复未修改的脱敏内联密码 → [`writer::render`](../../backend/src/config/writer.rs) 条目级写回 → `resolve_config_v2`（统计保留期缩短需 `confirm_retention_shortening`）→ SecretRef 检查、启动级字段沿用当前 Runtime 值后 prepare，经 `ServiceControl` 换代 → 发布新文档 → 再读磁盘摘要，仍等于保存前正文或已选择覆盖时 [`disk::write_atomic`](../../backend/src/config/disk.rs) 写源文件，然后写 `<work.path>/config.yaml` 镜像。

- 变更 wire 形态不变：单次 1–128 项、请求体 2 MiB；同一目标只能出现一次；`update` 的旧名称必须存在（`CONFIG_ENTRY_NOT_FOUND`）；`ClientEdit.client_id` 缺省沿用原值；改名按既有引用规则一次性改写 listener/DoH route 策略、bootstrap、组成员与 fallback、规则 hosts/upstream/rule_set selector、proxy 和客户端策略，不做链式改名。
- 源序列化：`SecretRefDto` 的内联 `url` 在源文件渲染与变更比较中使用 [`to_source_value`](../../backend/src/config/model.rs) 不脱敏输出，API 投影仍输出脱敏占位符。只修改内联代理密码时新密码会写入文件，且文件中不出现占位符。
- 条目级写回：[`layout.rs`](../../backend/src/config/layout.rs) 按行识别顶层段、列表模块（`listener`、`upstreams`、`strategy`、`hosts`、`outbound`、`rule_set`、`clients`）的列表项与单例段（`dns`、`statistics`、`logs`、`webui` 等）的子键；缩进与顶格列表、块标量、多行引号、CRLF、无末尾换行和文档标记均支持。只替换值发生变化的条目，新增项追加到段尾，段缺失或为空时整段写成块式；定位无法自证（如 flow 段、单行多键）时整段替换，该段内注释丢失。被编辑条目内部的注释和非规范写法按块式规范形式重写；条目外注释、空行和其他条目逐字节保留。需要保留的说明应写在条目上方。
- 语义复核：写回结果必须重新 `ConfigV2::parse`、解析路径并与目标配置的源序列化值完全相等，否则以 `CONFIG_SOURCE_LAYOUT_UNSUPPORTED` 拒绝；含 YAML 锚点、别名或 merge key 的被编辑条目同样拒绝。
- duration 写回与 API 回显统一经 `format_duration` 输出能整除的最大单位（`d/h/m/s/ms/us/ns`，零值 `0s`），可被 `parse_duration` 无损读回；读取仍接受任意既有写法，未修改的条目沿用原文。
- 失败语义：写锁忙 `OPERATION_BUSY`；磁盘已修改 `CONFIG_FILE_CHANGED`；变更/布局/字段非法 `VALIDATION_FAILED`；保留期缩短未确认 `CONFIRMATION_REQUIRED`；资源加载、绑定或换代失败 `APPLY_FAILED`，以上 Runtime、内存文档与磁盘均不变。换代后磁盘复核发现外改或写盘 I/O 失败时返回 200 `persisted: false`（分别带 `CONFIG_FILE_CHANGED_DURING_SAVE` 或 `CONFIG_FILE_WRITE_FAILED` 诊断），状态为 `modified`，配置已生效但重启后丢失。换代回执丢失时，coordinator 当前代次等于期望的新代次即视为已应用。

磁盘观测与写入：

- [`ConfigFileWatcher`](../../backend/src/app/config_watcher.rs) 每秒有界读取源文件（4 MiB、UTF-8），连续两次观测相同才上报 `DiskObservation::{Readable{digest}, Missing, Unreadable}`；派生镜像不参与观测，symlink 与 hard link 按普通文件读取。每次观测带代次，store 丢弃写入开始后才完成的旧代次结果。`disk.status` 在读取时由最近观测与当前摘要计算为 `in_sync`/`modified`/`missing`/`unreadable`；只比较内容摘要，编辑器原子保存产生的新文件身份不算外改。
- `write_atomic` 在目标同目录使用固定临时文件 `.<文件名>.fluxdns-tmp`（残留则截断复用），Unix 沿用目标 mode、Windows 复制 owner/group/DACL，`sync_all` 后原子替换（Unix `rename` 并同步父目录，Windows `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`）；只读目标拒绝写入。源文件是 symlink 时写入真实路径并保留链接；hard link 在替换后断开。不写 journal，崩溃时目标为旧正文或新正文。
- 镜像：源文件写成功后再写镜像；镜像写失败只记录 warning，并以 `CONFIG_MIRROR_WRITE_FAILED` 诊断附在响应中，仍算已持久化。运行期写入不创建工作目录，工作目录由启动快照创建。
- 外部变更：外改只更新观测，不会自动生效。`reload` 读取源文件（缺失或不可读返回 `CONFIG_FILE_UNREADABLE`），摘要相同直接返回，严格校验失败返回 `VALIDATION_FAILED`，prepare/换代失败返回 `APPLY_FAILED`，失败时 Runtime 与内存文档不变；成功后 `webui.users` 有变化时更新认证并撤销会话。reload 不写镜像。`persist` 把当前正文原子写入源文件与镜像，不触碰 Runtime，适用于 `modified`（放弃外改）、`missing`、`unreadable` 与 `persisted: false` 后的补写；源文件写失败返回 `PERSISTENCE_FAILED`。
- 首用户 setup 在写锁内要求仍无用户且磁盘与内存一致，经 writer 只替换 `webui.users`，复核候选只新增该用户后写源文件与镜像；不递增 runtime 代次，写盘失败不发布认证。
- 启动时若源文件旁存在旧版本遗留的 `.fluxdns-journal`、stage 或 lock 旁文件，只记录 `configuration_legacy_sidecars_found` warning 并保留文件，既不恢复也不删除。若旧版本恰在决定提交后、源文件替换前崩溃，升级后会加载旧配置，需按告警人工核对。

启动级配置：以下字段由进程启动时创建的组件消费，Runtime 换代不会替换它们。以 [`process_owned_reload_changes`](../../backend/src/service.rs) 为唯一定义：

| 组件 | 字段 | 修改后的行为 |
| --- | --- | --- |
| `database` | `database.type`、`database.path`、`database.records_path` | 保存和 reload 照常成功并写入文件，统计库与详情目录继续使用启动值 |
| `webui` | `webui.enable`、`webui.address`、`webui.port`、`webui.public_origin` | 同上；管理 listener 与浏览器 origin 继续使用启动值 |
| `dns.resolve_log` | `dns.resolve_log.enable` | 同上；解析详情记录行为保持启动值 |

save 与 reload 构造 Runtime 候选前把这些字段替换为当前 Runtime 的值，因此不会因需重启而失败；内存文档与文件采用新值，差异记入 `restart_required`，只经 `GET /api/v2/config/system` 暴露并在系统运行状态页提示。改回原值后列表清空，重启进程后新值生效、列表从空开始。`webui.users` 不属于启动级字段，reload 时动态生效。例如在 DNS 设置页切换“记录解析详情”后，页面显示新值，但实际要重启才生效。

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| 条目级写回 | `layout.rs`、`writer.rs`、`change.rs`、`model.rs::to_source_value` | `ConfigStore::save`、首用户 setup | 2026-11-26 Windows Rust 1.98.0：layout/writer/change 单元测试；运行验收复现规则重排、类型切换、增删与 ECS 修改，逐字节确认只有被编辑条目变化 | 被编辑条目内注释丢失；flow 段整段替换 |
| 内联代理密码写入 | `to_source_value`、`expand_redacted_changes` | save | 单元回归；运行验收修改内联密码后文件为新密码、无占位符 | — |
| 同步保存与写盘 | `ConfigStore::save`、`disk::write_atomic` | `POST /api/v2/config/save` | store/disk/config_api 定向测试；运行验收只读文件返回 `persisted: false` 后 persist 补写 | 无跨进程锁；复核与写盘之间的外改仍可能被覆盖 |
| 磁盘观测与 reload/persist | `ConfigFileWatcher`、`disk::observe`、`ConfigStore::{reload,persist}` | app watcher、`/config/reload`、`/config/persist` | watcher/store 测试；运行验收外改 2.5 秒内显示 `modified`，无效 YAML reload 被拒且运行配置不变 | Unix 权限/目录同步只有代码与 cfg(unix) 测试，未在本机执行 |
| 启动级配置 | `process_owned_reload_changes`、`apply_runtime` | save/reload、`/config/system` | store 测试；运行验收切换 `dns.resolve_log` 后记录行为不变、提示出现，改回清空，重启后生效 | 只在系统运行状态页提示 |

2026-11-26 验证（Windows、Rust 1.98.0、Node 26.8.1，基线 `be19359` 加工作树）：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 通过，`cargo test --manifest-path backend/Cargo.toml` 944 项通过、5 项忽略；前端 typecheck、Vitest 46 文件 326 项、契约测试 10 项与生产构建通过。运行验收使用内嵌 WebUI 的 release binary 与 `_fluxdns/` 下基于 [webui-local.yaml](../../backend/tests/fixtures/webui-local.yaml) 的独立副本，覆盖上述六类场景、浏览器内冲突对话框“重新加载后继续编辑”与提示条重新加载，以及更新后的 [HTTP 验收脚本](../../script/test-webui-http.mjs) 与 WS 脚本。另以 debug binary 在 WebUI 关闭的副本旁放置 `.config.yaml.fluxdns-journal`，启动日志出现 `configuration_legacy_sidecars_found` 且旁文件保留。未执行 Unix、磁盘满、断电或线上容器验收。

### 当前生产加载器

正式入口是 [`app::run_command`](../../backend/src/app.rs) -> [`ConfigV2Loader::load_from_path`](../../backend/src/config/load.rs) -> [contract](../../backend/src/config/contract.rs) / [resolve](../../backend/src/config/resolve.rs) / [validate](../../backend/src/config/validate.rs)。loader 限制配置为 4 MiB 并保留产生运行态的原始正文，`run` 加载时创建或以源覆盖 `<work.path>/config.yaml`；随后校验 SecretRef，并在建立 ConfigStore 与 prepare 前对旧版本 journal 旁文件告警（与 WebUI 是否启用无关）。`validate` 不写 snapshot，也不做资源、数据库或网络可用性检查。

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| 严格配置与路径 | ConfigV2Loader、ConfigV2、ResolvedConfig | run/validate/reload/首用户候选共用 v2 加载 | loader、根模板、路径与旧版本拒绝测试；2026-09-09 全量 Cargo | 无旧版本转换 |
| 资源与缓存 | async PreparedRuntime、PolicyDnsCore | run 加载后 prepare | 本轮核对生产接线 | 能解析不表示网络/文件/SQLite 已成功打开 |
| WebUI | webui model + ManagementService | enable 时创建服务 | P3 内嵌 debug binary 的真实 Bearer HTTP 与浏览器 | origin、DB 与 bind 必须可用；默认 binary 可仅提供 API，SPA 需要 embed feature，详见[管理端](backend/management.md) |
| 首用户写回 | `ConfigStore::create_initial_user` + `writer::set_webui_users` | setup | store 定向测试；运行验收 setup 后文件写入用户并可登录 | 写锁竞争或磁盘不一致时拒绝 |
| 热重载 | `ConfigStore::{save,reload}` + `ServiceControl` + `reload_prepared` | 十模块保存、reload；watcher 只更新磁盘观测 | 见[配置读写与磁盘同步](#配置读写与磁盘同步2026-11-26) | 启动级字段只写入文件并提示需重启；跨平台/磁盘满未验证 |

本表原始静态核对未执行 Cargo 或配置 validate，后续 P1 定向验证以上述分批记录为准。协议/策略字段定义不自动意味着所有 adapter 组合已验收，真实入口和未支持项见[后端实现](backend/README.md)，验证专项的已知边界见[验证范围与收口](backend/background-services.md#验证范围与收口)。既定契约与代码冲突时须保留复现证据并明确修复边界，不仅修改字段说明来掩盖实现缺口。

## 1. 配置模型概览

FluxDNS 的配置由命名资源和引用关系组成。请求入口选择策略，策略按顺序匹配规则，再决定使用本地 hosts 或远程上游：

```text
listener
  ├─ udp/tcp → strategy
  └─ doh → routes → strategy
                         ├─ hosts：本地回答
                         └─ rule_set → upstream / group → outbound
```

- `listener` 定义请求入口。一个普通 listener 可以展开为多个 IPv4/IPv6 socket。
- 一个 `type: doh` listener 是一个逻辑 DoH 服务，可通过多个 `endpoints` 同时提供不同绑定方式。
- `routes` 在同一个 DoH listener 内共享；每条路由选择一个策略。
- `clients` 按客户端标识或 IP 网段选择策略，并覆盖缓存、TTL 和 ECS 等设置。
- `upstreams`、`hosts`、`rule_set` 和 `outbound` 是可复用的命名资源。
- `webui` 定义独立的 WebUI Management Server、浏览器 origin 和登录用户。

## 2. 通用约定

### 2.1 版本

顶层 `version` 是配置 schema 版本。当前模板和生产加载器只接受 `version: 2`；其他版本直接拒绝并提示使用新的开发目录，不迁移或按旧字段猜测。

### 2.2 路径和工作目录

配置路径采用两级基准解析，不能直接把所有相对路径都拼到进程当前工作目录：

1. 先确定启动配置文件的绝对路径。命令行传入的配置文件路径若为相对路径，仅在这一步相对于进程启动时的当前工作目录解析；其父目录记为 `config_dir`。
2. 解析 `work.path`：绝对值直接使用；相对值按 `config_dir.join(work.path)` 解析。结果经词法归一化后形成绝对的 `resolved_work_path`。
3. 解析项目路径字段：绝对值直接使用；相对值统一按 `resolved_work_path.join(field_path)` 解析并做词法归一化，不能再回到 `config_dir` 或进程当前工作目录。
4. `work.rules_path`、`database.path`、`database.records_path`、`logs.path`、`dns.cache.persistence.path`、TLS 证书和私钥、SecretRef 文件、本地 hosts 与规则文件路径都遵循第 3 步。

词法归一化应消除 `.` 和可消解的 `..`，但不依赖目标文件或目录已经存在。通过 bytes/string 加载且没有配置文件来源路径时，不存在 `config_dir`；此时相对 `work.path` 必须报错，调用方需要提供来源路径或改用绝对 `work.path`，不得隐式回退到进程当前工作目录。

| 启动配置文件 | 配置值 | 解析结果 |
| --- | --- | --- |
| `/opt/_fluxdns/config.yaml` | `work.path: ./` | `resolved_work_path = /opt/_fluxdns` |
| `/opt/_fluxdns/config.yaml` | `work.path: ./`、`database.path: ./data/fluxdns.sqlite3` | `database.path = /opt/_fluxdns/data/fluxdns.sqlite3` |
| `/etc/fluxdns/bootstrap/config.yaml` | `work.path: ../runtime`、`logs.path: ./logs/fluxdns.log` | `logs.path = /etc/fluxdns/runtime/logs/fluxdns.log` |
| `/etc/fluxdns/config.yaml` | `work.path: /var/lib/fluxdns`、`database.path: /srv/fluxdns.sqlite3` | 两个绝对路径均不与其他基准拼接 |

启动时若配置文件本身不是 `<resolved_work_path>/config.yaml`，程序将其原子复制到该固定路径；仅仅位于同一目录但文件名不同仍会创建快照。该文件是源文件的单向镜像；启动流程负责创建工作目录和所需父目录。

镜像按 [Config 设计](../architecture/backend/modules/config.md#8-工作目录与配置快照)维护：目标不存在时在同目录写临时文件并以 no-replace 方式发布，内容相同则不操作，内容不同时直接用源文件覆盖。运行期保存和写入磁盘在源文件写成功后再覆盖镜像，镜像写失败只告警；磁盘状态与冲突判断只看源文件，不读取镜像。SecretRef 解析值不会写回镜像。

### 2.3 命名与引用

`listener`、`upstreams`、`strategy`、`hosts`、`outbound` 和 `rule_set` 中的 `name` 是资源标识，同一集合内必须唯一。引用字段必须指向对应集合中存在且类型正确的名称。

| 引用字段 | 目标集合或语法 |
| --- | --- |
| `listener[].strategy`、`clients[].strategy`、`listener[type=doh].routes[].strategy` | `strategy[].name` |
| `listener[].hosts`、`strategy[].rules[].hosts` | `hosts[].name` |
| `strategy[].rules[].rule_set` | `rule_set[].name`，或 `geosite:cn` 这类规则集子选择器 |
| `strategy[].rules[].upstream`、`strategy[].default_upstream` | `upstreams[].name` |
| `upstreams[].bootstrap` | `upstreams[].name` |
| `upstreams[type=group].upstreams[].name`、`fallbacks[].name` | `upstreams[].name` |
| `upstreams[].proxy`、`rule_set[].proxy` | `outbound[].name` |

应在加载阶段检查名称唯一性、引用存在性、引用类型和循环引用。

### 2.4 值类型

| 数据类型 | 模板示例 | 说明 |
| --- | --- | --- |
| 时长 | `10s`、`24h`、`1d` | 用于缓存、超时和定时更新；解析为 duration，而不是整数天数或秒数。读取接受 `ns/us/ms/s/m/h/d/w` 及复合写法；写回配置和 API 回显统一规范化为能整除的最大单位（`3d`、`36h`、`1500ms`，零值 `0s`），不输出纳秒数。 |
| 字节数 | `8388608` | `max_size_bytes` 使用整数，单位为字节。 |
| IP/CIDR | `192.168.1.0/24`、`fe80::/10` | 用于监听绑定、客户端匹配、ECS 和可信代理。 |
| URL | `https://dns.google/dns-query` | 必须按字段要求使用合法 URL。 |
| SecretRef | `{env: FLUXDNS_OUTBOUND_SG_URL}` | 从环境变量、文件或内联 `url` 取得完整敏感值；内联形式会把真实凭据写入配置文件，需自行承担。 |

`outbound[].proxy_url` 接受三种 SecretRef 形式，三选一：对象 `{env: NAME}`、对象 `{file: ./secrets/proxy.txt}`，或直接写完整 URL 字符串 / 对象 `{url: "socks5://user:password@host:1080"}`。内联形式读取结果即该 URL；引用形式读取结果为环境变量或文件内容。三种形式读取后都必须是完整代理 URL。

### 2.5 覆盖和继承

配置块未出现时继承较低优先级的设置；块中只出现的字段按字段继承。显式的禁用值用于停止继承。

#### ECS 优先级

当多个层级提供 `edns_client_subnet` 时，优先级由高到低为：

```text
strategy.rules[].edns_client_subnet
  > strategy[].edns_client_subnet
  > clients[].edns_client_subnet
  > upstreams[].edns_client_subnet
  > dns.edns_client_subnet
```

`mode` 的取值为 `disabled`、`client`、`custom`、`client_or_custom`：

- `disabled` 显式停止当前请求继续向上游传递 ECS；
- `client` 优先使用合法请求 ECS，清除 host bits，source 取 `min(原值, 24)`（IPv4）或 `min(原值, 56)`（IPv6），较短前缀保留；无合法请求 ECS 时按客户端地址推导 `/24` 或 `/56`；所有查询 scope 置 0；
- `custom` 使用必填的 `custom_ip` CIDR，清除 host bits、scope 置 0，不按 client 隐私上限截断；
- `client_or_custom` 优先使用合法请求 ECS，复用 `client` 的规范化规则；缺失或非法时使用本级必填 `custom_ip`，复用 `custom` 的处理规则，不按客户端 IP 推导。合法 `/0` 保留而不触发回退；整个 DNS 报文解析失败仍由协议入口拒绝，不进入此回退分支。

`custom` 和 `client_or_custom` 的 `custom_ip` 必须在当前配置块内提供，不跨层级拼接；`client` 和 `disabled` 禁止携带该字段。缺失整个 ECS 块表示继承，所有层级均缺失时默认 `disabled`。旧配置行为不变；旧版本不识别 `client_or_custom`，回退版本前应先改回旧模式。

当策略目标是 group 时，rule/strategy/client 的显式 ECS 覆盖所有成员；来自 global/default 时遍历全部可达 direct leaves（nested group、fallback、隐式继承成员），逐成员计算最终 ECS。最终 ECS 一致时使用 uniform scope 缓存；真正异构时保留 per-member query，以按成员 ID 排序的 `(member, 最终 ECS 或 None)` 摘要精确缓存，正常参与 lookup、single-flight 和 commit，不再绕过。Fast 路径已删除，每次请求均先完成规则与 target 决策再查缓存。

#### 缓存和 TTL

缓存和 TTL 覆写是两个独立配置：

```text
clients[].cache > strategy[].cache > dns.cache
clients[].ttl_override > strategy[].ttl_override > dns.ttl_override
```

`ttl_override.enabled: false` 显式关闭当前层级的 TTL 覆写；它不等价于关闭缓存。`ttl_override.min` 或 `max` 为 `0s` 表示该边界不设限。

缓存不是逐层查询的 fallback 链。每个请求只选择一个逻辑缓存池：客户端显式启用时选择“实际客户端身份 + 生效策略”池；否则策略显式启用时选择策略池；策略未配置 `cache` 时才选择全局池。策略或客户端显式 `enabled: false` 会终止选择，不再回退到全局池。详见 [`dns.cache`](#81-dnscache)。

### 2.6 配置版本与归一化

生产加载不直接把 YAML DTO 当作运行时配置，也不执行旧版本迁移：

```text
ConfigV2
  → 拒绝非 version: 2
  → normalize 路径、SecretRef、默认值和继承
  → semantic validate
  → ResolvedConfig
```

- v1 顶层 DTO、迁移注册表和测试加载路径已删除；测试复用正式 v2 配置。
- 缺失、显式 `null`、空数组和空对象的区别由严格 v2 parser 保留；cache/TTL/ECS 继承和来源信息在 `ResolvedConfig` 阶段一次性确定。
- `validate` 不写入原始配置或快照；不提供旧配置迁移命令。
- 配置 schema 版本、SQLite schema 版本、资源 parser/compiler 版本和 cache key format 版本彼此独立；升级一个版本不能隐式宣称其他版本兼容。
- 运行时升级沿用 `prepare candidate → preflight → atomic activate/keep old`：可热更新项替换 `RuntimeSnapshot`，需要重新绑定的项 drain 后切换，无法安全切换的项拒绝候选并保留旧运行时。

## 3. 顶层字段

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `version` | integer | 配置 schema 版本；当前必须为 `1`。 |
| `work` | object | 工作目录和规则资源落盘目录。 |
| `database` | object | 默认开启的聚合统计和其他主库存储能力；始终必填。P2 详情生产写入已分离到日分片。 |
| `logs` | object | 服务日志输出。 |
| `webui` | object | WebUI Management Server、浏览器 origin 和登录用户。 |
| `dns` | object | 全局缓存、TTL、ECS 和解析日志默认值。 |
| `listener` | array | UDP、TCP 和 DoH 请求入口。 |
| `upstreams` | array | hosts 上游、DoH 上游和上游组。 |
| `strategy` | array | 策略和有序路由规则。 |
| `hosts` | array | 可被入口或策略复用的本地 hosts 资源。 |
| `outbound` | array | 远程上游或规则下载使用的代理出口。 |
| `rule_set` | array | 可被策略引用的规则资源。 |
| `clients` | array | 客户端匹配和客户端级覆盖。 |

## 4. `work`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `work.path` | string | 必填 | 工作目录；绝对路径直接使用，相对路径以启动配置文件所在目录为基准。 |
| `work.rules_path` | string | 必填 | 规则资源落盘目录；相对路径以 `work.path` 为基准。 |
| `work.rule_set_max_size_bytes` | integer | 可选 | 每个规则资源读取、下载和解析的大小上限，单位为字节；必须大于 0，缺失时默认为 `16777216`（16 MiB）。 |
| `work.rule_set_max_rules` | integer | 可选 | 每个规则资源实际建索引的规则数量上限；dat 只统计被 strategy 引用的 selector。必须大于 0，缺失时默认为 `131072`。 |

启动时，应先得到绝对的 `resolved_work_path` 并确保目录存在；如果配置文件不在该目录中，再将其复制为 `<resolved_work_path>/config.yaml`。数据库、日志、缓存和证书等其他相对路径均以 `resolved_work_path` 为基准。

2026-11-08 局部核对（`d208820` 加工作树）：远程规则的首次下载、缓存恢复和自动刷新通过 [`remote_resource_options`](../../backend/src/runtime/prepared.rs) 传递完整 `RuleLimits`，不再只传大小而让规则数退回默认值。DAT 的规则数量自 2026-11-08（`de39a28` 加工作树）起只累计被 strategy 引用的分类，见本节末尾 `rule_set` 说明。该配置变更不会被文件 watcher 自动应用；当前管理面未提供 `work` 编辑入口，手动修改这两个上限后需重启进程读取实际启动源配置。解析失败会通过[安全错误诊断](backend/management.md#安全错误诊断与操作关联) 返回实际使用的上限，便于确认配置是否生效。

## 5. `database`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `database.type` | string | 必填 | 当前模板仅定义 `sqlite`。未知类型应拒绝。聚合统计默认开启，因此不能省略。 |
| `database.path` | string | 必填 | 带 v2 布局标记的统计 SQLite 文件路径；相对路径以 `work.path` 为基准。 |
| `database.records_path` | string | 必填 | UTC 日分片目录；必须与统计库、缓存快照及受保护文件隔离。 |

数据库在 prepare 阶段必须完成 v2 布局、当前 schema 和基本写入检查；失败时拒绝启动。空统计文件在同一事务直接创建当前 schema 9 和布局标记，重启校验并重开；未标记库、错误标记、其他 schema 或旧单库路径均拒绝，不升级、搬迁或清空。运行中数据库暂时不可写时，DNS 继续服务，由统计 writer 保留进程内补偿计数并重试；未恢复前进程退出可能造成 persistence gap，该状态必须可观测。

## 6. `logs`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `logs.enable` | boolean | 必填 | 是否启用服务日志。 |
| `logs.level` | string | 必填 | 日志级别；v1 接受 `trace`、`debug`、`info`、`warn`、`error`，大小写归一化，未知值拒绝。 |
| `logs.path` | string | 必填 | 日志文件路径；相对路径以 `work.path` 为基准。 |

正式 app 关闭日志时仍保持 telemetry 指标/health。enable/level/path 经已有 service 日志 owner 显式热切换；路径预开失败不切 Runtime 或旧输出。v2 表单保存经 ConfigStore 同步应用后写回文件，见[配置读写与磁盘同步](#配置读写与磁盘同步2026-11-26)；热切换边界见[日志热切换](backend/background-services.md#p1-日志热切换2026-09-07)。

日志保持单行 JSON，行首 `time` 使用服务器本地时区的可读日期时间、毫秒和 UTC 偏移；旧 `occurred_at_ms` 日志字段已替换，不影响数据库/API 时间字段。时区规则与完整字段顺序见 [Observability 日志格式](../architecture/backend/modules/observability.md#3-日志格式)，实现与验证边界见[后台服务](backend/background-services.md#observability)。

## 7. `webui`

`webui` 控制独立于 DoH 的 Management Server。FluxDNS 在该端口只提供 HTTP，不实现 TLS 终止，也不根据 `X-Forwarded-*` 推断浏览器 origin；公网部署应将监听限制在 loopback 或受保护的内网地址，并由 Nginx 等可信反向代理提供外部 HTTPS。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `webui.enable` | boolean | 必填 | `true` 启动 Management Server；`false` 不创建 management listener 或认证状态。 |
| `webui.address` | string | 必填 | HTTP Management Server 的监听地址；与 DNS TCP/DoH endpoint 统一检查端口冲突。 |
| `webui.port` | integer | 必填 | HTTP Management Server 的监听端口，范围为 `1..=65535`。 |
| `webui.public_origin` | absolute URL | `enable: true` 时必填 | 浏览器实际访问的唯一 `http` 或 `https` origin；不得包含凭据、path、query 或 fragment，用于严格 Origin 校验和 Cookie 策略。 |
| `webui.users` | array | 可选 | 省略或 `[]` 表示需要一次性初始化；非空表示已可登录；显式 `null` 拒绝。 |
| `webui.users[].name` | string | 必填 | 用户名；列表内必须唯一。 |
| `webui.users[].password_hash` | string | 必填 | 初始化时由一次性明文密码生成的单向 hash。 |

配置仍拒绝未知字段、重复用户名、非法 hash 和明文 `webui.users[].password`。首次初始化只把 Argon2id PHC hash 写入源配置和工作目录快照；真实密码和 hash 不应提交到公开仓库。

`public_origin` 使用 HTTP 时，session Cookie 不设置 `Secure`，因此凭据与 session 的传输安全完全依赖管理网络；这只适合 loopback 或明确受信任的隔离网络。常规部署应由反向代理提供 HTTPS，并把 `public_origin` 配置成浏览器看到的 `https://...` origin，此时后端仍只监听 HTTP。

## 8. `dns`

### 8.1 `dns.cache`

`cache` 描述全局池开关、共享内存容量、短期失败 TTL、乐观缓存和独立二进制快照；TTL 覆写不再嵌套在其中。

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `dns.cache.enabled` | boolean | 是否启用全局缓存池；它不是策略池和客户端池的总开关。 |
| `dns.cache.memory.max_size_bytes` | integer | 所有逻辑缓存池共享的内存计费容量上限，单位为字节。 |
| `dns.cache.failure_ttl` | duration | 非空 Answer 的 `SERVFAIL`、上游截断响应等失败类缓存时间；必须在 `1s..=5m`。REFUSED/空 Answer 固定 `10s`，不受此值影响。 |
| `dns.cache.negative_ttl_max` | duration | 非空 Answer 的 `NXDOMAIN` 缓存寿命及客户端可见 TTL 上限；可省略，默认 `5m`，必须在 `1s..=1d`。NODATA/空 NXDOMAIN 改用固定 `10s`，不受此上限影响。 |
| `dns.cache.optimistic.enabled` | boolean | 是否允许返回已过期记录并在后台刷新。 |
| `dns.cache.optimistic.answer_ttl` | duration | 乐观缓存应答使用的 TTL。 |
| `dns.cache.optimistic.max_age` | duration | 完整正应答过期后仍可乐观返回的最长时间。 |
| `dns.cache.optimistic.negative_max_age` | duration | 非空 Answer 的 NXDOMAIN 过期后仍可乐观返回的最长时间；可省略，默认 `5m`，`0s` 关闭。解析时取 `min(negative_max_age, max_age)`；REFUSED/空 Answer 到期即回源，不使用该窗口。 |
| `dns.cache.persistence.enabled` | boolean | 是否启用进程级 `FDCS` 快照；不等同于全局内存缓存开关。 |
| `dns.cache.persistence.path` | string | 当前进程级 `FDCS` 快照文件路径；相对路径以 `work.path` 为基准。 |
| `dns.cache.persistence.snapshot_interval` | duration | 完整快照周期，范围 `1s..=1d`。 |

`memory.max_size_bytes` 是所有 namespace/subnet 条目共享的全局 weight 预算，计入 key、DNS wire 和元数据，不承诺等于进程 RSS；没有每 base key 条目上限。`FDCS` 文件没有用户可配的磁盘配额，也不承诺文件大小等于 Moka weight；reader/writer 使用内部 1 GiB 文件上限和记录预算。旧 `persistence.max_size_bytes` 在 v2 配置中是未知字段。

`persistence.enabled` 开启时，正式 app 在 bind 前由唯一进程 owner 从 `FDCS` 分批预热活动 Moka，随后按 `snapshot_interval` 覆盖完整可见集合；缺失、损坏、不兼容、超时和内存预算不足只形成冷启/部分恢复状态。内存 CAS 不等待磁盘，也不再排入逐条 SQLite persistence。reload 只切换 owner generation/source，不恢复候选；shutdown 在 finalizer 排空后 best-effort 写最终快照。详见[Cache 设计](../architecture/backend/modules/cache.md)与[后台服务](backend/background-services.md#cache-persistence)。

策略级和客户端级 `cache` 只允许 `enabled` 与 `optimistic` 子对象，不包含 `memory`、`failure_ttl` 或 `persistence`。只要出现策略级或客户端级 `cache` 对象，`enabled` 就必须显式提供；整个对象缺失才表示继续向较低优先级选择。

#### 逻辑缓存池选择

当前有三类逻辑缓存池，并共享 `dns.cache.memory` 容量预算与快照 owner：

1. 全局池：namespace 为 `global`；
2. 策略池：namespace 包含生效的 `strategy.name`；
3. 客户端 + 策略池：namespace 包含实际客户端身份和生效的 `strategy.name`。客户端身份优先使用请求的精确 `client_id`，没有时使用规范化源 IP；不能只使用匹配规则的 `clients[].name`。

每个请求按以下顺序选择且只使用一个池，cache miss 时不跨池继续查找：

```text
clients[].cache.enabled == false  → 不使用缓存
clients[].cache.enabled == true   → 客户端 + 策略池
clients[].cache 整块缺失          → 继续检查策略

strategy[].cache.enabled == false → 不使用缓存
strategy[].cache.enabled == true  → 策略池
strategy[].cache 整块缺失          → dns.cache.enabled 为 true 时使用全局池，否则不缓存
```

cache key format v3 的 `AnswerKey` 由 pool namespace、规范化 qname/qtype/qclass、RD/CD/DO、排序后的保留 EDNS options、transport compatibility、target 子树 fingerprint 与最终成员 ECS 组成。EDNS 存在本身、DNS ID、COOKIE、PADDING、TCP keepalive、UDP payload size 不入 key；ECS 单独编码，未知 option 保守保留。没有 Fast/Resolved mode、policy/request fingerprint、listener/route/client/规则/hosts hash 或整体 revision 维度。

target fingerprint 仅覆盖所选 target 子树：group 模式、成员/权重/fallback/超时与嵌套结构，以及 direct 定义、完整 DoH URL（含 path/query）、connect_ip、bootstrap 链、所用 outbound 与上游 hosts 内容；URL 只输入摘要，不进 key 明文/日志。同一 namespace 内，不同入口、客户端和策略只要最终 target/查询相同就共享答案；要求隔离时显式选择 client/strategy 池。规则/hosts 更新先改变决策，不再让不相关答案失效；listener/strategy hosts 本地回答仍绕过 response cache，upstream hosts 按普通上游响应处理。

uniform ECS 按应答 scope 存储并最长前缀探测：一致应答 `C ≤ S` 存 covering `/C`；`C > S` 时，source 已达 `/24`、`/56` 上限则 covering `/S`，未达上限则 exact `/S`（仅相同 source/网段）。无应答 ECS 或 FAMILY/SOURCE/ADDRESS 不一致时忽略应答 ECS，按查询 source 存 covering `/S`；不一致应答仍返回与缓存，这是对 RFC 7871 MUST drop 的有意偏离，不扩大共享范围。查询 source 0 始终存 exact `/0`，区别于应答 scope 0 的 covering `/0`。查询无 ECS 使用独立桶，不服务带 ECS 请求；异构组合只精确匹配。

covering lookup 从长到短探测不超过查询 source 的已有前缀，最后探测 exact；single-flight 按查询 ECS 精确值合并，存储 key 等应答 scope 确定。Moka 前缀计数索引随插入和删除/淘汰维护，快照恢复经插入重建，不单独持久化。optimistic refresh 在最新 core 完整重新决策，记录回源前 store 版本水位；新 scope 写入成功后锁内确认新版本仍有效，再清除其覆盖且不晚于水位的旧条目，不删除刷新期间更新的条目，新条目失效则不清理。late-result 只复核重新决策的 AnswerKey，不比较 policy；失败不延长旧 stale。详细契约见 [Cache](../architecture/backend/modules/cache.md)。

cache key 由 v2 升为 v3，旧 v2 key 的快照记录恢复为 `Incompatible`，升级需冷启动一次；外层 `FDCS` 和 entry v3 不因此改版。entry codec 保留普通非空 v2 payload 按同布局转换的能力，但不能绕过 key 版本检查；旧 REFUSED/空 Answer payload 仍不兼容。恢复按绝对到期时间，不补满 TTL。配置 schema v2 与 cache key/entry 版本独立。

#### 响应缓存语义

- `hosts[]` 产生的本地回答不进入 response cache；它直接读取已编译内存 snapshot，并仍应用 TTL override 和统计。`upstreams[type=hosts]` 不属于此例外。
- 通过协议校验的 `REFUSED`（无论 Answer 是否为空），以及所有 Answer 为空的上游 DNS 响应，统一缓存 **10s**。覆盖 NODATA、空 NXDOMAIN、空 SERVFAIL、空 TC 与其他合法 RCODE 的空响应；即使没有 SOA、没有 TTL 或原 TTL 为零，也使用该期限。仍遵循当前逻辑池的缓存开关和原有安全 key/质量约束，不强行启用已禁用的缓存。
- 短响应首答的所有 RR TTL 与 SOA.MINIMUM 改写为 10s，fresh 命中时按条目剩余寿命递减（不足 1s 为 0）。没有记录时不伪造记录，仅设置内部期限。该规则优先于 `failure_ttl`、`negative_ttl_max` 和 TTL override，且不允许 stale；到期后的首查必须回源。缓存中的 origin response、RCODE 和 checksum 保持不变。
- 普通非空 `NOERROR`（包括仅 CNAME 链）仍按 RR TTL 缓存，保留现有 TTL override 与乐观窗口。非空 NXDOMAIN 沿用 SOA/最小 RR TTL/failure TTL 与 `negative_ttl_max`、负乐观窗口；非空 SERVFAIL/上游 TC 沿用 `failure_ttl`。TC 的 transport 隔离不变，不能把 UDP 截断结果复用于 TCP/DoH。
- malformed DNS、问题段不匹配、连接/TLS/HTTP 失败和超时不属于 DNS 响应，不写缓存。
- 缓存保存不含客户端 DNS ID 和传输 envelope 的 canonical response。若本地 UDP 输出因本次客户端 advertised size 而截断，应保存完整 canonical response，并在每次发送时重新编码；只有上游本身返回的 `TC=1` 才保存截断条目。
- 写入仍按响应质量做 compare-and-replace：完整 NOERROR 优于 NODATA/NXDOMAIN，后者优于 SERVFAIL/TC/REFUSED/Other；低质量条目不能覆盖仍 fresh 的完整答案。同质量条目不因晚到竞态反复覆盖。新返回的正答案可以替换短期拒绝/空结果。
- 条目已过期且不可乐观返回时，本次解析按 `expired` 记录并照常回源；该状态与「缓存从未存在该条目」分开保存，便于区分回源原因。
- 同一 key 的并发 miss 通过 single-flight 合并。leader 得到可缓存结果后先形成持有 lease 的 `CacheCommitCandidate` 并返回共享响应；后台 worker 使用独立 100ms deadline 完成 admission/CAS 并唤醒 waiter，进程快照由独立周期 owner 覆盖。客户端响应不等待 commit；candidate 被队列丢弃、取消或直接 drop 时，RAII lease 必须发布失败终态，不能永久挂住 follower。optimistic stale 由一次性 refresh permit 去重并进入统一后台刷新流程，不复用 miss 的 lease。

### 8.2 `dns.ttl_override`

`ttl_override` 与 `cache` 平级，用于限制返回给客户端的 DNS TTL：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `dns.ttl_override.enabled` | boolean | 是否启用当前层级的 TTL 覆写。 |
| `dns.ttl_override.min` | duration | 最小 TTL；`0s` 表示不设下限。 |
| `dns.ttl_override.max` | duration | 最大 TTL；`0s` 表示不设上限。 |

策略和客户端可使用同样的字段结构。省略字段按层级继承，显式 `enabled: false` 停止当前层级的 TTL 覆写。

例外：来自上游或缓存的 REFUSED/空 Answer 使用固定短 TTL 及剩余寿命，不应用 TTL override，避免再次延长客户端的拒绝/空结果缓存。本地 Hosts 路径保持原规则。

### 8.3 `dns.edns_client_subnet`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `mode` | enum | 必填 | `disabled`、`client`、`custom` 或 `client_or_custom`。 |
| `custom_ip` | CIDR | `custom`、`client_or_custom` 时本级必填，其余模式禁止 | 固定发送或作为合法请求 ECS 缺失时回退的 ECS 网段。 |

全局 ECS 设置可由上游、客户端、策略和规则覆盖，详见[覆盖和继承](#25-覆盖和继承)。

### 8.4 `dns.resolve_log`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `enable` | boolean | 是否记录每次解析请求的详情（请求、策略、规则、ECS、缓存、上游结果和耗时等）。关闭时不写详情表，但不关闭聚合统计。 |

v2 只接受 `enable`；旧条数/年龄配额是未知字段。请求任务只向统一 resolution ingress 附带 typed question 和共享 response，qname digest、canonical qname 与 answer JSON 在后台 detail projector 中生成，再进入唯一有界详情 channel；满批立即提交，低流量尾批最多等待 5 秒。worker 按事件 UTC 日写入 `YYYY-MM-DD.sqlite3`，批写只执行校验与 `INSERT`，不执行历史 `COUNT`、按条数/年龄 `DELETE` 或 `VACUUM`。projection/SQLite 队列满或分片提交失败时丢弃当前详情并计量，DNS 请求不得等待或失败。

详情中的 `duration_ms` 从 transport 接入计时点计到 DNS core 完成，`dns_core_duration_ms` 只计算 `DnsCore::resolve_with_completion` 主链；两者均在主链返回时冻结，不包含后台观测排队和 SQLite 写入。DoH 的 `duration_ms` 包含入站 TLS/HTTP 处理，但不包含响应编码和网络写回。schema v5 之前的历史记录无法回填主链耗时，Management API 返回 `null`。

业务 schema v6 将数据库创建/迁移、批次提交和解析发生时间统一为带 `_utc_millis` 单位的 `INTEGER` Unix UTC 毫秒列；配置没有新增单位开关，API 时间格式与两种耗时精度不变。字段、前向迁移及失败边界见[业务时间存储](backend/background-services.md#业务时间存储)。

聚合统计默认开启且始终依赖 `database`：至少按 UTC 自然日记录总请求数，并按有界的 client bucket、transport class、strategy、source/upstream、RCODE 和 cache lookup status 记录分项计数。未匹配客户端统一进入 `unknown` bucket；不能使用域名、完整客户端 ID 或原始 IP 作为无界维度。请求任务只做一次有界 `ResolutionEnvelope::try_publish`，后台 dispatcher 更新进程内 sharded counters，stats writer 周期性以带 `batch_id` 的 checkpoint 批量 upsert 到 SQLite，并用 batch ledger 幂等去重。detail 下游溢出不影响聚合统计；但统一 ingress 溢出会同时丢失该事件的 stats/detail/cache commit，因此必须累计 `dropped` 并冻结首次 `gap_started_at_utc_millis`。一次请求只计一次 `total_requests`；cache/hosts 命中归入本地 source，parallel 的多个上游尝试不重复计请求。统计不能因为详情记录被丢弃或 `enable: false` 而停止。数据库运行中暂时不可写时继续维护进程内计数并报告 `degraded`/persistence gap；启动阶段数据库不可用则拒绝启动。

“依赖数据库”表示统计依赖 `database.path`，详情依赖独立受管日分片；请求线程不同步等待任一 SQLite。Management overview 的 `resolution_pipeline` 暴露 ingress accepted/dropped/首次 gap 时间、cache commit 各终态以及 detail accepted/dropped/failed。详情仍是有界内存队列后的 best-effort 写入，不因取消条数上限变成无损审计；若需要无损语义仍须另行定义 spool、背压和磁盘配额。

## 9. `listener[]`

一个 listener 是一个逻辑服务。`addresses` 中的每个地址由运行时展开为独立 socket；同一个逻辑 listener 的地址共享协议和端口。

### 9.1 UDP/TCP listener

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 入口唯一名称。 |
| `type` | enum | 必填 | `udp` 或 `tcp`。 |
| `addresses` | array[string] | 必填 | IPv4/IPv6 绑定地址列表，例如 `[0.0.0.0, "::"]`。 |
| `port` | integer | 必填 | 所有地址共用的监听端口。 |
| `strategy` | string | 必填 | 默认策略，引用 `strategy[].name`。 |
| `hosts` | string | TCP 模板示例中可选 | 入口级本地 hosts 资源；命中时本地回答，未命中再使用 `strategy`。 |

通过把 IPv4 和 IPv6 地址放入同一个 `addresses` 数组，可以用一个逻辑 listener 表达双栈入口。实现必须明确 IPv6 socket 的 v6-only 行为，避免 `0.0.0.0` 与 `::` 展开后的绑定重叠。

### 9.2 DoH listener

DoH 不使用普通 listener 的顶层 `address`、`port`、`strategy` 或单个 `path` 字段，而是由共享路由和 endpoint 组成：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string | 逻辑 DoH listener 名称，必须唯一。 |
| `type` | enum | 固定为 `doh`。 |
| `routes` | array | 所有 endpoint 共享的 HTTP 路由列表。 |
| `endpoints` | array | 一个或多个绑定端点；每个 endpoint 独立处理 TLS 和客户端 IP 来源。 |

#### `listener[type=doh].routes[]`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `path` | string | HTTP 路径；无占位符时接受精确裸路径或追加一个非空 ID 段；可使用一次完整路径段形式的 `{client_id}` 占位符，显式占位符必须提供非空值。 |
| `strategy` | string | 路由命中的策略，引用 `strategy[].name`。 |

不带占位符的裸路由兼容两种请求，无需分别添加入口：`/doh-query` 精确匹配 `/doh-query` 时 `client_id = None`，也接受 `/doh-query/client-a` 并提取 `client_id = client-a`。隐式后缀只能是一个非空路径段，不接受空 ID 或额外多段。精确裸路径按 endpoint 提取的有效来源 IP/CIDR 匹配；带 ID 请求仍获取来源 IP，客户端精确 ID 优先、未命中时回退最长 CIDR 的规则不变，路径 ID 不是认证凭据。

含显式 `{client_id}` 的模板只接受完整模板对应的路径，例如 `/doh-query/{client_id}` 必须提供 `/doh-query/client-a`。占位符无论位于末尾还是中间均不可省略；空 ID、模板之外的额外路径段不匹配该模板，也不在模板末尾再附加隐式 ID。

尾随斜杠与根路径保留原文语义，不自动规范化配置：

| 单独配置的裸路由 | 精确请求（client_id 为 None） | 追加 ID 请求（client_id 为 client-a） |
| --- | --- | --- |
| `/dns` | `/dns` | `/dns/client-a` |
| `/dns/` | `/dns/` | `/dns/client-a`，沿用已有斜杠，不插入双斜杠 |
| `/` | `/` | `/client-a` |

`/dns` 与 `/dns/` 的精确路径不同，但隐式 ID 匹配集合重叠且优先级相同，因此配置校验拒绝二者在同一 listener 的 routes 中共存。

允许存在能够确定优先级的匹配重叠，优先级不依赖路由配置顺序：

1. 将裸路由的隐式 ID 后缀纳入从左到右的逐段比较，固定段优先于参数段。
2. 同样段结构下，显式 `{client_id}` 模板优先于裸路由的隐式 ID 后缀。
3. 重复路由和同优先级的重叠歧义仍在配置校验阶段拒绝。

例如固定路径 `/doh-query/inner` 对请求 `/doh-query/inner` 优先于 `/doh-query` 的隐式后缀和 `/doh-query/{client_id}`，不会把 `inner` 识别成 ID；该裸 inner 路由同时可以接受 `/doh-query/inner/client-a`。`/doh-query/inner/{client_id}` 也优先于 `/doh-query/{client_id}/query`，二者同时命中 `/doh-query/inner/query` 时取前者。

同时配置 `/doh-query`、`/doh-query/{client_id}`、`/doh-query/inner` 三条路由时：

| 请求路径 | 命中的 canonical route ID | 请求 client_id |
| --- | --- | --- |
| `/doh-query` | `/doh-query` | None，按 IP 匹配 |
| `/doh-query/client-a` | `/doh-query/{client_id}` | `client-a`，同形显式模板优先 |
| `/doh-query/inner` | `/doh-query/inner` | None，固定段优先 |
| `/doh-query/inner/client-a` | `/doh-query/inner` | `client-a`，裸路由隐式后缀 |

如果只配置 `/doh-query`，`/doh-query/client-a` 的 canonical route ID 仍为 `/doh-query`。如果再添加 `/doh-query/inner/{client_id}`，它会优先承接 `/doh-query/inner/client-a`，canonical route ID 为该显式模板原文。

**既有配置边界：** 显式模板仍不隐式包含省略 ID 的短路径。只配置 `/dns/inner/{client_id}` 时，`/dns/inner` 返回 404；如果需要相同策略下兼容不带 ID 和带 ID 的请求，可以配置裸路由 `/dns/inner`。但如果还存在其他裸/通用路由，短路径可能被其他路由按 ID 捕获，例如 `/dns` 或 `/dns/{client_id}` 可把 `/dns/inner` 中的 `inner` 作为 ID，不能把所有省略模板 ID 的请求一概理解为 404。来源 IP 的可信代理及 Header 缺失/非法处理仍按 endpoint 的 `client_ip` 配置执行，与路由是否含 ID 无关。

Transport 只对实际 HTTP path 匹配一次，并始终把配置 `path` 原文作为稳定 canonical route ID 传给 Policy；不会为隐式后缀改写成 `{client_id}` 模板，route ID 不包含实际请求中的 client ID，也不自动改写既有配置。`/dns-quer/inner*` 与 `/dns-quer/outside*` 是服务保留路径，不能作为自定义路由；它们与示例中的 `/dns/...` 前缀不是同一组路径。

每条路由固定同时支持 DoH GET 和 POST，不增加方法开关：

- GET 使用唯一的 `dns` query 参数，值为不带 `=` padding 的 base64url DNS wire；
- POST body 是原始 DNS wire，`Content-Type` 必须为 `application/dns-message`；
- 解码后的 GET 消息与 POST body 最大均为 65,535 字节；对应的无 padding GET `dns` 参数最多 87,380 个字符，HTTP request-target 限制不得小于路由路径加该参数所需长度；超限分别由 HTTP 层返回适当的 `413`/`414`，不进入 DNS 核心；
- 其他方法返回 `405` 并包含 `Allow: GET, POST`；不支持的媒体类型返回 `415`，非法 wire 返回 `400`；
- 通过 DNS 协议校验后的 `NXDOMAIN`、`REFUSED`、`SERVFAIL` 等响应仍使用 HTTP `2xx`，失败状态由 DNS RCODE 表达；
- 响应使用 `application/dns-message`。由于答案可能随客户端身份、策略和 ECS 变化，v1 固定发送 `Cache-Control: no-store`，只使用 FluxDNS 内部缓存。

#### `listener[type=doh].endpoints[]`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | endpoint 名称，在同一 DoH listener 内唯一。 |
| `addresses` | array[string] | 必填 | endpoint 的 IPv4/IPv6 绑定地址列表。 |
| `port` | integer | 必填 | endpoint 监听端口。 |
| `tls` | object | 必填 | 该 endpoint 的 TLS 所有权。 |
| `client_ip` | object | 必填 | 请求客户端 IP 的来源和可信边界。 |

同一个逻辑 DoH listener 可以同时包含以下两类 endpoint，而不需要创建两个顶层 DoH 配置：

- `tls.mode: terminate`：FluxDNS 自己完成 TLS 握手和解密，必须提供证书链和私钥文件；
- `tls.mode: external`：Nginx 等反向代理已经终止 TLS，FluxDNS 接收内部 HTTP，证书字段禁止出现。

#### `endpoints[].tls`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `mode` | enum | 必填 | `terminate` 或 `external`。 |
| `certificate_file` | string | `terminate` 时必填 | 证书链文件路径，相对路径以 `work.path` 为基准。 |
| `private_key_file` | string | `terminate` 时必填 | 私钥文件路径，相对路径以 `work.path` 为基准。 |

`external` 模式不得配置 `certificate_file` 或 `private_key_file`。反向代理占用公网端口时，external endpoint 通常绑定本机或内网端口，例如模板中的 `127.0.0.1:8053`。

#### `endpoints[].client_ip`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `source` | enum | 必填 | `peer`、`forwarded_header` 或 `proxy_protocol`。 |
| `header` | string | `forwarded_header` 时必填 | 允许 `X-Forwarded-For`、`X-Real-IP` 或 `Forwarded`。 |
| `trusted_proxies` | array[CIDR] | `forwarded_header`/`proxy_protocol` 时必填 | 可信的反代对端网段，不是最终客户端网段。 |
| `on_missing` | enum | `forwarded_header` 时可选 | Header 缺失时 `reject` 或 `use_peer`。 |
| `on_invalid` | enum | `forwarded_header` 时可选 | Header 非法时 `reject` 或 `use_peer`。 |

`peer` 直接使用 TCP 对端地址。`forwarded_header` 只接受来自 `trusted_proxies` 的请求；转发链应从右到左解析，并且反向代理必须先清理客户端自带的伪造 Header。

`proxy_protocol` 的自动识别只用于区分 PROXY v1 文本头和 v2 二进制头，不表示 header 可选。运行时先按 TCP peer 地址检查 `trusted_proxies`，再在 TLS handshake/HTTP 解析前读取前导头；不可信 peer、缺失头、非法长度、未知协议版本或不能提供 TCP4/TCP6 源地址的头均拒绝连接，不回退为 `peer`。v1 必须在前 107 字节内遇到 CRLF；v2 总前导长度上限固定为 536 字节。v2 中长度合法但未知的可选 TLV 可忽略，以兼容扩展；同一个 endpoint 不同时接收裸连接和 PROXY 连接，需要时应配置两个 endpoint。

### 9.3 绑定校验

`listener[].addresses`、`listener[type=doh].endpoints[].addresses` 与启用后的 `webui.address`/`port` 展开后不得产生相同协议、地址、端口的冲突；WebUI 只占用 TCP，不与同地址、同端口的 UDP listener 冲突。IPv4/IPv6 双栈是否共享 socket、以及 v6-only 行为必须在运行时明确。

当前模板没有 `dot` 或 `doq` listener 字段；DoT/DoQ 应在协议、TLS/QUIC 材料和握手校验契约确定后再加入。

## 10. `upstreams[]`

`upstreams` 中每个资源由 `type` 决定允许的字段集合。未知字段和不属于该类型的字段都应拒绝。

### 10.1 公共字段

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string | 上游唯一名称。 |
| `type` | enum | `hosts`、`doh` 或 `group`。 |

### 10.2 `type: hosts`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `format` | string | 必填 | 内容格式；按该格式严格解析。模板示例为 `json`。 |
| `hosts` | string | 必填 | 内联 hosts 数据。 |

该类型可作为其他 DoH 上游的 `bootstrap`，用于避免首次解析依赖系统默认 DNS。内联内容格式错误应报告字段路径。

### 10.3 `type: doh`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `address` | URL | 必填 | DoH 服务 URL。 |
| `bootstrap` | string | 可选 | 引导解析 `address` 主机名的上游名称。 |
| `connect_ip` | IP | 可选 | 实际连接目标 IP；不改变 HTTP `Host` 或 TLS SNI。 |
| `proxy` | string | 可选 | 代理出口名称，引用 `outbound[].name`。 |
| `edns_client_subnet` | object | 可选 | 上游级 ECS 覆盖。 |

`bootstrap` 和 `connect_ip` 解决的是连接建立路径：前者引用上游完成域名解析，后者直接指定连接 IP。二者互斥，均不改变远程服务的 HTTP `Host` 或 TLS SNI。

正式 bootstrap resolver 按 connector 缓存地址，TTL 下限为 0、上限为 3,600 秒；零 TTL 不复用，重新 prepare 不继承旧缓存。没有新增配置字段；并发、过期和失败边界见 [Upstream 设计](../architecture/backend/modules/upstream.md)。

未配置代理时，优先使用 `connect_ip`，其次使用 `bootstrap`，两者都没有时使用系统解析器。配置 `proxy` 时，先解析对应 SecretRef 的 URL scheme：

- `socks5://`：主机名在 FluxDNS 本地解析；顺序仍为 `connect_ip` → `bootstrap` → 系统解析器，再把 IP 交给代理；
- `socks5h://`：`connect_ip` 缺失时把原始主机名交给代理解析，因此禁止同时配置 `bootstrap`；若显式提供 `connect_ip`，代理接收该 IP，HTTP `Host`/TLS SNI 仍使用 URL 主机名。

### 10.4 `type: group`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `upstreams` | array[object] | 必填 | 主成员列表；每项的 `name` 必填，`weight` 可选。 |
| `upstream_mode` | enum | 必填 | `parallel`、`round-robin`、`load-balance` 或 `failover`。 |
| `timeout` | duration | 必填 | 主组总超时时间。 |
| `fallbacks` | array[object] | 可选 | 主组在 timeout 内没有终态 DNS 响应时使用的回退成员，字段约束与主成员相同。 |
| `fallback_upstream_mode` | enum | 有 `fallbacks` 时必填 | 回退成员选择模式。 |
| `fallback_timeout` | duration | 有 `fallbacks` 时必填 | 回退组总超时时间。 |
| `upstreams[].name` / `fallbacks[].name` | string | 必填 | 成员上游名称。 |
| `upstreams[].weight` / `fallbacks[].weight` | positive integer | 可选，默认 `1` | 正整数权重；省略后在 DTO 边界归一化为 `1`。 |

组成员使用对象而不是 `name:weight` 字符串，避免字符串解析歧义。精确算法见 [Upstream 模块方案](../architecture/backend/modules/upstream.md)：`round-robin` 使用 smooth weighted round-robin，`load-balance` 使用按 weight 归一化的 least-in-flight，`failover` 严格按配置顺序且 weight 只能省略或显式设为 `1`。三种模式都只在 transport failure 时尝试其他成员，任意终态 DNS 响应都会结束当前组；主组完全没有终态响应时才进入 fallback。

`parallel` 的 v1 语义固定如下：

1. 同时向主组成员发起请求。第一个通过 DNS wire 校验、响应标志校验且问题段与请求匹配的 DNS 响应立即返回客户端；`NXDOMAIN`、`REFUSED`、`SERVFAIL` 和 `TC=1` 都属于终态成功。超时、连接/TLS/HTTP 失败、非法 wire 和问题段不匹配不属于成功。
2. 若首个终态响应是完整的 `NOERROR/TC=0`，按缓存准入规则写入并取消其余请求；若首个响应是 `NXDOMAIN`、`REFUSED`、`SERVFAIL` 或 `TC=1`，已经发出的其他主组请求继续到各自完成或组 `timeout`，但不改变已经返回给当前客户端的响应，也不触发 fallback。
3. 对上一条继续运行的后台窗口，收集到全部成员完成或 `timeout` 后再定稿缓存：先从完整的 `NOERROR/TC=0` 响应中按成员配置顺序选择；若没有，再从允许缓存的其他终态响应中按成员配置顺序选择。`REFUSED` 与空 Answer 按上述短响应规则缓存 10s。这样迟到有效答案仍能写缓存，但缓存结果不由并发完成顺序决定。
4. 主组只有在 `timeout` 内完全没有终态 DNS 响应时才进入 `fallbacks`；fallback 组使用同样的响应和缓存语义。
5. `parallel` 不使用权重；该模式下成员应省略 `weight` 或显式设为 `1`，非 `1` 值会被拒绝，避免配置暗示不存在的优先级。

## 11. `strategy[]`

策略定义规则顺序、默认上游以及策略级覆盖。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 策略唯一名称。 |
| `rules` | array | 必填 | 有序规则列表；从上到下匹配，命中第一条后停止。 |
| `default_upstream` | string | 必填 | 没有规则命中时使用的上游名称。 |
| `cache` | object | 可选 | 策略级缓存覆盖，高于 `dns.cache`。 |
| `ttl_override` | object | 可选 | 策略级 TTL 覆写，与 `cache` 平级。 |
| `edns_client_subnet` | object | 可选 | 策略级 ECS 覆盖。 |

### 11.1 `strategy[].rules[]`

每条规则必须且只能提供一个匹配目标：`rule_set` 与 `hosts` exactly-one-of。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `rule_set` | string | 与 `hosts` 二选一 | 规则集名称或子选择器，例如 `geosite:cn`。 |
| `hosts` | string | 与 `rule_set` 二选一 | 本地 hosts 资源名称。 |
| `upstream` | string | `rule_set` 规则通常必填 | 命中规则集后使用的上游；引用 `upstreams[].name`。 |
| `edns_client_subnet` | object | 可选 | 规则级 ECS 覆盖，优先级最高。 |

校验约束：

- `rule_set` 和 `hosts` 不能同时出现，也不能同时缺失；
- `rule_set` 规则应提供 `upstream`；
- `hosts` 规则是本地回答，不应依赖 `upstream`；
- 所有资源引用必须存在且类型正确。

WebUI 有序规则卡片支持本级 ECS 的继承、禁用、客户端地址、自定义 IP/CIDR 和请求 ECS 优先/自定义兜底；继承提交时省略对象，只有 `custom` 与 `client_or_custom` 提交并要求 `custom_ip`。Hosts 规则可保留 ECS 配置，但本地回答不向上游发送 ECS，提交时清除 `upstream`。规则 DTO 不支持逐规则 cache/TTL；这两项继续在策略或其他既有层级配置。非法规则 CIDR 的管理接口错误包含对应 `rules[index].edns_client_subnet.custom_ip` 字段路径，不返回提交值或底层解析错误。

### 11.2 策略级 `cache`

策略级缓存只允许以下字段，不包含全局 `memory`、`failure_ttl` 或 `persistence`：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `enabled` | boolean | `true` 选择策略逻辑池；`false` 对该策略请求完全禁用缓存。 |
| `optimistic.enabled` | boolean | 是否允许过期记录先返回并异步刷新。 |
| `optimistic.answer_ttl` | duration | 乐观缓存应答 TTL。 |
| `optimistic.max_age` | duration | 过期记录可被乐观返回的最长时间。 |
| `optimistic.negative_max_age` | duration | 可选；NODATA/NXDOMAIN 的乐观窗口，语义与 [`dns.cache.optimistic.negative_max_age`](#81-dnscache) 相同。 |

整个 `strategy[].cache` 缺失时才允许回退到全局池；对象存在时 `enabled` 必填。策略级 `ttl_override` 使用 [`dns.ttl_override`](#82-dnsttl_override) 的相同字段结构和继承语义。

策略表单保存前先校验已挂载字段，再从完整 Form store 取值，因此禁用 TTL 的隐藏上下限不会因只改名称或上游而丢失。未修改覆盖块保留原 DTO；`ttl_override.enabled` 缺失、显式 false、`0s` 以及 cache.optimistic 的继承/显式配置保持区别。编辑与验证证据见[前端页面实现](frontend/pages.md)。

## 12. `hosts[]`

顶层 `hosts` 是可被 listener 或策略规则直接命中的本地回答资源；它不同于 `upstreams[type=hosts]`，后者是一个解析上游资源。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | hosts 资源唯一名称。 |
| `type` | enum | 必填 | `const` 或 `file`。 |
| `format` | enum | 必填 | `json` 或 `hosts`；内容必须按格式严格解析。 |
| `hosts` | string | `type: const` 时必填 | 内联 hosts 内容。 |
| `path` | string | `type: file` 时必填 | 本地 hosts 文件路径，相对路径以 `work.path` 为基准。 |
| `auto_update` | boolean | `type: file` 时可选 | 是否自动重载本地文件。 |
| `update_interval` | duration | `auto_update: true` 时必填 | 自动重载间隔。 |

文件型 hosts 的 `auto_update` 只表示本地文件重载，不表示远程同步。

hosts 与 rule_set 使用相同的 per-resource snapshot 机制：本地文件变更只发布该 hosts 资源的新版本，保留其他资源版本；Policy matcher/version/hash 同步更新，hash 不再进入答案 key。请求和 optimistic refresh 按最新资源重新进行 hosts/rule 匹配，最终 target/查询不变时复用原答案，不因单资源变化扫描或全局清除缓存。

## 13. `outbound[]`

`outbound` 定义访问远程上游或远程规则集时使用的代理出口。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 出口唯一名称。 |
| `type` | enum | 必填 | v1 固定为 `socks5`，表示 SOCKS5 协议族。 |
| `proxy_url` | SecretRef | 必填 | `{env}`、`{file}` 或内联 `url`/字符串，三选一，取得完整代理 URL。 |
SecretRef 解析后的 URL scheme 必须为 `socks5://` 或 `socks5h://`：前者在 FluxDNS 本地解析目标主机名，后者把主机名交给代理解析。实际值可能包含用户名、密码或令牌；不要将环境变量内容或文件内容写入 Git 跟踪文件。内联形式会把明文 URL 写入配置文件，Management API 与 WebUI 只回显脱敏后的密码位（占位符 `FLUXDNS_REDACTED_SECRET`）：编辑仍指向同一 scheme/host/port 时，后端按活动源恢复原密码并只写入用户实际改动的字段；改了 scheme/host/port 或重填则按新值写入；若提交的仍是占位符而无法恢复（含采用外部文件差异、活动源不是同址内联来源），该编辑以 `VALIDATION_FAILED`（HTTP 422）被拒绝，用户需重新提交完整 URL，占位符不会作为密码持久化。其他代理类型尚未形成配置契约。

## 14. `rule_set[]`

规则集是策略匹配的数据源。资源可以内联、来自本地文件或从远程 URL 下载。运行时每个资源独立形成 `ResourceSnapshot`，拥有自己的 revision/epoch、content hash、来源 fingerprint 和 parser/compiler version；某个资源刷新不会要求其他资源同步刷新或自动清空全局缓存，但新 content hash 会进入后续请求的 policy fingerprint。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 规则集唯一名称，也可作为落盘文件名前缀。 |
| `type` | enum | 必填 | `const`、`file` 或 `remote`。 |
| `format` | enum | 必填 | 模板示例为 `json`、`clash`、`dat`。 |
| `rule` | string | `type: const` 时必填 | 内联规则内容。 |
| `path` | string | `type: file` 时必填 | 本地规则文件路径，相对路径以 `work.path` 为基准。 |
| `url` | URL | `type: remote` 时必填 | 远程规则下载地址。 |
| `proxy` | string | `type: remote` 时可选 | 下载远程规则使用的出口，引用 `outbound[].name`。 |
| `auto_update` | boolean | `type: file` 或 `remote` 时可选 | 是否定期刷新或重载资源。 |
| `update_interval` | duration | `auto_update: true` 时必填 | 刷新/重载间隔。 |

`format: clash` 表示 Clash 行格式，不是标准 YAML。监听器绑定前，所有已配置的 `const`、`file` 和 `remote` 资源都必须形成有效的首次内存快照；解析、读取、下载、代理或格式校验失败均拒绝启动。远程资源可先尝试加载 `work.rules_path` 中上一轮已成功校验并原子落盘的快照；没有有效快照且本次下载失败时必须拒绝启动。

`format: json` 接受两种 JSON dialect：

- FluxDNS legacy JSON：顶层只允许 `domain`、`domain_suffix`、`domain_keyword`、`domain_regex`，各字段接受单个 string 或 string array；
- sing-box source JSON：顶层为 `version` 和 `rules`，当前接受 `version: 1..=5`。每个 `rules[]` 只投影与 legacy JSON 同名的四个域名字段；`type`、`invert`、`network`、`ip_cidr`、端口、客户端条件、嵌套 logical rule 等其他 rule 字段全部忽略，即使忽略会改变原 sing-box 匹配语义。没有上述四个字段的单条 rule 被跳过，整份文档最终没有产生任何域名 matcher 时拒绝该资源。

该行为是面向域名集合文件的部分兼容，不是完整 sing-box source 语义兼容；使用者必须确保输入文件本身适合按四类域名字段做并集投影。sing-box 二进制 `.srs` 不支持。legacy JSON 和 sing-box 顶层结构不能混合，未知顶层字段继续拒绝。规则 parser/compiler 版本已提升为 `rule-index-v2`，旧 parser version 的远程 manifest 不作为可恢复 snapshot，启动时需要重新获取并校验资源。

进程启动后的刷新只有在下载/读取、完整解析和内容校验都成功后才能原子替换当前快照；失败时保留上一份有效快照并记录带字段路径的错误。`auto_update` 只控制启动后的刷新，不降低首次快照要求；重试采用指数退避并封顶 5 分钟，连续三次计划刷新失败或超过 `3 × update_interval` 未成功时标记资源 `stale`，但仍可使用上一份有效快照。没有旧快照的资源引用必须 fail-closed。模板暂未提供远程版本锁定或 expected checksum 字段，当前仅记录内部 content hash/source fingerprint，生产部署的固定版本策略需后续定稿。

`geosite:cn` 这类写法表示引用 V2Ray `GeoSiteList` protobuf `dat` 地理规则集中的命名子集。解析时先尝试完整资源名；完整名不存在时再按第一个 `:` 拆分。资源名大小写敏感；selector 必须非空、不超过 128 bytes，且只包含不含空白的可打印 ASCII，保留拒绝 `:` 分隔符和 `@` attribute 语法，并归一化为小写。因此 `geosite:geolocation-!cn` 可直接引用。格式不支持 selector 或 selector 不存在时在 prepare 阶段失败。

2026-11-08 局部核对（`de39a28` 加工作树）：`dat` 按引用按需加载。[`resolve.rs`](../../backend/src/config/resolve.rs) 的 `collect_rule_set_selectors` 汇总全部 strategy 对每个 dat 资源的 canonical selector，写入 `ResolvedRuleSet.selectors`；[`RuleIndex::parse_dat_with_limits`](../../backend/src/resource/rules.rs) 以 `DatSelection::Only` 只为这些 selector 解码 domain 并建索引，其余 selector 只校验 site 外层 protobuf 框架、selector 合法性/唯一性和 selector 数量上限，domain 条目不解码，坏条目也不会导致资源失败。`work.rule_set_max_rules` 只统计实际建索引的规则。例如 Loyalsoldier `geosite.dat` 约 53 万条规则，只引用 `cn`、`gfw` 时约 11.5 万条入索引，低于默认上限。被引用 selector 在文件中缺失时报 `RULE_DAT_SELECTOR_NOT_FOUND` 并在诊断中给出 selector；自动刷新遇到该错误时保留旧 snapshot。增删 selector 引用会使该资源在 reload 时按新集合重新解析，remote 从已落盘 content 恢复，不额外下载。同日在 Windows 本机 release 上，用最小配置加载 11.4 MB 的 Loyalsoldier `geosite.dat`（file 资源）并引用 `cn`、`gfw`，默认 `rule_set_max_rules` 下可以正常启动。进程 Private 内存比无 dat 基线高约 10–11 MB；改造前只引用 `cn` 时，高出约 44 MB（且需调高规则上限）。启动峰值 Working Set 从约 67 MB 降到约 34 MB。该结果未在 Linux/glibc 部署环境复测。查询热路径只查当前 rule 引用的 canonical selector 和既有 matcher，不会重新解析 dat。`dat` 内容按二进制 protobuf 读取，不先转换为 UTF-8。

## 15. `clients[]`

客户端规则按请求级选择策略，并提供客户端级缓存、TTL 和 ECS 覆盖。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 客户端规则名称，列表内唯一。 |
| `client_id` | string | 必填 | 唯一、大小写敏感的请求身份，不从 `name` 派生。 |
| `match` | object | 可选 | 客户端 IP 匹配条件；缺失时使用空 `ips`。 |
| `match.ips` | array[CIDR] | 可选 | 按客户端 IP/CIDR 匹配。 |
| `strategy` | string | 可选 | 命中后使用的策略；省略时继承 listener 或 DoH route 的策略。 |
| `cache` | object | 可选 | 客户端级缓存覆盖，优先级最高。 |
| `ttl_override` | object | 可选 | 客户端级 TTL 覆写，与 `cache` 平级。 |
| `edns_client_subnet` | object | 可选 | 客户端级 ECS 覆盖。 |

匹配优先级固定为：先精确 `client_id`，再按 IP 的最长 CIDR 前缀。在同一优先级出现多个冲突规则时在配置校验阶段报错，不依赖数组顺序。单个客户端可以仅由 `client_id` 标识而没有 IP 条目。

修改 `client_id` 立即替换运行时匹配索引：使用旧 ID 的 DoH 请求不再命中该客户端（可能回落到 IP 匹配），客户端缓存池按新身份重新建立，旧身份的条目不再被命中，按常规过期与容量淘汰回收。已写入的详情和统计保留请求时冻结的旧 ID，不迁移或重写；历史页面按当前目录把 ID 投影为名称，因此旧 ID 的记录不再显示当前名称，按名称筛选也只覆盖新 ID。

客户端级 `cache.enabled: true` 选择“实际客户端身份 + 生效策略”逻辑池，`false` 完全禁用当前请求的缓存；只有整个客户端 `cache` 对象缺失时才继续选择策略池或全局池。客户端级 `ttl_override` 和 `edns_client_subnet` 分别遵循[覆盖和继承](#25-覆盖和继承)中的层级规则。

## 16. 配置校验清单

v2 配置加载阶段至少执行以下校验：

1. `version` 存在且为 `2`；其他版本拒绝加载且不迁移。
2. 拒绝未知字段；每种 `type` 只允许自己的字段集合，条件字段满足 exactly-one-of/required-if 约束。
3. 所有资源集合中的 `name` 唯一，所有引用存在、类型正确且无循环引用。
4. `work.path` 非空；相对值可基于启动配置文件目录解析为绝对的 `resolved_work_path`，缺少来源目录时拒绝；目录不存在时创建，启动配置不在该目录时复制为 `<resolved_work_path>/config.yaml`。
5. `webui.enable` 为 `true` 时创建独立 Management Server，`false` 时不创建管理 listener；普通 listener、DoH endpoint 和 Management endpoint 地址展开后不存在 TCP/UDP bind 冲突，并明确 IPv6 v6-only 行为。
6. `mode`、`format`、`type`、端口、CIDR、URL、duration、权重和内嵌内容格式合法；group 成员缺省权重为 `1`，`parallel` 和 `failover` 的显式权重只能为 `1`。
7. `ttl_override` 与 `cache` 平级；策略/客户端 cache 对象存在时 `enabled` 必填，显式 `false` 不得回退到全局池。
8. ECS 块未配置时继承，显式 `mode: disabled` 才停止继续传递。
9. `webui.users[].password_hash` 必须是受支持算法生成的单向 hash，禁止明文 `password` 字段。
10. `dns.cache.memory.max_size_bytes` 为正数，`failure_ttl` 在 `1s..=5m`，`negative_ttl_max` 在 `1s..=1d`，`persistence.snapshot_interval` 在 `1s..=1d`。
11. DoH route 的 path 模板合法，允许固定段优先或同形显式模板优先的重叠，拒绝重复及同优先级歧义（包含隐式 ID 后缀）；endpoint 的 `tls.mode` 独立校验：`terminate` 必须有证书和私钥，`external` 不得有证书字段；GET/POST wire、Content-Type 和固定消息上限合法。
12. `forwarded_header`/`proxy_protocol` 必须配置 `trusted_proxies`，且可信范围只覆盖反代对端；PROXY v1/v2 前导头缺失、未知或非法时拒绝。
13. DoH 上游的 `bootstrap` 与 `connect_ip` 互斥；SecretRef 解析后的代理 scheme 合法，`socks5h://` 不得同时使用 `bootstrap`。
14. `database.type`/`database.path`/`database.records_path` 始终存在；prepare 阶段布局标记、数据库打开、migration 或写探针失败必须阻止启动，分片目录与统计库、缓存快照物理隔离。
15. 聚合统计默认开启，按日和有界 client/transport/strategy/upstream/RCODE/cache 维度持久化；`dns.resolve_log.enable` 只控制详情记录，R/G/T 来自 `statistics.retention`。
16. 所有配置资源在 bind 前形成有效首次 snapshot；任何首次读取、下载或校验失败都阻止启动，后续单资源刷新失败才保留该资源旧版本；成功刷新更新 policy fingerprint，但不能触发全局 cache clear。

## 17. v2 范围外与版本化边界

以下项目不在当前 v2 配置 schema 中，不应通过猜测扩展字段：

1. `dot`/`doq` listener 的字段、TLS/QUIC 材料来源和协议特有校验。
2. 主动上游健康检查、熔断器和持久健康分数配置。
3. 远程规则的 expected checksum、版本锁定和签名验证字段；当前只记录内部 content hash/source fingerprint。
4. 旧配置/旧数据库兼容窗口或转换工具；当前业务库升级链见 [Storage 模块](../architecture/backend/modules/storage.md)。
5. WebUI 配置写操作、缓存清理、权限分级和更长期的历史统计保留策略。

## 18. 协议依据

- [RFC 8484: DNS Queries over HTTPS](https://www.rfc-editor.org/rfc/rfc8484)
- [RFC 1035: Domain Names - Implementation and Specification](https://www.rfc-editor.org/rfc/rfc1035)
- [RFC 2308: Negative Caching of DNS Queries](https://www.rfc-editor.org/rfc/rfc2308)
- [RFC 9520: Negative Caching of DNS Resolution Failures](https://www.rfc-editor.org/rfc/rfc9520)
- [HAProxy PROXY protocol specification](https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt)
