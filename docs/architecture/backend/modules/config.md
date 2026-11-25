# Config 模块设计

> 文档状态：有效
>
> 适用范围：v2 配置加载、归一化、校验、引用图和安全快照
>
> 最后评审：2026-09-09（旧配置与迁移路径退出）
>
> 2026-11-06 局部评审：单次保存、活动源内存模型和内部版本/文件确认边界；其余范围沿原评审记录。
>
> 2026-11-08 局部评审：仅同步 dat selector 引用集合归一化到 `ResolvedRuleSet`；其余范围沿原评审记录。
>
> 2026-11-08 局部评审：仅同步 DoH 裸路由隐式 ID 后缀、同形显式模板优先及重叠拒绝契约；其余范围沿原评审记录，实施与验证证据见[DNS 管线](../../../implementation/backend/dns-pipeline.md#能力与证据)。
>
> 2026-11-26 局部评审：配置读写重构，重写 §1–§2 的写入职责、§8 配置快照、§11 快照验证要求和 §12–§14；其余范围沿原评审记录。
>
> 关联实现：[load.rs](../../../../backend/src/config/load.rs)、[resolve.rs](../../../../backend/src/config/resolve.rs)、[validate.rs](../../../../backend/src/config/validate.rs)、[store.rs](../../../../backend/src/config/store.rs)、[writer.rs](../../../../backend/src/config/writer.rs)、[disk.rs](../../../../backend/src/config/disk.rs)
>
> 关联文档：[配置字段参考](../../../implementation/configuration.md) · [后端架构](../overview.md)

## 1. 职责

v2 采用“唯一 name 管理键、独立 client_id、内存文档唯一权威、先应用后写盘、外部文件只提示”。[`contract.rs`](../../../../backend/src/config/contract.rs) 直接接入生产 loader/resolve/快照和 `ConfigStore`；契约默认值和边界见[配置参考](../../../implementation/configuration.md#v2-契约与生产基线2026-09-08)。保存、重新加载、写入磁盘与首用户写入共用 `ConfigStore` 的一把写锁，HTTP 接线见[Management 实现](../../../implementation/backend/management.md#配置保存与磁盘操作2026-11-26)，不保留旧 loader 或迁移入口。

Config 模块把用户 YAML 转换为不可变、无歧义、可直接用于 prepare 的 `ResolvedConfig`。资源内容首次 snapshot 与 listener 装配属于 Resource/Runtime/Application，不是 YAML loader 的职责。

它负责：

- schema version 识别与旧版本拒绝；
- 严格 DTO 反序列化和字段路径错误；
- 路径、URL、CIDR、duration、SecretRef source 和默认值归一化；SecretRef 实际值只通过显式 accessor 读取；
- 引用、循环、条件字段、继承和 bind 冲突校验；
- 生成配置摘要并保留产生运行态的源正文；
- 在类型化配置上应用变更，并只把变化的条目写回源文件；
- 观测源文件摘要，以单向镜像维护工作目录中的 `config.yaml`。

字段含义和默认值只在[配置参考](../../../implementation/configuration.md)定义，本模块文档说明解析、信任边界和写入不变量。

## 2. 内部结构

| 文件 | 职责 |
| --- | --- |
| `doh_route.rs` | DoH path 模板的共享编译、匹配和语义重叠检测 |
| `contract.rs` | 正式 v2 DTO、新字段预算、客户端身份与两级路径碰撞检查 |
| `model.rs` | 资源 DTO、协议类型与严格字段 parser |
| `load.rs` | v2 文件读取、大小/编码检查、直接 resolve 和安全配置快照 |
| `hash.rs` | 运行态和资源比较使用的非加密内容指纹 |
| `resolve.rs` | 默认值、三态、继承和来源信息归一化 |
| `validate.rs` | 名称、引用图、循环、条件字段和 bind |
| `change.rs` | 变更 DTO、`apply_changes`、引用改名与脱敏密码恢复 |
| `layout.rs` | 按行扫描 YAML 顶层段、段内列表项与子键的字节区间 |
| `writer.rs` | 条目级差异、块式渲染、文本替换与语义复核；首用户的 `webui.users` 写入 |
| `document.rs` | `ConfigDocument`（正文、SHA-256 摘要、`ConfigV2`、runtime 代次、需重启项） |
| `disk.rs` | 有界读取与摘要、同目录原子替换、沿用权限与旧版旁文件检测 |
| `store.rs` | 当前文档、磁盘观测与写锁；`save`/`reload`/`persist`/`create_initial_user` |

DTO、ValidatedConfig 和 ResolvedConfig 必须是不同类型，不能用布尔字段表示“也许已校验”。

## 3. 加载流水线

```text
read bounded UTF-8 bytes
  → parse minimal version header
  → require version: 2 without migration
  → deserialize strict ConfigV2
  → validate DTO values, references, cycles and bind conflicts
  → resolve config_dir and work.path, build BindPlan
  → normalize project paths, SecretRef sources and inheritance
  → build immutable ResolvedConfig + reports
  → optionally create a safe work-directory config snapshot
```

YAML 文件必须是 UTF-8。唯一 v2 loader 以 4 MiB 限制输入，避免在解析前无界分配；同时拒绝空输入、重复 document、显式 `null`、YAML tag 和未知字段，并保留安全字段路径。

所有 DTO 使用 `deny_unknown_fields` 或等价严格机制。tagged variant 只接受自身字段，不能把拼写错误吞入扁平 map。配置示例的 strict load 只使用离线 fixture，不访问远程资源。

## 4. 版本拒绝边界

只有 v2 配置参与加载和候选校验。旧版本、未来版本或缺失版本直接报错；不映射旧字段、不拆分旧客户端、不创建迁移报告，也不修改原文件。测试使用同一正式 loader，不另留兼容入口。工作目录配置快照的读写独立于版本拒绝，见[§8](#8-工作目录与配置快照)；启动不恢复任何写事务，见[§14](#14-磁盘观测写入与启动级配置)。

## 5. 归一化

`resolve.rs` 一次性完成：

- 先以启动配置文件所在目录解析相对 `work.path`，得到绝对的 `resolved_work_path`；
- 再以 `resolved_work_path` 为唯一基准，将其他相对项目路径规范化为绝对路径；
- duration、URL、CIDR 和枚举转换为强类型；
- group member 缺失的 `weight` 在 DTO 输入边界归一化为 `1`；
- `resource:selector` 引用中的 selector 使用与 Resource dat parser 相同的规则规范化为 lowercase canonical key；全部 strategy 对同一 dat 资源的 selector 引用汇总为 `ResolvedRuleSet.selectors`，供 Resource 按需加载，非 dat 资源恒为空集合；
- cache、TTL、ECS 的缺失/显式禁用/字段继承；
- client、strategy、route、resource 和 upstream 名称转换为 typed ID；
- SecretRef source 归一化为 env/file/inline `url` 引用；实际值不会在普通 YAML load 中读取，只能由后续 adapter 通过 `ResolvedSecretRef::resolve` 或 `resolve_proxy_url` 等显式 accessor 请求，并包装为 Debug/Display 脱敏、不可 Serialize 的 secret 类型；内联 `url` 在序列化、Debug 与日志中同样只输出脱敏密码位；
- cache/TTL/ECS 等继承相关值保留 `ValueSource`；不是所有字段都有完整来源追踪。

请求热路径禁止再次读取 YAML、解析 duration 或计算继承。loader 不负责资源内容首次加载和 `ResourceSnapshot` 发布。

## 6. 校验顺序

`resolve_config_v2` 先调用 `ConfigV2::validate`，再解析工作目录、构造 `BindPlan` 和归一化模型。校验聚合基础值、集合、引用、upstream cycle 与 bind 错误；严格反序列化或语义校验失败后，不继续生成 `ResolvedConfig`。

检查内容包括 exactly-one-of/required-if、cache/TTL/ECS 阈值、WebUI origin/users、SecretRef source、DoH 模板重叠，以及 IPv4/IPv6 和 Management/DNS TCP 地址冲突。实际输出是 `ValidatedConfig { resolved: Arc<ResolvedConfig> }`，其中含 `BindPlan`；没有独立的 `BindPlanInput`、`ResourcePlan`、`StoragePlan` 类型，也没有通用“跳过 pass”报告。

资源内容和 dat selector 是否存在，以及 SecretRef 解析后的代理组合是否合法，仍须在后续 Resource/Upstream prepare 或显式 SecretRef 检查中确认，不能由 YAML 校验成功推断。

## 7. 引用图

名称先在各自命名空间构建 symbol table，再解析引用。错误区分：

- duplicate definition；
- missing reference；
- wrong target kind；
- cycle；
- unsupported selector。

当前循环检测覆盖 DoH bootstrap 与 group 主成员/fallback 的 upstream 依赖图；对节点和边排序后用 DFS 输出发现的闭环路径，不承诺图论上的最短环。outbound/resource 引用另行检查，不存在另一套统一依赖图。

## 8. 工作目录与配置快照

路径解析必须显式携带配置来源，顺序固定如下：

1. `load_from_path(path)` 先把启动配置路径转换为词法归一化的绝对路径，并取其父目录作为 `config_dir`；只有启动配置路径本身允许在这一步使用进程启动时的当前工作目录。
2. `work.path` 为绝对路径时直接词法归一化；为相对路径时按 `config_dir.join(work.path)` 解析，结果记为 `resolved_work_path`。
3. 其余项目路径为绝对路径时直接词法归一化；为相对路径时按 `resolved_work_path.join(value)` 解析。实现不得将这些路径直接拼到原始 `work.path`、`config_dir` 或当时的进程当前工作目录。
4. `load_from_bytes`/`load_from_str` 没有物理配置来源。若 DTO 中的 `work.path` 是相对路径，应返回缺少配置基准的稳定错误；需要支持该场景的调用方必须显式传入来源路径/目录，或使用绝对 `work.path`。
5. 解析使用词法归一化，使尚未创建的工作目录也能参与计算；目录创建、symlink/special-file 拒绝和权限检查仍放在有文件系统副作用的 prepare/快照边界执行。

例如启动文件为 `/opt/_fluxdns/config.yaml`、`work.path: ./` 时，`resolved_work_path` 是 `/opt/_fluxdns`；随后 `database.path: ./data/fluxdns.sqlite3` 解析为 `/opt/_fluxdns/data/fluxdns.sqlite3`，而不是相对于进程当前工作目录或再次相对于配置文件路径拼接。

当启动配置本身不等于 `<resolved_work_path>/config.yaml` 时，按契约复制到该固定路径；同目录不同文件名仍必须创建派生快照。快照逻辑只能接收已经解析完成的绝对工作目录，不能再次解释原始 `work.path`。派生快照是源文件的单向镜像，源文件始终是权威：

1. 创建工作目录和父目录；
2. 目标不存在时，在同目录写临时文件、flush/fsync 后以 hard link no-replace 发布，并发启动只有一个创建者；
3. 目标存在且字节相同时不操作；
4. 目标存在但内容不同（含空文件、超限或非 UTF-8）时，用源正文原子替换；
5. 目标是 symlink 时拒绝，是目录等非普通文件时以冲突拒绝启动；
6. 不把 SecretRef 解析后的值写回配置快照。

运行期保存与写入磁盘先写源文件，再用同一原子写覆盖镜像；镜像写入失败只记录 warning 并附在响应诊断中，不影响源文件已写入的结论。磁盘观测与冲突判断从不读取镜像。临时文件、错误消息和日志不得包含 secret；快照只保存输入 YAML 的 SecretRef 占位符，不保存解析后的实际值。

## 9. 输出模型

`ResolvedConfig` 应满足：

- 所有引用已解析为 typed handle/ID；
- 资源级默认值和字段继承已展开；client/strategy 的请求级优先级由后续 Policy prepare 在 typed `Resolved*` 上组合，不重新解析 YAML；
- 路径、URL、CIDR、duration 已强类型化；
- secret 只能通过受控 accessor 使用；普通 YAML load 不读取实际值，读取动作留给后续 adapter 边界；
- `ResolvedConfig` 保留 `input_hash`、`normalized_hash`，loader 输出保留源路径；错误携带字段路径及可用行列，不提供覆盖所有 resolved 字段的 source span map；
- 可安全输出 redacted view，但不能直接 Serialize 原对象。

Runtime 只能接收 `Arc<ResolvedConfig>` 和 prepare plans，不能接收原始 YAML DTO。

## 10. 错误与安全

配置错误包含：

- schema version；
- 稳定分类；
- YAML/逻辑字段路径；
- 可选行列；
- 安全的 expected/actual 摘要；
- 修复提示。

SecretRef 的实际值、proxy credential、password hash 全文和证书私钥不出现在 Debug、Display、日志或 migration report 中。

## 11. 契约验证要求

- 当前示例配置可离线严格解析并得到稳定 normalized snapshot；其中远程规则、本地资源和代理 SecretRef 只做配置级校验，不执行网络或资源首次 snapshot；
- 未知字段、错误 variant 字段、空/`null`/缺失差异；
- 所有 exactly-one-of 和 required-if；
- 名称重复、缺失引用、类型错误和确定性的闭环路径；
- cache/TTL/ECS 全继承矩阵；
- group member 缺省 `weight: 1` 及各 mode 的显式权重约束；
- 包含 `!` 等可打印 ASCII 的 dat selector canonicalization；
- DoH 裸路由精确路径与单个非空隐式 ID 后缀、尾随斜杠/根路径，以及显式 `{client_id}` 必填且不接受额外多段；隐式后缀纳入逐段固定段优先比较，同样段结构下显式模板优先，不依赖配置顺序；允许有确定优先级的重叠，拒绝重复和同优先级重叠，包含 `/dns` 与 `/dns/` 的隐式后缀冲突；canonical route ID 保留配置原文且不改写配置（2026-11-08 局部评审，完整样例见[配置参考](../../../implementation/configuration.md)）；
- IPv4/IPv6 bind 冲突；
- SecretRef env/file/inline `url`、缺失、空值、非法 scheme 和脱敏；
- migration golden test、空链幂等和有损 warning 边界；
- 配置快照创建、相同内容 no-op、不同内容以源覆盖、并发 no-replace、symlink 防护、目录同步和临时文件不污染目标；
- 条目级写回：缩进/顶格列表、块标量、多行引号、注释与空行归属、CRLF、无末尾换行、文档标记、flow 段降级；规则重排/类型切换/增删与 ECS 增删时未修改条目和条目外注释逐字节不变；改名只改动引用方条目；含锚点条目被拒绝；
- 修改内联代理密码后新密码写入文件且不含脱敏占位符；
- 保存冲突、覆盖写入、写盘失败后 `persisted: false` 与补写、reload 成功与校验失败、写锁忙、回执丢失按代次判定、首用户 setup、启动级字段变化与改回；
- 原子替换、沿用权限（含 Windows DACL）、符号链接写入真实路径、临时文件复用。
- 路径解析矩阵：配置文件参数为绝对/相对路径，`work.path` 为绝对/`.`/含 `..` 的相对路径，项目路径为绝对/相对路径，以及无来源 bytes/string 加载时的错误边界。

实际加载与支持边界见[配置参考](../../../implementation/configuration.md)；本节是验证要求，不代表本次执行结果。

## 12. 配置文档与写锁

`ConfigStore` 用 `ArcSwap` 持有当前 `ConfigDocument { text, digest, config, runtime_revision, restart_required }`，它是唯一的配置权威，也是当前 Runtime 的来源（启动级字段除外，见 §14）。核心不变量：

| 编号 | 不变量 |
| --- | --- |
| I1 | 所有读接口都从当前文档投影；`current()` 无锁返回 `Arc<ConfigDocument>`，不读磁盘。 |
| I2 | 唯一的配置版本是 `config_digest = SHA-256(正文)`。`runtime_revision` 只是 Runtime 内部代次，用于读一致性，不作为写入前置条件。 |
| I3 | 磁盘状态只比较磁盘正文摘要与 `config_digest`，不比较文件身份、mtime 或权限。 |
| I4 | save、reload、persist 和首用户 setup 共用一把进程内写锁，取不到立即返回 busy；不加跨进程锁。 |
| I5 | 磁盘外改永远不会自动生效，只有显式 reload 才会应用。 |
| I6 | 写回只替换发生变化的条目；结果必须重新严格解析并与目标配置语义相等，否则不写盘。 |
| I7 | 先应用 Runtime，再写盘；写盘失败不回滚 Runtime，而是表现为磁盘与内存不一致，与外改共用同一提示入口。 |
| I8 | 源文件渲染和变更比较使用不脱敏的源序列化（`to_source_value`）；只有 API 投影使用脱敏序列化。 |

保存流程：写锁 → 磁盘摘要与当前文档不一致且未要求覆盖时返回冲突 → `apply_changes` 在类型化配置上替换/追加条目并改写引用 → 条目级写回 → 严格 resolve（保留期缩短需显式确认）→ prepare 与 `ServiceControl` 换代 → 发布新文档 → 复核磁盘摘要后原子写源文件，再写镜像。换代回执丢失时按 coordinator 当前代次判定是否已应用，不暴露“结果未知”。prepare 期间磁盘被外改时不写盘，返回“已生效未写盘”，由用户选择覆盖或重新加载。除这两次摘要比较外没有其他版本比较：同一条目并发编辑时后保存者覆盖，不同条目互不影响。

reload 只在磁盘正文可读、摘要不同且严格校验通过时 prepare 并换代；失败时 Runtime 与当前文档都不变。`webui.users` 变化时同步认证并撤销会话。persist 只把当前正文写回源文件与镜像，不触碰 Runtime。首用户 setup 经 writer 只替换 `webui.users`，写盘成功后才发布认证快照，不递增 runtime 代次。

启动时若发现旧版本遗留的 `.fluxdns-journal`、stage 或 lock 旁文件，只记录 warning 并保留文件，不恢复也不删除；源文件始终是权威。

## 13. 条目级写回

`layout.rs` 按行扫描源文本，只识别三种结构：第 0 列 `key:` 开始的顶层段；列表模块段内相同缩进 `- ` 开始的列表项（缩进列表与顶格列表均支持）；单例段内相同缩进 `key:` 开始的子键。段尾或项尾的空行与注释归入下一段/项，块标量和多行引号的续行更深缩进，不会被误认为起点。定位必须自证：每个切片单独解析须与 `base` 对应值相等，数量与 `name` 顺序一致，否则该段降级为整段替换（段内注释丢失）。

`writer.rs` 按顶层段比较 `base` 与 `next` 的源序列化值：相等的段、按 `name`（经改名映射）匹配且值相等的列表项原文不动；值不同的项或子键用块式渲染替换其字节区间；新增项追加到段尾，段缺失或为空时整段写成块式；单例段子键可增删改。渲染沿用源文件换行符（LF/CRLF）。替换后的全文必须通过 `ConfigV2::parse` 与路径解析，且源序列化值与 `next` 完全相等，否则以 `CONFIG_SOURCE_LAYOUT_UNSUPPORTED` 拒绝；含 YAML 锚点、别名或 merge key 的被编辑条目同样拒绝。这是写回层唯一的安全检查。

取舍：被编辑条目内部的注释和非规范写法会按规范形式重写；条目外的注释、空行和其他条目逐字节保留。结果确定，不依赖 CST 对任意 YAML 写法的支持。

## 14. 磁盘观测、写入与启动级配置

磁盘观测只看源文件：`DiskObservation::{Readable{digest}, Missing, Unreadable}`，上限 4 MiB、必须 UTF-8，symlink 与 hard link 按普通文件读取。`disk.status` 在读取时由最近观测与当前摘要计算为 `in_sync`/`modified`/`missing`/`unreadable`，因此不需要识别自写；本进程写盘成功后直接把观测设为所写摘要。watcher 1 秒轮询、连续两次相同才上报；每次观测带代次，store 丢弃写入之后才完成的旧代次结果。

`write_atomic` 在目标同目录创建固定名的临时文件（已存在则截断），沿用目标权限（Unix mode、Windows DACL），`sync_all` 后原子替换（Unix `rename` 并同步父目录，Windows `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`）；只读目标拒绝写入。源文件是 symlink 时写入其真实路径并保留链接；hard link 在替换后断开。不写 journal：崩溃时目标要么是旧正文，要么是新正文。

启动级配置以 `process_owned_reload_changes` 为唯一定义：`database`、`webui` 的 `enable`/`address`/`port`/`public_origin` 与 `dns.resolve_log`。save 与 reload 构造 Runtime 候选前把这些字段替换为当前 Runtime 的值，因此换代不会触发需重启错误；文件与内存文档照常采用新值，并把差异记入 `restart_required`。改回原值后列表为空，进程重启后从空开始。该列表只通过 `GET /api/v2/config/system` 暴露。`webui.users` 不属于启动级字段。
