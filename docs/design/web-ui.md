# Web UI 设计与实现方案

> 本文描述 awaken 管理台的当前作用域和信息架构。早期草案中的
> `Workspace ▸ Project` 设计已被 ADR-0048 的 2026-07-10 修订取代，
> 不再是实现依据。

## 1. 产品边界

awaken 的治理层级只有：

```text
Cloud:         Organization -> Workspace -> Managed resources
Single host:   (default Organization, hidden) -> Workspace -> Managed resources
```

awaken 不定义 Project、ProjectId、Project API key、Project 路由或
`ScopeRef::Project`。Agent、Environment、Session、Vault、MemoryStore、
Deployment、Skill、File、模型供给和推理凭证都位于 Workspace 内。

awaken-flow 可以在自己的编排域中定义
`Organization -> Workspace -> Project`，但向 awaken 发起调用前必须把 flow
Project 解析为一个已授权的 awaken Workspace 请求。Project 和 WorkUnit
不得进入 awaken 的 API、配置快照、资源仓储、运行时或授权判断。

这一行为与 Anthropic Managed Agents 一致：stock SDK 使用 `/v1/...`，认证
上下文选择 Workspace，不要求客户端再提供 Project。awaken 的显式
`/v1/workspaces/{workspace_id}/...` 路径只用于自托管管理、调试和平台代理；
它与裸路径访问同一个 Workspace 聚合，不增加一层租户。

## 2. 设计原则

1. **Workspace 是唯一公开作用域。** 所有列表、详情、创建和运行操作都在
   当前 Workspace 下执行。
2. **本地模式隐藏 Organization。** 未登录模式使用部署配置给出的默认
   Workspace；默认 Organization 只用于云端外层归属，不进入资源对象。
3. **认证选择主体，授权判断操作。** API key、Bearer token 或云端登录态由
   PEP 验证，并由 PDP 判断该主体能否在目标 Workspace 执行动作。
4. **资源服务不感知 IAM。** PEP 将已验证的 `WorkspaceId + Operation` 交给
   资源应用服务；资源仓储只维护 Workspace 分区、状态、CAS、引用和回收栅栏。
5. **stock SDK 保持兼容。** 裸 `/v1/...` 从可信认证上下文取得 Workspace；
   本地无登录模式从显式部署配置取得默认 Workspace。请求体不增加
   `organization_id` 或 `project_id`。
6. **配置只解析一次。** Agent 发布时解析模型、凭证引用和资源配置版本；
   Session/dispatch 使用已发布快照，不在运行路径二次解释作用域或策略。

## 3. 信息架构

```text
┌ Sidebar ─────────────────────┐┌ Main ───────────────────────────────┐
│ [A] <workspace>  Workspace ▾ ││ breadcrumb / readiness / actions   │
│ [ Search…              ⌘K ] ││                                    │
│ Home                         ││ selected Workspace surface         │
│ RUN                          ││                                    │
│  Sessions                    ││                                    │
│  Agents                      ││                                    │
│  Environments                ││                                    │
│  Deployments                 ││                                    │
│ RESOURCES                    ││                                    │
│  Files                       ││                                    │
│  Memory stores               ││                                    │
│  Skills                      ││                                    │
│  Vaults                      ││                                    │
│ SUPPLY                       ││                                    │
│  Models                      ││                                    │
│  Credentials                 ││                                    │
│  MCP / A2A                   ││                                    │
│ GOVERN / OBSERVE             ││                                    │
│  Access · Audit · Evals      ││                                    │
│  Settings                    ││                                    │
└──────────────────────────────┘└────────────────────────────────────┘
```

- 云端登录时可切换用户有权访问的 Workspace。
- 单机无登录模式不显示 Organization 选择器；只有一个配置好的默认 Workspace
  时也可隐藏 Workspace 切换器。
- 自定义本地 IAM 属于高级部署配置，只在 awakenworks.com 运维文档说明，产品
  首次使用流程不引导用户开启。
- Admin assistant 使用浮层，不成为另一种作用域或资源所有者。

### 3.1 部署能力决定供给呈现

本地和 Cloud 使用同一模型目录与发布契约，但不向用户呈现相同的基础设施任务：

| 部署能力 | 模型目录 | 推理凭证 / Provider 连接 | Endpoint / Dialect | 模型测试 |
|---|---|---|---|---|
| 本地 / BYOK | 显示已发现模型 | 显示并允许配置 | 作为高级连接事实显示 | 建立临时 Session，自动发送一次真实请求；失败可重试 |
| Cloud 托管 | 按原始 Provider 分组显示全部已发布模型 | 不显示；由 Cloud 托管 | 不显示；属于运维路由事实 | 不提供连接配置测试；实际 Agent Run 是用户可执行验证，健康探测属于 Cloud Operations |

Cloud 托管时 Models 归入 Build；不呈现空的 AI Supply 分组，也不把目录缺失解释为
租户需要配置 Provider。`ConfigCapabilitiesView.models` 是唯一呈现开关。前端不得根据域名、环境变量或
目录内容猜测部署模式，也不得另存一份 `is_cloud` 状态。Cloud 模型列表是
Cloud 已确认 Provider 路由的只读投影；隐藏 Endpoint 和凭证不会改变发布或
执行事实。

模型测试只在 `byok_enabled=true` 时出现。它不是目录健康检查：创建临时
Session 后必须自动提交一条固定的无工具测试消息，并显示创建中、执行中、失败和
重试结果。Cloud Hosted 的模型健康由 Fleet / Provider Operations 观测，普通用户
通过正常 Agent Run 验证使用结果，不能从管理 UI 绕过 Hosted admission、配额或
计费边界直连 Provider。

### 3.2 进程组合能力决定页面呈现

同一个前端可以由本地 `AllInOne` 或云端拆分的 `Control` 提供，但不能把当前
origin 无法到达的页面显示成可用后再以 `404 Not Found` 失败。前端只读取
`ConfigCapabilitiesView.surfaces`：

| 组合事实 | 呈现结果 |
|---|---|
| `managed_runtime=true` | 当前 origin 本地挂载或通过已声明的 hosted facade 到达 canonical Coordinator；显示 Sessions、Environments、Deployments、Skills、Memory、Vaults、协议、A2A 和运行助手 |
| `managed_runtime=false` | 当前 origin 没有 canonical Managed runtime 路由；仅保留 Agent/模型配置页面，旧深链接回到 Workspace 概览 |
| `access_management=true` | 显示当前进程拥有的 Access/token 管理页 |
| `access_management=false` | 隐藏 Access；远端 IAM 的账号/成员管理回到套件入口 |

侧栏、命令面板、Settings、浮动助手和直接路由共用这一份判定，不允许分别维护
页面清单。Cloud HostedRun/Flow 不是 `/v1/sessions` 的兼容实现；hosted facade
只能把 Awaken 导出的运行时 route profile 转发到 canonical Coordinator，不能增加
翻译代理或第二套资源 API。进程本地 mount 与 origin 可达性是两个事实，前者不能
再被用作后者的替代判定。

运行时 route profile 只从 `ROUTE_POLICIES` 中保存的 canonical flat matcher 导出。
每个 matcher 同时生成 flat 路径和
`/v1/workspaces/{workspace_id}/...` 显式 Workspace wrapper；已有参数必须保留，
例如 Environment work 导出包含 `workspace_id` 与 `environment_id` 的双参数模板。
Cloud 只编译这些 matcher，不维护 Workspace 路径清单。

## 4. 路由和寻址

| 用途 | 路径 | Workspace 来源 |
|---|---|---|
| Anthropic SDK 兼容 | `/v1/agents`, `/v1/sessions`, ... | 已验证 token/login，或本地默认 Workspace |
| 显式管理 | `/v1/workspaces/{workspace_id}/agents`, ... | 路径 Workspace，必须与认证权限一致 |
| 配置面 | `/v1/config/...` 或 Workspace 显式路径 | 同一 Workspace 上下文 |

禁止新增：

- `/projects/{project_id}/v1/...`；
- `project_id` 请求字段或持久化列；
- Project-bound API key；
- 通过“默认 Project”模拟 Workspace；
- 从资源 ID 反推或猜测 Workspace。

## 5. 资源与授权正交

```text
request
  |
  v
AuthN / API-key validation
  | principal + credential attributes
  v
PEP ---------------------> PDP / PIP
  |                         policy decision
  | trusted WorkspaceId + typed operation
  v
Application service
  |
  v
Workspace-scoped repository
  |-- ownership partition
  |-- lifecycle / CAS
  |-- immutable config history
  |-- references / reclamation fence
  `-- no principal, role, token, policy, Org, Project, WorkUnit
```

UI 只根据能力结果显示、禁用或解释操作；它不是授权边界。服务端 PEP 必须对
每次请求重新执行判断。资源返回 404 的跨 Workspace 访问不能因 owner 缓存丢失
而退化为可读；仓储查询始终包含 Workspace 分区。

## 6. 页面与资源生命周期

| Surface | Authoring | Runtime use | Terminal handling |
|---|---|---|---|
| Agents | Workspace 内 draft/update/publish/version | Session 引用已发布配置 | archive 阻止新 Session，历史保留 |
| Sessions | 选择 Agent 和临时输入 | 使用冻结配置；事件是提交事实投影 | archive/delete 释放绑定和沙箱 |
| Files | 上传不可变内容 | Agent 或 Session 可绑定；内容 ID 固定 | 逻辑删除后按引用安全回收 |
| Memory stores | 发布行为配置版本 | Session 固定配置版本，内容保持可变 | deny/archive/delete 后提取 fail closed |
| Repositories | 保存地址、凭证引用和 clone 策略 | 每次 Session/Run clone 当时远端状态 | 释放时按策略写回/丢弃，不 pin commit |
| Skills | 发布二进制安全 bundle 版本 | Session 固定版本并只读物化 | 旧版本按存活引用保留 |
| Vaults/Credentials | Workspace 内写入，读回脱敏 | PEP 后由 host/egress 注入 | revoke/archive 立即阻止新使用 |

File 可以作为 Agent 默认输入，也可以在 Session 创建或允许的运行阶段临时绑定。
Agent 只 pin File 身份/配置；不可变 File 的内容 ID 本身就是内容版本。Memory 和
Repository 是可变资源，只 pin 它们的配置版本，不 pin 内部内容或 commit。

## 7. 前端工程约束

- OpenAPI/codegen 是 wire 类型单一来源；禁止手写另一套 Project DTO。
- `paths.ts` 是导航和路由 SSOT；路径参数只有 Workspace 和资源 ID。
- 单一 fetch/SDK 出口附加认证信息；业务组件不解析 token 或实现权限规则。
- append-only 事件 reducer 只投影已提交事实。
- secret 创建只显示一次；列表、详情、日志和 telemetry 永不回显明文。
- capability/authorization 结果用于 UX，服务端拒绝仍是最终事实。
- `byok_enabled=false` 时隐藏推理凭证、Provider 连接和网络路由细节；直接访问旧
  凭证 URL 回到 Models，不形成第二套 Hosted 凭证页面。
- Workspace 标题只显示人类可读标签或有界缩略值；完整 opaque id 仅保留在
  `title`/诊断上下文中。侧栏在所有断点禁止横向滚动。
- 未提供的后端能力以明确 capability gate 展示，不发明占位领域对象。
- 可选的托管套件入口只读取 Foundation `SuiteNavigation`；产品 Shell 不推导
  Cloud 域名、不读取 sibling 产品拓扑，也不复制 Cloud 的 Products/Billing 页面。
  直接访问的当前 URL 只通过 Foundation 生成的规则作为 opaque continuation
  交回 Hub；Awaken 不校验或持久化它，也不据此构造 OAuth/Workspace 路由。

## 8. 验收条件

1. 代码和 schema 不含 awaken Project 类型、`project_id` 或 Project 路由。
2. 裸 Managed Agents SDK 路径与显式 Workspace 路径访问同一聚合。
3. 跨 Workspace 读写、版本查询、归档、挂载和回收全部 fail closed。
4. 无登录单机模式使用默认 Organization/Workspace，并隐藏 Organization。
5. 云端登录、本地无登录和高级自定义 IAM 共用相同资源服务与 Workspace 仓储。
6. flow Project 只在 flow 内出现；ACL/adapter 投影后 awaken 请求不携带 Project。
7. UI、API、ADR 和代码使用一致的 Workspace 术语。

权威边界见：

- `docs/adr/0048-iam-host-adoption-org-workspace-path-alignment-and-a2a-carve-out.md`
- `docs/adr/0061-selectable-identity-and-platform-managed-resource-scopes.md`
- `docs/adr/0062-published-inference-access-and-runtime-credential-injection.md`
- `docs/adr/0063-resource-input-identity-configuration-pinning-and-lifecycle.md`
- `docs/design/resources-memory-files-skills.md`
