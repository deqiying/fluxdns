# Config 模块设计

> 文档状态：有效
>
> 适用范围：配置加载、迁移、归一化、校验、引用图和安全快照
>
> 最后评审：2026-09-05（模块边界与关键契约静态核对，基线见[模块索引](README.md)；不含运行验收）
>
> 关联实现：[load.rs](../../../../backend/src/config/load.rs)、[resolve.rs](../../../../backend/src/config/resolve.rs)、[validate.rs](../../../../backend/src/config/validate.rs)、[store.rs](../../../../backend/src/config/store.rs)
>
> 关联文档：[配置字段参考](../../../implementation/configuration.md) · [后端架构](../overview.md)

## 1. 职责

Config 模块把用户 YAML 转换为不可变、无歧义、可直接用于 prepare 的 `ResolvedConfig`。资源内容首次 snapshot 与 listener 装配属于 Resource/Runtime/Application，不是 YAML loader 的职责。

它负责：

- schema version 识别与显式迁移；
- 严格 DTO 反序列化和字段路径错误；
- 路径、URL、CIDR、duration、SecretRef source 和默认值归一化；SecretRef 实际值只通过显式 accessor 读取；
- 引用、循环、条件字段、继承和 bind 冲突校验；
- 生成配置摘要、来源信息和 migration report；
- 安全地维护工作目录中的 `config.yaml` 快照。

字段含义和默认值只在[配置参考](../../../implementation/configuration.md)定义，本模块文档说明解析、信任边界和写入不变量。

## 2. 内部结构

| 文件 | 职责 |
| --- | --- |
| `doh_route.rs` | DoH path 模板的共享编译、匹配和语义重叠检测 |
| `model.rs` | 当前 schema DTO、按 `type` 区分的 tagged model |
| `load.rs` | 文件读取、大小/编码检查、字段路径解析、版本迁移和安全配置快照 |
| `migrate.rs` | `MigrationStep` 注册表和 `MigrationReport` |
| `resolve.rs` | 默认值、三态、继承和来源信息归一化 |
| `validate.rs` | 名称、引用图、循环、条件字段和 bind |
| `store.rs` | 首用户配置事务、fingerprint 冲突、journal 与恢复 |
| `source_edit.rs` | 保留源 YAML 表达的定向 users 编辑 |

DTO、ValidatedConfig 和 ResolvedConfig 必须是不同类型，不能用布尔字段表示“也许已校验”。

## 3. 加载流水线

```text
read bounded UTF-8 bytes
  → parse minimal version header
  → run ordered migration chain
  → deserialize current strict DTO
  → validate DTO values, references, cycles and bind conflicts
  → resolve config_dir and work.path, build BindPlan
  → normalize project paths, SecretRef sources and inheritance
  → build immutable ResolvedConfig + reports
  → optionally create a safe work-directory config snapshot
```

YAML 文件必须是 UTF-8。加载器以 `DEFAULT_MAX_CONFIG_BYTES`（当前 8 MiB）限制输入，避免在解析前无界分配；超限错误记录上限，v1 不新增配置字段。当前 loader 还拒绝空输入、重复 document 和显式 `null` 旁路，并通过 `serde_path_to_error` 保留解析路径和位置。

所有 DTO 使用 `deny_unknown_fields` 或等价严格机制。tagged variant 只接受自身字段，不能把拼写错误吞入扁平 map。配置示例的 strict load 只使用离线 fixture，不访问远程资源。

## 4. Migration

迁移注册表按单步链组织：

```text
MigrationStep {
  id,
  from_version,
  to_version,
  transform,
}
```

约束：

- 单步转换无网络、数据库、Secret 或系统时间依赖；
- 输入相同则输出和报告相同；
- 缺步、分叉、重复 version 或结果版本不符时失败；
- 有损删除必须产生 warning，服务启动不自动确认有损迁移；
- migration report 记录 step IDs、变更摘要、warning、输入 hash 和输出 hash；归一化后的 `ResolvedConfig` 另行记录 `normalized_hash`；
- 当前只有 version 1 时仍建立空链测试，防止未来把兼容逻辑散落到字段解析中。

## 5. 归一化

`resolve.rs` 一次性完成：

- 先以启动配置文件所在目录解析相对 `work.path`，得到绝对的 `resolved_work_path`；
- 再以 `resolved_work_path` 为唯一基准，将其他相对项目路径规范化为绝对路径；
- duration、URL、CIDR 和枚举转换为强类型；
- group member 缺失的 `weight` 在 DTO 输入边界归一化为 `1`；
- `resource:selector` 引用中的 selector 使用与 Resource dat parser 相同的规则规范化为 lowercase canonical key；
- cache、TTL、ECS 的缺失/显式禁用/字段继承；
- client、strategy、route、resource 和 upstream 名称转换为 typed ID；
- SecretRef source 归一化为 env/file 引用；实际值不会在普通 YAML load 中读取，只能由后续 adapter 通过 `ResolvedSecretRef::resolve` 或 `resolve_proxy_url` 等显式 accessor 请求，并包装为 Debug/Display 脱敏、不可 Serialize 的 secret 类型；
- cache/TTL/ECS 等继承相关值保留 `ValueSource`；不是所有字段都有完整来源追踪。

请求热路径禁止再次读取 YAML、解析 duration 或计算继承。loader 不负责资源内容首次加载和 `ResourceSnapshot` 发布。

## 6. 校验顺序

`resolve_config_with_base_dir` 先调用 `validate_config`，再解析工作目录、构造 `BindPlan` 和归一化模型。`validate_config` 聚合基础值、集合、引用、upstream cycle 与 bind 校验错误；严格反序列化或这轮语义校验失败后，不继续生成 `ResolvedConfig`。

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

当启动配置不位于 `resolved_work_path` 时，按契约复制为 `<resolved_work_path>/config.yaml`。快照逻辑只能接收已经解析完成的绝对工作目录，不能再次解释原始 `work.path`。为避免覆盖用户已有配置，v1 采用：

1. 创建工作目录和父目录；
2. 对输入字节计算 hash；
3. 目标不存在时，在同目录写临时文件、flush/fsync 后原子 no-replace 发布；
4. 目标存在且 hash 相同时不操作；
5. 目标存在但内容不同时拒绝启动，并提示显式处理，不自动覆盖；
6. 不把 SecretRef 解析后的值写回配置快照。

实现使用同目录临时文件、`create_new`、hard-link no-replace 发布、文件和目录同步；目标 symlink/special file 会拒绝，Unix 临时快照使用 owner-only 权限。临时文件、错误消息和日志不得包含 secret。快照只保存输入 YAML 的 SecretRef 占位符，不保存解析后的实际值。

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
- DoH 尾部 `{client_id}` 裸路径和 route 语义重叠拒绝；
- IPv4/IPv6 bind 冲突；
- SecretRef env/file、缺失、空值、非法 scheme 和脱敏；
- migration golden test、空链幂等和有损 warning 边界；
- 配置快照创建、相同内容 no-op、不同内容拒绝覆盖、并发 no-replace、symlink 防护、目录同步和临时文件不污染目标。
- 路径解析矩阵：配置文件参数为绝对/相对路径，`work.path` 为绝对/`.`/含 `..` 的相对路径，项目路径为绝对/相对路径，以及无来源 bytes/string 加载时的错误边界。

实际加载与支持边界见[配置参考](../../../implementation/configuration.md)；本节是验证要求，不代表本次执行结果。

## 12. 首用户配置写入

`ConfigStore` 只修改 CLI 源 YAML 的 `webui.users`，不序列化 ResolvedConfig；后者已丢失原始路径/SecretRef 表达。源文件之外的 `<work.path>/config.yaml` 是派生 snapshot，写入时必须同步，否则下次启动会被 no-replace 冲突保护拒绝。

写入不变量：

1. 获取进程内排他锁与跨进程 lock，竞争有界失败，不无限等待。
2. 重读源配置和 snapshot，比较预期 fingerprint；拒绝覆盖外部编辑，确认 users 仍为空。
3. 使用 source-preserving YAML adapter 只替换/新增 users；不支持的表达明确失败，不能退回整份 resolved 序列化。
4. 候选重新走同一严格 parser/validator，验证用户与其他配置语义未被意外改变。
5. 在目标同目录创建受限临时文件，write/flush/fsync；写入并同步 journal 后再替换两个目标。
6. 只有文件提交完成才发布认证快照、内部 fingerprint 和新 session。

两个目录的两次 rename 不是整体原子事务。journal 必须记录旧/新 fingerprint、目标与 staged candidate，启动加载前恢复：都已是新内容则清理；只完成一个目标则验证 staged 内容后补完；出现未知内容或损坏 journal 时 fail-closed，不猜测或覆盖外部修改。

源与 snapshot 是同一文件时去重，不生成虚假的双文件事务。临时文件、journal 和最终文件不得放宽原权限；Unix/Windows 替换、目录同步、权限和 crash point 需要各自的验证证据。

source-preserving 验收至少覆盖注释、键序、未知字段拒绝、块/流格式、引号、锚点/别名与不支持语法、SecretRef 原样保留、路径表达不变、双路径竞争、journal 损坏和中断恢复。无法安全修改的输入必须显式拒绝。

真实入口、writer 与 loader 大小上限差异及故障验证边界见[管理端实现](../../../implementation/backend/management.md)。
