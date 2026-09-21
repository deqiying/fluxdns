# 解析记录缓存标签与身份列收口

> 文档状态：有效
>
> 计划状态：待验收
>
> 代码基线：`aa65fcb`；实施内容在本次未提交工作树
>
> 适用范围：`/queries` 解析记录的来源标签分类、身份列内容与位置、缓存状态筛选展示

## 问题与现状依据

诉求是把解析记录的来源标签按缓存结果重新划分——命中缓存（TTL 未失效）、乐观缓存（TTL 过期但符合乐观缓存配置）、缓存过期（TTL 过期且无乐观配置或乐观窗口已过）——把 `upstream` 标签改为“请求上游”，`Hosts` 标签保留但首字母大写；身份列主内容改为客户端名称与 IP 而非客户端 ID，并移到结果列与路由列之后。

按当前基线核查后，`04a0132 feat(cache): 区分缓存过期与请求上游`（随 v0.2.11 发布）已经落地：后端 `CacheLookup` 的 `Fresh`/`Stale`/`Expired`/`Miss` 对应 `cache=hit`/`stale`/`expired`/`miss`，前端 [`sourceLabel`](../../frontend/src/modules/queries/QueriesPage.tsx) 先按 `cache` 分类再回退 `source`，身份列已位于路由列之后且主文本为客户端名称。剩余差距落在四处展示细节：

| 现状 | 目标口径 |
| --- | --- |
| `sourceLabel` 对 `source=hosts` 返回 `hosts` | 保留原标签，首字母大写为 `Hosts` |
| 身份列次文本重复主文本的当前名称（`当时按 IP 匹配 X · 当前 Y`） | 名称已在主文本，次文本只保留当时的匹配结论 |
| 「缓存状态」高级筛选下拉显示裸枚举 `HIT/STALE/EXPIRED/MISS/BYPASS` | 与结果列标签一致，显示分类名称 |
| 路由列回退文案 `upstream 未确定` | 术语统一为 `上游未确定` |

## 目标与非目标

目标：解析记录页的来源标签、身份列文本、缓存状态筛选与路由回退文案与上述分类口径一致，且不改变请求契约。

非目标：后端 `cache` 判定与乐观缓存窗口（沿用既有实现与 [cache 设计](../architecture/backend/modules/cache.md)）；v2 API 枚举与生成类型；详情弹层中的原始客户端 ID 展示；来源筛选与缓存状态筛选的提交值。

## 相对基线的变更

| 位置 | 变更 |
| --- | --- |
| `frontend/src/modules/queries/QueriesPage.tsx` `sourceLabel` | `hosts` → `Hosts`，保持紫色标签 |
| 同上 `cacheLabels` 与 `FilterSelect` | 新增缓存状态显示名映射（命中缓存/乐观缓存/缓存过期/未命中/未启用）；`FilterSelect` 增加可选 `labels`，未提供时仍回退大写枚举 |
| 同上 `formatClientIdentity` | `detail` 去掉 `· 当前 {名称}` 后缀，只保留当时匹配结论 |
| 同上 `formatRoute` | 回退文案 `upstream 未确定` → `上游未确定` |
| `docs/implementation/frontend/pages.md` | 解析记录段落按新口径重写，核对基线刷新到本次基线 |

## 实施步骤

1. 调整 `sourceLabel`、`formatClientIdentity`、`formatRoute` 与缓存状态显示名映射。
2. 更新 `frontend/src/modules/queries/QueriesPage.test.tsx`：`Hosts` 期望、身份列 `detail` 精确断言、路由回退断言，并新增缓存状态筛选用例（断言下拉显示分类名称且提交值仍为 `cache=stale`）。
3. 同步 `docs/implementation/frontend/pages.md` 的解析记录段落与核对基线。
4. 执行前端定向与全量验证，再按退出条件做浏览器验收。
5. 验收通过后删除本方案与计划索引项；新逻辑已沉淀到 implementation，本次未改变后端设计，因此不联动 architecture。

## 风险

- 身份列去掉重复名称后，若后续要求主文本回退为历史匹配 ID 时仍突出当前名称，需要恢复该后缀：当前实现中 `current_client_name` 存在时主文本即为该名称，后缀恒为重复。
- `cacheLabels` 用 `Record` 覆盖全部枚举，后续新增缓存状态时会在编译期暴露缺失项；`FilterSelect` 的 `labels` 只影响展示，不影响提交值。
- 标签分类优先级是 `cache` 先于 `source`，因此“过期后回源”的记录显示「缓存过期」而不是「请求上游」；若口径改为按本次是否访问上游标注，需要调整优先级并同步文档。

## 验证与退出条件

已执行（2026-09-21，`frontend` 工作目录，基线 `aa65fcb` 加本次工作树）：

| 命令 | 结果 |
| --- | --- |
| `pnpm exec vitest run src/modules/queries` | 退出码 0，2 个测试文件 13 个用例通过 |
| `pnpm run test` | 退出码 0，24 个测试文件 102 个用例通过 |
| `pnpm run typecheck` | 退出码 0，无类型错误 |
| `pwsh -File .agents/skills/project-doc-maintenance/scripts/check-docs.ps1` | 退出码 0，42 个 Markdown、670 个链接/引用无问题（含本方案） |
| `git diff --check` | 退出码 0，无空白错误 |

未执行：浏览器内 `/queries` 的交互验收——本机没有运行中的前后端与真实解析数据，未做运行时确认。

退出条件：浏览器中确认 `cache=hit`/`stale`/`expired` 与 `source=hosts`/`upstream` 的记录分别显示「命中缓存」「乐观缓存」「缓存过期」「Hosts」「请求上游」；身份列位于结果列与路由列之后，并展示客户端名称与 IP；「缓存状态」下拉显示分类名称。验收通过后删除本方案与计划索引项。
