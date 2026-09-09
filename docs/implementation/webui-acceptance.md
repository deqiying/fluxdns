# WebUI 联合验收

> 文档状态：有效
>
> 适用范围：新版 WebUI 的 Windows 集成证据、复现入口与未验证边界
>
> 最后核对：2026-09-09（P5 旧路径退出、release、浏览器与联合回归）
>
> 核对基线：`f052545` 加本次联合验收脚本/夹具工作树；P0-P4 和 BC-26 为已完成基线

## 运行与复现入口

Windows 本机使用项目声明的 Rust 1.98.0、Node.js 26.8.1、pnpm 11.25.0；没有升级工具链、新增依赖、迁移旧数据或访问远程 DNS。新目录夹具来自 [webui-local.yaml](../../backend/tests/fixtures/webui-local.yaml)，所有配置、数据库、日志、凭据和报告留在 `_fluxdns/`。本轮实例使用 loopback WebUI 18085、UDP/TCP 15355、DoH 18086；DoH 为 external 模式的本地 HTTP 入口，不表示外部 HTTPS 代理已经验收。

命令从仓库根目录执行，前端命令从 `frontend/` 执行；凭据与夹具准备按[本地测试规范](../rules/local-testing.md#webui-真实-httpws-联合验收)。

| 入口 | 实际范围与结果 |
| --- | --- |
| `cargo test --manifest-path backend/Cargo.toml --locked -- --test-threads=1` | 790 passed，0 failed，4 ignored |
| `cargo test --manifest-path backend/Cargo.toml --locked --all-features -- --test-threads=1` | 792 passed，0 failed，4 ignored，包含内嵌资源测试 |
| `cargo clippy --manifest-path backend/Cargo.toml --all-targets --all-features -- -D warnings` | 通过；现有告警已处理，事务参数和仅显式运行的环境断言使用带原因的局部 `expect` |
| `cargo fmt --manifest-path backend/Cargo.toml -- --check` | 通过 |
| `pnpm run generate:api`、`typecheck`、`test`、`test:contract:v2`、`build` | 唯一 v2 生成类型；98 项 Vitest、4 项 schema contract、类型与构建通过 |
| `pwsh -File script/package-embedded.ps1` | 前端、默认 release、Windows target 内嵌 release 三阶段通过，deploy 文件与 target 文件 SHA-256 相同 |
| `pwsh -File script/test-backend-contracts.ps1 -Suite WebUI` | 真实 10 客户端、主链路计时、快照/回收/热更新并行通过，详见下节 |
| `node script/test-webui-http.mjs _fluxdns/<run>` | v2 Bearer/Origin/预算/只读边界、四种 DNS 请求、改名/冲突/历史、外改还原、指标和登出通过 |
| `pwsh -File script/test-webui-events.ps1 -WorkDirectory _fluxdns/<run>` | 真 HTTP ticket、UDP/SQLite/WS、断线 replay 与登出撤销通过 |
| 文档检查器与 `git diff --check` | 在文档收口后执行，检查 Markdown 结构、链接、编码及 diff 空白 |

4 个默认忽略项分别为新 Windows release 主链路验收、两个旧手工往返 profile、1024-session 专项；本轮显式执行新主链路验收，其余不属于约 10 客户端门槛。较早的 Management 并行测试进程曾以 Windows `0xc0000409` 异常退出；未将该轮认领为通过，最终采用上述完整串行结果，不从本次工作推断异常退出的根因。

## 主链路耗时

测点和可复现夹具由[后台服务实现](backend/background-services.md#windows-webui-主链路验收)维护，使用 `EventPublishingDnsCore::resolve` 的生产 `dns_core_duration_micros`，不是客户端往返或单次 cache lookup。报告所有请求及命中率，仅对命中集合施加 2ms 门槛；不丢弃超限样本，不用平均值或 P99 代替最大值要求。

| 样本 | 命中 | P50 | P95 | P99 | 最大 | >2ms |
| --- | --- | --- | --- | --- | --- | --- |
| 10 客户端 × 100 = 1000 | 990（99%） | 5μs | 37μs | 44μs | 56μs | 0 |

热更新建立新 core 后的 10 次冷 miss 仍计入总数。请求窗口内观察到快照成功写入、1 个真实受管旧日空分片物理回收和 Runtime revision 1→2；1000 条详情无漏写/重复，每个匹配客户端恰好 100 条。此处受控调用正式 retention owner 触发清理，01:00/补跑/时区/重试由同批真实 SQLite 加受控墙钟测试验证，不声称等待了整夜或经历真实 DST 切换。

## 联合场景矩阵

| 场景 | 当前证据与结果 | 边界 |
| --- | --- | --- |
| E2E-01 初始化/登录/导航/退出 | 新目录 release 初始化；12 个受保护入口；重启后旧会话进入登录；HTTP logout 同时拒绝旧 access 与 refresh；前端认证代次/迟到结果测试 | 浏览器会话清理与认证测试分别覆盖，不把本地状态清理当成服务端撤销 |
| E2E-02 依赖创建/改名 | 本轮重跑组合创建、前向引用、同时改名与真实 Runtime 应用用例；本轮 HTTP 将 local 改名后策略引用同步，实际 DoH 正常，再恢复原名 | 完整领域表单依赖链沿用已完成 P3 基线；不重新引入删除或任意 YAML 编辑 |
| E2E-03 并发/外改/持久化 | 双 revision 冲突、外改不自动 reload、差异读取、明确还原且 Runtime 不变；Windows journal crash、文件锁、缺失文件、覆盖确认/重试用例重跑 | 真实磁盘满和不可中断 OS I/O 未制造；注入失败不等于物理介质故障 |
| E2E-04 保存后 DNS 生效 | Hosts 保存后 UDP、TCP、DoH GET/POST 均返回新地址；非法候选被拒且旧地址继续有效 | DoH HTTP 入口为 loopback；外部 TLS 代理未测试 |
| E2E-05 身份/历史 | 10 个 DoH ID 优先匹配；未知 ID 回退 IP；UDP 无 ID；改客户端 name 后原记录匹配不变、当前名称更新；CIDR/只读 ID/唯一 name 的现有测试通过 | 不从旧记录推造新身份，不保留 legacy 分支 |
| E2E-06 缓存快照 | 最终 release 预热/落盘/受控重启后首次请求为 cache hit，原分片记录仍可读；损坏、预算缩小、TTL、失败保留旧快照、reload/关闭交错用例通过 | 热更新的冷 miss 与恢复命中分开记录 |
| E2E-07 保留/分片 | R/G/T 等号边界、跨日、迟到写入、watermark、lease、删除失败重试与重启补跑用例通过；10 客户端负载中确实回收分片 | 新目录没有历史清理时 `last_completed_at_ms=null` 是有效缺数；不制造完成时间 |
| E2E-08 实时/详情 | 最终 binary 的 snapshot→push→断线 replay cursor 为 1→2→3，新旧稳定 ID 不同；登出关闭码 4401；缓冲满、epoch/retention resync 和慢订阅者预算测试通过 | 慢读饱和为定向测试，未做长时间网络压测 |
| E2E-09 指标/进程 | 真实 10 身份在线；服务页与进程页读取同一 RSS；固定窗口、warmup、gap、精确 QPS/RPM、在线容量用例通过 | Linux procfs 与真实 OS 采样失败未实机验证 |
| E2E-10 安全 | Cookie-only 401、跨源 ticket 403、超限 Content-Length 413、只读字段注入 400、非法名称 422、旧 API JSON 404；WS Origin/单次 ticket/撤销测试；恶意 qname/Answer 只显示文本 | 大 body 上传期间服务端关闭连接可能表现为客户端 ECONNRESET；脚本用 headers-only 明确核验 413 |
| E2E-11 新基线/恢复 | 新日志父目录冷启修复；新统计库无单库详情表；当前 v2 schema 重开，旧库/旧配置明确拒绝；最终 binary 配置文件摘要不变，旧详情 ID 可读取 | active revision 是进程相关 opaque token，不要求跨重启相同；无数据迁移或清库 |
| E2E-12 页面/交付 | 最终内嵌包 12 路由 × 4 视口，无页面级横向溢出；10 类表单窄屏开闭；移动导航、键盘、焦点、稳定详情与生产无 mock；服务状态局部深色预览 | 快速切换出现 2 次受控 Busy 提示，重新读取后恢复；触摸采用组件 TouchA 和内嵌浏览器合成事件，原生触控限制见下节 |
| E2E-13 Windows 时延 | 上述 1000 请求、990 命中，最大 56μs，0 个超出 2ms；快照/清理/热更新重叠 | 只覆盖当前 Windows 短时低并发，不外推硬件、远程网络或 Linux 性能 |

## 浏览器与视觉范围

视口为 1600×1040、1280×800、768×1024、390×844。宽表仅在自身容器内滚动；表单使用内部滚动，窄屏按钮和可访问名称保留。真实 Escape 回归发现 Popover key 替换按钮导致焦点落在旧节点，已修复并在最终内嵌 release 复验。服务状态深色预览只改变本页，离开路由不保留，不扩展全站主题。

原 29 张评审图按以下映射进入正式组件；图中的同名客户端、需重启日志和数字页码不作为实现契约。图稿及临时评审 HTML 随完成计划退出，Git 保留历史。

| 原图组 / 数量 | 正式去向 |
| --- | --- |
| 服务状态浅/深 / 2 | DashboardPage、局部深色预览与 MetricsTrendChart |
| 记录列表/结果详情/IP 详情 / 3 | QueriesPage 与同一稳定 ID 的详情，展示原始、历史和当前身份 |
| Listener 列表/通用编辑/DoH 编辑 / 3 | ListenersPage 的类型分支与 routes/endpoints 表单 |
| 上游列表/DoH 编辑/上游组列表/组编辑 / 4 | UpstreamsPage 的两个 tab 与类型化编辑 |
| DNS 概览/cache/TTL-ECS/保留 / 4 | DnsSettingsPage 的 DNS 分区与独立保留 preview 表单 |
| 策略列表/编辑 / 2 | StrategiesPage 的有序规则与默认上游 |
| Hosts 列表/编辑 / 2 | HostsPage 的 const/file 分支 |
| 规则集列表/编辑 / 2 | RuleSetsPage 的 const/file/remote 分支 |
| 客户端列表/编辑 / 2 | ClientsPage 的唯一 name、固定 ID、IP/CIDR 与覆盖 |
| 代理列表/编辑 / 2 | ProxiesPage 的 SecretRef 来源表单 |
| 系统配置/日志编辑 / 2 | SystemSettingsPage 的只读路径与日志热应用 |
| 系统运行状态 / 1 | SystemPage 的唯一 v2 进程快照 |

浏览器 Network 核对业务与 ticket 请求使用 Bearer；WS URL 无 query，ticket 在 subprotocol 中，101 只返回 `fluxdns.v1`。浏览器可能自动在 WS handshake 携带刷新 Cookie，服务端不把它用于 WS 鉴权。localStorage/sessionStorage 为空，脚本不可读 HttpOnly 刷新 Cookie，生产没有 MSW service worker 接管；凭据和 Secret 实际值未写入报告。

内嵌浏览器支持触摸环境模拟（`maxTouchPoints=1`、coarse pointer）及合成 TouchEvent/兼容 click，已验证详情开启/关闭；组件 TouchA 验证同一交互、恶意文本和 Escape 焦点。原生 `Input.dispatchTouchEvent` 在内嵌浏览器不受支持，另开的 Chrome 验收页被本机其他扩展 UI 阻断，因此未认领原生触控或物理触屏验收，没有禁用用户扩展或改其设置。Linux/macOS、外部 HTTPS 代理、物理设备及真实磁盘介质故障均不在本轮实测结论内。

## 本地证据与 Git 边界

原始报告保留在 `_fluxdns/contract-validation/20260909T034945513Z-56004/`、`_fluxdns/p5-live-20260909/` 及 `_fluxdns/p5-*.log`；运行脚本输出实际报告路径。HTTP、WebSocket、重启和 core 报告不包含凭据。文档只保存必要结论，不提交个人配置、数据库、日志或浏览器会话。

最终 HTTP 报告为 `http-report-1788929601743.json`；同目录的最新 `websocket-report-*.json`、`restart-before.json`、`restart-after.json` 和 `assets-report.json` 分别记录真实 replay/撤销、恢复及交付边界。最终静态资源校验覆盖 SPA 深链接 200、未知 API JSON 404、CSP/nosniff、immutable asset 和 ETag 304，日志没有测试密码或认证 hash。

开始时 main 工作树和 index 干净，本地 HEAD 与本地 origin/main、origin/HEAD 同为 `48c81b2`。远端实时引用核验经沙盒外重试仍报 `git@github.com: Permission denied (publickey)`；所以不能把本地缓存的远端引用当成当前远端事实。本轮仅按模块创建本地中文 Conventional Commit，没有 fetch、push、tag 或外部发布。
