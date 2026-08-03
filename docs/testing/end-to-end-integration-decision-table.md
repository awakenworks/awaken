# 端到端集成不变量判定表

本文承载完整因果图设计中模块 M15 的约束和判定表，因 C119–C129 与果
E117–E127 的定义及因果边见
[`cause-effect-graph-test-design.md`](./cause-effect-graph-test-design.md)。

## 约束

- **O**：C125 的 absent/equal/different 恰一成立；C120 的 basic/full、C121 的
  upload/artifact 及 C122 的 File/Memory-or-Repository 分区均互斥。
- **R**：内容断言要求 `view=full`；不同模型覆盖要求新的 immutable publication，
  单独字符串不构成执行证据；Resource admission 要求 substrate 的真实 capability。
- **M**：transport/status 失败遮蔽任何业务字段断言；immutable publication 遮蔽
  后续 catalog 写入对既有 Session binding 的影响。

## 判定表 M15

| 因\用例 | T108 | T109 | T110 | T111 | T112 | T113 | T114 | T115 | T116 | T117 | T118 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| C119 多次 resume | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C120 basic/full | - | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C121 upload/artifact | - | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C122 File/Memory/Repo | - | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C123 显式 binding | - | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| C124 同 ID 跨 scope | - | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| C125 模型三分支 | - | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| C126 动态路由 | - | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| C127 假绿诱因 | - | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| C128 beta gate | - | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| C129 旧 Hand 路径 | - | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| **对应果** | E117 | E118 | E119 | E120 | E121 | E122 | E123 | E124 | E125 | E126 | E127 |

## 模块 M16 · 跨拓扑隐私、发布与 Cloud PEP

### 因(C130–C137)与果(E128–E135)

| ID | 因 | 果 |
|---|---|---|
| C130 | attempt 经 direct / durable / recovered / resume 任一拓扑交付 | E128 四条路径复用同一 attempt-context decorator，只替换 claim/commit authority |
| C131 | 请求有 `user_profile_id`，部署上限 Full，主体 consent Full，capture sink 已装 | E129 prompt/completion 以精确主体入库，擦除删除正数记录且重试回执幂等 |
| C132 | 远端 IAM 保护 Catalog / 普通配置 / Resource 三类路由 | E130 分别发出 `model_supply.read` / `workspace.read` / resource action，scope 保持 trusted Workspace |
| C133 | Profile 等依赖改变导致 fingerprint 改变；Agent source revision 相同/已提升 | E131 同 revision 不同 fingerprint 冲突；提升 revision 后新 publication/register 成功 |
| C134 | Provider credential 的已证明 endpoint 与新 connection endpoint 相同/不同 | E132 相同可复用；不同返回 422，须新 secret/OAuth proof，不能把 provider 相同当兼容 |
| C135 | Cloud 上游返回 malformed/unsupported projection | E133 整次 refresh 503 且旧快照不变；不是调用方 422 |
| C136 | 协议路由要求 beta，header 缺失/存在 | E134 缺失先返回 400；存在才允许业务/PEP 断言，不能把 beta gate 当授权结果 |
| C137 | PDP 返回 Deny / RequireApproval / Allow | E135 前两者在 management/resource PEP 均 403 fail-closed；仅 Allow 进入 handler |

### 约束与判定表 M16

- **O**：C130 的四种拓扑、C132 的三类 action、C137 的三种 decision 各自互斥。
- **R**：E129 要求先断言数据库存在主体行再擦除；E131 的不同 fingerprint 成功必须有新 source revision；E135 必须选 beta-independent 路由或先满足 C136。
- **M**：beta gate 遮蔽 handler/PEP 结果；相同 registration identity 的 fingerprint conflict 遮蔽 current pointer 更新；withdrawn/缺失 consent 遮蔽内容写入。

| 因\用例 | T119 | T120 | T121 | T122 | T123 | T124 | T125 | T126 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| C130 direct/durable | direct | durable | - | - | - | - | - | - |
| C131 attribution×Full×consent×sink | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| C132 route family | - | - | 三分区 | - | - | - | - | - |
| C133 dependency/fingerprint/revision | - | - | - | 同/新 revision | - | - | - | - |
| C134 exact endpoint proof | - | - | - | - | 同/不同 | - | - | - |
| C135 invalid Cloud projection | - | - | - | - | - | 1 | - | - |
| C136 beta absent/present | - | - | - | - | - | - | 1 | present |
| C137 PDP decision | - | - | action | - | - | - | - | deny/approval/allow |
| **对应果** | E128/E129 | E128/E129 | E130 | E131 | E132 | E133 | E134 | E135 |

测试证据：`attributed_run_meets_deployment_capture_with_control_consent`、
`management_capture_erasure_loop_e2e.mjs`、authorization route-table tests、
executable registration R1–R6、ProviderConnection compatibility tests，以及
`management_cloud_iam_e2e.mjs` 的 publication/Cloud projection/PDP 矩阵。

## 端到端 FMECA 续表

| ID | 失效模式与影响 | 消解/处理 | 判定表与测试证据 | S/O/D/RPN |
|---|---|---|---|---|
| EF24 | durable/recovered attempt 丢主体、consent 或 sink，而 direct 正常 → 运行成功但零内容记录，GDPR 擦除回执为 0 | 删除 direct/durable 两套上下文装配；唯一 executor decorator 每 attempt 解析 privacy context；静态 context 只建一次 | M16 T119/T120；host 单测 + 真实 run→DB row→erase E2E | 5/1/1/5 |
| EF25 | Catalog 被误映射为普通 Workspace action（或反向）→ hosted role 过权/欠权 | 权威 route table 把 model-supply、Workspace、Resource action 分区；Cloud E2E 断言 PDP 原始请求 | M16 T121；authz route table + Cloud IAM E2E | 5/1/1/5 |
| EF26 | 外部 Profile 改变但复用同一 Agent source revision → 新 fingerprint 先持久化、registration 冲突而不可执行 | `(Workspace,Agent,revision)` identity immutable；不同 fingerprint fail-closed；要发布新依赖必须推进 authoring revision；reconciler 重放同一 immutable value | M16 T122；registration lifecycle R2/R3/R5/R6 + publication matrix | 4/1/2/8 |
| EF27 | 把同 Provider 的 endpoint-scoped credential 复用于另一 endpoint → 未经证明的 secret route | CredentialSource 冻结精确 endpoint；ProviderConnection join 不兼容即 422，新 endpoint 重新完成 write-only proof | M16 T123；ProviderConnection tests + Cloud publication matrix | 5/1/1/5 |
| EF28 | Cloud 上游 projection 非法被报为调用方 422或部分覆盖快照 → 错误重试责任、可用 Catalog 损坏 | 上游 refresh 统一 503；validate 后原子 reconcile；失败保留上一权威快照 | M16 T124；Cloud invalid-projection matrix | 4/1/1/4 |
| EF29 | E2E 未满足 beta gate，却把 400 当 PDP/业务结果 → 授权分支未执行而测试误判 | 前置条件进入因果图；beta 场景显式 header；PDP fail-closed 用 beta-independent Resource + Management 路由交叉验证 | M16 T125/T126；Cloud deny/approval matrix；beta gate tests | 5/1/1/5 |

## M17：外部镜像构建的有界终止

原因 C138：包镜像源、Docker/Podman daemon 或 BuildKit 在构建期间停止响应。结果 E136：权威 sandbox 镜像入口在可配置的 30 分钟构建期限后终止命令并返回稳定状态 124；普通容器验收操作使用独立的 60 秒期限。约束：三个 E2E 调用方继续复用同一个 `deploy/images/sandbox/build.sh`，不各自维护超时策略。

| 规则 | 外部命令在期限内完成 | 期限到达时仍活动 | 结果 | 覆盖 |
|---|---:|---:|---|---|
| T127 | 是 | 否 | 保留原始退出状态 | `build.sh --self-test` H1 |
| T128 | 否 | 是 | TERM、宽限后 KILL，返回 124 | `build.sh --self-test` H2 |

| ID | 失效模式与影响 | 消解/处理 | 判定表与测试证据 | S/O/D/RPN |
|---|---|---|---|---|
| EF30 | 外部包源或构建器停滞使确定性门禁永久悬挂 → 后续协议、管理、durable、环境矩阵全部饥饿，CI 无终态 | 唯一构建入口统一有界；超时 fail closed，允许慢宿主显式覆盖 | M17 T127–T128；完整 deterministic E2E | 7/4/8/224 |

## M18：SandboxExecutionPolicy 错误分类保持

原因 C139：Environment application 把 typed policy error 转成字符串。结果 E137：缺失 exact policy 的 404、版本冲突 409、无效输入 422、存储不可用 503 跨 application/wire 边界保持，不折叠成通用 422。

| 规则 | domain cause | HTTP | 覆盖 |
|---|---|---:|---|
| T129 | exact policy missing | 404 | Managed/Awaken mapper unit tests + Environment E2E P1 |
| T130 | version conflict | 409 | Managed mapper P2 |
| T131 | disabled/invalid | 422 | Managed mapper P3 |
| T132 | store failed/unavailable | 503 | Managed/Awaken mapper P4/C5 |

| ID | 失效模式与影响 | 消解/处理 | 判定表与测试证据 | S/O/D/RPN |
|---|---|---|---|---|
| EF31 | policy domain error 被字符串化，missing/store failure 均误报 422 → 客户端承担错误重试责任且监控失真 | application 保留 `SandboxExecutionPolicyError`；两个 wire mapper 穷举分类 | M18 T129–T132；`management_environments_e2e.mjs` | 4/3/4/48 |

## M19：Deployment 对 Environment 生命周期的可知边界

原因 C140：本地 launcher 只能读取 Coordinator 的 current executable projection；归档会由 Control 撤回该 projection，不携带归档 tombstone。结果 E138：撤回后本地 launch 稳定返回 `environment_not_found_error`；只有收到权威 lifecycle fact 的 launcher 才能返回 `environment_archived_error`，并由调度器决定是否自动暂停。

| 规则 | launcher 可见输入 | 结果 | 覆盖 |
|---|---|---|---|
| T133 | current projection 缺失/已撤回 | local launch: environment_not_found，manual deployment 保持 active | Deployment E2E D6 |
| T134 | 权威 launcher 明确返回 archived | scheduled 自动暂停；manual 保持 active | DeploymentState F1/F3 |

| ID | 失效模式与影响 | 消解/处理 | 判定表与测试证据 | S/O/D/RPN |
|---|---|---|---|---|
| EF32 | 从 Coordinator 的“无 current projection”猜测 Control 侧已归档 → missing/archived 两种原因混淆，且形成不可达 archived 分支 | 删除本地猜测分支；按端口可知信息返回 not_found；保留外部权威 archived outcome | M19 T133–T134 | 3/3/3/27 |

## M20：Agent tool identity 的唯一执行所有权

原因 C141：inline client tool 与 enabled Agent toolset/catalog tool 使用同一 identity。结果 E139：发布前 400 原子拒绝且不持久化 Agent；identity 唯一时精确保留 client descriptor。

| 规则 | identity overlap | 结果 | 覆盖 |
|---|---:|---|---|
| T135 | 是 | 400，无 Agent side effect | compile decision table + Agent E2E C1 |
| T136 | 否 | 发布并往返完整 client-tool contract | Agent E2E C2 |

| ID | 失效模式与影响 | 消解/处理 | 判定表与测试证据 | S/O/D/RPN |
|---|---|---|---|---|
| EF33 | 同一 tool identity 同时归 host/toolset 与 client 执行 → 插入顺序决定执行 owner，可能绕过预期权限/沙箱 | 一个 identity 一个 owner；compile fail-closed；E2E 使用唯一名称并显式覆盖 collision | M20 T135–T136 | 7/3/3/63 |

## M21：Managed Agent MCP URL 资源边界

原因 C142：合法 HTTP(S) MCP URL 超过 2048 wire bytes。结果 E140：2048 bytes 包含性边界成功，2049+ 在 authoring 前 400 且无 Agent/revision 副作用；URL 语法仍只由 `McpTarget` 规范化。

| 规则 | URL bytes | 结果 | 覆盖 |
|---|---:|---|---|
| T137 | 2048 | 成功并精确往返 | Control U1 + Agent E2E boundary |
| T138 | 2049 | 400，无持久化 | Control U2 + Agent E2E invalid matrix |

| ID | 失效模式与影响 | 消解/处理 | 判定表与测试证据 | S/O/D/RPN |
|---|---|---|---|---|
| EF34 | 无 MCP URL 长度边界 → 超大 Agent 配置放大存储、hash、projection 与网络开销；测试声明的 max+1 无效 | Control authoring seam 执行 2048-byte 上限；不复制 URL parser | M21 T137–T138 | 4/3/4/48 |

## M22：Models API 的显式配置前提

原因 C143：测试在 production live `ModelDirectory` 上未写入 Catalog，却假设 bare-host 默认模型存在。结果 E141：空 Catalog 投影空列表；只有 Provider Connection 成功发现并原子写入后才列出精确模型。

| 规则 | Catalog facts | discovery | 结果 | 覆盖 |
|---|---:|---:|---|---|
| T139 | 无 | - | 空列表，不回退 fixture defaults | ModelDirectory unit tests |
| T140 | 显式 Provider Connection | 成功 | list/retrieve exact model，missing 404 | Files/Models E2E M2 |

| ID | 失效模式与影响 | 消解/处理 | 判定表与测试证据 | S/O/D/RPN |
|---|---|---|---|---|
| EF35 | E2E 假设默认模型并把空 live Catalog 当实现故障 → 无法验证真正 discovery→catalog→projection 流程 | 共享 fake provider 同时提供 inference/discovery；测试显式创建 Provider Connection | M22 T139–T140 | 3/4/5/60 |

## M23：主 Agent 归档与 Deployment 聚合的单实例接线

原因 C144：AllInOne 先组装 Control Agent 路由、后恢复 Coordinator Deployment 聚合，导致 Agent archive handler 的可选级联边为空。结果 E142：进程组合先从唯一 `DeploymentRepository` 恢复一个 `DeploymentState`；Coordinator 独占该执行聚合，Control 只接收中立 `AgentArchiveCascade` 命令端口；同一实例仍服务路由、launcher、调度器和该端口，不存在第二份内存状态或同步轨道。

| 规则 | Workspace/Agent owner | lifecycle command edge | 结果 | 覆盖 |
|---|---|---|---|---|
| T141 | 相同 | 已绑定唯一聚合 | Agent 请求返回前归档 Deployment；零 Run；后续 run 409 | schedule E2E S6/S7；DeploymentState P1 |
| T142 | Agent 或 Workspace 不同 | 已绑定唯一聚合 | 不改变无关 Deployment | DeploymentState P2/P3 |
| T143 | 相同 | 重复归档通知 | 幂等，新增归档数为 0 | DeploymentState P4 |
| T144 | split Control 无本地边 | 无 | Coordinator 在调度时以 executable registration fail-closed；不构造执行仓储副本 | role/composition tests；missing-Agent scheduler tests |

| ID | 失效模式与影响 | 消解/处理 | 判定表与测试证据 | S/O/D/RPN |
|---|---|---|---|---|
| EF36 | Agent 与 Deployment 单元逻辑均正确，但组合根未把两者的生命周期边接到同一聚合 → 归档 Agent 后定时任务仍可继续触发；若 Control 直接持有聚合又反向污染所有权 | 在组装任何兄弟组件前恢复唯一 Deployment 聚合；Control 只依赖 `AgentArchiveCascade`，Coordinator 保持状态/持久化/调度所有权 | M23 T141–T144；crate-boundary fitness | 6/3/3/54 |

## M24：Environment × Resource 矩阵的物化前提

原因 C145：测试把冻结的网络事实等同于已创建 sandbox workload，并假设 Workdir 能成功承载只读 File。结果 E143：矩阵显式区分“无 sandbox consumer”的 in-process turn 与“File 触发物化”；三个协议适配器必须对同一冻结 baseline 得出相同成功或 fail-closed 结果。

| 规则 | in-process executor | File RO | Workdir RO capability | 结果 | 覆盖 |
|---|---:|---:|---:|---|---|
| T145 | 是 | 否 | 否 | 成功；网络事实冻结但本轮无 sandbox consumer | matrix 12 个 absent rows |
| T146 | 是 | 是 | 否 | 物化前拒绝 `ReadOnlyUnsupported`；不执行模型 | matrix 12 个 File rows |
| T147 | 任意 | 是 | 是 | 可物化；由 namespace/container capability suites 验证 | strict sandbox + pairwise provider suites |

| ID | 失效模式与影响 | 消解/处理 | 判定表与测试证据 | S/O/D/RPN |
|---|---|---|---|---|---|
| EF37 | E2E 把未消费的网络配置当作已执行隔离，并要求弱 Workdir 后端成功挂载只读 File → 既制造假红，又会诱导实现静默降级 | 将 sandbox workload existence 纳入因果图；12 行无资源成功、12 行 File 能力不足 fail-closed；网络强制交给真实 provider 能力门禁 | M24 T145–T147 | 5/4/5/100 |

## M25：Workspace-scoped Agent identity 与跨聚合引用

原因 C146：测试仍把 Agent ID 当全局身份，并期待 B 写同名 Agent 返回 200 后却不可读。结果 E144：Agent 以 `(Workspace, id)` 分区；B 可拥有独立同名草稿，但 publication 必须在 B 已具备可解析模型的前提下重新校验其每个 exact credential 引用，A credential 不得进入 B snapshot。

| 规则 | B 同名草稿 | B 模型前提 | MCP credential owner | 结果 | 覆盖 |
|---|---:|---:|---|---|---|
| T148 | 否 | - | - | B GET/list 404/不含，A 不泄漏 | CLI E2E ownership reads |
| T149 | 是 | 满足 | B | 可独立发布；A 保持不变 | scoped Agent/config unit tests |
| T150 | 是 | 满足 | A | 409 credential unavailable；零 publication/registration | CLI E2E cross-owner publication |
| T151 | 是 | 不满足 | 任意 | model resolution 先失败并遮蔽后续 credential 校验 | publication decision tests |

| ID | 失效模式与影响 | 消解/处理 | 判定表与测试证据 | S/O/D/RPN |
|---|---|---|---|---|---|
| EF38 | 测试/调用方按全局 Agent ID 推断所有权 → 同名合法租户被错误隐藏，或跨 Workspace credential 未在发布边界验证 | identity 统一为 `(Workspace,id)`；authoring 与 publication 分离；显式满足模型前提后验证 credential owner/revision | M25 T148–T151 | 6/3/5/90 |

## M26：远程 Worker 的 A2A 实现与能力清单同源

原因 C147：场景 Worker 未安装 A2A attempt executor，但恢复 E2E 通过一个无人读取的、endpoint-specific `AWAKEN_WORKER_CAPABILITIES` 环境变量声称可执行远程 Agent。结果 E145：删除该并行声明路径；场景 Worker 复用生产匿名 A2A 安装器，标准 Worker builder 仅从实际安装的实现派生有限能力 `a2a-runtime`，调度 admission 与执行路径不可漂移。

| 规则 | A2A executor 已安装 | 手工 capability 覆盖 | 清单/调度结果 | 覆盖 |
|---|---:|---:|---|---|
| T152 | 是 | 无 | 自动发布 `a2a-runtime`，可 claim 并执行任意已冻结 endpoint | scenario Worker topology unit + remote recovery E2E |
| T153 | 否 | 无 | 不发布 `a2a-runtime`，A2A dispatch 不可 claim | `awaken-worker` standard-manifest unit |
| T154 | 否 | 有 | 不提供旁路；环境变量不参与产品清单 | orchestration/static search + scenario topology |

| ID | 失效模式与影响 | 消解/处理 | 判定表与测试证据 | S/O/D/RPN |
|---|---|---|---|---|
| EF39 | Worker 清单声称 endpoint-specific A2A 能力但未安装 executor，或已安装 executor 却只声明旧 URL → dispatch 永久 Pending、恢复流程饥饿，亦可能形成能力虚证 | 删除环境变量旁路；复用唯一生产 A2A adapter；由 canonical builder 从已安装实现派生有限 `a2a-runtime` | M26 T152–T154；lost-receipt→kill A→epoch B recovery→stale-fence E2E | 6/3/2/36 |

## M27：Split Control 启动前提的遮蔽顺序

原因 C148：角色 E2E 想验证 local mode 被 Split Control 拒绝，但遗漏服务边界令牌；安全令牌校验先失败，模式分支未执行。结果 E146：把服务令牌和 executable-registration URL/token 配对纳入因果图；分别验证缺令牌的安全拒绝、满足安全前提后的模式拒绝，以及全部前提满足时只暴露 Control 路由。

| 规则 | Control mode | service token | registration URL/token | 结果 | 覆盖 |
|---|---|---:|---:|---|---|
| T155 | local | 缺失 | 缺失 | 拒绝 `control_service_token_file` | service roles K4a |
| T156 | local | 有 | 缺失 | 安全前提已满足，拒绝 `mode=server` | service roles K4b |
| T157 | server | 有 | 成对存在 | 启动 Control；config 200，Session 404 | service roles K5 |
| T158 | server | 有 | 单边存在 | 配置解析 fail-closed | CLI service-boundary unit tests |

| ID | 失效模式与影响 | 消解/处理 | 判定表与测试证据 | S/O/D/RPN |
|---|---|---|---|---|
| EF40 | 测试遗漏较高优先级启动前提，却把先发生的安全拒绝当目标业务分支 → 覆盖虚证；或为通过测试削弱校验顺序 | 明确遮蔽关系；用真实 0600 token 文件满足前提；每个 fail-closed 原因单独成规则 | M27 T155–T158；`test:coverage-gaps` | 5/3/4/60 |

## M28：Worker 身份权威的持久化与跨进程观察闭环

原因 C149–C152：Coordinator 选择 SQLite/Postgres Worker registry、是否具备持久化坐标、Control 与 Coordinator 是否分进程、私有观察边界是否可达。结果 E147–E151：整个 Coordinator 只打开并显式注入一个 durable `WorkerDirectory`；默认产品不能导出 Memory 实现；split Control 通过既有私有 URL/token 读取窄只读投影，并以同一 fingerprint gate 周期重试发布重算。

| 规则 | backend/topology | 前提或故障 | 结果 | 覆盖 |
|---|---|---|---|---|
| T159 | SQLite Coordinator | 无 `storage_dir` | 启动拒绝，不创建易失身份权威 | `sqlite_worker_authority_is_durable_and_missing_storage_fails_closed` R1 |
| T160 | SQLite Coordinator | 可写路径并重启 | identity/generation/tombstone 从同一 DB 恢复 | 同测试 R2/R3 + registry conformance |
| T161 | Postgres Coordinator | migrate/verify ledger | 同一 PG registry 供 transport/placement/observation | Postgres registry conformance + migration verify |
| T162 | split Control | token 正确/错误、URL 合法/非法 | 正确时精确只读；错误时 401/构造失败 | `boundary_and_remote_adapter_preserve_auth_and_read_only_projection` |
| T163 | split Control | 观察源失败→恢复→未变化 | 不推进 fence；下次轮询重试；相同投影合并 | `observation_source_failure_retries_without_advancing_the_fence` |
| T164 | default/test-support build | 默认/显式 feature | 默认无 Memory API；test-support 仍跑共享 transition conformance | crate-boundary fitness + `memory_registry_conforms` |

| ID | 失效模式与影响 | 消解/处理 | 判定表与测试证据 | S/O/D/RPN |
|---|---|---|---|---|
| EF41 | Coordinator 未初始化时隐式创建进程内 WorkerDirectory → 重启丢 incarnation/generation/tombstone，旧 Worker fence 的历史依据消失，调度观察与 transport 权威也可能取到不同实例 | 删除 `OnceLock` 与隐式 fallback；启动按 typed backend 显式打开 SQLite/PG，一份 `Arc` 注入所有消费者；无持久化坐标拒绝启动 | M28 T159–T161/T164 | 6/3/4/72 |
| EF42 | split Control 读取自己的空内存 registry，而 heartbeat 只到 Coordinator → ACP/credential readiness 永久陈旧，publication 不随 Worker 变化 | 抽出只读 `WorkerObservationSource`；复用现有 Control→Coordinator 私有 URL/token；AllInOne 心跳即时触发，split Control 5 秒轮询同一 fingerprint/retry gate | M28 T162–T163 | 5/4/4/80 |
| EF43 | 产品 Router helper 复制完整启动流程并绕过持久化初始化 → CLI 主路径正常而 public assembly/场景路径 fail-close 或错误降级 | 删除重复 assembly；标准、公开和场景模型入口统一调用唯一 `build_runtime_process_assembly` | 静态单调用审查；CLI 默认/all-feature compile + assembly tests | 6/2/3/36 |
