# Config 模块设计

> 状态：v1 方案和阶段 2 实现已完成并验证
>
> 更新日期：2026-08-31
>
> 目标代码：`backend/src/config/*`
>
> 字段唯一契约：[配置字段参考](../configuration-reference.md)
>
> 上位设计：[后端架构](../backend-architecture.md) · [开发计划](../backend-development-plan.md)

## 1. 职责

Config 模块把用户 YAML 转换为不可变、无歧义、可直接用于 prepare 的 `ResolvedConfig`。当前实现覆盖配置边界本身；资源网络首次 snapshot、Runtime 接线和 App 启动闭环属于后续阶段。

它负责：

- schema version 识别与显式迁移；
- 严格 DTO 反序列化和字段路径错误；
- 路径、URL、CIDR、duration、SecretRef source 和默认值归一化；SecretRef 实际值只通过显式 accessor 读取；
- 引用、循环、条件字段、继承和 bind 冲突校验；
- 生成配置摘要、来源信息和 migration report；
- 安全地维护工作目录中的 `config.yaml` 快照。

字段含义和默认值只在 [configuration-reference.md](../configuration-reference.md) 定义，本模块文档只说明实现方式。

## 2. 内部结构

| 文件 | 职责 |
| --- | --- |
| `model.rs` | 当前 schema DTO、按 `type` 区分的 tagged model |
| `load.rs` | 文件读取、大小/编码检查、字段路径解析、版本迁移和安全配置快照 |
| `migrate.rs` | `MigrationStep` 注册表和 `MigrationReport` |
| `resolve.rs` | 默认值、三态、继承和来源信息归一化 |
| `validate.rs` | 名称、引用图、循环、条件字段、bind 和 feature gate |

DTO、ValidatedConfig 和 ResolvedConfig 必须是不同类型，不能用布尔字段表示“也许已校验”。

## 3. 加载流水线

```text
read bounded UTF-8 bytes
  → parse minimal version header
  → run ordered migration chain
  → deserialize current strict DTO
  → normalize paths, SecretRef sources and inheritance
  → semantic validation passes
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

- 相对路径基于 `work.path` 规范化为绝对路径；
- duration、URL、CIDR 和枚举转换为强类型；
- cache、TTL、ECS 的缺失/显式禁用/字段继承；
- client、strategy、route、resource 和 upstream 名称转换为 typed ID；
- SecretRef source 归一化为 env/file 引用；实际值不会在普通 YAML load 中读取，只能由后续 adapter 通过 `ResolvedSecretRef::resolve` 或 `resolve_proxy_url` 等显式 accessor 请求，并包装为不可 Debug、不可 Serialize 的 secret 类型；
- 每个生效值保留来源层级，便于错误和诊断。

请求热路径禁止再次读取 YAML、解析 duration 或计算继承。阶段 2 不负责远程/本地资源内容的首次加载和 `ResourceSnapshot` 发布。

## 6. 校验顺序

语义校验分 pass 执行，错误按字段路径聚合后一次返回：

1. 基础值：端口、duration、URL、CIDR、大小和枚举；
2. 条件字段：exactly-one-of、required-if、forbidden-if；
3. 集合唯一性和 typed reference；
4. upstream/bootstrap/group、resource/proxy 等有向图循环；
5. cache、TTL、ECS 继承与阈值；
6. listener/endpoint 展开和 IPv4/IPv6 bind 冲突；
7. WebUI v1 feature gate；
8. SecretRef scheme 与敏感值安全检查；
9. 生成 `BindPlanInput`、`ResourcePlan`、`StoragePlan` 等 prepare 输入。

同一轮可以报告多个互不依赖的配置错误；依赖前置解析成功的 pass 在前置失败后跳过，并记录“未执行”而不是产生级联噪声。

## 7. 引用图

名称先在各自命名空间构建 symbol table，再解析引用。错误区分：

- duplicate definition；
- missing reference；
- wrong target kind；
- cycle；
- unsupported selector。

循环报告应给出完整最短环，例如 `upstreams.a.bootstrap → upstreams.b → upstreams.a`，而不是只报“存在循环”。

## 8. 工作目录与配置快照

当启动配置不位于 `work.path` 时，按契约复制为 `<work.path>/config.yaml`。为避免覆盖用户已有配置，v1 采用：

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
- 保留 source span/path 与 normalized hash；
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

## 11. 测试

- 当前示例配置可离线严格解析并得到稳定 normalized snapshot；其中远程规则、本地资源和代理 SecretRef 只做配置级校验，不执行网络或资源首次 snapshot；
- 未知字段、错误 variant 字段、空/`null`/缺失差异；
- 所有 exactly-one-of 和 required-if；
- 名称重复、缺失引用、类型错误和最短环；
- cache/TTL/ECS 全继承矩阵；
- IPv4/IPv6 bind 冲突；
- SecretRef env/file、缺失、空值、非法 scheme 和脱敏；
- migration golden test、空链幂等和有损 warning 边界；
- 配置快照创建、相同内容 no-op、不同内容拒绝覆盖、并发 no-replace、symlink 防护、目录同步和临时文件不污染目标。

阶段 2 当前基线验证（测试数量可能随后续阶段增量）：

- 阶段 2 记录起点为 69 tests；当前工作树已增量至 99 tests，`cargo test --manifest-path backend/Cargo.toml --locked -- --test-threads=1`：99 passed、0 failed；
- `cargo clippy --manifest-path backend/Cargo.toml --locked -- -D warnings`：通过；
- `cargo fmt --manifest-path backend/Cargo.toml --all -- --check`：通过。

## 12. 实现检查清单

- [x] 建立 version 1 strict DTO；
- [x] 完成 v1 空 migration registry/report；
- [x] 完成 normalize 和来源跟踪，SecretRef 实际读取保留在显式 accessor/后续 adapter 边界；
- [x] 完成分 pass 校验、typed reference graph、cycle 和 bind plan；
- [x] 完成安全配置快照；
- [x] 建立 strict example、migration、SecretRef、snapshot、继承和 bind matrix tests；property tests 与最终全量复核仍可在后续验证中补充；
- [x] 生成独立 `ValidatedConfig`/`ResolvedConfig` 和 prepare 输入边界；Runtime 实际消费与 App 启动接线属于阶段 3。

当前实现进度：**100%**（Config 模块边界已完成；资源网络首次 snapshot、Runtime/App 启动闭环不计入本阶段）。
