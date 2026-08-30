# FluxDNS Rust 后端架构设计

> 状态：v1 实现基线
>
> 日期：2026-08-30
>
> 配置契约：[configuration-reference.md](configuration-reference.md)

## 1. 结论

v1 采用单进程、单 Rust binary、异步事件驱动架构：

- `tokio` 负责 runtime、socket、timer、任务监督和优雅停机；
- `hickory-proto` 只负责 DNS wire、RR、EDNS/ECS 编解码；
- UDP/TCP transport 自行实现，不把权威 DNS 的 `Catalog/Authority` 模型作为网关核心；
- DoH 入站使用 `axum` + `hyper`/`hyper-util`，TLS 使用 `rustls`；
- DoH 上游使用 `reqwest` + `rustls`，按上游和 outbound 组合复用 client；
- 内存缓存使用 `moka::future::Cache`，持久化缓存使用独立 SQLite 文件；
- 解析日志和聚合统计使用 `sqlx` + SQLite，并通过有界单 writer 与 DNS 数据面隔离；
- 规则和 hosts 编译成不可变 snapshot，使用 `arc-swap` 原子替换；
- WebUI 配置和 Web 模块边界保留，但 v1 不实现管理 UI、管理 API 或认证服务。

v1 不建立多 crate workspace。先使用一个 binary crate 和清晰模块边界，等出现可独立复用或需要独立发布的组件后再拆 crate。

## 2. 技术栈

| 领域 | 采用 | 主要职责 |
| --- | --- | --- |
| 异步 runtime | `tokio`、`tokio-util` | I/O、timer、`JoinSet`、`CancellationToken`、有界 channel |
| socket | `socket2` + Tokio socket | bind 前设置 IPv6 v6-only 等平台相关选项，再交给 Tokio |
| DNS wire | `hickory-proto` | `Message`、RR、EDNS、ECS、DNS 编解码 |
| HTTP/DoH server | `axum`、`hyper`、`hyper-util`、`tower` | DoH routing、GET/POST 校验、连接上下文和 middleware |
| TLS | `rustls`、`tokio-rustls`、`rustls-pemfile` | DoH 入站 TLS 和上游 HTTPS，避免 native TLS 差异 |
| DoH client | `reqwest` | HTTP/1.1、HTTP/2、连接池、SOCKS5/SOCKS5H、Host/SNI 保持 |
| PROXY protocol | `ppp` | 流式识别和解析 PROXY v1/v2；外层仍执行信任边界和长度限制 |
| YAML/config | `serde`、`yaml_serde`、`serde_path_to_error` | 严格反序列化、字段路径错误；所有对象拒绝未知字段 |
| 地址和值 | `url`、`ipnet`、`humantime` | URL、CIDR、duration 的强类型解析 |
| 内存缓存 | `moka` | 并发缓存、按 entry weight 限制总字节预算、逐条过期 |
| 数据库 | `sqlx` + SQLite | migration、解析明细、聚合统计、持久化缓存 |
| snapshot | `arc-swap` | 低频原子替换、高频无锁读取规则/hosts snapshot |
| 错误与日志 | `thiserror`、`anyhow`、`tracing` | 库层结构化错误、进程边界上下文、结构化事件 |

直接依赖只启用需要的 feature；`reqwest` 禁用默认 feature 并显式启用 Rustls、HTTP/2 和 SOCKS。`Cargo.lock` 纳入版本控制，首个实现阶段再固定精确版本。

## 3. 模块边界

```text
src/
├── main.rs                 # 进程入口、退出码
├── app.rs                  # 启动顺序、任务监督、优雅停机
├── config/
│   ├── model.rs            # 与 version: 1 对应的 serde DTO
│   ├── load.rs             # YAML、SecretRef、路径解析
│   └── validate.rs         # 引用、环、bind、条件字段和 feature gate
├── transport/
│   ├── udp.rs
│   ├── tcp.rs
│   ├── doh.rs
│   ├── tls.rs
│   └── proxy_protocol.rs
├── dns/
│   ├── message.rs          # canonical query/response
│   ├── context.rs          # QueryContext
│   └── handler.rs          # transport 无关的请求管线
├── policy/
│   ├── client.rs           # client_id/IP 匹配
│   ├── strategy.rs
│   └── route.rs
├── upstream/
│   ├── doh.rs
│   ├── group.rs
│   ├── bootstrap.rs
│   └── outbound.rs
├── cache/
│   ├── key.rs
│   ├── memory.rs
│   ├── persistence.rs
│   └── service.rs
├── resource/
│   ├── hosts.rs
│   ├── rules.rs
│   ├── loader.rs
│   └── snapshot.rs
├── storage/
│   ├── sqlite.rs
│   ├── resolve_log.rs
│   └── statistics.rs
└── observability.rs
```

不预先创建空的 `webui` 模块。v1 的 HTTP router 只挂载 DoH；未来实现 WebUI 时，在同一 transport 边界下增加独立 management router，不把管理逻辑混入 DNS handler。

## 4. 核心状态

```rust
struct AppState {
    config: Arc<ValidatedConfig>,
    resources: ArcSwap<ResourceSnapshot>,
    upstreams: Arc<UpstreamRegistry>,
    cache: Arc<CacheService>,
    resolve_log_tx: mpsc::Sender<ResolveLogEvent>,
    statistics: Arc<Statistics>,
    shutdown: CancellationToken,
}
```

`ValidatedConfig` 在 v1 启动后不可变；配置热加载尚未进入契约。`ResourceSnapshot` 可以定时刷新，但每个请求只持有一次 snapshot guard，保证同一请求不会混用两个 generation。

统一的请求上下文至少包含：

```rust
struct QueryContext {
    request: CanonicalQuery,
    transport: Transport,
    peer_addr: SocketAddr,
    client_addr: IpAddr,
    client_id: Option<String>,
    listener: ListenerId,
    route_strategy: StrategyId,
    resource_generation: u64,
}
```

## 5. 启动与停止顺序

启动阶段严格分为 prepare 和 serve，任何 prepare 失败都不绑定 listener：

1. 读取 YAML，按 `version` 反序列化，拒绝未知字段；
2. 解析 `work.path`、SecretRef、URL、CIDR、duration 和证书路径；
3. 执行引用、循环、条件字段、缓存阈值、WebUI feature gate 和 bind 计划校验；
4. 打开 SQLite、执行内置 migration，并验证可写性；
5. 构建远程资源首次下载所需的 outbound connector；
6. 加载并编译全部 hosts/rule resources，形成首个 `ResourceSnapshot`；
7. 构建完整的 bootstrap 和 DoH upstream client registry；
8. 初始化内存/持久化缓存、日志 writer 和统计；
9. 创建全部 socket；任何 bind 失败则关闭已创建 socket 并退出；
10. 同时启动 UDP、TCP、DoH、资源刷新、缓存持久化和日志 writer 任务。

停止时先停止接收新请求，再等待正在处理的请求到 grace deadline；随后 flush 有界日志/缓存队列、关闭 SQLite，最后退出。后台任务必须由 supervisor 持有，不能 detach 后丢失 panic 或错误。

## 6. 请求管线

```text
UDP / TCP / DoH
        │
transport 校验与 client IP 恢复
        │
DNS wire parse → QueryContext
        │
client matcher → effective strategy → rule/hosts/upstream
        │
选择唯一 cache namespace → lookup
        │ miss
upstream/group resolve → cache admission
        │
TTL/ECS/ID/UDP size response transform
        │
返回客户端 + 非阻塞 resolve log/statistics
```

UDP、TCP、DoH 只处理 framing 和传输限制；策略、缓存和上游逻辑只实现一次。TCP 使用两字节 DNS length framing。DoH 和 TCP 不受客户端 EDNS UDP payload size 限制；UDP 在发送时按当前请求重新编码并在必要时设置 TC。

## 7. 缓存实现

### 7.1 namespace 与容量

`CacheService` 只有一个共享的 Moka 实例，key 中携带 pool namespace，形成全局、策略、客户端+策略三类逻辑池。这样 `dns.cache.memory.max_size_bytes` 是整个进程的单一容量预算，不会随策略或客户端数量成倍扩张。

Moka 使用 `weigher` 计算 key、canonical wire、索引和元数据的计费字节；该值用于可预测淘汰，但不是操作系统 RSS 的硬限制。

### 7.2 entry 与过期

缓存 entry 不保存客户端 DNS ID、HTTP header 或本地 UDP 截断 envelope，保存 canonical response、写入时间、TTL metadata、response class 和 resource generation。上游 TC entry 额外携带入口 transport，只允许同 transport 命中。正向/负向 TTL、`failure_ttl` 和 optimistic max age 由 per-entry expiry 实现；compare-and-replace 禁止 SERVFAIL/TC 覆盖未过期的完整回答。

持久化缓存使用 `dns.cache.persistence.path` 指定的独立 SQLite 文件，与解析日志数据库分离。`max_size_bytes` 转换为主数据库 page budget；WAL/SHM 的短时额外占用不计入该值。写入通过有界队列批量提交；超出预算或持久化失败只降级为内存缓存，不让 DNS 请求失败。恢复时必须重新检查 expiry、schema version 和 resource generation。

## 8. `parallel` 上游组

每个并行成员作为受监督 task 运行，并向 aggregator 发送以下分类结果：

- terminal DNS response：wire 合法、响应标志合法、问题段匹配；
- transport failure：timeout、连接、TLS、HTTP、解析或问题段错误。

aggregator 收到第一个 terminal response 后立即完成客户端响应。缓存 finalizer 独立于客户端 responder：

- 首响应是完整 `NOERROR/TC=0` 时直接作为缓存候选，并取消其余成员任务；
- 首响应是 NXDOMAIN、REFUSED、SERVFAIL 或 TC 时，继续持有已发任务直到完成或 group timeout，并收集缓存候选；
- 窗口结束后先从完整 `NOERROR/TC=0` 中按成员配置顺序选择；没有时再从其他允许缓存的响应中按成员配置顺序选择；REFUSED 不缓存；
- late result 只能写缓存和观测日志，不改变已返回给本次客户端的响应；
- 只有主组完全没有 terminal response 才执行 fallback。

必须为“快速 SERVFAIL + 慢速 NOERROR”“快速 TC + 慢速完整响应”“快速 REFUSED + 慢速 NXDOMAIN”和全部 transport failure 编写确定性异步测试。

## 9. DoH 与连接接入层

DoH route 同时实现 GET/POST，并在进入 DNS core 前完成 method、URI、Content-Type、body/wire length 和 DNS parse 校验。有效 DNS 错误响应仍返回 HTTP 2xx；HTTP 错误只表示没有形成可处理的 DNS transaction。

HTTP access log 和 tracing 不记录 query string、raw DNS wire、完整 `client_id` 或 ECS；只记录 route template、方法、状态、wire 字节数和脱敏后的请求关联 ID。

接入层使用固定协议常量，不新增运行时配置：

```text
MAX_DNS_WIRE_BYTES       = 65_535
MAX_DOH_POST_BODY_BYTES  = 65_535
MAX_DOH_GET_DNS_CHARS    = 87_380
MAX_PROXY_V1_BYTES       = 107
MAX_PROXY_V2_BYTES       = 536
```

HTTP request-target 上限必须容纳路由路径、`?dns=` 和完整的 `MAX_DOH_GET_DNS_CHARS`，否则 GET 会在达到 DNS wire 上限前被框架提前拒绝。

DoH endpoint 的 accept pipeline 为：

```text
TCP accept
  → peer CIDR trust check
  → optional required PROXY v1/v2 parser
  → optional rustls handshake
  → hyper connection
  → axum route
```

其中 “optional” 由 endpoint 的 `client_ip.source`/`tls.mode` 决定；一旦选择 `proxy_protocol`，前导头就是 required。解析器只自动识别 v1/v2，不猜测裸 HTTP/TLS。未知 v2 TLV 在长度合法时跳过，未知协议版本拒绝。

## 10. DoH 上游与代理

每个 `(upstream, outbound, bootstrap/connect_ip)` 组合复用一个 `reqwest::Client`，避免每个 DNS 请求重建 TLS 和 HTTP/2 连接池。

- `connect_ip` 通过 host-to-address override 建连，URL host 继续用于 HTTP `Host` 和 TLS SNI；
- `bootstrap` 先通过已验证的上游解析地址，再构建/刷新该 client 的地址 override；
- `socks5://` 走本地解析，可使用 `connect_ip`、`bootstrap` 或系统解析器；
- `socks5h://` 在没有 `connect_ip` 时把主机名交给代理；若存在 `connect_ip`，connector 内部改用本地解析模式并注入该 IP，使 SOCKS 请求携带 IP，而 TLS 仍使用原始 URL host 作为 SNI；
- `bootstrap`/group/outbound 引用图在启动时检查循环，禁止解析路径自依赖。

上游响应必须验证 HTTP status、Content-Type、DNS ID/问题段和 wire 完整性。HTTP 非 2xx、无效媒体类型或 malformed DNS 都属于 transport failure，而不是可返回的 SERVFAIL。

## 11. 资源 snapshot

资源加载流水线固定为：

```text
读取/下载 → 原始格式解析 → 规范化 → 交叉校验 → 编译索引
          → 临时文件 fsync/rename → ArcSwap::store(new_snapshot)
```

首次启动必须为所有已配置资源产生有效 snapshot。远程下载失败时可使用上一轮已校验并落盘的 snapshot；两者都不可用则启动失败。运行中的刷新失败只记录错误并保留旧 snapshot。

domain exact、suffix、regex、CIDR 等匹配结构在加载时编译，查询热路径不读取文件、不解析 YAML/JSON/Clash/dat，也不持有更新锁。

## 12. SQLite 日志与统计

解析请求线程只执行两件事：更新内存聚合计数，并尝试 `try_send` 详细事件到有界 channel。不能等待 SQLite。

单 writer 按批次执行：

1. 删除超过 `max_record_age` 的详细记录；
2. 达到 `eviction_threshold_records` 时按最旧时间淘汰；
3. 为当前批次预留不超过 `max_records` 的容量；
4. 插入可容纳的详细记录并提交；
5. 超出的新记录计入 `dropped_detail_records`。

聚合统计使用固定维度和固定表结构，与详细记录分开提交；高基数字段只进入受上限约束的详细表。数据库 busy、磁盘满或 writer 队列满都只影响详细日志，不能改变 DNS 响应。

## 13. WebUI 预留

v1 保留 `webui` schema，目的是避免未来新增顶层配置时破坏结构，但运行时执行 feature gate：

- `enable: false`：继续启动，不创建 socket、router、session 或认证状态；
- `enable: true`：在 prepare 阶段返回明确的 unsupported-feature 错误；
- `address`、`port`、`users` 仍做类型、安全和未知字段校验；
- WebUI 不参与 v1 bind planner，也不会与 DoH 共享 router 或端口。

后续启用 WebUI 时，应先补齐 management API、认证/session、CSRF、TLS/bind、权限模型和历史统计保留契约，再解除 feature gate。

## 14. 验证基线

实现阶段至少覆盖：

- 配置 golden tests：未知字段、互斥字段、引用环、cache tri-state、日志阈值、WebUI feature gate；
- DNS wire tests：UDP/TCP/DoH 一致性、ID 重写、NODATA/NXDOMAIN/SERVFAIL/TC 缓存、ECS key；
- DoH interoperability：GET/POST、媒体类型、65,535 字节边界、HTTP/DNS 错误分层；
- PROXY tests：v1/v2、分片读取、未知 TLV、不可信 peer、缺失/非法 header；
- outbound tests：direct、bootstrap、connect_ip、SOCKS5、SOCKS5H、Host/SNI 保持；
- SQLite stress：软阈值淘汰、硬上限、queue overflow、busy/磁盘失败时 DNS 不受影响；
- snapshot tests：首次失败阻止 bind、刷新失败保留旧版本、generation 隔离缓存。

## 15. 推荐实现顺序

1. config model、严格校验、资源首次 snapshot；
2. canonical DNS core + UDP/TCP；
3. 单 DoH 上游、direct/bootstrap/connect_ip；
4. 三类缓存 namespace、TTL 和 Moka 容量；
5. strategy/client/rule routing；
6. group 与 `parallel` late-cache 语义；
7. DoH 入站、TLS、forwarded header、PROXY v1/v2；
8. SOCKS5/SOCKS5H outbound；
9. SQLite 持久缓存、解析日志和聚合统计；
10. 资源定时刷新、故障注入和压力验证。

不在 v1 顺手实现 DoT、WebUI、配置热加载、上游健康检查或远程资源版本锁定；这些能力应先形成独立配置契约。
