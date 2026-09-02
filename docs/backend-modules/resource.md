# Resource 模块设计

> 状态：v1 方案已完成，已实现 hosts/rule parser、immutable matcher、const/file loader、remote manifest 原子持久化与恢复、资源版本 CAS 及 scheduler/coordinator 的 Runtime-facing 编排边界；已实现一次性 `ResourceRefreshWorker` 的 remote fetch/parse/persist reservation 接线，并由 `ReqwestResourceFetcher` 提供 direct HTTP/HTTPS 与 SOCKS5/SOCKS5H 生产读取；async `PreparedRuntime` 已在 bind 前完成 remote rule-set restore-or-fetch、file hosts/rule-set snapshot 加载和 typed Policy 构造；`auto_update=true` 的 remote、file rule-set、file hosts 均由 service Supervisor 持有长期 refresh task，并在同一 ActiveRuntime 内完成 Policy live publish 与 Runtime 资源摘要原子更新；`ResourceRegistrySnapshot` 已提供按资源过滤的更高版本合并原语，`ResourceRefreshRuntime` 现可迁移已发布 registry 版本和稳定 schedule/backoff 状态，供候选 Runtime 合并使用；service reload 现按资源 ID 增量复用未变化 worker、取消移除 worker；真正跨 Runtime 配置候选发布和独立 listener 生命周期仍未完全接入
>
> 更新日期：2026-09-03
>
> 目标代码：`backend/src/resource/*`
>
> 上位设计：[后端架构](../backend-architecture.md) · [配置字段参考](../configuration-reference.md)
>
> 相关方案：[Policy](policy.md) · [Runtime](runtime.md) · [Upstream](upstream.md)

## 1. 职责

Resource 模块负责 hosts 和 rule_set 的读取、下载、解析、规范化、编译、落盘、刷新和 per-resource snapshot。

内部结构：

| 文件 | 职责 |
| --- | --- |
| `hosts.rs` | JSON/hosts 格式、本地 RR 索引 |
| `rules.rs` | JSON/Clash/dat 规则解析和 matcher |
| `loader.rs` | const/file、大小限制、稳定读取与 parser 边界；remote 内容加载由 `remote.rs` 编排 |
| `orchestrator.rs` | schedule、refresh coordinator、due/backoff、CAS publish 和 stop 语义的 Runtime-facing 纯逻辑编排 |
| `snapshot.rs` | metadata、revision、registry 和 publish input |
| `remote.rs` | remote fetch、manifest/content 原子落盘与恢复校验 |

查询热路径只读取编译后的不可变索引，不访问文件、网络或 parser。

## 2. ResourceSnapshot

每个资源独立记录：

- typed resource ID/name；
- monotonically increasing epoch/revision；
- content hash；
- source fingerprint：file metadata、ETag/Last-Modified 等；
- parser/compiler version；
- fetched/compiled time；
- source kind 与脱敏位置；
- 是否使用已落盘 fallback；
- stale/degraded 状态；
- immutable compiled index。

顶层 ResourceRegistrySnapshot 是资源名到 `Arc<ResourceSnapshot>` 的不可变 map。

候选 Runtime 以自身 registry 为基准合并旧 Runtime 的资源结果时，只允许定义兼容的资源进入合并集合，并且仅接收严格更高的 `ResourceVersion`；同版本内容保留候选自身的值，避免旧 Runtime 覆盖新配置候选。该原语只处理 immutable registry，不负责 Policy、worker schedule 或 Runtime metadata 的同步，后续由 Runtime prepare 边界统一接线。

## 3. 加载流水线

```text
bounded read/fetch
  → content hash
  → format parse
  → canonical normalize
  → semantic validation
  → compile immutable index
  → persist raw+manifest atomically when needed
  → publish candidate with epoch
```

解析和编译必须全部成功后才能落盘为“有效 snapshot”。下载了一半或只通过语法解析的内容不能替换旧版本。

## 4. Source

### const

直接读取 Config 已提供的字符串，仍执行大小、格式和语义校验。

### file

- 路径已由 Config 归一化；
- 使用周期性 stat/content fingerprint，不依赖平台文件 watcher；
- 文件变化后完整读取并编译；
- 读取期间文件变化时丢弃并重试；
- `auto_update=false` 只在 prepare 加载一次。

### remote

- 通过已绑定 ResourceFetcher/outbound 下载；
- 使用 deadline、重定向上限、body 大小上限和 HTTPS 校验；
- 可发送 `If-None-Match`/`If-Modified-Since`；
- 304 只更新时间状态，不创建新 content revision；
- response body 先写临时文件并计算 hash，再解析；
- v1 没有 expected checksum 配置，不把内部 hash 宣称为来源真实性校验。

## 5. Hosts 格式

v1 本地回答支持 A、AAAA 和 CNAME：

- JSON 可为单值或数组，并尊重显式 `enable=false`；
- hosts 行格式接受 `IP name...`，同一 owner 的 A/AAAA 合并；
- `*.example.com` 只匹配子域，不匹配 apex；
- exact 优先于 wildcard，wildcard 取最长后缀；
- 同一 owner 的 CNAME 不能与 A/AAAA 并存；
- CNAME target 也按 canonical domain 校验；
- 重复相同 RR 去重，冲突类型报错；
- TTL 使用模块实现常量，最终仍受 Policy TTL override。

`upstreams[type=hosts]` 可复用 parser/compiler，但产出 connector；顶层 `hosts[]` 产出本地回答 matcher，两个资源命名空间不混用。

## 6. Rule 格式

当前实现覆盖 JSON 与 Clash 的 exact/suffix/受限 regex matcher；`dat` selector map 仍保留为后续边界。

### JSON

支持明确字段：

- `domain`：exact；
- `domain_suffix`：apex 与所有子域；
- `domain_regex`：受限 regex 列表或单值。

未知字段拒绝。regex 在加载时编译，并限制数量、长度和总 program size。

### Clash

v1 接受 `DOMAIN`、`DOMAIN-SUFFIX` 和 `DOMAIN-REGEX` 行。空行和注释忽略；未知 rule type、缺列或多余不可解释字段报带行号错误。

### dat

解析为 selector → domain matcher map。selector 必须是非空 ASCII 标识，加载时统一转为小写；大小写归一化后重复的 selector 报错。`geosite:cn` 先解析大小写敏感的资源名 `geosite`，再查 canonical selector `cn`。selector 不存在时 prepare/refresh 失败，不返回空 matcher。

## 7. Matcher

编译索引：

- exact hash set；
- reversed-label suffix trie；
- wildcard suffix trie；
- 预编译 regex set；
- dat selector map。

匹配优先级由 Policy 固定。matcher 无内部 mutable cache，保证 snapshot 可跨线程共享。

## 8. 首次启动与 fallback

所有配置资源在 bind 前必须有有效 snapshot：

1. const/file 直接加载；
2. remote 先读取 content/manifest pair，并校验 manifest schema、资源身份、格式、parser 版本、字节长度和内容 hash；
3. 已校验的落盘 snapshot 可作为 fallback，并标记 `used_fallback`；
4. 没有可用 fallback 时再尝试本次远程获取；
5. 获取成功使用新内容；
6. 两者都不可用则 prepare 失败。

落盘 manifest 包含 hash、parser version、source fingerprint 和成功时间；版本不兼容时不使用。

## 9. 刷新与发布

每个资源 single-flight：

- scheduler 分配新 epoch；
- 下载/读取可并发；
- 编译成功后提交 `PublishResource(resource, epoch, snapshot)`；
- epoch 旧于当前时丢弃 stale result；
- Runtime coordinator 基于最新 ActiveRuntime 合并并 CAS；
- 成功发布只替换该资源；
- 不触发全局 cache clear。

失败退避指数增长并封顶 5 分钟。连续三次计划刷新失败或超过 `3 × update_interval` 无成功时标记 stale，但继续使用旧 snapshot。

当前已实现 `ResourceRefreshRuntime`：它为 registry 中的资源建立 schedule，将 due 检查与 `ResourceRefreshCoordinator` 的 per-resource single-flight、epoch reservation、CAS publish、failure backoff、cancel 和 shutdown 组合起来。该 facade 不执行网络、磁盘或 parser I/O；remote `ResourceRefreshWorker` 在 reservation 生命周期内调用 `ResourceFetcher`，完成 bounded fetch/parse/persist、epoch 重绑定和 CAS publish；file hosts/rule-set worker 复用同一 reservation 边界执行稳定读取、hash、解析和 CAS publish。生产 `ReqwestResourceFetcher` 在 prepare 边界装配 direct HTTP/HTTPS 与配置驱动 SOCKS5/SOCKS5H client，固定 `no_proxy`、禁止重定向、响应体上限、deadline/cancellation 和安全 URL 边界。async `PreparedRuntime` 首次构造会为 file 资源加载 typed snapshot，为 remote rule-set 恢复已校验的 content/manifest pair，恢复失败后才执行本次 fetch，并把 compiled snapshot 交给 Policy 初始索引；service 为三类 `auto_update=true` 资源注册受 Supervisor 持有的长期 task，按 due/backoff 触发 worker，并把成功候选提交到同一 ActiveRuntime 的 Policy CAS 和 Runtime 元数据 CAS。真正跨 Runtime owner 发布、独立 listener set 生命周期和长期压力验收仍由后续 Runtime 接入。

## 10. 原子落盘

remote 有效内容与 manifest：

1. 写同目录临时文件；
2. flush/fsync；
3. 写 manifest 临时文件；
4. 原子 rename 内容；
5. 原子 rename manifest；
6. fsync 目录；
7. 清理旧临时文件。

恢复时只有 resource id、format、parser version、byte length 和 content hash 均一致的 pair 才可使用；校验失败不会产生 fallback snapshot。

## 11. 安全与观测

- URL 只记录 scheme/host 的脱敏摘要，不记录 credential/query；
- 资源内容、域名清单和 regex 原文不进入普通日志；
- parser error 可记录行号、字段路径和短截断 token；
- remote body、压缩解码和 selector 数量均有上限；
- 不跟随非 HTTP(S) scheme，不把 file URL 当远程资源。

## 12. 测试

- hosts JSON/line、A/AAAA/CNAME、wildcard/exact；
- JSON/Clash/dat rule 和 selector；
- canonical domain、重复、冲突和 regex 限制；
- const/file/remote 首次加载；
- ETag/304、body limit、重定向、代理 failure；
- fallback manifest/hash/version；
- 单资源刷新失败保留旧版本；
- epoch 乱序、并发不同资源 CAS 不丢更新；
- 原子落盘中断恢复；
- 刷新不触发全局 cache invalidation。

## 13. 实现检查清单

- [x] 实现 hosts/rule parser；
- [x] 实现 canonical matcher/index；
- [x] 实现 const/file loader；
- [x] 实现资源版本 snapshot/CAS publish；
- [x] 实现 remote loader、snapshot manifest、原子落盘与 content/manifest 恢复校验；
- [x] 实现纯逻辑 refresh/single-flight scheduler/stale policy 与 Runtime-facing 编排边界；
- [x] 接入一次性 `ResourceRefreshWorker`，完成 remote fetch/parse/persist、epoch 绑定和 CAS publish；
- [x] 接入生产 `ReqwestResourceFetcher`，验证 direct HTTP、HTTPS TLS、SOCKS5H、body limit、取消和安全错误边界；
- [x] 在 async `PreparedRuntime` 中接入 remote rule-set 首次 restore-or-fetch、解析和原子持久化，并在 bind 前构造 compiled Policy snapshot；
- [x] 接入 Runtime supervisor 的长期 remote 调度和资源 I/O task；
- [x] 接入当前 ActiveRuntime 的 file/hosts 长期刷新、Policy live publish 和 Runtime 元数据 CAS；
- [x] 接入候选 Runtime 的兼容 snapshot/registry 合并和稳定 worker schedule 迁移，并与 revision CAS 绑定；
- [x] 完成当前解析、安全边界、文件稳定读取和并发 CAS 测试；
- [ ] 完成 remote 恢复、原子落盘和长期刷新测试。

阶段证据：hosts/rule focused tests、loader const/file/symlink/UTF-8/size tests、snapshot epoch/CAS tests、remote fetch/restore/mismatch tests 和 DNS/Policy 资源接线 tests 均通过；`resource::fetcher::tests` 7 项通过，覆盖 direct HTTP、HTTPS TLS handshake、SOCKS5H proxy、body limit、非 2xx、取消、未知 proxy、SecretRef 脱敏和 prepare 错误；reqwest 与项目 `ring` provider 的初始化顺序已统一，并通过 515 项后端并行全量测试；async PreparedRuntime restore/fetch 与 file snapshot 测试验证 bind 前资源准备，ResourceRefreshWorker focused tests 验证 remote/file worker 的 due/reservation、CAS publish、backoff、cancel 和 shutdown；service 已为 remote/file rule-set/hosts 注册长期 refresh task，成功候选经 Policy CAS 和 Runtime metadata CAS 发布；新增跨 Runtime 合并测试验证更高资源版本、compiled Policy、metadata 和 worker schedule 状态迁移，service 增量测试验证 reload 时 unchanged worker 复用与 removed worker 取消。独立 resource-only swap、完整配置候选生命周期和长期故障验收仍未完成。

当前实现进度：**90%**。
