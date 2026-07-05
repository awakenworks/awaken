# Web UI 设计与实现方案 — Oversight 外壳 × Awaken 管理面

> 来源四方:
> 1. **Oversight Prototype**(claude.ai/design handoff,`Oversight Prototype.dc.html` 已全文精读)——信息架构、交互模式、视觉意象;
> 2. **goal 仓库 admin-console**(`~/Codes/awaken-worktrees/goal/apps/admin-console`)——管理面页面套路与部分工程模式;
> 3. **oversight-next `/web`**(`~/Codes/oversight-next/web`,即 awaken-flow 的**生产实现**,~87k LOC、239 个 API 操作、179 个测试文件)——**工程架构的主要蓝本**:token 流水线、contract-first codegen、事件日志 reducer、导航 SSOT;
> 4. **本仓库(hapi-web-ui 分支)的真实 API 面**——决定页面必须覆盖的功能。本分支目前没有任何前端代码,从零开始。

## 0. 设计论点(一句话)

采用 Oversight 原型的**「Workspace ▸ Project 两级作用域 + 全局 cockpit(Home/Inbox)」外壳**,把原型虚构的 issue-tracker 领域替换为我们的真实领域(Session 是工作单元,Agent/Model/Credential/MCP/Project 是 Workspace 供给);**工程实现整体对照 oversight-next/web 的成熟做法**(W3C token 流水线、OpenAPI codegen 门禁、单出口 fetch、append-only 事件日志 reducer),并避开它已暴露的几个坑。

## 1. 四方对照:各取什么

| 来源 | 采纳 | 放弃 / 替换 |
|---|---|---|
| Oversight 原型 | 两级作用域侧栏(2a merged shell)、Home/Inbox 全局层、statChip 全可点、needs-you hero 行内 Approve、live banner 批量刷新、agent 紫=AI 活动、readiness(Bind & ready)清单、resolved binding 链式 chips、record-before-effect 日志表、编辑器外壳(顶栏版本+Publish / 主区 / 右 rail) | Issues/Cycles/Board、状态机工作流编辑器、CEL 转移、Automation when→then(我们无对应 REST 面;**模式**保留,映射到我们的实体) |
| goal admin-console | ⌘K palette、capabilities 门控、secret 全链路脱敏、URL 同步列表状态、unsaved-changes guard、toast/confirm、admin token modal、行内工具审批卡(Allow/Deny → 自动续跑) | 其单层 admin 目录式 IA(被两级作用域取代);RJSF;Tailwind(见 §6 的 CSS 决策) |
| **oversight-next /web** | **token 三层流水线**(W3C tokens JSON → build 脚本 → tokens.css,`data-theme` 切换,token-contract 测试);**contract-first codegen**(OpenAPI → 类型 + 生成客户端 + ajv 校验器,`*:check` git-diff 门禁);**单出口 fetch**(`client.ts` 唯一 egress + `no-raw-fetch` 测试);**`paths.ts` 导航 SSOT**(router/侧栏/palette 共用);**append-only 事件日志 reducer**(单 query cache 条目,按 id 去重、按 seq 排序,transcript 是纯投影);session-as-query 认证模式;LOADING sentinel(roster 未就绪不发项目级请求);structure tests 钉住 IA;chat 组件套件(ChatMessageList/ToolCallCard/ReasoningBlock);react-router v7 薄路由包装 Surface 组件 | 其 213KB `global.css`(改为按 surface 拆分)、67KB 单文件 Surface(强制拆分)、挂空的 `@xyflow` 依赖(不引入)、事件 payload `as Record<string,unknown>` 逃逸(我们的事件是封闭 tagged union,类型化到底)、jsdom 冒充 e2e(P2 起补 Playwright 走真 SSE) |
| 本仓库 API | 全部管理面(§3 逐页端点);`contracts/model-config.d.ts` 复用 | — |

原型领域 → 我们的实体映射(关键决定):

| 原型概念 | 我们的实体 |
|---|---|
| Workspace(共享供给) | `workspace_id`(IAM fence):catalog(providers/endpoints/offerings)、credentials+pools、vaults、MCP servers、inference profiles、IAM tokens、已发布 agents |
| Project(工作) | `Project` 实体 + `/projects/{id}` ingress;per-project agent MCP 绑定 |
| Issue(工作单元) | **Session**(`sesn_*`,idle/running/requires_action) |
| Inbox 审批 | `requires_action`(tool_confirmation)、durable deliver 决策、dead-letters、凭证 validate 失败 |
| 工作流编辑器 Publish v8 | Agent config **draft → validate → publish**(`publication_id`/`fingerprint`) |
| Automation firing log(record-before-effect) | Durable dispatch 队列(`status/attempts`)+ dead-letters |
| Resolved binding 链(Agent → model → provider ●verified) | `POST /v1/config/inference/resolve` 等三个 dry-run 端点直接供数 |
| Bind & ready(缺项必须真实创建) | 我们的 fail-closed resolve |

## 2. 信息架构与导航外壳

原型 2a merged shell,oversight-next 已验证同一模型可落地(其 `primaryNavGroups`:global 常驻 + project/workspace 二选一,scope 存 localStorage、随路由自动同步):

```
┌ Sidebar 236px (--rail) ────────────────┐┌ Main ─────────────────────────────┐
│ [A] <workspace>       Workspace ▾      ││ 46px 面包屑: workspace / scope / 页 │
│  └─ 📁 <project>          PROJECT ▾    ││ ─────────────────────────────────  │
│ [ Search…                        ⌘K ]  ││  内容区 padding 20/24, max 1280    │
│ GLOBAL · ALL PROJECTS                  ││  (dawn glow 径向渐变置于画布顶部)   │
│  Home        Inbox ❸(紫)              ││                                    │
│ ── 按 scope 二选一 ──                   ││                                    │
│ PROJECT · <id>          WORKSPACE      ││                                    │
│  Overview                Agents        ││                                    │
│  Sessions                Models        ││                                    │
│  Agents(绑定)            Credentials   ││                                    │
│  Settings                MCP Servers   ││                                    │
│                          Access        ││                                    │
│ footer: EN/中 · ☀/☾ · 头像             ││                                    │
└────────────────────────────────────────┘└───────────────────────────────────┘
```

实现要点(照抄 oversight-next):
- **`src/lib/navigation/paths.ts` 是唯一事实源**:typed 路由模式 + URL builder,router、侧栏、⌘K、面包屑标题全部由它派生,无漂移。
- 子项 expand-on-select(父 surface 激活才显示子项);Home/Inbox/Settings scope 无关;scope 持久化 `localStorage`。
- Settings 用统一页 + 两段左 rail(Project 段 / Workspace 段),带 scope banner。
- 侧栏 Sessions 项挂**紫色 agent 徽章** = 正在运行的会话数(oversight-next 用 scheduling map 的 `processing` 计数,我们用 running 会话计数)。
- 移动端:侧栏 off-canvas + scrim + Escape;<680px 弹层转 bottom-sheet。

## 3. 路由与页面清单(功能全覆盖,每页标注后端端点)

路由文件是薄包装,渲染 `*Surface` 组件(oversight-next 模式)。

### Global

| 路由 | 内容 | 端点 |
|---|---|---|
| `/` Home | KPI 行(Projects / Active sessions / Needs you)、Needs-you 列表(行内 Approve)、Projects 表(活跃会话数、紫 working、红 blocked) | `GET /v1/config/projects`;会话聚合见 §7 缺口 |
| `/inbox` | 过滤 chips(All / Approvals / Attention):① `requires_action` 会话 tool confirmation(行内 Allow/Deny → `POST /v1/sessions/:id/events` `user.tool_confirmation`);② dead-letters(`GET /v1/durable/threads/:t/dead-letters` + purge/reap);③ 凭证失效 →「Fix in Credentials ↗」。每行带 ReasonCode 说明(oversight-next `ReasonCodeNotice` 模式) | 各来源端点 + §7 缺口 |

### Project scope(会话操作走 `/projects/{pid}` ingress,天然拿到 project-scoped 工具面)

| 路由 | 内容 | 端点 |
|---|---|---|
| `/p/:pid/overview` | 项目脉搏:活跃会话卡、最近 dispatch、agent 绑定健康(resolve chips) | resolve 端点 + 会话列表(§7) |
| `/p/:pid/sessions` | 会话列表(状态点、agent、model、标题、紫色 agent-active 侧边条);「New session」拆分按钮(agent / environment / vault / 内联 MCP) | `POST /projects/:pid/v1/sessions` |
| `/p/:pid/sessions/:sid` | **核心界面,见 §4** | events + SSE + durable |
| `/p/:pid/agents` | 每个 agent 的 MCP 绑定(reference-don't-copy:列 workspace 供给,内联勾选,Manage ↗ 抽屉);保存前 resolve 预览 | `PUT/GET /v1/config/projects/:pid/agents/:aid/mcp`,`POST /v1/config/agents/:aid/mcp/resolve` |
| `/p/:pid/settings` | 项目信息(id 只读、display_name)、ingress baseURL(一键复制,标注「寻址而非授权」ADR-0042) | `PUT/GET /v1/config/projects/:id` |

### Workspace scope

| 路由 | 内容 | 端点 |
|---|---|---|
| `/agents` | 已发布 agent 列表(能力徽章);**编辑器**:draft → Validate → Publish(顶栏 Publish + fingerprint/publication_id,原型编辑器外壳;spec 编辑用 CodeMirror + JSON Schema,oversight-next `WorkflowSpecEditor` 同款);workspace MCP 绑定 tab | `PUT /v1/config/agents/:id`、`POST …/validate`、`POST …/publish`、`PUT/GET /v1/config/agents/:id/mcp` |
| `/models` | Catalog:Providers / Endpoints / Offerings;Inference profiles +「Resolve 试算」→ 链式 chips `model → credential → provider ●verified` | `/v1/config/providers|endpoints|offerings|catalog`、`inference-profiles/:id(+/resolve)`、`POST /v1/config/inference/resolve` |
| `/credentials` | ① Sources(enter 只写、列表恒 secret-free、Validate 活探针、Archive);② Pools(ordinal/weight 失效顺位);③ Vaults(三型凭证 + `mcp_oauth_validate`;标注 host-ephemeral) | `/v1/config/credentials*`、`credential-pools/:id`、`/v1/vaults*` |
| `/mcp-servers` | 定义 CRUD +「Bound by」反查(删除前警示下游绑定) | `GET/PUT /v1/config/mcp-servers*` |
| `/access` | IAM tokens:mint(明文仅一次)、列表、revoke;角色×动作矩阵;bootstrap 提示 | `POST/GET/DELETE /v1/config/iam/tokens*` |
| `/settings` | 两段 rail 汇总入口 + workspace 身份 | — |

**暂不做**(无 REST 面,不发明功能):Cycles/Insights/Analytics、Automation 规则 CRUD、Workers 舰队页。durable ops 收进会话详情抽屉(§4)。

## 4. 核心界面:Session 详情

三栏式:左转录、右属性 rail(340px,窄屏折叠)。

**数据层(oversight-next 的关键模式,原样采用)**:committed 事件(`GET /v1/sessions/:id/events` + SSE replay)进入**单个 React Query cache 条目**,由纯 reducer 处理——按 `evt_*` id 去重、按序合并,乱序/重放自然收敛;**转录 UI 是该日志的纯投影**,不另设聊天 store。我们的事件是封闭 tagged union(`OutboundKind`),投影函数全程强类型,不允许 `as Record<string,unknown>` 逃逸。

**转录渲染**:

| 事件 | 渲染 |
|---|---|
| `agent.message` | 左侧气泡,渲染 ContentBlock 数组(text、image 内联);markdown 用 marked+dompurify(oversight-next `ChatMarkdown`) |
| `agent.tool_use` | 可折叠 tool 卡(🛠 mono 工具名 + 状态 pill,`ToolCallCard` 同款);展开 Input JSON;`evaluated_permission` 小字标注 |
| `agent.custom_tool_use` | 同上,标「client-executed」,结果回填表单 → `user.custom_tool_result` |
| `agent.tool_result` | 折叠进对应 tool 卡 Output/Error 面板 |
| `session.status_running` | 紫色脉冲点「Agent working」(iaPulse) |
| `session.status_idle` | `end_turn` 静默;`requires_action` → **行内审批卡**(amber 边,Allow / Deny + deny_message)→ `user.tool_confirmation`;`retries_exhausted` 红色告警条 |
| `span.outcome_evaluation_*` | 折叠「Outcome iteration N」时间线(result + explanation) |

- 所有 tool i/o、错误文本过 secret 脱敏管道再进 DOM。
- SSE 是 committed-replay 而非 live push → 用**原型 live banner**:轮询发现新事件不打断阅读,顶部浮出「N new updates · Refresh」,点击合并渲染。轮询间隔参照 oversight-next 分级:活跃会话 5s、列表 15s、dispatch/凭证 30s。
- 用户消息右对齐(`--sunk` 底)。

**Composer**:文本 + 发送(`user.message`);model 覆盖下拉(per-turn);「⚙」:`user.interrupt` / `user.pause` / `user.resume`;Define outcome 表单(→ `user.define_outcome`)。

**右 rail**:Agent 卡(能力广告:builtin_tools(ask 标记)/custom_tools/skills/delegates)、Session 属性(id mono、created、metadata、resources)、**Durable 抽屉**(端点 400 时隐藏):Dispatches 表(`run_id/status/attempts`,record-before-effect 账本样式)、Submit background / Supersede / Cancel / Wake / Deliver、Superseded、Dead-letters(+purge)、Reconcile / Reap(危险操作走 confirm)。

## 5. 设计系统

**基调(经 oversight-next 生产校准修订)**:原型的暖纸底(`#f8f5ef`)在 awaken-flow 产品化时**没有存活**——生产 token 收敛为**无色中性 chrome(#fff/#fafafa/#f7f7f7,暗 #0a0a0a/#111)+ amber 强调(oklch 78 色相)+ agent 紫 + 红 spark**,即品牌系统的统一底座。我们跟随这一收敛,与 oversight-next 同底不同款:

- chrome 无色;**颜色只给语义**:amber = 需要**人**(主按钮、当前项、approval);紫 = **agent 活动**(脉冲、徽章、活跃侧边条);红 = 阻塞/告警;绿 = 通过/verified;蓝 = 角色/引用;
- Inter 正文(CJK 回退 Noto Sans SC,`zh` 时 letter-spacing 归零)+ JetBrains Mono **只**用于 id/数据/代码(A/B 校准 B 方案);
- 基准字号 14px、radii sm4/md6/lg8/card10/pill9999、间距 4 的倍数、hairline ring 阴影代替重边框;
- dawn glow 径向渐变作为画布顶部的品牌隐喻保留(低透明度,不干扰内容);
- 动效克制:iaPulse(agent 呼吸)、iaBlink(流式光标)、浮层 180ms,共三种。

**Token 工程(照抄 oversight-next 流水线)**:
- 事实源 `design-tokens/awaken-console.tokens.json`(W3C Design Tokens 格式)→ `scripts/build-tokens.mjs` → `src/styles/tokens.css` + manifest;
- 三层:primitives(`--brand-color-*`)→ 语义(`--brand-bg/--brand-accent/--brand-agent/--brand-tone-*`,含 color-mix 派生 tint)→ 短别名 + `--aw-*` 别名;组件只消费后两层;
- 明暗:`:root,[data-theme="light"]` / `[data-theme="dark"]`,localStorage 持久化;
- `token-contract` 测试钉住 CSS 变量面,防漂移。
- 关键值(与 oversight-next 对齐):accent `oklch(62% .14 78)` 亮 / `oklch(74% .13 78)` 暗;agent `oklch(56% .15 300)` / `oklch(70% .135 270)`;spark `#cf4040`/`#fa6863`;状态色、优先级 ramp 按其 `--brand-state-*`/`--pri-*` 取值。
- **品牌可切换性是免费的**:若日后确认本产品应归入「awaken-agents」品牌(indigo 265、compact、dark-first),只需换 tokens.json 的 4 个 themed 旋钮(accent/默认明暗/密度),chrome、紫、spark、状态色全部不动。默认先用 amber,与用户要实现的 Oversight 视觉家族一致。

**形态配方**(原型逐条提取,数值不变):卡片 radius 10 + `0 0 0 1px var(--edge), 0 1px 1px rgba(26,29,30,.03)`;导航项 h32 r6,active = card 底 + ring + icon 变 accent;pill r9999 h20–24 11/600 tint 底;状态点 6–9px;拆分按钮;statChip 全可点;通用 dropdown 手机端 bottom-sheet;micro-label 10–11/700 大写宽字距;KPI 值 `tabular-nums`。

## 6. 工程蓝图(对照 oversight-next 的落地形态)

**目录**(pnpm workspace,`apps/console/`):

```
apps/console/
  design-tokens/awaken-console.tokens.json
  scripts/build-tokens.mjs · build-types.mjs · build-api-client.mjs (*:check 门禁)
  src/
    routes.tsx            # createBrowserRouter + RouteObject[]
    routes/*.tsx          # 薄包装 → Surface
    lib/navigation/paths.ts   # 导航 SSOT
    lib/api/client.ts     # 唯一 fetch 出口(auth 注入 + ApiClientError{status,request_id})
    lib/api/*-client.ts   # 按域薄封装(sessions/config/vaults/durable/iam)
    lib/query/use-*.ts    # React Query hooks
    lib/query/session-event-log.ts  # append-only reducer(去重/排序/派生 lifecycle)
    lib/security/redact.ts
    components/app/       # AppShell / Sidebar / TopBar / CommandPalette / ScopePicker
    components/chat/      # MessageList / ToolCallCard / ApprovalCard / Composer
    components/ui/        # button/pill/chip/card/drawer/segmented/…
    surfaces/<domain>/    # 每 surface 一目录,自带 css(禁止单一 global.css 膨胀)
    styles/tokens.css (generated) · base.css
```

- **栈**:React 19、Vite、TS strict、react-router v7(data router + lazy)、TanStack Query v5、i18next(en/zh-CN)、lucide-react、CodeMirror 6(agent spec JSON 编辑)、marked+dompurify、Vitest+Testing Library。
- **CSS 决策**:跟 oversight-next 用手写 CSS + token 变量(不引 Tailwind,与其风格一致、便于日后共库),但**按 surface 拆文件**,吸取 213KB global.css 的教训;单文件上限约束(lint 或约定 <400 行)。
- **Contract-first(后端配套已落地)**:`contracts/openapi.generated.json`(OpenAPI 3.1)由 `awaken-admin-config-api` 的 `export_openapi` 导出(operation 注册表 + schemars schema 复用,`generate-contracts.sh --check` 门禁,`openapi_contract` 测试锁路由挂载)。前端接 `openapi-typescript` → `_api.d.ts` + 生成客户端 + dev-only ajv 响应校验(打日志不抛错)。
- **认证**:session-as-query(瞬态故障在 loading 内重试;区分「API 未代理返回 HTML」的 diagnostic 与真 401);IAM bearer 存 localStorage;`RequireSession` 守卫 + returnUrl。
- **项目 roster sentinel**:project 未解析前,project-scoped queries 全部 disabled(oversight-next LOADING_DIRECTORY 模式),避免错闪。
- **测试**:structure tests 钉 IA(侧栏分组、路由→面包屑);token-contract 测试;reducer 纯函数单测(乱序/重放收敛);`no-raw-fetch` 测试;P2 起补 Playwright 真浏览器 e2e 走 SSE(oversight-next 的已知空白)。
- **组件迁移策略**:从 oversight-next **copy-adapt**(不是共享包,两 repo 两产品,避免过早抽库):token 流水线脚本、AppShell/Sidebar/CommandPalette、chat 套件、Inbox surface 骨架、`client.ts`/`errors.ts`/no-raw-fetch、run-event-stream reducer(改名 session-event-log,类型换成我们的 OutboundKind)。其 239 个 feature client 不迁移——按我们的路由重写。
- **明确不引入**:@xyflow(oversight-next 也没真用)、Redux/Zustand、RJSF、Tailwind。

## 7. 后端缺口(前端落地的前置项,按优先级)

1. **`GET /v1/sessions`(列表 + 按 status/project 过滤)**——Home、Inbox、会话列表全依赖枚举;没有它 P2 只能按 id 打开会话。
2. **needs-attention 聚合**(或列表带 `status=requires_action` 过滤)——Inbox 数据源。
3. ~~OpenAPI 契约~~ **已落地**(`contracts/openapi.generated.json`):采用 oversight-next 方式而非 utoipa——手写 operation 注册表 + 复用现有 schemars 导出(OpenAPI 3.1 原生接受 JSON Schema 2020-12)+ `export_openapi` example + `openapi_contract` 测试锁路由;理由:schemars 已是我们的 schema SSOT(utoipa 有自己的 ToSchema,会造成双源漂移),managed 会话 wire 属官方 SDK 需要「策展排除」,且路由按模式动态装配、跨多 crate。当前覆盖 admin config plane 19 路径 / 28 操作;IAM tokens 与 durable ops 的注册表扩展是后续增量。
4. Session 元数据更新端点(title/archive;字段已存在,无 PATCH)。
5. SSE live push(当前 committed-replay)——非阻塞,live banner + 分级轮询先行(oversight-next 生产也全靠轮询)。
6. Workspace 枚举(非一等实体)——P1 固定单 workspace(取自 token)。

## 8. 分期

- **P1 地基 + 管理面(零后端改动)**:token 流水线、外壳(paths.ts/侧栏/⌘K/明暗/i18n)、单出口 fetch、Workspace 全部资源页(Models、Credentials、MCP、Access、Agents 列表)+ Settings。
- **P2 会话面**:事件日志 reducer + 转录 + composer + HITL 审批 + live banner;Inbox(先聚合 dead-letters 与凭证告警;缺口 1 落地后补会话审批);Playwright 首条 SSE 用例。
- **P3 项目面**:project 切换器 + roster sentinel、per-project MCP 绑定 + resolve 预览、`/projects/{id}` ingress 会话。
- **P4 深水区**:Agent 编辑器 publish 流(CodeMirror spec + readiness 清单)、Durable 运维抽屉全量、outcome 面板、vault mcp_oauth 全流程。
