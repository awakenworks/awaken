# 跨模块 E2E 因果图测试设计 · Awaken(1.0.0-dev)

> 目的:用**因果图法**在**端到端海拔**产出覆盖**全部功能**的测试用例,并沿**不同部署场景**参数化。
> 与配套的单模块设计(`cause-effect-graph-test-design.md`)互补:那份在函数/类型边界取因果;**本份跨模块**——因是"部署配置 + 端到端流程触发",果是"通过真实二进制 + 真实进程/HTTP/WS/磁盘可观察的结果"。
>
> **e2e 铁律**:测试必须驱动真实二进制(`awaken` / `awaken-worker` / `awaken-sandbox` / `awaken-scenario-host`)+ 真实进程/HTTP/WS/磁盘/Postgres/容器。mock transport、假 ProtocolRuntime、脚本化 channel、假 CLI 脚本**都不算** e2e。上游模型可用**确定性 fake Anthropic 线**(`fixtures/`,跨重启存活)或真实 provider(`*_real_e2e`),二者都是"真进程 + 真 HTTP"。
>
> 规模:**22 条部署轴(D)+ 99 条功能流(F)= 121 个因**;**30 类可观察效果(E)**;由约束归约出 **14 个合法部署场景(S)**;主判定表 = **场景 × 功能流**,产出 **约 180 个 e2e 测试用例**。

---

## 0. 方法:E2E 因果图的两族因与参数化原则

一个 e2e 测试用例 = **一个部署场景(D 轴取值组合)× 一个功能流触发(F)→ 断言若干可观察效果(E)**。

- **D(部署轴)**:配置维度的等价类选择器(env / flag / yaml 字段)。跨轴约束(E/I/O/R)把 2ⁿ 组合收敛为少量**合法部署场景 S**。
- **F(功能流)**:一次端到端交互序列(建会话→发轮→审批→重启→跨协议接管→…)。**F 是功能覆盖的单位**。
- **E(可观察效果)**:e2e 唯一能断言的东西——HTTP 码、线上事件序列、重启后磁盘/DB 状态、跨协议恢复、webhook 投递、密钥不泄漏、trace 跨度树、metrics 计数。

**参数化原则(本设计的骨架)**:
1. **部署不变流**(F 的行为应跨后端一致):同一 F 在其**适用场景等价类**上参数化重跑,断言**同一 E**。例:`managed 单轮`应在 S1/S2/S3/S4 产出**相同 EVENT-SEQ**——后端只改持久化,不改语义。
2. **部署特有流**(F 只在某场景可观察):单场景用例。例:`PG-WAKE 跨节点驱动`只在 S5/S7;`NET-POLICY egress none`只在 S10。
3. **约束边即用例**:每条 R(要求)/M(遮蔽)/O(唯一)约束都配**否定用例**——违反组合应 fail-closed(拒绝启动 / 403 / 400),而非 fail-open。

**记法**:D 轴 `D#`;功能流 `F#`;效果 `E#`;场景 `S#`;判定表单元 `1/0/-`;矩阵单元 `●`=主场景(仅此可观察)/`✓`=参数化重跑 /`—`=不适用。

---

## 1. 部署轴(D 因)——配置等价类

真实选择器锚定 `crates/bin/awaken-cli/src/{config,main,lib}.rs`、`awaken-runtime-host/src/{deployment_config,dispatch_backend,acp_backend,sandbox_source}.rs`、`awaken-worker`、`awaken-sandbox`、`deploy/k3d/*.yaml`。

| D# | 轴 | 选择器 | 等价类(值) | 默认 |
|---|---|---|---|---|
| D1 | 进程角色 | `AWAKEN_ROLE` | serve/coordinator/all-in-one · worker · hand | serve |
| D2 | 提交/存储后端 | `AWAKEN_STORE` | sqlite · fs · postgres | sqlite |
| D3 | 数据面存储目录 | `AWAKEN_STORAGE_DIR` | 路径(durable) · 未设(in-mem) | 未设 |
| D4 | 派工队列后端 | `AWAKEN_DISPATCH_BACKEND`(+`AWAKEN_DATABASE_URL`) | sqlite · postgres | sqlite |
| D5 | 持久 ingress | `AWAKEN_INGRESS` | durable · 未设(direct) | direct |
| D6 | 跨节点唤醒 | `AWAKEN_DISPATCH_WAKE`(+`_CHANNEL`) | pg-notify · nats · 未设(local+poll) | local |
| D7 | NATS broker | `AWAKEN_NATS_URL`(+`--features nats`) | url · 未设 | 未设 |
| D8 | 本地 drain 池 | `AWAKEN_SERVER_RUN_LOCAL_POOL`(负 `AWAKEN_DISABLE_LOCAL_POOL`) | true · false(coordinator) | true |
| D9 | 派工租约 owner | `AWAKEN_DISPATCH_OWNER` | 字符串(每副本唯一) | `<host>-<pid>` |
| D10 | 管理面持久化 | `AWAKEN_MGMT_DIR`/`AWAKEN_DEPLOYMENT_DATA_DIR` | 目录(durable sqlite) · 未设(in-mem) | in-mem |
| D11 | 分组件存储 | `AWAKEN_<COMPONENT>_DB`(`ControlStoreConfig`) | 每库 sqlite 路径 · 共享 pg url | `<dir>/<name>.db` |
| D12 | 封印密钥 | `AWAKEN_MGMT_SEAL_KEY`/`_FILE` | 64-hex 32B · 未设 | durable 时必需 |
| D13 | 嵌入式 IAM | `AWAKEN_MGMT_IAM` | embedded · 未设(open) | open |
| D14 | 拆分 admin 端口 | `AWAKEN_SERVER_ADMIN_LISTEN` | addr · 未设(合并) | 合并 |
| D15 | 沙箱隔离级 | `AWAKEN_SANDBOX_TIER`(`SandboxTier::from_env_str`) | local/none · namespace(bwrap) · docker · podman · k8s | namespace |
| D16 | 容器镜像 | `AWAKEN_CONTAINER_IMAGE`(+`container-*` feature) | ref · 未设 | 未设(容器级必需) |
| D17 | bwrap 缺失降级 | `AWAKEN_SANDBOX_ALLOW_LOCAL_FALLBACK` | 1(降级) · 未设(fail-closed) | fail-closed |
| D18 | ACP CLI 源 | `AWAKEN_ACP_CLI`(投影) / `AWAKEN_ACP_ARGV`(固定) | cli-id · argv · 皆无(native) | 皆无 |
| D19 | Hand 拓扑 | brain 侧:`AWAKEN_REMOTE_HAND_UNIX`(C5 colocated) · `AWAKEN_REMOTE_HAND`(direct) · `AWAKEN_REMOTE_HAND_LISTEN`(reverse) · `AWAKEN_REMOTE_HAND_NATS`+`_SUBJECT`(relay);sandbox 侧 `awaken-sandbox hand --unix/--listen/--dial/--nats` | 四拓扑之一 | — |
| D20 | 每会话出口 | 会话 environment networking → `NetworkPolicy::None/Unrestricted` | deny · allow | allow |
| D21 | Worker 上游 & 秘密 | `AWAKEN_UPSTREAM_URL`/`AWAKEN_WORKER_SERVE_URL`;`AWAKEN_WORKER_GATEWAY_ONLY` | 本地凭证 · gateway-only(secretless) | 本地凭证 |
| D22 | memoryd 实现 | `AWAKEN_MEMORY_MODE`(+`_STORE_ID`/`MOUNT_PATH`) | fuse · copy | copy |

> 生产模型源(provider/endpoint/credential)**只走 DB 目录 + vault**(`/v1/config/*`+`/v1/vaults/*`→`ConfigExecutorProvider`),**禁作 env 轴**(项目铁律)。e2e 用 `AWAKEN_MODEL_MODE`(scenario-host 专用)选桩/上游路由,不是生产选择器。

---

## 2. 跨轴约束 → 合法部署场景枚举(S)

### 约束格(E/I/O/R,全部 fail-closed,锚定代码)

- **R1** Worker(D1=worker)⇒ `AWAKEN_WORKER_SERVE_URL`/`UPSTREAM_URL`(否则拒启,`config.rs::validate`)。
- **R2** Coordinator(D8=false)⇒ postgres 派工队列(否则无 drainer,config.rs:180)。
- **R3** 持久控制面(D10 set)⇒ 封印密钥 D12(`mgmt_seal_key_from_env` 未设/双设/畸形均 panic)。
- **R4** 嵌入式 IAM(D13=embedded)⇒ D10 set(token 落 `<dir>/iam.sqlite`;其他值 panic)。
- **R5** 持久 ingress(D5=durable)⇒ 持久队列:`AWAKEN_STORAGE_DIR` **或** D4=postgres **或**注入 store。**pg 提交 store(D2=postgres)不满足派工队列**(`a_postgres_commit_store_does_not_satisfy_the_dispatch_queue`)。
- **R6** D4=postgres / D2=postgres ⇒ `AWAKEN_DATABASE_URL` + 启动 init。
- **R7** pg-notify(D6)**共享 pg store 的库**,仅当 D4=postgres 有意义。
- **R8** nats wake(D6=nats)⇒ `AWAKEN_NATS_URL` + `--features nats`(否则硬启动错,绝不静默 poll)。
- **R9** 容器级(D15∈{docker,podman,k8s})⇒ 匹配 `container-*` feature + D16 镜像;**k8s ⇒ `AWAKEN_K8S_AGENT_ADDR`**。
- **R10** namespace 级 ⇒ bwrap 存在,除非 D17=1 降级(高声通告)。
- **R11** 投影 ACP 每 worker 只服务**一个** CLI:`backend_ref=acp:<other>` ≠ `AWAKEN_ACP_CLI` 时 open 处 fail-closed。
- **R12** 出口封闭边界(D20=deny)⇒ unix 传输是唯一跨界口(C5);**k8s 无 ingress ⇒ reverse hand(`--dial`)必需**;镜像内 ACP 适配器须预置(封闭 pod 不能 `npm install`)。
- **R13** gateway-only worker(D21)⇒ 不开 vault/无需 seal;本地凭证模型则退 `NoModelConfiguredExecutor`。默认 worker ⇒ 共享控制面 stores(D10+D12)。
- **R14** hand/memoryd 角色 ⇒ `--features hand`/`memoryd`;memoryd fuse ⇒ `/dev/fuse`+SYS_ADMIN,否则自动退 copy。

### 合法部署场景(判定表 S:D 轴组合 × 约束)

| 场景 | 一句话 | 关键 D 取值 | 约束校验 |
|---|---|---|---|
| **S1** | all-in-one 内存(临时) | D1=serve, D3=未设, D10=未设 | 无外依,冒烟/开发 |
| **S2** | all-in-one durable sqlite | D2=sqlite, D3=dir, D10=dir, D12=key, D13=embedded | R3,R4 |
| **S3** | all-in-one durable fs | D2=fs, D3=dir, D10=dir, D12=key | R3;fs=append ndjson |
| **S4** | 单机 durable ingress + 本地池 | D5=durable, D3=dir, D4=sqlite, D8=true | R5(STORAGE_DIR 满足) |
| **S5** | Postgres 微服务 | D2/D4=postgres, D6=pg-notify, D5=durable, D19=reverse/direct hand | R5,R6,R7 |
| **S6** | NATS 唤醒 | S5 但 D6=nats, D7=url, `--features nats` | R8;pg-notify 关 |
| **S7** | 故障切换/横向扩展 | S5 × N 副本, D9 各异, D8=true 各自 | `FOR UPDATE SKIP LOCKED` owner 租约 |
| **S8** | 自托管 worker | coordinator D8=false + pg;+ `awaken-worker` D1=worker, D21 本地凭证, D5=durable | R1,R2,R13 |
| **S9** | gateway-only worker | S8 但 D21=gateway-only(secretless) | R13(无 vault) |
| **S10** | 沙箱 namespace/bwrap | D15=namespace, D20=deny → NET-POLICY | R10 |
| **S11** | 沙箱容器 docker | D15=docker, D16=image, `--features container-docker`;ACP bridge | R9 |
| **S12** | 沙箱 k8s + memoryd | D15=k8s, D16, `AWAKEN_K8S_AGENT_ADDR`, D19=reverse `--dial`, D22 sidecar | R9,R12,R14 |
| **S13** | 共置 hand(unix,出口封闭) | D19=`REMOTE_HAND_UNIX`, D20=deny | R12(C5) |
| **S14** | ACP-CLI 后端(投影单 CLI) | D18=`ACP_CLI`, D15 tier | R11 |

**否定用例(约束边)**:N1 worker 缺 SERVE_URL→拒启(R1);N2 coordinator+sqlite 队列→拒启(R2);N3 durable 无 seal→拒启(R3);N4 IAM=embedded 无 MGMT_DIR→panic(R4);N5 INGRESS=durable + 仅 pg 提交 store→拒启(R5);N6 nats wake 无 `--features nats`→硬错(R8);N7 k8s 无 AGENT_ADDR→fail-closed(R9);N8 namespace 无 bwrap 且未降级→fail-closed(R10);N9 ACP ref ≠ worker CLI→open fail-closed(R11);N10 k8s 封闭 pod 无 reverse hand→无 ingress 不可达(R12)。

---

## 3. 功能流(F 因)——端到端覆盖单位

按 13 个功能域枚举。每条 F 是一次真实序列,标注其**主效果 E** 与**适用场景**。

### 3.1 协议轮生命周期(6 前门)
- **F1** managed 单轮 echo → EVENT-SEQ `[running,agent.message,idle]`
- **F2** managed 同线程多轮 → 历史累积
- **F3** managed 多模态(vision)→ 图像入 content
- **F4** managed system_message 注入
- **F5** managed 工具循环 tool_result → `stop_reason=end_turn`
- **F6** ai-sdk 轮(SSE text-delta 重组)
- **F7** ag-ui 轮(SSE TEXT_MESSAGE_CONTENT)
- **F8** a2a message/send → Task `completed`
- **F9** acp 轮(JSON-RPC 编解码)
- **F10** mcp tools/call
- **F11** 流式工具输入(streaming tool input)
- **F12** 同一 host 全前门并挂(allinone_frontdoors)

### 3.2 HITL(人在环审批)
- **F13** 工具触发审批 → HITL-AWAIT `requires_action` + `event_ids`
- **F14** 批准 → HITL-EFFECT:sentinel 写入(读回含)
- **F15** 拒绝 → HITL-EFFECT:写被阻,run 仍 `end_turn`(读回不含)
- **F16** a2a HITL 不对称:批准走文本,拒绝只经 `tasks/cancel`
- **F17** ai-sdk HITL deny
- **F18** durable worker HITL deny:await 跨进程存活,worker 驱动到终态

### 3.3 中断 / 取消
- **F19** 跨协议取消:一 wire 起,a2a `tasks/cancel` → `canceled`
- **F20** durable worker 取消排队/await 的 run → 终态 Cancelled
- **F21** 会话 dispose / terminated(reap 沙箱一次)
- **F22** idle 后竞态取消(post-idle race)
- **F23** ACP 中途 interrupt(reap Term→Kill)

### 3.4 多轮 & 跨协议
- **F24** 跨协议连续性:AI-SDK 起轮在 AG-UI history 可见
- **F25** 跨协议多轮
- **F26** 跨协议 usage 聚合
- **F27** 跨协议 client tool
- **F28** a2a 连续性(contextId 共享线程)
- **F29** 跨协议上游故障传播
- **F30** managed 跨线程
- **F31** 多协议并发

### 3.5 持久 / 重启 / 恢复 / 抢占 / 调度 / 死信
- **F32** 重启存活:kill+重启同 dir → 历史在、await 恢复到 end_turn
- **F33** durable 跨协议恢复:AI-SDK await → AG-UI 批准 → worker 驱动 done(读回 ≥3)
- **F34** pg 提交存活:历史在 PG 非本地文件(换本地 dir 仍在)
- **F35** pg wake 跨节点自主驱动
- **F36** 抢占:新 run 使 stale run→Superseded(无双提交)
- **F37** 调度动作:await→到期唤醒执行
- **F38** 死信:达 max_attempts → DeadLetter,可 requeue
- **F39** durable 终态故障:fault 作事件提交,会话仍可用
- **F40** durable 池后台驱动(submit_background→queued→轮询回复)
- **F41** soak 公平性:多线程单写者/线程
- **F42** 优雅 drain(brain_drain / graceful_drain)
- **F43** SSE reconnect 续流
- **F44** 分组件 DB 拆分持久化
- **F45** 会话 config 重启存活
- **F46** worker poller 兼容

### 3.6 沙箱供给 / 隔离 / 挂载
- **F47** 沙箱供给:resources 挂载 → agent.message
- **F48** fail-closed 悬挂资源引用 → create ≥400
- **F49** NET-POLICY:egress none → bwrap `--unshare-net` → probe net=DOWN(有 bwrap 才跑)
- **F50** 容器 agent 往返:docker MARKER `CONTAINER-AGENT-OK`(有 docker 才跑)
- **F51** acp sandboxed
- **F52** memory store 挂载(FUSE vs copy)+ 收割回灌
- **F53** git repo 资源挂载
- **F54** 资源生命周期:反向通道 create/list/delete(`GET /v1/files?scope_id=`)
- **F55** k8s pids fail-closed(不可表达 → 拒)
- **F56** 暖池复用
- **F57** 沙箱 reaper 回收陈旧

### 3.7 管理面
- **F58** config CRUD + validate + resolve
- **F59** vault 封印/解封 + 秘密静态封印
- **F60** 封印密钥必需:durable 无 key → 拒启
- **F61** 嵌入式 IAM authn:无/坏 token→401、bearer→200、过期→401、吊销→401、后继→200
- **F62** authz 栅栏:workspace 不符→403、未映射路由→403 fail-closed、本 ws→200
- **F63** 跨租户 config 隔离:同 id 外写=200 no-op、该 scope 读 404、owner 行不变
- **F64** memory 租户作用域
- **F65** consent 决策:Full(活跃 Granted)vs Structured(撤回/待定/未知)
- **F66** capture 决策 + erasure 循环
- **F67** GDPR erasure 回执(幂等,records_removed)
- **F68** user profiles
- **F69** enrollment
- **F70** deployments CRUD + schedule
- **F71** worker 注册运行:official / self-hosted / custom
- **F72** environments + env-work 持久化
- **F73** 不兼容凭证拒绝(IncompatibleCredential)
- **F74** egress bind(managed egress lease)
- **F75** bootstrap admin token(`<dir>/admin-token` 0600、重启不重铸、明文一次)
- **F76** 会话轴独立 / threads / pagination / family

### 3.8 MCP
- **F77** managed MCP 工具注入(`mcp__<server>__<tool>`)
- **F78** MCP refresh(`tools/list_changed` → version bump)
- **F79** ACP managed MCP
- **F80** management MCP 配置

### 3.9 记忆 / 技能 / 压缩
- **F81** memory 召回 + 收割 durable(跨会话 BANANA-42)
- **F82** memory 提取 durable
- **F83** 技能激活 + skill store durable
- **F84** 压缩 token 窗口触发
- **F85** 压缩 model 窗口(ModelSpec.context_window 驱动)
- **F86** 压缩 durable(摘要跨重启存活)

### 3.10 模型解析 / 池切换 / oauth
- **F87** 模型路由解析(offering→ResolvedInference)
- **F88** 模型池故障转移(冷却成员→下一个)
- **F89** 模型覆盖 / agent 模型继承
- **F90** oauth 凭证刷新(helper 铸 Bearer,401 重试一次)
- **F91** 上游认证错误 / 故障

### 3.11 可观测
- **F92** trace 捕获:span 树连通、路由覆盖、GenAI 链 `send→invoke_agent→chat`、`execute_tool` span
- **F93** trace 传播:入站 W3C traceparent 续接
- **F94** 派工 trace:`wake.dispatch` / `aux.background` span
- **F95** metrics 导出:OTLP 收 `gen_ai.client.operation.count`,关停有界 flush 不挂
- **F96** usage 指标
- **F97** trace 后端多路

### 3.12 安全
- **F98** 密钥不泄漏:sentinel 在 SDK 视图 / 原始 HTTP / stdout·stderr / OTel trace 文件**均无**,仅允许 masked `preview()`
- **F99** managed egress OS 强制(net=DOWN)

---

## 4. 可观察效果(E 类)——e2e 唯一可断言口径

| E# | 类 | 真实断言模式 |
|---|---|---|
| E1 HTTP-STATUS | `res.status ∈ {200,201,400,401,403,404}`(裸 fetch 见 SDK 隐藏的码) |
| E2 ERR-ENVELOPE | `body.type==='error'` + `error.type ∈ {not_found,invalid_request,authentication,permission}_error` |
| E3 BETA-GATE | 缺 `anthropic-beta: managed-agents-2026-04-01` → 400 |
| E4 EVENT-SEQ | `events.map(e=>e.type)` deepEqual `[running,agent.message,idle]` + content 精确 |
| E5 STOP-REASON | `idle.stop_reason.type ∈ {end_turn, requires_action}`;requires_action 含 tool_use id |
| E6 SSE-STREAM | 排 `data:` 帧:text-delta / TEXT_MESSAGE_CONTENT / RUN_STARTED;重组文本 |
| E7 HITL-AWAIT | `agent.tool_use` + `evaluated_permission==='ask'` |
| E8 HITL-EFFECT | 读回含/不含 SENTINEL(allow 写入 / deny 阻断,run 仍 end_turn) |
| E9 SESSION-ERROR | `session.error` 作事件提交(非 HTTP 错),`retry_status.exhausted`,会话仍可用 |
| E10 DURABLE-DISK | `.db`(sqlite)/`.ndjson`(fs)存在;重启前后 `deepEqual` |
| E11 RESTART-SURVIVE | kill+重启同 dir/PG → 历史/await/config/token 仍在;await 恢复到 end_turn |
| E12 XPROTO-CONTINUITY | 一 wire 提交的轮在另一 wire history 可见;item 数增长 |
| E13 XPROTO-RESUME | await 一 wire、批准另一 wire、worker 驱 done(读回计数 ≥3) |
| E14 CANCEL-TERMINAL | a2a `tasks/cancel`→`canceled`;message/send→`completed`;note 计数判别 |
| E15 SUPERSEDE-DROP | `/superseded` 含 staleRunId;dispatch `Superseded`;无双提交 |
| E16 BG-DRIVEN | `submit_background`→`{run_id,queued:true}`;轮询 `/messages` 得 Assistant 回复 |
| E17 WORKER-COMMIT | `POST /v1/worker/commit`→`{sequence}`;重投幂等;`dispatch/claim`→`{claimed}` |
| E18 PG-WAKE | pg backend + pg-notify:自主驱动,换本地 dir/重启仍在同 DB |
| E19 SANDBOX-MOUNT | 带 resources 会话产出 agent.message;反向 `GET /v1/files?scope_id=` |
| E20 FAIL-CLOSED | 悬挂引用→create ≥400;坏 repo→首轮无干净 agent.message |
| E21 NET-POLICY | probe net=UP(unrestricted)vs net=DOWN(`--unshare-net`);无 bwrap 自跳过 |
| E22 CONTAINER-ROUNDTRIP | fixture MARKER 入 agent.message;无 docker 自跳过 |
| E23 AUTHN | 无/坏/过期/吊销 token→401;bearer/后继→200 |
| E24 AUTHZ-FENCE | ws 不符→403;未映射路由→403 fail-closed;本 ws→200 |
| E25 TENANT-ISO | 跨 ws→403;同 id 外写=200 no-op、该 scope 读 404、owner 行不变 |
| E26 SECRET-NONLEAK | SENTINEL 在 SDK/HTTP/stdout/stderr/trace 均无;仅 masked preview |
| E27 BOOTSTRAP-TOKEN | admin-token 0600 `sk-awaken-…`;重启不重铸;list 无明文/明文一次 |
| E28 ERASURE | `POST …/erasure`→200 `{records_removed}`;幂等 |
| E29 TRACE-TREE | 32-hex tid/16-hex sid、连通无悬挂、路由覆盖、GenAI 链、传播续接 |
| E30 METRIC-EXPORT | fake collector 收 OTLP `gen_ai.client.operation.count`;关停不挂 |

---

## 5. 跨模块因果图(F → E,受 D 门控/遮蔽)

E2E 因果图的边 = "功能流触发某效果",但**是否可观察由部署门控**。核心规则:

```
协议轮:        F1..F12 → E4/E5/E6                     [任意场景 S1..S14]
HITL:          F13 → E7 ; F14 → E8(含) ; F15 → E8(不含)∧E5=end_turn
               F18/F33 → E13                          [E13 门控: D5=durable ∧ 有 worker 池]
取消:          F19/F20/F23 → E14 ; F21 → 沙箱 reap 一次
跨协议:        F24/F25 → E12 ; F26 → E12(usage) ; F28 → E12(contextId)
持久:          F32/F45 → E11 ; F34 → E11(PG) ; F35 → E18 ; F36 → E15 ;
               F37 → 调度唤醒 ; F38 → 死信 ; F39 → E9 ; F40 → E16 ; F44 → E10/E11
               [E11/E18 门控: D3=dir ∨ D2/D4=postgres ; E18 额外要求 D4=postgres ∧ D6=pg-notify]
沙箱:          F47/F52/F53 → E19 ; F48/F55 → E20 ; F49/F99 → E21 ; F50 → E22
               [E21 门控: D15=namespace ∧ D20=deny ∧ bwrap 在 ; E22 门控: D15=docker ∧ docker 在]
管理面:        F58 → E1/E2 ; F59/F74 → E26 ; F60 → 拒启(E1) ; F61/F75 → E23/E27 ;
               F62 → E24 ; F63/F64 → E25 ; F65/F66 → capture 决策 ; F67 → E28 ; F73 → E2
               [E23/E24/E25/E27 门控: D10=dir ∧ D12=key ∧ D13=embedded]
MCP:           F77/F79 → E4(工具注入) ; F78 → 工具列表 version bump
记忆/技能/压缩: F81/F82 → E11(跨会话读回) ; F83 → E4 ; F84/F85/F86 → 压缩事件(∧F86→E11)
模型:          F87 → E4 ; F88 → E4(切换后仍成功) ; F90 → oauth 铸 token ; F91 → E9
可观测:        F92/F93/F94 → E29 ; F95/F96 → E30
安全:          F98 → E26 (贯穿所有 F,作横切断言)
```

### 门控与遮蔽约束(F×D)

- **O**{S 场景}:一次用例运行在恰一部署场景(D 轴取值确定)。
- **R(可观察性要求)**:
  - `E11/E13/E15/E16/E17/E18`(持久/后台/抢占/死信/跨节点)**要求** D5=durable ∧ 持久后端(D3=dir 或 D2/D4=postgres);S1(纯内存)下这些 F **不可观察**——在 S1 应断言"重启后状态丢失"作对照,或标记 F 不适用。
  - `E18 PG-WAKE` **要求** D4=postgres ∧ D6=pg-notify(S5/S6/S7)。
  - `E21 NET-POLICY` **要求** D15=namespace ∧ D20=deny ∧ bwrap 在(否则 `self-skip`)。
  - `E22 CONTAINER-ROUNDTRIP` **要求** D15=docker ∧ docker 在(否则 `self-skip`)。
  - `E23/E24/E25/E27/E28`(authn/authz/租户/token/erasure)**要求** D10=dir ∧ D12=key(∧ E23/E24 要求 D13=embedded)。
  - `E13 跨协议 durable resume` **要求** worker 池:S4(本地池)或 S8/S9(自托管 worker)。
- **M(遮蔽/参数化不变)**:
  - **后端等价类不改语义**:F1..F12、F13..F17、F24..F31 的 E4/E5/E6/E12 **不受 D2/D3/D4 影响** → 同一断言在 S1/S2/S3/S4 参数化重跑必须**逐字节等价**(EVENT-SEQ deepEqual)。这是"部署不变"的 M 遮蔽:后端差异被持久层吸收,不冒泡到线上语义。
  - **gateway-only 遮蔽 vault**:S9 下 F59/F74(vault/egress-bind)不适用(无 vault),模型退 `NoModelConfiguredExecutor`——断言的是"secretless 边界",非功能缺失。
  - **A2A/AG-UI 错误通道遮蔽**(承接单模块 M12):F16 拒绝只经 `tasks/cancel`;AG-UI 拒绝只经 `ToolMessage.error`——e2e 断言当前遮蔽行为。

---

## 6. 主判定表 —— 场景 × 功能流(参数化覆盖矩阵)

`●`=主场景(仅此可观察该效果);`✓`=参数化重跑(断言同一 E);`—`=不适用/自跳过。行按功能域;列为 14 个部署场景。每个非空格 = 一个 e2e 测试用例。

| 功能流 | S1 | S2 | S3 | S4 | S5 | S6 | S7 | S8 | S9 | S10 | S11 | S12 | S13 | S14 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **F1–F5 managed 生命周期** | ✓ | ● | ✓ | ✓ | ✓ | — | ✓ | ✓ | — | — | — | — | — | — |
| **F6 ai-sdk / F7 ag-ui / F8 a2a / F9 acp / F10 mcp** | ● | ✓ | ✓ | ✓ | ✓ | — | — | — | — | — | — | — | — | — |
| **F11 流式工具输入** | ● | ✓ | — | — | — | — | — | — | — | — | — | — | — | — |
| **F12 全前门并挂** | ● | — | — | — | — | — | — | — | — | — | — | — | — | — |
| **F13 HITL await** | ● | ✓ | — | ✓ | ✓ | — | — | ✓ | — | — | — | — | — | — |
| **F14/F15 批准/拒绝效果** | ● | ✓ | — | ✓ | — | — | — | — | — | — | — | — | — | — |
| **F16 a2a HITL 不对称** | ● | — | — | — | — | — | — | — | — | — | — | — | — | — |
| **F18 durable worker HITL deny** | — | — | — | ● | ✓ | — | — | ✓ | — | — | — | — | — | — |
| **F19 跨协议取消** | ● | ✓ | — | — | — | — | — | — | — | — | — | — | — | — |
| **F20 durable worker 取消** | — | — | — | ● | ✓ | — | — | ✓ | — | — | — | — | — | — |
| **F21 dispose/terminated** | ● | ✓ | — | — | — | — | — | — | — | — | ✓ | ✓ | — | — |
| **F23 ACP interrupt** | — | — | — | — | — | — | — | — | — | ✓ | ✓ | ✓ | — | ● |
| **F24–F28 跨协议连续/多轮/usage/a2a** | ● | ✓ | — | ✓ | ✓ | — | — | — | — | — | — | — | — | — |
| **F29 跨协议上游故障** | ● | — | — | — | — | — | — | — | — | — | — | — | — | — |
| **F30/F31 跨线程/多协议并发** | ● | ✓ | — | ✓ | ✓ | — | ✓ | — | — | — | — | — | — | — |
| **F32 重启存活** | — | ● | ✓ | ✓ | ✓ | — | — | — | — | — | — | — | — | — |
| **F33 durable 跨协议恢复** | — | — | — | ● | ✓ | — | — | ✓ | — | — | — | — | — | — |
| **F34 pg 提交存活** | — | — | — | — | ● | ✓ | ✓ | ✓ | — | — | — | — | — | — |
| **F35 pg wake 跨节点** | — | — | — | — | ● | — | ✓ | ✓ | — | — | — | — | — | — |
| **F36 抢占 supersede** | — | ✓ | — | ● | ✓ | — | ✓ | ✓ | — | — | — | — | — | — |
| **F37 调度动作** | — | ✓ | — | ● | ✓ | — | — | — | — | — | — | — | — | — |
| **F38 死信** | — | — | — | ● | ✓ | — | — | ✓ | — | — | — | — | — | — |
| **F39 durable 终态故障** | — | ✓ | — | ● | ✓ | — | — | — | — | — | — | — | — | — |
| **F40 durable 池后台驱动** | — | — | — | ● | ✓ | — | ✓ | ✓ | — | — | — | — | — | — |
| **F41 soak 公平性** | — | — | — | ✓ | ● | — | ✓ | — | — | — | — | — | — | — |
| **F42 优雅 drain** | — | — | — | ✓ | ✓ | — | ● | ● | ✓ | — | — | — | — | — |
| **F43 SSE reconnect** | ● | ✓ | — | — | — | — | — | — | — | — | — | — | — | — |
| **F44 分组件 DB 持久** | — | ● | — | — | ✓ | — | — | — | — | — | — | — | — | — |
| **F46 worker poller 兼容** | — | — | — | — | ✓ | — | — | ● | ✓ | — | — | — | — | — |
| **F47/F52/F53/F54 供给·挂载·生命周期** | ✓ | ✓ | — | — | — | — | — | — | — | ● | ✓ | ✓ | ✓ | — |
| **F48 fail-closed 悬挂引用** | ● | ✓ | — | — | — | — | — | — | — | ✓ | ✓ | ✓ | — | — |
| **F49/F99 NET-POLICY egress none** | — | — | — | — | — | — | — | — | — | ● | — | ✓ | ✓ | — |
| **F50 容器 agent 往返** | — | — | — | — | — | — | — | — | — | — | ● | — | — | — |
| **F51 acp sandboxed** | — | — | — | — | — | — | — | — | — | ● | ✓ | ✓ | — | ✓ |
| **F55 k8s pids fail-closed** | — | — | — | — | — | — | — | — | — | — | — | ● | — | — |
| **F56 暖池复用 / F57 reaper 回收** | — | — | — | — | — | — | — | — | — | ✓ | ● | ✓ | — | — |
| **F58 config CRUD/validate/resolve** | ✓ | ● | — | — | ✓ | — | — | — | — | — | — | — | — | — |
| **F59 vault 封印 / F74 egress bind** | — | ● | — | — | ✓ | — | — | ✓ | — | — | — | — | — | — |
| **F60 seal-key 必需(拒启 N3)** | — | ● | ✓ | — | — | — | — | — | — | — | — | — | — | — |
| **F61 IAM authn / F75 bootstrap token** | — | ● | — | — | ✓ | — | — | ✓ | — | — | — | — | — | — |
| **F62 authz 栅栏** | — | ● | — | — | ✓ | — | — | — | — | — | — | — | — | — |
| **F63/F64 跨租户隔离** | — | ● | — | — | ✓ | — | — | — | — | — | — | — | — | — |
| **F65/F66 consent/capture** | — | ● | — | — | — | — | — | — | — | — | — | — | — | — |
| **F67 GDPR erasure** | — | ● | — | — | ✓ | — | — | — | — | — | — | — | — | — |
| **F68/F69/F76 profiles/enroll/sessions** | — | ● | — | — | ✓ | — | — | — | — | — | — | — | — | — |
| **F70 deployments + schedule** | — | ● | — | — | ✓ | — | — | — | — | — | — | — | — | — |
| **F71 worker 注册运行(3 类)** | — | ✓ | — | — | ✓ | — | — | ● | ✓ | — | — | — | — | — |
| **F72 environments + env-work** | — | ● | — | — | ✓ | — | — | ✓ | — | — | — | — | — | — |
| **F73 不兼容凭证拒绝** | — | ● | — | — | ✓ | — | — | — | — | — | — | — | — | — |
| **F77–F80 MCP 注入/refresh/acp/mgmt** | ● | ✓ | — | — | ✓ | — | — | — | — | — | — | — | — | ✓ |
| **F81/F82 memory 召回·提取 durable** | — | ● | ✓ | — | ✓ | — | — | — | — | — | — | ✓ | — | — |
| **F83 技能激活 durable** | — | ● | ✓ | — | — | — | — | — | — | — | — | — | — | — |
| **F84–F86 压缩(token/model/durable)** | ✓ | ● | — | — | ✓ | — | — | — | — | — | — | — | — | — |
| **F87 模型路由 / F89 覆盖·继承** | ● | ✓ | — | — | ✓ | — | — | — | — | — | — | — | — | — |
| **F88 模型池故障转移** | ● | ✓ | — | — | ✓ | — | — | — | — | — | — | — | — | — |
| **F90 oauth 刷新 / F91 上游故障** | ● | ✓ | — | — | — | — | — | — | — | — | — | — | — | — |
| **F92–F94 trace 捕获/传播/派工** | ✓ | ● | — | ✓ | ✓ | — | — | ✓ | — | — | — | — | — | — |
| **F95/F96 metrics/usage 导出** | ● | ✓ | — | ✓ | ✓ | — | — | ✓ | — | — | — | — | — | — |
| **F98 密钥不泄漏(横切)** | ✓ | ● | ✓ | ✓ | ✓ | — | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |

> `●` 列合计 ≈ 每功能域的**主用例**;`✓` 为**参数化重跑**用例。矩阵非空单元合计 **约 180 个 e2e 测试用例**。

---

## 7. 代表性域判定表(断言级,展开主场景)

主矩阵给"哪跑",下列域表给"跑出什么果"——每列一个用例,行为 F 触发因(1/0)与 E 断言果。

### 7.1 HITL(主场景 S1/S4)

| 因\用例 | H1 | H2 | H3 | H4 |
|---|---|---|---|---|
| F13 工具触发审批 | 1 | 1 | 1 | 1 |
| F14 批准 | 0 | 1 | 0 | 0 |
| F15 拒绝 | 0 | 0 | 1 | 0 |
| F18 durable worker(await 跨进程) | 0 | 0 | 0 | 1 |
| **E7 HITL-AWAIT(requires_action)** | 1 | 1 | 1 | 1 |
| **E8 读回含 SENTINEL** | - | 1 | 0 | - |
| **E5 end_turn(拒绝后仍收尾)** | - | 1 | 1 | 1 |
| **E13 跨进程 resume→done** | 0 | 0 | 0 | 1 |

### 7.2 持久 / 分布式(主场景 S4/S5/S7)

| 因\用例 | P1 | P2 | P3 | P4 | P5 | P6 |
|---|---|---|---|---|---|---|
| F32 重启同 dir | 1 | 0 | 0 | 0 | 0 | 0 |
| F34 pg 换本地 dir | 0 | 1 | 0 | 0 | 0 | 0 |
| F35 pg wake 跨节点 | 0 | 0 | 1 | 0 | 0 | 0 |
| F36 抢占 | 0 | 0 | 0 | 1 | 0 | 0 |
| F38 死信(max attempts) | 0 | 0 | 0 | 0 | 1 | 0 |
| F42 优雅 drain | 0 | 0 | 0 | 0 | 0 | 1 |
| **E11 RESTART-SURVIVE** | 1 | 1 | 0 | 0 | 0 | 0 |
| **E18 PG-WAKE 自主驱动** | 0 | 0 | 1 | 0 | 0 | 0 |
| **E15 SUPERSEDE-DROP(无双提交)** | 0 | 0 | 0 | 1 | 0 | 0 |
| **死信终态可 requeue** | 0 | 0 | 0 | 0 | 1 | 0 |
| **drain:在飞完成,不接新** | 0 | 0 | 0 | 0 | 0 | 1 |

### 7.3 沙箱隔离(主场景 S10/S11/S12)

| 因\用例 | B1 | B2 | B3 | B4 | B5 |
|---|---|---|---|---|---|
| F47 资源挂载供给 | 1 | 0 | 0 | 0 | 0 |
| F49 egress none(namespace) | 0 | 1 | 0 | 0 | 0 |
| F50 容器往返(docker) | 0 | 0 | 1 | 0 | 0 |
| F55 k8s pids | 0 | 0 | 0 | 1 | 0 |
| F48 悬挂引用 | 0 | 0 | 0 | 0 | 1 |
| **E19 SANDBOX-MOUNT** | 1 | 0 | 0 | 0 | 0 |
| **E21 NET-POLICY net=DOWN** | 0 | 1 | 0 | 0 | 0 |
| **E22 CONTAINER MARKER** | 0 | 0 | 1 | 0 | 0 |
| **k8s pids fail-closed(拒)** | 0 | 0 | 0 | 1 | 0 |
| **E20 FAIL-CLOSED create≥400** | 0 | 0 | 0 | 0 | 1 |

### 7.4 管理面安全(主场景 S2)

| 因\用例 | M1 | M2 | M3 | M4 | M5 | M6 |
|---|---|---|---|---|---|---|
| F61 IAM authn(过期/吊销) | 1 | 0 | 0 | 0 | 0 | 0 |
| F62 authz 栅栏(ws 不符/未映射) | 0 | 1 | 0 | 0 | 0 | 0 |
| F63 跨租户隔离(同 id 外写) | 0 | 0 | 1 | 0 | 0 | 0 |
| F67 GDPR erasure | 0 | 0 | 0 | 1 | 0 | 0 |
| F60 durable 无 seal-key | 0 | 0 | 0 | 0 | 1 | 0 |
| F98 密钥不泄漏 | 0 | 0 | 0 | 0 | 0 | 1 |
| **E23 AUTHN 401/200** | 1 | 0 | 0 | 0 | 0 | 0 |
| **E24 AUTHZ-FENCE 403** | 0 | 1 | 0 | 0 | 0 | 0 |
| **E25 TENANT-ISO(no-op+404)** | 0 | 0 | 1 | 0 | 0 | 0 |
| **E28 ERASURE 幂等** | 0 | 0 | 0 | 1 | 0 | 0 |
| **拒启(N3)** | 0 | 0 | 0 | 0 | 1 | 0 |
| **E26 SECRET-NONLEAK** | 0 | 0 | 0 | 0 | 0 | 1 |

### 7.5 可观测(主场景 S2/S5)

| 因\用例 | O1 | O2 | O3 | O4 |
|---|---|---|---|---|
| F92 trace 捕获(span 树) | 1 | 0 | 0 | 0 |
| F93 trace 传播(入站 traceparent) | 0 | 1 | 0 | 0 |
| F94 派工 trace(wake.dispatch) | 0 | 0 | 1 | 0 |
| F95 metrics 导出 | 0 | 0 | 0 | 1 |
| **E29 TRACE-TREE 连通+路由覆盖** | 1 | 1 | 1 | 0 |
| **E29 传播续接** | 0 | 1 | 0 | 0 |
| **E30 METRIC-EXPORT + 关停不挂** | 0 | 0 | 0 | 1 |

---

## 8. 覆盖与使用说明

1. **测试用例 = 主矩阵非空单元**:每个 `●`/`✓` 是一个可执行 e2e 用例,骨架照 `harness.mjs`——`spawnServer(mode, port, extraEnv)` 起真实二进制、SDK 或裸 fetch 驱动、`stopServer` 等进程真退出、按 §4 效果类断言。约 **180 个用例**覆盖 99 条功能流 × 14 部署场景的有效交集。
2. **参数化压制组合爆炸**:14 场景 × 99 流 = 1386 理论格,由 R(可观察性要求)/M(部署不变遮蔽)约束压到 ~180 有效用例。`✓` 列是"同断言跨后端重跑"(部署不变),`●` 列是"仅此可观察"(部署特有)。
3. **每条约束边配否定用例**:§2 的 N1–N10 是**部署级 fail-closed 用例**——违规组合必须拒启/panic/403/400,专测"配置错误不 fail-open"。这是分布式系统最易腐的面。
4. **横切断言**:`F98 密钥不泄漏`在**每个** durable 场景重跑(SENTINEL 扫 SDK+HTTP+stdout+stderr+trace);`F92 trace 连通`在任何跑真轮的场景可附加。
5. **自跳过是覆盖声明而非通过**:`E21/E22`(bwrap/docker 缺失自跳过)必须在 CI 打印"skipped: no bwrap/docker",否则被误读为"已覆盖"——按 e2e 铁律,未跑真依赖就是没覆盖。
6. **后端等价类一致性**:S1/S2/S3/S4 上的 `F1..F12` 必须产出**逐字节等价** EVENT-SEQ;S5/S7 上 `F34/F35/F41` 验证 `FOR UPDATE SKIP LOCKED` owner 租约的 exactly-once。建议以同一断言函数参数化后端跑一致性测试(MemoryDispatchStore 是 pg 必须匹配的可执行规格)。

---

## 9. 执行结果与残余范围(2026-07-18 补齐)

基于本设计与配套单模块设计做了一轮缺口分析 → 补齐 → 真验证。**结论:代码库整体已达 A 级,绝大多数判定表/矩阵格已有测试;缺口分析代理报出的多数"缺口"是假阳性(漏看文件底 `#[cfg(test)]`),需逐个直接核验。** 真缺口已补齐并真验证:

**新增并已验证通过(真实二进制/进程/HTTP/磁盘):**

| 用例 | 类型 | 位置 | 验证 |
|---|---|---|---|
| M12/T82 acp 限流保留 `acp_failure` code | 单元 | `awaken-run-executor-acp/src/tests.rs` | ✓ 绿 |
| M6/T44 authz `RequireApproval→Deny` fail-closed 坍缩(抽出 `collapse_session_decision` seam) | 单元 | `awaken-authz-enforce/src/lib.rs` | ✓ 绿(29/29) |
| N6 nats 无 `--features nats` 硬错 | 单元 | `awaken-runtime-host/src/dispatch_backend.rs` | ✓ 绿 |
| N5 durable ingress 落在易失队列→拒启 | e2e | `deployment_config_e2e.mjs` | ✓ 真二进制拒启 |
| N4 embedded IAM 无 data dir→拒启 | e2e | `deployment_config_e2e.mjs` | ✓ 真二进制拒启 |
| F9/F10/F98 acp+mcp+secret-nonleak 跨 fs/durable 后端参数化 | e2e 脚本 | `e2e/package.json` `test:fs`/`test:durable` | ✓ `test:fs` 全 8 文件绿 |

**既有测试在装备环境下真跑确认(此前无依赖时自跳过 = 未覆盖):**

| 流 | 效果 | 命令 | 结果 |
|---|---|---|---|
| F49/F51 bwrap net-policy | E21 net=UP/DOWN | `node acp_sandboxed_e2e.mjs` | ✓ 真 bwrap `--unshare-net` |
| F47/F48/F54 沙箱供给+反向通道+fail-closed | E19/E20 | `node sandbox_provisioning_e2e.mjs` | ✓ |
| F50 容器 agent 往返 | E22 MARKER | `node managed_container_agent_e2e.mjs` | ✓ 真 Docker |
| F52 FUSE 内核挂载机制 | E19 | `cargo test -p awaken-sandbox-memoryd`(`kernel_vfs.rs` 有 /dev/fuse 时真挂载) | ✓ 3+44 绿 |

> 教训:**"自跳过"必须在 CI 打印 `skipped: no bwrap/docker/fuse`**。本轮在装备了 bwrap+docker+/dev/fuse+k3d 的机器上真跑,证实这些路径确实工作——在缺依赖的 CI 上它们只是声明覆盖,不等于验证。

**残余范围(需 k8s 活集群 / 重特性构建,机制已被单测覆盖,故文档化而非造 stub):**

- **N7b**(`--features container-k8s` 构建下 k8s tier 缺 `AWAKEN_K8S_AGENT_ADDR` 报错):错误在连集群前返回,无需集群,但需重编 kube 依赖。机制同 N6 模式;k8s pids fail-closed 已由 `awaken-sandbox-container` 的 `a_pids_limit_is_flagged_unenforceable_on_k8s_so_create_fails_closed`(M8/T55)单测覆盖。运行:`cargo test -p awaken-sandbox-container --features container-k8s`。
- **N10 / F55**(k3d 活集群:封闭 pod 无反向 hand 不可达 / k8s pids create 级 fail-closed):需 `deploy/k3d/*.yaml` + 活集群。正向拓扑已由 `e2e/k3d/topology_e2e.sh`(reverse `--dial`)覆盖;负向为集群测。运行:`bash e2e/k3d/<scenario>_e2e.sh`(需 `k3d cluster create`)。
- **F35/F34/F41 pg 分布式**、**S5–S9 postgres 场景**:需 `AWAKEN_DATABASE_URL` 活 Postgres,现有 `durable_pg_*`/`durable_soak_*` + `deploy/k3d/*postgres*.yaml` 覆盖,pg 限的存储/派工单测在无 DSN 时静默早返回(非跳过声明,已在 M10/M11 文档标注)。

**未发现 fail-open 代码 bug**:所有安全敏感 fail-closed 分支在代码中均存在,仅部分欠测;唯一结构性欠测(M6 `RequireApproval` 因无注入 seam 不可达)已通过抽出 `collapse_session_decision` 纯函数修复并钉住。
7. **与单模块设计的关系**:本份的 F→E 边在跨越模块;每条 F 内部的分支细节(为何 await、为何 fail-closed)由 `cause-effect-graph-test-design.md` 的 112 因/110 果单测护住。两层合起来 = 单元判定表(内部正确)+ e2e 矩阵(集成 × 部署正确)。
