# Web UI 设计与实现方案

> 输入四方:设计稿 handoff(信息架构、交互模式、视觉意象)、既有管理台的功能基准(本 UI 必须覆盖其全部能力)、经生产验证的前端工程模式(token 流水线、contract-first codegen、事件日志 reducer、导航 SSOT),以及本仓库的真实 API 面(决定各页面今天能接什么、什么排入后端补齐路线)。

## 0. 设计论点(一句话)

采用**「Workspace ▸ Project 两级作用域 + 全局 cockpit(Home/Inbox)」外壳**;功能范围 = **既有管理台全集 ∪ 本仓库管理面**(Session/Project 是超出既有管理台的部分,Eval/Audit/观测/目录/沙箱/助手是超出本分支现状、必须纳入并随后端补齐逐步点亮的部分)。没有对应后端概念的功能一律不发明;后端未备的页面**照常设计、以能力门控置灰**,不因后端进度裁剪 IA。

## 1. 关键设计与工程决策

**交互模式(采纳)**:两级作用域侧栏(聚焦式 scope 切换)、Home/Inbox 全局层、statChip 全可点、needs-you hero 行内 Approve、live banner 批量刷新、agent 紫=AI 活动、readiness(Bind & ready)清单、resolved binding 链式 chips、record-before-effect 日志表、编辑器外壳(顶栏版本+Publish / 主区 / 右 rail)、Sandbox 沙箱预览(流式 + 行内审批 + 附件)、assistant 草稿手递编辑器、capabilities 门控、secret 全链路脱敏、URL 同步列表状态、unsaved guard、token modal、⌘K。

**工程模式(采纳)**:W3C token 三层流水线 + token-contract 测试;OpenAPI codegen `*:check` 门禁;单出口 fetch + no-raw-fetch 检查;`paths.ts` 导航 SSOT(router/侧栏/palette 共用);append-only 事件日志 reducer(单 query cache 条目,transcript 是纯投影);session-as-query 认证;roster sentinel;structure tests;react-router v7 薄路由包装 Surface 组件。

**明确规避**:巨石 global.css(按 surface 拆文件)、超大单文件 Surface(限行数)、未使用的图编辑库依赖、事件 payload 类型逃逸(我们的事件是封闭 tagged union,投影全程强类型)、仅 jsdom 冒充 e2e(P2 起补真浏览器 SSE 用例);不引 Tailwind/Redux/RJSF。

**领域映射(关键决定)**:

| 外壳概念 | 我们的实体 |
|---|---|
| Workspace(共享供给) | `workspace_id`:catalog、credentials+pools、MCP、A2A、inference profiles、IAM tokens、agents、skills/tools 目录 |
| Project(运行面容器) | `Project` + `/projects/{id}` ingress;sessions、vaults、per-project agent MCP 绑定 |
| 工作单元 | **Session**(idle/running/requires_action) |
| Inbox 审批 | `requires_action` tool confirmation、durable deliver、dead-letters、凭证失效 |
| 编辑器 Publish | Agent config draft → validate → publish(fingerprint) |
| 自动化触发日志 | Durable dispatch 队列 + dead-letters(record-before-effect) |
| Resolved binding 链 | 三个 dry-run resolve 端点直接供数 |
| Bind & ready | fail-closed resolve |
| Observe 组 | Dashboard / Audit / Datasets / Evals(workspace 观测分组) |

## 2. 信息架构与导航外壳

workspace 段较长,用 10px 大写分组标题分成 **Supply / Observe / Govern** 三小组:

```
┌ Sidebar 236px ─────────────────────────┐┌ Main ─────────────────────────────┐
│ [A] <workspace>       Workspace ▾      ││ 46px 面包屑 + dawn glow            │
│  └─ 📁 <project>          PROJECT ▾    ││                                    │
│ [ Search…                        ⌘K ]  ││                                    │
│ GLOBAL · ALL PROJECTS                  ││                                    │
│  Home        Inbox ❸(紫)              ││                                    │
│ ── 按 scope 二选一 ──                   ││                                    │
│ PROJECT · <id>     │ WORKSPACE · SUPPLY ││                                   │
│  Overview          │  Agents ❹(紫)     ││                                   │
│  Sessions          │  Skills(ro) Tools  ││                                   │
│  Vaults            │  Models Credentials││                                   │
│  Agents(绑定)      │  MCP  A2A          ││                                   │
│  Settings          │ · OBSERVE          ││                                   │
│                    │  Dashboard         ││                                   │
│                    │  Evals  Datasets   ││                                   │
│                    │  Audit log         ││                                   │
│                    │ · GOVERN           ││                                   │
│                    │  Access  Settings  ││                                   │
│ footer: EN/中 · ☀/☾ · 头像   [✦ 助手 FAB 右下角常驻]                          │
└────────────────────────────────────────┘└───────────────────────────────────┘
```

- `paths.ts` 导航 SSOT;子项 expand-on-select;scope 持久化;Settings 统一页双段 rail + scope banner。
- **Admin assistant** 不占导航:右下角 FAB 浮窗,⌘K 可唤起;助手产出的 agent 草稿手递到编辑器。
- 未就绪的功能面(后端 404/registry_unavailable)不隐藏导航项:显示能力门控占位页(说明缺什么、链接到配置/路线),保持 IA 稳定。

## 3. 路由与页面清单(标注端点与就绪度)

**就绪度**:✅ 本分支已有端点;🔶 部分(可先上,能力门控残缺项);⛔ 待后端(页面照常设计、置灰)。

### Global

| 路由 | 内容 | 端点 / 就绪度 |
|---|---|---|
| `/` Home | KPI(Projects/Active sessions/Needs you)+ Needs-you 行内 Approve + Projects 表 + 容量/健康摘要 hero | 🔶 projects ✅;会话聚合与 runs summary ⛔ |
| `/inbox` | Approvals(requires_action 行内 Allow/Deny)/ Attention(dead-letters、凭证失效、MCP 不健康) | 🔶 |
| FAB 助手 | 流式对话、工具卡、生成 agent 草稿→跳编辑器;设置(模型/策略 prompt) | ⛔ `/v1/admin/assistant/*` |

### Project scope(运行面容器 = Managed Agents 资源全体)

| 路由 | 内容 | 端点 / 就绪度 |
|---|---|---|
| `/p/:pid/overview` | 项目脉搏:活跃会话、最近 dispatch、绑定健康(resolve chips) | 🔶(会话列表⛔) |
| `/p/:pid/sessions` | 会话列表 + New session 拆分按钮 | 🔶(列表端点⛔;创建 ✅ `/projects/:pid/v1/sessions`) |
| `/p/:pid/sessions/:sid` | **§4 核心界面**:转录 + HITL + outcomes + durable 抽屉 | ✅ |
| `/p/:pid/vaults` | 运行凭证容器(managed wire 资源,随 session 消费):vault 及其三型 credential、`mcp_oauth_validate`;标注 host-ephemeral | 🔶 vault 面 ✅,挂到 `/projects/{pid}` ingress ⛔(见 §7) |
| `/p/:pid/agents` | per-project MCP 绑定 + resolve 预览(reference-don't-copy) | ✅ |
| `/p/:pid/settings` | 项目信息、ingress baseURL、权限说明(project 容器统一 `ScopeRef::Project` 授权) | ✅(guard ⛔) |

### Workspace · Supply

| 路由 | 内容 | 端点 / 就绪度 |
|---|---|---|
| `/agents` | 列表(model、能力徽章、来源徽章、运行统计列);删除 | 🔶 list/meta ⛔,publish 面 ✅ |
| `/agents/:id`(编辑器) | 全 tab 集:**Basics / Tools / Skills / Plugins(JSON-Schema 配置表单)/ Delegates / Permissions(模式编辑 + 预览)/ Advanced(原始 JSON,CodeMirror)/ History(审计历史 + restore)**;顶栏 draft→Validate→**Publish**(fingerprint);diff 弹窗、unsaved guard、readiness 清单;右侧 **Sandbox**(§4b) | 🔶 validate/publish ✅;meta/history/restore、permission-preview ⛔ |
| `/agents/:id/dashboard` | per-agent 运行看板:推理延迟分布、生命周期事件、工具调用延迟 | ⛔ runtime-stats |
| `/skills` + `/skills/:id` | 技能目录(ro):context、user/model-invocable、allowed_tools、arguments | ⛔ capability catalog(plugin-configuration.md 已规划) |
| `/tools` + `/tools/:id` | 工具目录 + 工具编辑器(overrides) | ⛔ 同上 |
| `/models` | Catalog 三层(Provider/Endpoint/Offering)+ Inference profiles + resolve 试算链 chips + Test model(经 credential validate) | ✅ |
| `/credentials` | **供给侧凭证**:Sources(enter/validate/archive)、Pools(ordinal failover)。Vault 属运行面,页面在 Project scope | ✅ |
| `/mcp-servers` + `/:id` | 定义 CRUD + Bound-by 反查;状态/健康点 + Restart;nav 健康点 | 🔶 CRUD ✅;status/restart ⛔(ExtMcpProbe 可扩) |
| `/a2a-servers` | 远程委托服务器目录:delegate card 查看;CRUD | 🔶 card ✅(`/v1/delegates/:id/card`);CRUD ⛔ |

### Workspace · Observe

| 路由 | 内容 | 端点 / 就绪度 |
|---|---|---|
| `/dashboard` | 运维看板:能力/健康摘要、负载 hero、活动 4 格、近期 audit 事件、时间范围切换 | ⛔ capabilities/system-info/runs-summary |
| `/audit-log` | 审计表,`?resource=` 过滤(各资源页「查看审计」入口跳这) | ⛔ |
| `/datasets` + `/:id` | Eval 数据集 CRUD;items/fixtures/expectations | ⛔ eval 面 |
| `/eval-runs` + `/:id` | 评测运行列表(按 dataset 过滤)、baseline 对比 | ⛔ |
| `/eval-reports` | 回放/评测报告 + Trace 详情抽屉(延迟/tokens/成本/scorer 分组/工具用量)+ 存为 fixture | ⛔ |

### Workspace · Govern

| 路由 | 内容 | 端点 / 就绪度 |
|---|---|---|
| `/access` | IAM tokens mint(明文一次)/list/revoke + 角色×动作矩阵 | ✅ |
| `/settings` | 双段 rail 汇总 + workspace 身份 | ✅ |

**不做**(无对应后端概念的外壳虚构域):Cycles/Insights/Analytics、Automation 规则 CRUD、Workers 舰队页(durable ops 收进会话抽屉;观测由 Dashboard 承担)。

## 4. 核心界面

### 4a. Session 详情

数据层:committed 事件 + SSE replay 进**单 query cache 条目**,纯 reducer 去重排序,transcript 是纯投影;事件是封闭 tagged union,全程强类型。

| 事件 | 渲染 |
|---|---|
| `agent.message` | 左气泡,渲染 ContentBlock 数组(text、image 内联) |
| `agent.tool_use` / `tool_result` | 折叠 tool 卡(状态 pill、Input/Output/Error 面板、`evaluated_permission` 标注) |
| `agent.custom_tool_use` | 标 client-executed,结果回填表单 |
| `session.status_running` | 紫色脉冲「Agent working」 |
| `session.status_idle` | `requires_action` → 行内审批卡(Allow/Deny+deny_message);`retries_exhausted` 红条 |
| `span.outcome_evaluation_*` | Outcome 迭代时间线 |

Composer:`user.message` + per-turn model 下拉;interrupt/pause/resume;define_outcome 表单。右 rail:能力广告卡、属性、**Durable 抽屉**(dispatches 账本、submit_background/supersede/cancel/wake/deliver、dead-letters、reconcile/reap)。SSE 为 replay → live banner「N new updates · Refresh」+ 分级轮询(活跃 5s/列表 15s/后台 30s)。所有 payload 过 secret 脱敏管道。

### 4b. Agent Sandbox

编辑器右侧常驻沙箱面板,对**当前草稿**试跑:
- 通道:✅ `POST /v1/ai-sdk/agents/:id/runs`(AI SDK UI Message Stream,`@ai-sdk/react` useChat 直连);对**未发布草稿**试跑需 ⛔ preview 端点。
- 渲染:Readable/JSON 双视图、reasoning 折叠、文件附件(≤4 个/8MB)、统计条(消息数/工具调用/上轮延迟)、Recent runs 抽屉、Reset 新会话。
- **行内审批**:tool 卡 `approval-requested` → Allow/Deny → 自动续跑;PermissionConfirm 特判显示目标工具名。
- 全部 i/o 走脱敏管道。

### 4c. Agent 编辑器 + 助手手递

编辑器骨架 + 我们的 publish 语义:tab 集见 §3;顶栏 Validate→Publish(产出 fingerprint/publication_id);Plugins tab 用 capability catalog 下发的 JSON Schema 自动生成配置表单(plugin-configuration.md 的既定设计);History tab 接审计历史 + restore。FAB 助手生成的 AgentSpec 草稿推入编辑器并跳转,`needsApproval` 标记待确认。

## 5. 设计系统

基调:**无色中性 chrome + amber 强调(oklch 78)+ agent 紫(hue 300/270)+ 红 spark**;Inter 正文 + JetBrains Mono 只管 id/数据/代码;基准 14px、radii 4/6/8/10/pill、hairline ring 阴影、dawn glow 低调保留;动效仅 iaPulse/iaBlink/180ms 浮层。色彩纪律:amber=需要人、紫=agent 活动、红=阻塞、绿=通过/verified、蓝=角色/引用;chrome 不带情绪色。

Token 工程:`design-tokens/*.tokens.json`(W3C)→ build 脚本 → 三层 CSS 变量(primitives → `--brand-*` 语义 → 短别名/`--aw-*`)+ manifest + token-contract 测试;`data-theme` 明暗持久化。品牌切换只动 4 个 themed 旋钮(accent/默认明暗/密度)。

形态配方:卡片 r10 + `0 0 0 1px var(--edge), 0 1px 1px rgba(26,29,30,.03)`;导航项 h32 r6 active=card 底+ring;pill r9999 11/600;拆分按钮;statChip 全可点;dropdown 手机端 bottom-sheet;micro-label 10-11/700 大写;KPI `tabular-nums`。

## 6. 工程蓝图

目录(`web/`):routes.tsx + `routes/*.tsx` 薄包装 → `surfaces/<domain>/`(每 surface 自带 css,禁全局巨石);`lib/navigation/paths.ts` SSOT;`lib/api/client.ts` 唯一 fetch 出口(bearer 注入 + `ApiClientError{status,request_id}` + no-raw-fetch 检查);`lib/query/use-*`;`lib/query/session-event-log.ts` reducer;`lib/security/redact.ts`;`components/{app,chat,ui}`。

- 栈:React 19、Vite、TS strict、react-router v7、TanStack Query v5、轻量 i18n(en/zh-CN)、CodeMirror 6(agent spec)、`@ai-sdk/react`(沙箱/助手流)、Vitest+TL。
- **Contract-first(已落地)**:`contracts/openapi.generated.json` 由 `awaken-admin-config-api::export_openapi` 生成(operation 注册表 + schemars SSOT;`generate-contracts.sh --check` 门禁;`openapi_contract` 测试锁路由挂载)→ 前端 `openapi-typescript` codegen + dev-only 响应校验。新后端面(eval/audit/观测)落地时同步扩注册表。
- 能力门控:按端点探活(404→route_absent 置灰页,503→registry_unavailable);认证 session-as-query;项目 roster sentinel;structure tests + token-contract 测试;P2 起真浏览器 SSE 用例。

## 7. 后端缺口(按优先级)

**P2 前置(会话面)**:
1. `GET /v1/sessions`(列表 + status/project 过滤)——Home/Inbox/会话列表的硬前置。
2. needs-attention 聚合(或列表过滤);session title/archive 更新端点。

**功能对齐(Observe/目录/沙箱/助手),按依赖排序**:
3. ~~OpenAPI 契约~~ **已落地**(`contracts/openapi.generated.json`,19 路径/28 操作;IAM/durable 注册表扩展是后续增量)。
4. `GET /v1/capabilities`(能力目录:skills/tools/插件 schema)——Skills/Tools 页 + Plugins 配置表单 + 全局门控的共同前提。
5. Agent 管理读面:list/meta/history/restore、permission-preview——编辑器 History/Permissions tab。
6. 运行观测:runs summary、per-agent runtime-stats——Dashboard + agent 看板。
7. `GET /v1/audit-log`——审计页。
8. Eval 面:datasets/eval-runs/reports CRUD(observability-eval-dataset-boundary.md 已界定)。
9. MCP status/restart、A2A servers CRUD、agent-preview 端点(对草稿沙箱)、admin assistant 面。

**project 容器统一权限(已定的架构决定)**:
10. vault router 挂进 `/projects/{pid}` ingress(今天 ingress 只转发 session router),vault 创建 stamp `ProjectScope` → vault 归属 project;
11. managed 运行面纳入 IAM guard:按路径 project 段做 `ScopeRef::Project{workspace_id, project_id}` 校验(词汇已在 awaken-iam-contract),动作词汇为 sessions/vaults 扩展;裸 `/v1/sessions` 保留 stock-SDK 兼容(project-bound key 或默认 project)。
12. **managed API key 绑定 project 层级(已定)**:runtime key 与平台/管理 key 同一 IAM 机制、同一 wire(`x-api-key` `sk-ant-…`),差别只在绑定 scope 与动作集——管理 key 绑 Workspace/Global + `workspace.*`/`apikey.*`;**runtime key 绑 `ScopeRef::Project` + 仅 run-plane 动作(`run.read`/`run.write`,覆盖 sessions/events/vaults)**。授权规则:经 `/projects/{pid}` 进入时 key 的 scope 必须覆盖该 project(workspace key 经 scope 图祖先关系覆盖其下所有 project);裸 `/v1/sessions` 时 project-bound key 隐含其绑定的 project(寻址从 key 推出,stock-SDK 零改动)。治理仍在 workspace IAM(key 注册表不搬家,project 只是绑定 scope——reference-don't-copy);Console 上 Project ▸ Settings 增设「API keys」段铸造/吊销本项目 runtime key(明文一次),Workspace ▸ Access 继续管管理 key。对齐 Anthropic:其 Console API key 即绑定在 workspace(运行容器)而非组织——我们的 project 正是该运行容器的对应物。

**非阻塞**:SSE live push(live banner + 轮询先行);workspace 枚举(P1 单 workspace)。

## 8. 分期

- **P1 地基 + 管理面(零后端改动)**:token 流水线、外壳(paths/⌘K/明暗/i18n/门控框架)、单出口 fetch + codegen;Models、Credentials、MCP、A2A(card 只读)、Access、Agents(publish 面)、Vaults、Sessions(创建 + 详情)+ Settings;Sandbox 对已发布 agent 先行(ai-sdk 端点已备)。
- **P2 会话面完善**:事件 reducer 强化 + live banner;Inbox;真浏览器 SSE 用例。(前置:缺口 1–2)
- **P3 项目面完善**:roster sentinel、guard 落地后的权限 UI。
- **P4 Observe/目录对齐**:capabilities 目录(Skills/Tools/Plugins 表单)→ Dashboard/audit → agent 看板 → eval 三页 → 助手 FAB;各页随后端端点落地逐个点亮(门控页先行占位)。

## 9. 线框图

约定:Project 容器**完全参照 Anthropic Console 的设计语言**(单行表格 + mono id + 状态点、右上主按钮、右侧抽屉、密钥只显示一次);Workspace 采用**双段 rail 的统一设置设计**(每段 hint 文案、scope 色调 banner)。amber=需要人,紫=agent 活动。

### 9a. Project · Sessions 列表

```text
┌ Acme Inc / acme-web / Sessions ──────────────────────────────┬───────────────┐
│                                                              │ [+ New session]│
├──────────────────────────────────────────────────────────────┴───────────────┤
│ (All) (Running ●) (Needs you ⚠2) (Idle)        [ Search sesn_…        ] [⟳]  │
├───────────────────────────────────────────────────────────────────────────────┤
│  STATUS      SESSION            TITLE                AGENT        UPDATED     │
│  ● running   sesn_01hx…f2a4     Fix flaky auth test  builder@3    2m ago    ▸ │
│▐ ⚠ needs you sesn_01hx…9c1b     Migrate settings     reviewer@1   18m ago   ▸ │  ← amber 行内标注
│  ○ idle      sesn_01hx…77d0     Draft ADR summary    docs@1       1h ago    ▸ │     tool: bash 待确认
│  ○ idle      sesn_01hw…be32     (untitled)           builder@3    3d ago    ▸ │
├───────────────────────────────────────────────────────────────────────────────┤
│  4 sessions · baseURL  …/projects/acme-web   [copy]                           │
└───────────────────────────────────────────────────────────────────────────────┘
```

### 9b. Project · Session 详情(转录 = 事件日志纯投影)

```text
┌ ‹ Sessions   sesn_01hx…9c1b · Migrate settings          ⏸ pause  ⏹ interrupt ┐
├──────────────────────────────────────────────────┬────────────────────────────┤
│  ▲ 2 new updates · Refresh          (live banner)│ AGENT                      │
│ ┌──────────────────────────────────────────────┐ │  reviewer@1 · claude-opus-4│
│ │ you                                          │ │  toolset: bash✓ edit✓     │
│ │   Migrate the settings schema to v2          │ │  read✓ write(ask) …       │
│ └──────────────────────────────────────────────┘ │  skills: adr-writer        │
│ ┌──────────────────────────────────────────────┐ │  delegates: sec_reviewer   │
│ │ ⬡ agent          ⣿ working (紫脉冲)          │ ├────────────────────────────┤
│ │  I'll start by inspecting the current schema.│ │ PROPERTIES                 │
│ │ ▸ 🛠 read · settings/schema.json      done ✓ │ │  created   2026-07-05      │
│ │ ▸ 🛠 bash · npm run migrate       ⚠ 待确认   │ │  env       env_node20      │
│ │ ┌──────────────────────────────────────────┐ │ │  vaults    vlt_9f…(1)      │
│ │ │ ⚠ Approve `bash` execution?              │ │ │  metadata  {…}             │
│ │ │   npm run migrate --workspace…           │ │ ├────────────────────────────┤
│ │ │   evaluated_permission: ask              │ │ │ OUTCOME                    │
│ │ │        [ Deny + note… ]  [ ✓ Allow ]     │ │ │  iteration 1 · revising    │
│ │ └──────────────────────────────────────────┘ │ ├────────────────────────────┤
│ └──────────────────────────────────────────────┘ │ ▸ DURABLE OPS              │
│                                                  │   dispatches 2 · dlq 0     │
│ ┌──────────────────────────────────────────────┐ │   [background] [supersede] │
│ │ Message…                [model: opus-4 ▾] ➤ │ │   [reconcile]  [reap]      │
│ └──────────────────────────────────────────────┘ │                            │
└──────────────────────────────────────────────────┴────────────────────────────┘
```

### 9c. Project · New session

```text
┌ New session · acme-web ───────────────────────────────────────────┐
│ Agent        [ builder@3 · claude-sonnet-4.5              ▾ ]     │
│ Model        [ inherit from agent                          ▾ ]     │
│ Environment  [ env_node20                                  ▾ ]     │
│ Vaults       [x] vlt_9f…  runtime-mcp   [ ] vlt_02…  scratch      │
│ MCP servers  ┌───────────────────────────────────────────────┐    │
│  (inline)    │ name  docs-search   url  https://mcp.acme…    │ ✕  │
│              └───────────────────────────────────────────────┘    │
│              [+ add inline server]   ⓘ project 绑定的 MCP 自动并入 │
│ Title        [ Migrate settings schema                       ]    │
│                                        [ Cancel ] [ Create ➤ ]    │
└───────────────────────────────────────────────────────────────────┘
```

### 9d. Project · Vaults(密钥只显示一次;host-ephemeral 提示)

```text
┌ Acme Inc / acme-web / Vaults ────────────────────────────┬───────────────────┐
│ ⓘ Vault 视图随进程重建(host-ephemeral)                  │   [+ Create vault] │
├──────────────────────────────────────────────────────────┴───────────────────┤
│ ▾ vlt_9f3a…c210   runtime-mcp                    2 credentials       [Delete] │
│    ┌─────────────────────────────────────────────────────────────────────────┐
│    │ crd_11ab…  mcp_oauth        token_endpoint https://… ● valid  [Validate]│
│    │ crd_58cd…  static_bearer    ●●●●●●●● (write-only)             [—]       │
│    └─────────────────────────────────────────────────────────────[+ Add]────┘
│ ▸ vlt_02be…77   scratch                          0 credentials       [Delete] │
├───────────────────────────────────────────────────────────────────────────────┤
│  Add credential (drawer) ──────────────────────────────────────────────       │
│   Type   (env-var) (static_bearer) (mcp_oauth●)                               │
│   token_endpoint [https://…]  client_id [......]  auth [none ▾]  scope [ ]    │
│   refresh_token  [●●●●●●●●●●]   ⓘ 密封后不再回显                [ Save ]      │
└───────────────────────────────────────────────────────────────────────────────┘
```

### 9e. Workspace · Settings 统一页(双段 rail)

```text
┌ Acme Inc / Settings ──────────────────────────────────────────────────────────┐
│ ┌─ rail 216px ──────────┐  ┌──────────────────────────────────────────────┐  │
│ │ PROJECT · ACME-WEB    │  │ ⚑ Scoped to workspace · Acme Inc             │  │
│ │   General             │  │   Shared across every project.    (banner)   │  │
│ │   Agent MCP bindings  │  ├──────────────────────────────────────────────┤  │
│ │   Vaults ↗            │  │ AI providers                                 │  │
│ │ WORKSPACE · ACME INC  │  │ hint: Providers, endpoints, offerings and    │  │
│ │   Identity            │  │ profiles — inference routing shared across   │  │
│ │ ▸ AI providers        │  │ every project.                               │  │
│ │   Credentials         │  │ ┌──────────────────────────────────────────┐ │  │
│ │   MCP / A2A servers   │  │ │ PROVIDER   ENDPOINT        MODELS  STATUS│ │  │
│ │   Access              │  │ │ anthropic  api.anthropic…  5   ● verified│ │  │
│ │                       │  │ │ openai     api.openai…     4   ● verified│ │  │
│ │                       │  │ └──────────────────────────────────────────┘ │  │
│ └───────────────────────┘  └──────────────────────────────────────────────┘  │
└───────────────────────────────────────────────────────────────────────────────┘
```

### 9f. Workspace · Credentials(供给侧;resolve 链是签名交互)

```text
┌ Acme Inc / Credentials ──────────────────────────────────┬────────────────────┐
│                                                          │ [+ Enter credential]│
├──────────────────────────────────────────────────────────┴────────────────────┤
│ SOURCES                                       ?workspace_id=wrkspc_default    │
│  ID          KIND    PROVIDER    STATUS       LAST PROBE                       │
│  cs_anth_01  vault   anthropic   ● active     ● valid 5m   [Validate][Archive] │
│  cs_env_02   env     anthropic   ● active     ◌ unknown    [Validate][Archive] │
│  cs_oai_03   vault   openai      ◌ disabled   ✗ invalid    [Validate][—]       │
├────────────────────────────────────────────────────────────────────────────────┤
│ POOLS                                                                          │
│  pool_main   members: ① cs_anth_01 (w=10)  ② cs_env_02 (w=1)   failover: ordinal│
├────────────────────────────────────────────────────────────────────────────────┤
│ RESOLVE 试算:model [claude-sonnet-4.5 ▾] binding [pool_main ▾]  [Resolve]      │
│  → ( claude-sonnet-4.5 ) → ( cs_anth_01 · credential ✓ ) → ( anthropic ● )     │
└────────────────────────────────────────────────────────────────────────────────┘
```
