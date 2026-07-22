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
- 未提供的后端能力以明确 capability gate 展示，不发明占位领域对象。

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
