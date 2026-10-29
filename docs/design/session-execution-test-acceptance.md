# Session 执行与资源测试验收清单

本清单覆盖 Host、MCP、Environment、Memory 及其 SDK、协议、持久化和物理执行边界。源码基准为 `8e600b62ac8d47a01ea6db48d7d00d11cd6a3ee6`，tree 为 `2dc06c1882b54d7792e10678c0626741a4ce84ff`，校准日期为 2026-10-22。本次交付是设计和执行证据校准，不是新增 Rust 全量执行报告。

原始输入是用户提供的《测试设计和执行清单》附件，含 T00–T36。该编号不是仓库预先存在的 37 个可执行测试套件。原始附件身份和摘要保存在 [机器可读清单](session-execution-test-acceptance/catalog.json)；不把它追认成已通过的标准。

原 37 项中，13 项保留，20 项细化，4 项改为公共门禁；另补充 5 项。最终保留 T00–T41 共 42 个需求编号，其中 38 个行为域、4 个公共门禁，展开为 127 个具体场景。数量用于追溯，不是完整性或覆盖率目标。

完整性审查按生产操作列出合法状态转换、参与身份、写入与副作用，以及责任交接；再把适用故障位置映射到 oracle 和实际执行切片。新入口、新状态或新适配器没有对应条目时必须扩展台账，不能因已有 127 个场景而省略。

## 基线之后的进展增补（2026-10-24）

下表只记录 `8e600b62ac` 之后已进入 `origin/main` 的相关提交，用于追溯；**不改变本清单的基线、场景判定和已绑定的执行证据**。各场景的“已验证 / 待处理”状态仍以基线为准，下列提交需要在下一次源码绑定的重新校准中，逐场景核对断言和执行范围后再更新，不能据此把整组标为闭环。

| 提交 | 内容 | 可能影响的编号（按提交说明推断，待逐场景核对） | 当时的执行范围 |
|---|---|---|---|
| `86f84f9aa5` | MCP 准备阶段在取消后保留责任，已提交结果可重放；Windows 原生验证精确 Host 采用与 Memory 恢复 | T16、T17、T28 | 提交门禁；未单独声明非 Relay 各分支的取消、root 替换、迟到结果矩阵已闭环 |
| `e81752fdd4` | Environment 在策略准入前重放已提交的 create，返回不可变命令结果；覆盖 delivered / pending 重放，三个存储后端 | T20、T21 | 提交门禁；update / archive 末步故障和进程崩溃逐路径仍待处理 |
| `c11e11432e` | Control 在生命周期效果之前拒绝不支持的 archive 视图 | T18 | 提交门禁 |
| `04764504f9` | Worker 在零值之后保留已封存的 bootstrap 身份 | T09、T15 | 提交门禁 |
| `38966db48d`、`7cd3267dce`、`e970f4ea9b`、`92c5f3ff28` | Windows 原生 NATS、PostgreSQL 持久性、PostgreSQL HTTP 故障转移、S3 与关键 PostgreSQL 流程的 CI 资格验证 | T34、T35、T37 | Windows / MSVC 原生服务；不覆盖 Linux / FUSE、真实 Kubernetes |

仍保持待处理的主要缺口：MCP 各分支统一的“登记 owner → 外部准备 → 再校验 → 发布 → drain”矩阵；Environment 将“已提交、投递待处理”表达为明确结果；Memory 生产写路径的真实进程崩溃与 PostgreSQL 对应验证；TS / Python SDK 到真实 Awaken 服务的生命周期与恢复。

## 适用范围与设计结论

原清单按风险而不是 crate 组织，覆盖身份、授权、事务、回执和恢复，方向合理。它的不足是混合业务行为、后端维度和平台资格，若直接把它们作笛卡尔积全量运行，会重复而昂贵；仅按组名打勾又会漏掉真正的故障窗口。

T00 是追溯门禁，T34 是后端资格，T35 是平台和执行器资格，T36 是选取及证据门禁。它们约束相关行为，不再各自复制整套业务用例。T37–T41 补充持久格式迁移、容量与背压、镜像交付、缓存能力边界和恢复诊断。文件系统越界、只读/磁盘满和资源耗尽分别细化到 T29、T38，不再为每种错误添加一个顶层组。

这是一份本范围内的正式验收规范，所有条目有判定规则；整体执行验收仍待完成。它不声称覆盖整个产品的全部安全、性能或所有外部托管部署。未来新增状态机、持久格式、授权模式或物理适配器时，应先更新适用性和场景，不能以固定编号数量宣称永远完整。

### 必须明确的语义边界

- 权威事务要求唯一合法提交和原回执重放。外部网络效果不笼统承诺“恰好一次”：按稳定 operation identity 观察/接续；无法证明原结果时保留 uncertainty，拒绝不安全重启。
- 冻结的发布版本、模型、价格和输入身份与动态撤销权分开。只在契约规定的物理效果授权点检查当前动态权威；不把已发出效果在稍后撤权后说成零效果。
- 能力是否必需由启用模式决定。普通 SDK 不必拥有 checkpoint；声明可恢复、stdio、容器或中介能力的模式要在构造/readiness 阶段证明所需依赖。类型存在不代表真实适配器可用。
- Copy 是快照加完整路径/generation 的条件写回；FUSE 打开句柄跟随文件 ID。单 store 快照不推导跨独立 store 的全局快照。进程内失效通知不推导分布式实时一致性。
- 客户端断流不自动等于 Run 取消。每个部署明确 detach/cancel 策略；连接生命周期不能替代业务状态机。
- Volatile 只承担内存语义。SQLite 持久崩溃用真实文件和进程重开；PostgreSQL 用真实服务、独立客户端和 schema。手工丢弃测试 SQL 事务不能代替生产方法的实际崩溃。

## 分层 分级与适用性

复用 [现有测试设计与执行规范](test-validation.md) 的 L0–L4 与 P0–P2。风险分配给行为，不给整个 crate。L0 检查源码/边界/选取；L1 检查规则、属性、API 与组件；L2 检查真实 adapter、事务、迁移和子进程持久恢复；L3 检查原生进程及真实服务组装；L4 检查有界模型、内核/引擎/集群及物理恢复资格。L4 的模型通过不替代 L2/L3 实现执行。

- 快速反馈执行受影响 L0/L1；事务、授权、构造和共享类型变化必须扩大到相关 L2/L3。需要内核、引擎或 formal 的条目由现有 owner 在其资格环境执行。
- P0 表决策、并发、持久失败和实际效果观察按交互关系穷举；独立普通选项可采用等价类、边界值或成对组合，不做无意义全乘积。
- 每个场景先登记 mode、backend、target/features 和责任人，再选择 fixture。未实现的名字不是测试绑定。记录代码责任组件，不虚构某个人已经承接任务；实施时须登记实际维护人。
- 已登记的原生 Windows 证据只覆盖各自声明的范围。后续验证按受影响行为和平台矩阵选择；尚未执行的 Linux/FUSE 等条目继续保持未验收。仅当部署确实不支持且有能力拒绝证明时可记“不适用”，不得记“通过”。
- 后端资格按真实支持范围展开；SQLite 内存库可用于部分组件规则，但不能签收文件崩溃恢复。真实服务 E2E 可以用确定性模型 Provider，不能把路由、授权、事务及传输都替换成 Stub。

## 公共故障位置

| 编号 | 注入位置 | 必须观察 |
| --- | --- | --- |
| F01 | 接纳前拒绝 | 零未授权效果；已有准备责任按原身份补偿 |
| F02 | 同事务提交前失败 | root、事实、版本、outbox、command/receipt 在各自事务内全回滚 |
| F03 | 提交成功但响应丢失 | 原 operation、原 committed revision 和原回执重放 |
| F04 | 意图提交后、效果前进程死亡 | 原意图可发现，责任可继续，零第二接纳 |
| F05 | 效果后、回执前死亡或断网 | 观察原操作；未知保留；不擅自重复效果 |
| F06 | 回执后、ack 前失败 | 只重试原确认，不重复效果 |
| F07 | 锁/网络/凭据等待期间撤权或换 owner | 按定义的 effect gate 再校验，检查效果调用次数 |
| F08 | expiry-1、expiry、expiry+1 | 保持各权威既有精确边界；不同 epoch 不混用 |
| F09 | SQL 忙、只读、受控空间/句柄/连接额度耗尽 | 有界失败、无部分成功、责任保留、peer 不饥饿 |
| F10 | 重复/乱序、未知/畸形事实、分页错误或 cursor 不进展 | 单调拒绝、零猜测性授权、不根据成功前缀决定全集 |

只把适用故障位置用于相应生产入口；纯 codec 不要求启动进程或杀数据库。每个场景的反向控制是必须落地并记录的验收检查，不因列入本文件就视为已执行。

## 执行证据和签收规则

以下记录要求用于 CI、发布和正式验收，由对应工具自动生成。日常直接运行
`cargo check`、`cargo test` 不强制生成、保留或提交这些记录。

证据分四层：测试声明存在、实际执行通过、断言覆盖条款、条款在声明切片完成验收。前一层不自动推出后一层。`current_subset`、`historical_subset` 和套件计数都是证明范围，不是整个场景已验收。

每次资格记录至少包含：源码 SHA/tree 及未提交差异摘要、测试和配套脚本输入、OS/target、features/编译器/链接器、backend/fixture/companion、完整 argv 和环境变量名称（不记录凭据值）、列出与实际执行的具名病例及 occurrence、exit/result/duration/skip、stdout/harness 日志摘要、预期负控、清理结果及源码前后身份。期望失败的负控和子用例必须保留原退出码及预期结果。

具名列表、实际结果与必须用例逐条对齐，零用例、同总数但名字不同、必需 fixture 不可用、意外忽略/跳过、子进程失败和中途源码变动都不能签收。负控要能在删除对应不变量检查时使验证失败，或通过已记录的非法输入/故障注入说明；不得只验证 error.is_err。

历史证明保留原提交、命令、二进制/日志身份。若要继承到新提交，单列 owner、反向依赖、features、编译 flags、fixture 和配置的输入闭包比较及差异审查；未改变一个源文件不等于整个执行输入未变。cache 命中、编译成功和静态守卫通过不代替实际执行。

一个场景签收需要：所有适用 oracle、负控和切片有具名证明，且公共门禁均满足。组签收需要其所有适用场景签收；N/A 有原因、mode 和能力拒绝证明。不得由套件总数生成完成百分比；本次没有测量代码覆盖率。

当前 5 个现有断言子范围已人工核对并绑定原执行记录，分别是 T05-C02、T20-C02、T21-C01/C02/C03。它们不等于扩展后 5 个完整场景或 5 个完整组已签收。新增清单的完整场景及组验收数均为 0；这是严格台账尚待完成，不是仓库没有有效测试。

### 已有证据范围

原始 receipt 保留在本任务的 ignored evidence 目录。下表和 JSON 登记原始相对路径、source 和 SHA256；复核需要取得原始 artifact，干净 clone 没有这些 artifact 时不能自行把证明升级为可用。Markdown 不链接仅存在于本机的 target 文件，避免破坏干净 checkout 文档门禁。

| 证据 | 原源码 | 已执行范围 |
| --- | --- | --- |
| E01 | `8e600b62ac8d` | Environment 与当前 Session/Dispatch/Tool 组件，1064 例，范围内 Windows 三后端 |
| E02 | `489252bc4f3b` | Memory 第二阶段 717 例，原提交 489252bc4f；不是当前提交重新执行 |
| E03 | `489252bc4f3b` | Host/realization/Memory 810 例，原提交 489252bc4f 的 Windows 适用组件 |
| E04 | `8e600b62ac8d` | 当前 Windows 进程树、Git 路径和 Provider admission，25 例 |
| E05 | `8e600b62ac8d` | 当前官方 SDK mock 请求/序列化/传输，48 例；不是实际 Awaken 服务 E2E |
| E06 | `8e600b62ac8d` | 当前 Windows Copy/mounter，24 个唯一病例、双特性 48 次执行；不含内核 FUSE |
| E07 | `8e600b62ac8d` | 当前独立 SDK consumer 8 例、静态/CI/正式模型等分项汇总，不能重复累计子证明 |
| E08 | `8e600b62ac8d` | 当前有界 TLC 4 个正常配置及 9 个预期语义反例；不是任意状态证明 |
| E09 | `489252bc4f3b` | 历史 Resources API 消费者 6 个正反编译控制 |
| E10 | `489252bc4f3b` | 历史文件授权 19 个用例 |

各套件有重叠，不累计为覆盖率。E05 的 48 例不能当作真实服务或外部托管服务通过。E06 的两套 feature 运行是 24 个唯一病例的 48 次执行。E08 只证明所列有界配置与预期反例；Kani、真实集群和 Linux 内核不从中推导。

## 原清单逐项校准

| 编号 | 需求 | 处理 | 校准原因 |
| --- | --- | --- | --- |
| T00 | 清点与边界门禁 | 改为公共门禁 | 治理要求应作为每次变更的公共门禁，不与业务用例混算。 |
| T01 | 构造与关闭 | 细化 | 能力是否必需取决于启用模式；明确构造顺序、实例及关闭责任。 |
| T02 | SDK 独立使用 | 保留 | SDK 独立性仍是发布边界，不能由服务端集成测试替代。 |
| T03 | SDK 等待和结果语义 | 细化 | 区分等待返回、恢复命令接纳及 Run 终态，避免把协议等待当业务完成。 |
| T04 | 创建与错误顺序 | 细化 | 要求已定义的错误优先级；不要为无关错误任意规定全局顺序。 |
| T05 | Session CAS 与回执重放 | 保留 | 保留原子 root 与历史回执语义，分开逻辑竞争和持久崩溃。 |
| T06 | 归属与当前授权 | 细化 | 冻结配置身份与动态撤销权分开验证，减少不必要的重复目录读取。 |
| T07 | Run 接纳与激活 | 细化 | 分别约束权威提交和外部效果，不承诺任意网络副作用恰好一次。 |
| T08 | Work 与 Run 桥接 | 细化 | Work 是领取/租约协议事项；ack 不等于 Run 成功，healthcheck 不强制创建 Run。 |
| T09 | 租约边界与竞争 | 保留 | 不同租约各有状态机；共用时钟和边界规则，不合并 epoch。 |
| T10 | 物理停止与未知结果 | 细化 | 停止证明必须按适配器能力声明，future 被 drop 不等于物理静止。 |
| T11 | 工具回复接纳与重放 | 保留 | 保持 ticket/version/Run/Thread 坐标约束及消费语义。 |
| T12 | 事件批次 | 保留 | 协议语法验证与 aggregate 业务判定保持分层。 |
| T13 | 预算与动态策略 | 细化 | 明确冻结配置与动态限制；不把模型价格变化追溯应用到已接纳操作。 |
| T14 | 资源替换与引用修复 | 保留 | 保留确切 manifest 身份和原子 root；索引是可修复投影。 |
| T15 | 精确凭据与明文边界 | 细化 | 保留身份精确性，同时显式区分宿主内凭据、工作负载凭据和外部中介契约。 |
| T16 | MCP generation 生命周期 | 细化 | 现有 WorkerRelay 保护不能泛化为所有 Native、PlatformRelay、stdio 分支已等价。 |
| T17 | prepare 与补偿责任 | 细化 | 不能依赖尚未创建的 Session root 作为准备资源的唯一清理入口。 |
| T18 | archive 与 delete 竞争 | 保留 | 终态与物理静止分开，retirement 资格观察不得消费 cleanup 责任。 |
| T19 | checkpoint 与续接 | 细化 | 每种 Provider 声明支持的恢复能力；无能力时提前拒绝，不伪造通用恢复保证。 |
| T20 | Environment 定义事务 | 保留 | 定义、不可变 revision 和 intent 是同一事务，不能由 mutable current row 重建历史。 |
| T21 | Environment 注册投递 | 保留 | 明确 at-least-once 投递、单调确切身份及可恢复责任，不要求网络恰好一次。 |
| T22 | Memory 内容事务与作者 | 保留 | 写入、路径覆盖和历史原子；装饰器只在提交后通知，作者不在中间层丢失。 |
| T23 | Memory 一致快照 | 细化 | 快照保证按声明 store/事务范围定义，不承诺跨独立 store 或任意多次请求全局快照。 |
| T24 | Copy 写回与 ABA | 细化 | Copy 按原路径和完整 generation 比较；FUSE 打开句柄跟随 ID，二者不能混为一条规则。 |
| T25 | Memory 历史脱敏 | 保留 | 历史脱敏、普通删除和物理 purge 是三个不同授权操作。 |
| T26 | 内容回收与保护 | 保留 | 删除内容权限不包含物理 purge；检查保护事实的读取失败必须保守拒绝。 |
| T27 | Sandbox 能力与同一物理实例 | 细化 | 按消费者角色约束能力，保留同一物理实例；不要按方法机械拆 trait。 |
| T28 | 物理副作用前的授权 | 细化 | 为每个副作用定义授权线性化点和不确定性边界，而非宣称任意时刻撤权可追溯取消已发出的请求。 |
| T29 | 进程 文件 资源和产物 | 细化 | 增加文件系统与 Windows 路径边界；生命周期、产物来源和数据责任分别观察。 |
| T30 | Anthropic Managed Agents 兼容 | 细化 | 拆分类型/oracle、SDK mock transport 和真实 Awaken 服务行为；外部托管服务属于单独部署资格。 |
| T31 | 持久投影与公开身份 | 保留 | 区分组件级 active-active 与真实多进程，公开身份由持久坐标决定。 |
| T32 | 连接与恢复策略 | 细化 | 断开连接不默认取消业务；分别验证部署明确选择的断连策略与显式取消。 |
| T33 | 错误分类与重试 | 细化 | 区分接纳失败、确定回滚、提交成功丢响应和副作用结果未知四类。 |
| T34 | 真实后端资格门禁 | 改为公共门禁 | 这是适用于相关行为的轴，不再复制一组全部业务测试；组合按事务/状态机风险选择。 |
| T35 | 平台与执行器资格门禁 | 改为公共门禁 | 平台是资格维度；本任务 Windows 限定不要求现在执行 Linux，但不得把范围外标为通过。 |
| T36 | 选取与证据有效性门禁 | 改为公共门禁 | 名称、执行、断言覆盖和验收四层分开；清单不是另一个可执行 suite manifest。 |
| T37 | 持久格式迁移与跨版本恢复 | 新增 | 原清单未把迁移/已发布历史/高水位与非法持久行列为独立风险域。 |
| T38 | 容量 背压和故障隔离 | 新增 | 正确性清单缺少积压、慢依赖、非饥饿和资源耗尽下的有界行为。 |
| T39 | Environment 镜像与软件包交付 | 新增 | 原 T20/T21 只覆盖定义与投递，不能覆盖包到冻结镜像的异步构建状态机。 |
| T40 | Memory 缓存与投影能力边界 | 新增 | 区分同进程 FUSE 失效、远端 CopyOnly 和未来分布式实时一致性；不默认要求实现新网络总线。 |
| T41 | 可诊断性与恢复操作 | 新增 | 开发者体验不仅是命名，还需要知道操作是否提交、谁负责恢复及哪些能力未就绪。 |

## 具体用例与验收

具体用例按编号拆分到三个文件：

- [T00–T13](session-execution-test-acceptance-cases-1.md)
- [T14–T27](session-execution-test-acceptance-cases-2.md)
- [T28–T41](session-execution-test-acceptance-cases-3.md)

## 设计场景补测批次

本节记录原 `design_only` 场景的补测批次。场景的条款、判定规则和基线仍以上文和[机器可读清单](session-execution-test-acceptance/catalog.json)为准；这里只说明每批新增的测试、执行证据、产品缺口和平台待验原因，不改变基线，也不据此把任何组或场景记为已验收。

### 新增证据状态与守卫

[台账守卫](../../scripts/ci/check_session_acceptance.py)由 [check-docs.sh](../../scripts/ci/check-docs.sh) 执行，拒绝以下情况：计数与场景不符；绑定的文件或函数不存在；条款绑定没有指向断言行；见证被跳过、仅编译或不在 receipt 的已执行列表中；见证与 receipt 的源码身份不一致；历史证据未经输入审查即被继承；已登记证据被改写；`current_clause_complete` 场景有未绑定的 oracle、反向控制或切片；公共门禁未闭合时声明验收；产品缺口没有原因、后续事项或被忽略的复现测试。每条拒绝由守卫自测对变异台账实际执行。

| 状态 | 含义 |
| --- | --- |
| `current_clause_complete` | 每条 oracle 和反向控制都有当前提交本机执行通过的具名断言绑定，并列出已执行和未验收切片；不等于场景验收 |
| `product_gap` | 产品尚不满足某条 oracle；复现测试按正确行为断言并以 `#[ignore = "product gap: …"]` 忽略，不计为执行证据；已成立的 oracle 仍可另有执行绑定 |
| `platform_pending` | 只能在当前授权范围外的平台验证（例如 Linux 内核 FUSE），写明原因；不报告通过 |

`acceptance_state` 均保持 `pending`：原始校准按 Windows 范围记录；后续按用户当前授权补充本机 Linux 证据。历史 receipt 不改写，公共门禁和完整场景仍须分别闭合。

### 批次

| 批次 | 场景 | 状态 | 证据 |
| --- | --- | --- | --- |
| 静态门禁 | T00-C01、T00-C02、T00-C03、T34-C03、T36-C01、T36-C02、T36-C03 | `current_clause_complete` | E11、E12、E15 |
| 组件 | T01-C01、T14-C01 | `current_clause_complete` | E13 |
| 组件 | T06-C01、T07-C01、T11-C02、T12-C01、T13-C03、T15-C01、T27-C01、T27-C02、T41-C01 | `current_clause_complete` | E13、E40、E41 |
| SQL、全后端与原生效果 | T05-C01、T11-C03、T18-C01、T26-C03、T25-C03、T24-C02、T20-C03、T34-C01、T34-C02、T07-C03、T28-C03、T28-C02 | `current_clause_complete` | E16（T26-C03 另有 E18） |
| SQL、全后端与原生效果 | T04-C03、T17-C01、T17-C02、T17-C03、T12-C03、T33-C02、T38-C03、T23-C03、T08-C02、T08-C03、T09-C02、T10-C03、T38-C02、T06-C02、T16-C01、T10-C02 | `product_gap` | E16（已成立部分）；E17 为复现测试的预期失败 |
| 凭据读取与回执等待 | T15-C02 | `current_subset` | E42（Linux Provider/A2A 的具名等待断言）；完整场景未验收 |

#### 静态门禁

[门禁测试](../../scripts/ci/test_session_acceptance.py)以真实工作区和台账为输入，先证明未修改的输入被接受，再注入一个反例并要求生产守卫按自己的规则拒绝：越界依赖进入 Host/MCP/Environment/Memory、通过改名或移动文件的再导出暴露宽对象、在所有者外直接写 Session 状态、移动组入口、共享契约变更的真实反向依赖、只按文件所在 crate 选取的变异规划器、未知或全局输入、按名称和次数对齐的执行验证与只比较总数的变异验证器，以及台账的名称、断言、跳过、仅编译、改写历史 receipt 等规则。T36-C02 的“源码不变”条款绑定既有的执行围栏自测。

已知限制：依赖守卫按自身表格允许部分跨上下文基础设施依赖（例如 memory-store 到 session-executor），本批只证明其规则禁止的边会被拒绝；影响选择只证明 Cargo 反向依赖，L2/L3 层级扩展由各门禁脚本负责。

E11 是 13 个门禁测试在本机 Windows 的执行，E15 是 rebase 到 `f3ae320728` 后的同一组复跑；E12 是 Rust 必需执行验证器自测（27 例通过，10 例为 POSIX/Linux 专属固件而跳过，未绑定）。原始 receipt 和日志在 `target/session-acceptance-evidence/`，哈希登记在证据注册表。

#### 组件

各测试调用生产入口，以副作用计数、持久 root 与确切身份断言，并在测试内执行反向控制（变异检查器或注入故障）。E13 为 23 个非忽略用例在本机 Windows 的 nextest 执行，全部通过；E14 以 `--run-ignored only` 执行 14 个产品缺口复现测试，全部按预期失败，只用于证明缺口存在。E40 是修复后在本机 Windows 的 nextest 复跑：其中 9 个原复现测试取消忽略并通过，改为条款绑定；其余 5 个仍为忽略的复现测试。E41 是 [ADR-0094](../adr/0094-session-access-and-run-admission-outcomes.md) 落地后的复跑：其余 5 个复现测试取消忽略并通过（T07 复现按 ADR 改为断言“结果未定、修复恰好接纳一次”），本批组件缺口全部关闭。

T01-C01 在本批修复了一个小缺口：Secret Broker 原先只安装到当时已选的 Provider，之后选择、替换或迁移的 Provider 不会获得它；现在 builder 保存 Broker，并与 Memory mounter 一起重新安装。

本批 9 个组件缺口均已关闭：T12-C01、T15-C01、T27-C01、T27-C02、T41-C01 见 E40；T06-C01、T07-C01、T11-C02、T13-C03 按 [ADR-0094](../adr/0094-session-access-and-run-admission-outcomes.md) 修复，见 E41。

本节新增的绑定从 B101 起登记在 `bindings-4.json` 及后续分片，每条带断言行、覆盖的 oracle 编号与反向控制标记；下文“现有测试绑定索引”只列基线时的 100 个绑定。

## 现有测试绑定索引

下表列出实际源码声明和具名执行证明，合计 100 个声明绑定。相同名字在多个 adapter 出现时逐位置记录。helper、源码绑定和套件级汇总不能冒充独立实际执行的病例。原始 log/harness 哈希、行及文件摘要均保留在 JSON；后续源码变更需重新定位。

| 绑定 | 测试或 helper | 源码位置 | 实际执行证明 |
| --- | --- | --- | --- |
| B001 | `a_failed_mutation_publishes_no_invalidation` | [crates/resources/awaken-resource-application/src/memory.rs:673](../../crates/resources/awaken-resource-application/src/memory.rs#L673) | E02 |
| B002 | `a_lease_is_live_exactly_until_its_expiry` | [crates/stores/awaken-work-store/tests/lease_book_properties.rs:23](../../crates/stores/awaken-work-store/tests/lease_book_properties.rs#L23) | E01 |
| B003 | `a_mutation_publishes_an_invalidation_others_receive` | [crates/resources/awaken-resource-application/src/memory.rs:592](../../crates/resources/awaken-resource-application/src/memory.rs#L592) | E02 |
| B004 | `a_physical_failure_releases_the_fence_and_remains_retryable` | [crates/server/awaken-resource-reclaimer/src/lib.rs:704](../../crates/server/awaken-resource-reclaimer/src/lib.rs#L704) | E02 |
| B005 | `a_read_only_copy_mount_does_not_harvest` | [crates/worker/awaken-sandbox-memoryd/src/mounter.rs:639](../../crates/worker/awaken-sandbox-memoryd/src/mounter.rs#L639) | E06 |
| B006 | `active_resident_source_retirement_preserves_the_original_session` | [crates/server/awaken-session-application/src/tests/realization/worker_retirement.rs:128](../../crates/server/awaken-session-application/src/tests/realization/worker_retirement.rs#L128) | E01 |
| B007 | `approval_timeout_preserves_ticket_and_rebuilt_runtime_can_resume_once` | [crates/runtime/awaken-runtime/tests/sdk_api.rs:336](../../crates/runtime/awaken-runtime/tests/sdk_api.rs#L336) | 未绑定原始具名执行；不可据名称报通过 |
| B008 | `authored_packages_become_one_frozen_image_before_session_use` | [crates/server/awaken-environment-image-build/src/application.rs:738](../../crates/server/awaken-environment-image-build/src/application.rs#L738) | E01 |
| B009 | `both_create_routes_finalize_resources_before_root_or_runtime_effects` | [crates/server/awaken-protocol-managed/tests/session_resources/creation_inputs.rs:20](../../crates/server/awaken-protocol-managed/tests/session_resources/creation_inputs.rs#L20) | E01, E02 |
| B010 | `budget_settlement_keeps_failed_and_response_lost_cas_on_the_original_root` | [crates/server/awaken-session-application/src/tests/budget_settlement.rs:17](../../crates/server/awaken-session-application/src/tests/budget_settlement.rs#L17) | E01 |
| B011 | `builder_freezes_execution_services_and_mount_checks_reject_foreign_instances_without_reads` | [crates/server/awaken-session-application/src/tests/construction.rs:285](../../crates/server/awaken-session-application/src/tests/construction.rs#L285) | E01 |
| B012 | `builder_rejects_invalid_configuration_before_observing_or_starting_services` | [crates/server/awaken-session-application/src/tests/construction.rs:14](../../crates/server/awaken-session-application/src/tests/construction.rs#L14) | E01 |
| B013 | `cancelled_call_stops_ready_descendants_before_session_reuse` | [crates/runtime/awaken-ext-builtin-tools/tests/hand/windows_process.rs:105](../../crates/runtime/awaken-ext-builtin-tools/tests/hand/windows_process.rs#L105) | E04 |
| B014 | `complete_generation_rejects_same_content_aba` | [crates/resources/awaken-memory-store/tests/common/memory_repository.rs:737](../../crates/resources/awaken-memory-store/tests/common/memory_repository.rs#L737) | 未绑定原始具名执行；不可据名称报通过 |
| B015 | `complete_recovery_scan_reaches_page_two_and_purge_fails_closed` | [crates/server/awaken-session-application/src/tests/authority/work_projection.rs:74](../../crates/server/awaken-session-application/src/tests/authority/work_projection.rs#L74) | E01 |
| B016 | `concurrent_postgres_claim_reclaim_and_restart_preserve_one_exact_authority` | [crates/server/awaken-environment-image-build/src/postgres.rs:178](../../crates/server/awaken-environment-image-build/src/postgres.rs#L178) | E01 |
| B017 | `continuation_refuses_inconsistent_evidence_and_superseded_operation_leases` | [crates/server/awaken-session-application/src/tests/continuation.rs:2332](../../crates/server/awaken-session-application/src/tests/continuation.rs#L2332) | E01 |
| B018 | `controlled_memory_bindings_retain_one_backend_and_original_bus` | [crates/resources/awaken-resource-application/src/memory.rs:716](../../crates/resources/awaken-resource-application/src/memory.rs#L716) | E02 |
| B019 | `copy_inputs_use_the_one_atomic_snapshot_primitive` | [crates/worker/awaken-sandbox-memoryd/src/copy.rs:591](../../crates/worker/awaken-sandbox-memoryd/src/copy.rs#L591) | E06 |
| B020 | `copy_only_mount_realizes_copy_and_harvests_through_the_public_api` | [crates/worker/awaken-sandbox-memoryd/src/mounter.rs:741](../../crates/worker/awaken-sandbox-memoryd/src/mounter.rs#L741) | E06 |
| B021 | `corrupt_persisted_environment_is_never_silently_defaulted_or_panicked` | [crates/stores/awaken-env-store/src/lib.rs:958](../../crates/stores/awaken-env-store/src/lib.rs#L958) | E01 |
| B022 | `disposal_failure_retries_without_recheckpointing` | [crates/server/awaken-session-application/src/tests/continuation.rs:1579](../../crates/server/awaken-session-application/src/tests/continuation.rs#L1579) | E01 |
| B023 | `dropping_the_session_stops_ready_background_descendants` | [crates/runtime/awaken-ext-builtin-tools/tests/hand/windows_process.rs:234](../../crates/runtime/awaken-ext-builtin-tools/tests/hand/windows_process.rs#L234) | E04 |
| B024 | `durable_outcome_projector_is_warm_cold_restart_and_active_active_stable` | [crates/server/awaken-protocol-managed/src/state/events/tests.rs:11967](../../crates/server/awaken-protocol-managed/src/state/events/tests.rs#L11967) | E01, E02 |
| B025 | `durable_session_truth_owns_one_work_projection_path` | [crates/server/awaken-session-application/src/tests/authority/work_projection.rs:4](../../crates/server/awaken-session-application/src/tests/authority/work_projection.rs#L4) | E01 |
| B026 | `event_batch_idempotency_replays_the_root_receipt_across_restart` | [crates/server/awaken-session-application/src/tests/event_batches.rs:1954](../../crates/server/awaken-session-application/src/tests/event_batches.rs#L1954) | E01 |
| B027 | `every_forwarded_mutation_keeps_the_author_the_caller_stated` | [crates/resources/awaken-resource-application/src/memory.rs:550](../../crates/resources/awaken-resource-application/src/memory.rs#L550) | E02 |
| B028 | `every_registry_adapter_obeys_the_registration_outbox_decision_table` | [crates/stores/awaken-env-store/src/lib.rs:1194](../../crates/stores/awaken-env-store/src/lib.rs#L1194) | E01 |
| B029 | `expired_checkpoint_delete_failure_retains_reference_for_retry` | [crates/server/awaken-session-application/src/tests/continuation.rs:2228](../../crates/server/awaken-session-application/src/tests/continuation.rs#L2228) | E01 |
| B030 | `expired_claim_is_recovered_and_stale_owner_is_fenced` | [crates/server/awaken-resource-reclaimer/src/lib.rs:596](../../crates/server/awaken-resource-reclaimer/src/lib.rs#L596) | E02 |
| B031 | `expired_effect_fence_rejects_before_runtime_or_mount_materialization` | [crates/worker/awaken-sandbox-container/src/tests.rs:3975](../../crates/worker/awaken-sandbox-container/src/tests.rs#L3975) | 未绑定原始具名执行；不可据名称报通过 |
| B032 | `failed_dependency_observations_never_authorize_cleanup` | [crates/resources/awaken-resource-application/src/reclamation.rs:198](../../crates/resources/awaken-resource-application/src/reclamation.rs#L198) | E02 |
| B033 | `failed_intent_insert_rolls_back_the_whole_create_transaction` | [crates/stores/awaken-env-store/src/lib.rs:888](../../crates/stores/awaken-env-store/src/lib.rs#L888) | E01 |
| B034 | `fuse_constructor_refuses_a_second_bus_before_any_listener_or_mount` | [crates/worker/awaken-sandbox-memoryd/src/mounter.rs:834](../../crates/worker/awaken-sandbox-memoryd/src/mounter.rs#L834) | 未绑定原始具名执行；不可据名称报通过 |
| B035 | `image_recipe_identity_ignores_authoring_only_changes` | [crates/contract/awaken-executable-environment-contract/src/image_realization.rs:477](../../crates/contract/awaken-executable-environment-contract/src/image_realization.rs#L477) | E01 |
| B036 | `latest_build_outage_does_not_starve_frozen_history_or_grant_older_queue_work` | [crates/server/awaken-environment-image-build/src/application.rs:941](../../crates/server/awaken-environment-image-build/src/application.rs#L941) | E01 |
| B037 | `local_repository_commands_never_mutate_an_ancestor_checkout` | [crates/worker/awaken-sandbox-local/src/git_transport/command.rs:1105](../../crates/worker/awaken-sandbox-local/src/git_transport/command.rs#L1105) | E04 |
| B038 | `mcp_drain_acknowledges_removed_only_after_busy_call_quiesces` | [crates/server/awaken-session-executor/src/host/tests/mcp_generation.rs:210](../../crates/server/awaken-session-executor/src/host/tests/mcp_generation.rs#L210) | E03 |
| B039 | `mcp_quiescence_retains_failed_effect_owners_for_retry` | [crates/server/awaken-session-executor/src/host/tests/mcp_generation.rs:514](../../crates/server/awaken-session-executor/src/host/tests/mcp_generation.rs#L514) | E03 |
| B040 | `mcp_transport_faults_have_stable_origin_classification` | [crates/server/awaken-session-mcp/src/mcp.rs:589](../../crates/server/awaken-session-mcp/src/mcp.rs#L589) | 未绑定原始具名执行；不可据名称报通过 |
| B041 | `migration_from_v8_seeds_exact_registration_history_once` | [crates/stores/awaken-env-store/src/lib.rs:789](../../crates/stores/awaken-env-store/src/lib.rs#L789) | E01 |
| B042 | `native_and_acp_project_the_same_generation_across_hot_replacement` | [crates/server/awaken-session-executor/src/host/tests/mcp_generation.rs:18](../../crates/server/awaken-session-executor/src/host/tests/mcp_generation.rs#L18) | E03 |
| B043 | `one_outbox_fanout_refreshes_open_sse_and_replay_is_idempotent` | [crates/server/awaken-coordinator/src/managed_lifecycle.rs:231](../../crates/server/awaken-coordinator/src/managed_lifecycle.rs#L231) | 未绑定原始具名执行；不可据名称报通过 |
| B044 | `operation_activity_replay_uses_the_root_receipt_as_its_only_epoch_authority` | [crates/server/awaken-session-application/src/tests/authority.rs:3058](../../crates/server/awaken-session-application/src/tests/authority.rs#L3058) | E01 |
| B045 | `physical_purge_admission_requires_every_safety_fact` | [crates/server/awaken-resource-reclaimer/src/lib.rs:365](../../crates/server/awaken-resource-reclaimer/src/lib.rs#L365) | E02 |
| B046 | `platform_held_mcp_is_mediated_without_resolving_material_into_the_host` | [crates/server/awaken-session-executor/src/host/tests/session_projection.rs:2150](../../crates/server/awaken-session-executor/src/host/tests/session_projection.rs#L2150) | E03 |
| B047 | `postgres_allocator_work_and_watermark_roll_back_as_one_transaction` | [crates/stores/awaken-work-store/tests/support/work_identity_durable.rs:209](../../crates/stores/awaken-work-store/tests/support/work_identity_durable.rs#L209) | E01 |
| B048 | `postgres_both_published_histories_preserve_live_leases_and_fence_deleted_ids` | [crates/stores/awaken-work-store/src/schema/identity_upgrade_tests.rs:101](../../crates/stores/awaken-work-store/src/schema/identity_upgrade_tests.rs#L101) | E01 |
| B049 | `postgres_delayed_registration_after_reconnect_cannot_restore_withdrawn_work` | [crates/control/awaken-environment-application/tests/registration_replay.rs:177](../../crates/control/awaken-environment-application/tests/registration_replay.rs#L177) | E01 |
| B050 | `postgres_history_cannot_consume_unapplied_higher_registration` | [crates/control/awaken-environment-application/tests/registration_replay.rs:490](../../crates/control/awaken-environment-application/tests/registration_replay.rs#L490) | E01 |
| B051 | `postgres_independent_pools_allocate_concurrently_without_reset` | [crates/stores/awaken-work-store/tests/support/work_identity_durable.rs:74](../../crates/stores/awaken-work-store/tests/support/work_identity_durable.rs#L74) | E01 |
| B052 | `postgres_memory_roles_share_the_same_atomic_memory` | [crates/resources/awaken-memory-store/tests/conformance.rs:268](../../crates/resources/awaken-memory-store/tests/conformance.rs#L268) | E02 |
| B053 | `postgres_missing_malformed_and_exhausted_allocators_fail_closed` | [crates/stores/awaken-work-store/tests/support/work_identity_durable.rs:347](../../crates/stores/awaken-work-store/tests/support/work_identity_durable.rs#L347) | E01 |
| B054 | `postgres_parity_over_a_live_db` | [crates/stores/awaken-env-store/src/lib.rs:1253](../../crates/stores/awaken-env-store/src/lib.rs#L1253) | E01 |
| B055 | `postgres_pending_history_does_not_swallow_new_registration_effect` | [crates/control/awaken-environment-application/tests/registration_replay.rs:530](../../crates/control/awaken-environment-application/tests/registration_replay.rs#L530) | E01 |
| B056 | `postgres_snapshot_heads_follow_one_committed_cross_file_prefix` | [crates/resources/awaken-memory-store/tests/conformance.rs:258](../../crates/resources/awaken-memory-store/tests/conformance.rs#L258) | E02 |
| B057 | `postgres_work_identity_is_not_reused_after_lifecycle_removal` | [crates/stores/awaken-work-store/tests/work_identity.rs:278](../../crates/stores/awaken-work-store/tests/work_identity.rs#L278) | E01 |
| B058 | `preparation_failure_has_zero_physical_disposal_and_retries_before_disposing` | [crates/server/awaken-session-application/src/tests/continuation.rs:1770](../../crates/server/awaken-session-application/src/tests/continuation.rs#L1770) | E01 |
| B059 | `preparation_requires_private_cleanup_and_preserves_existing_output_on_retry` | [crates/server/awaken-coordinator/src/dream/private_content_tests.rs:50](../../crates/server/awaken-coordinator/src/dream/private_content_tests.rs#L50) | E02 |
| B060 | `recall_entries_use_only_the_atomic_memory_snapshot` | [crates/server/awaken-session-run-delivery/src/memory_content/tests.rs:186](../../crates/server/awaken-session-run-delivery/src/memory_content/tests.rs#L186) | E02, E03 |
| B061 | `recovered_copy_reconciles_surviving_files_without_a_live_guard` | [crates/worker/awaken-sandbox-memoryd/src/mounter.rs:540](../../crates/worker/awaken-sandbox-memoryd/src/mounter.rs#L540) | E06 |
| B062 | `registration_worker_and_readiness_follow_one_demand_path` | [crates/server/awaken-environment-image-build/src/application.rs:538](../../crates/server/awaken-environment-image-build/src/application.rs#L538) | E01 |
| B063 | `rename_replace_is_crash_atomic` | [crates/resources/awaken-memory-store/src/sqlite.rs:1300](../../crates/resources/awaken-memory-store/src/sqlite.rs#L1300) | E02；手动丢弃内存 SQLite 测试事务，不能证明生产 rename 进程死亡。 |
| B064 | `required_tool_compiler_postgres_preserves_the_frozen_configuration` | [crates/server/awaken-session-application/src/tests/construction.rs:432](../../crates/server/awaken-session-application/src/tests/construction.rs#L432) | E01 |
| B065 | `required_tool_compiler_precedes_session_creation_and_update_effects` | [crates/server/awaken-session-application/src/tests/construction.rs:422](../../crates/server/awaken-session-application/src/tests/construction.rs#L422) | E01 |
| B066 | `resident_projection_failure_retries_without_restoring_again` | [crates/server/awaken-session-application/src/tests/continuation.rs:1962](../../crates/server/awaken-session-application/src/tests/continuation.rs#L1962) | E01 |
| B067 | `resource_reference_projection_fails_closed_and_repairs_cas_losers` | [crates/server/awaken-session-application/src/resource_references/tests.rs:43](../../crates/server/awaken-session-application/src/resource_references/tests.rs#L48) | E01 |
| B068 | `restarting_the_session_stops_ready_background_descendants` | [crates/runtime/awaken-ext-builtin-tools/tests/hand/windows_process.rs:202](../../crates/runtime/awaken-ext-builtin-tools/tests/hand/windows_process.rs#L202) | E04 |
| B069 | `restore_failure_retries_the_committed_restore_operation` | [crates/server/awaken-session-application/src/tests/continuation.rs:1911](../../crates/server/awaken-session-application/src/tests/continuation.rs#L1911) | E01 |
| B070 | `root_assistant_identity_is_equal_warm_cold_and_active_active` | [crates/server/awaken-protocol-managed/src/state/events/tests.rs:9048](../../crates/server/awaken-protocol-managed/src/state/events/tests.rs#L9048) | E01, E02 |
| B071 | `serving_readiness_isolated_from_projection_degradation` | [crates/control/awaken-control/src/registration_supervisor.rs:289](../../crates/control/awaken-control/src/registration_supervisor.rs#L289) | E01 |
| B072 | `session_mcp_realizer_is_constructed_once_without_a_host_cycle` | [crates/server/awaken-session-executor/src/host/tests/mcp_credentials.rs:1657](../../crates/server/awaken-session-executor/src/host/tests/mcp_credentials.rs#L1657) | E03 |
| B073 | `sqlite_allocator_work_and_watermark_roll_back_as_one_transaction` | [crates/stores/awaken-work-store/tests/support/work_identity_durable.rs:127](../../crates/stores/awaken-work-store/tests/support/work_identity_durable.rs#L127) | E01 |
| B074 | `sqlite_both_published_histories_preserve_live_leases_and_fence_deleted_ids` | [crates/stores/awaken-work-store/src/schema/identity_upgrade_tests.rs:23](../../crates/stores/awaken-work-store/src/schema/identity_upgrade_tests.rs#L23) | E01 |
| B075 | `sqlite_build_contention_does_not_starve_authority_timers` | [crates/server/awaken-environment-image-build/src/sqlite.rs:199](../../crates/server/awaken-environment-image-build/src/sqlite.rs#L199) | E01 |
| B076 | `sqlite_delayed_registration_after_reopen_cannot_restore_withdrawn_work` | [crates/control/awaken-environment-application/tests/registration_replay.rs:158](../../crates/control/awaken-environment-application/tests/registration_replay.rs#L158) | E01 |
| B077 | `sqlite_history_cannot_consume_unapplied_higher_registration` | [crates/control/awaken-environment-application/tests/registration_replay.rs:480](../../crates/control/awaken-environment-application/tests/registration_replay.rs#L480) | E01 |
| B078 | `sqlite_independent_handles_allocate_concurrently_and_reopen_without_reset` | [crates/stores/awaken-work-store/tests/support/work_identity_durable.rs:50](../../crates/stores/awaken-work-store/tests/support/work_identity_durable.rs#L50) | E01 |
| B079 | `sqlite_lifecycle_outbox_insert_failure_rolls_back_every_root_path` | [crates/stores/awaken-session-store/tests/session_repo_conformance.rs:277](../../crates/stores/awaken-session-store/tests/session_repo_conformance.rs#L277) | 未绑定原始具名执行；不可据名称报通过 |
| B080 | `sqlite_materialized_path_cas` | [crates/resources/awaken-memory-store/tests/conformance.rs:102](../../crates/resources/awaken-memory-store/tests/conformance.rs#L102) | E02 |
| B081 | `sqlite_memory_roles_share_the_same_atomic_memory` | [crates/resources/awaken-memory-store/tests/conformance.rs:476](../../crates/resources/awaken-memory-store/tests/conformance.rs#L476) | E02 |
| B082 | `sqlite_missing_malformed_and_exhausted_allocators_fail_closed` | [crates/stores/awaken-work-store/tests/support/work_identity_durable.rs:297](../../crates/stores/awaken-work-store/tests/support/work_identity_durable.rs#L297) | E01 |
| B083 | `sqlite_pending_history_does_not_swallow_new_registration_effect` | [crates/control/awaken-environment-application/tests/registration_replay.rs:515](../../crates/control/awaken-environment-application/tests/registration_replay.rs#L515) | E01 |
| B084 | `sqlite_snapshot_heads_follow_one_committed_cross_file_prefix` | [crates/resources/awaken-memory-store/tests/conformance.rs:40](../../crates/resources/awaken-memory-store/tests/conformance.rs#L40) | E02 |
| B085 | `sqlite_work_authority_waiters_never_block_runtime_timers` | [crates/stores/awaken-work-store/src/tests.rs:125](../../crates/stores/awaken-work-store/src/tests.rs#L125) | E01 |
| B086 | `sqlite_work_identity_is_not_reused_after_lifecycle_removal` | [crates/stores/awaken-work-store/tests/work_identity.rs:265](../../crates/stores/awaken-work-store/tests/work_identity.rs#L265) | E01 |
| B087 | `staged_clone_over_git_directory_limit_preserves_exact_plan_and_replay` | [crates/worker/awaken-sandbox-local/src/git_transport/command.rs:386](../../crates/worker/awaken-sandbox-local/src/git_transport/command.rs#L386) | E04 |
| B088 | `supervisor_rebuilds_with_all_until_success_then_uses_pending_only` | [crates/control/awaken-control/src/registration_supervisor.rs:358](../../crates/control/awaken-control/src/registration_supervisor.rs#L358) | E01 |
| B089 | `terminal_tool_reply_rejects_new_admission_but_replays_exact_receipt` | [crates/server/awaken-protocol-managed/src/state/events/tests.rs:2703](../../crates/server/awaken-protocol-managed/src/state/events/tests.rs#L2703) | E01, E02 |
| B090 | `timed_out_call_stops_ready_descendants_before_returning` | [crates/runtime/awaken-ext-builtin-tools/tests/hand/windows_process.rs:167](../../crates/runtime/awaken-ext-builtin-tools/tests/hand/windows_process.rs#L167) | E04 |
| B091 | `update_replay_keeps_the_original_atomic_receipt_after_a_later_root_write` | [crates/server/awaken-session-application/src/tests/observed_commit.rs:242](../../crates/server/awaken-session-application/src/tests/observed_commit.rs#L242) | E01 |
| B092 | `volatile_delayed_registration_cannot_restore_withdrawn_work` | [crates/control/awaken-environment-application/tests/registration_replay.rs:151](../../crates/control/awaken-environment-application/tests/registration_replay.rs#L151) | E01 |
| B093 | `volatile_history_cannot_consume_unapplied_higher_registration` | [crates/control/awaken-environment-application/tests/registration_replay.rs:472](../../crates/control/awaken-environment-application/tests/registration_replay.rs#L472) | E01 |
| B094 | `volatile_materialized_path_cas` | [crates/resources/awaken-memory-store/tests/conformance.rs:93](../../crates/resources/awaken-memory-store/tests/conformance.rs#L93) | E02 |
| B095 | `volatile_memory_roles_share_the_same_atomic_memory` | [crates/resources/awaken-memory-store/tests/conformance.rs:468](../../crates/resources/awaken-memory-store/tests/conformance.rs#L468) | E02 |
| B096 | `volatile_pending_history_does_not_swallow_new_registration_effect` | [crates/control/awaken-environment-application/tests/registration_replay.rs:504](../../crates/control/awaken-environment-application/tests/registration_replay.rs#L504) | E01 |
| B097 | `volatile_snapshot_heads_follow_one_committed_cross_file_prefix` | [crates/resources/awaken-memory-store/tests/conformance.rs:31](../../crates/resources/awaken-memory-store/tests/conformance.rs#L31) | E02 |
| B098 | `work_dispatch_reconciliation_isolates_each_session_failure` | [crates/server/awaken-session-application/src/tests/authority/work_projection.rs:35](../../crates/server/awaken-session-application/src/tests/authority/work_projection.rs#L35) | E01 |
| B099 | `worker_relay_drain_retains_exact_route_across_cancel_and_timeout_retry` | [crates/server/awaken-session-executor/src/host/tests/mcp_generation.rs:364](../../crates/server/awaken-session-executor/src/host/tests/mcp_generation.rs#L364) | E03 |
| B100 | `worker_retirement_requires_each_terminal_source_without_mutating_roots` | [crates/server/awaken-session-application/src/tests/realization/worker_retirement.rs:180](../../crates/server/awaken-session-application/src/tests/realization/worker_retirement.rs#L180) | E01 |

## 实施顺序与完成标准

1. 先落地 T01-C01/C02 和 T16-C01：构造顺序无关、模式必需能力、所有 MCP 分支的预 I/O 责任与授权窗口。用 barriers、外部操作计数和确切 owner 观察给出失败反例。
2. 接着完成 T17、T19、T22/T24/T26 的生产路径故障与真实恢复；扩展 T20-C03。对实际事务和物理责任插入受控故障点，以原操作身份恢复；测试不引入新的产品 authority。
3. 同步补 T37–T41 缺失切片及 T30/T32 的真实服务 SDK 验证。现有测试优先复用；只是缺绑定的场景先审查 oracle，不重复编译全仓。
4. 按 T00/T34/T35/T36 逐条签收，先关闭当前 Windows 适用矩阵。跨平台或外部引擎范围保持明确未验收，待其原生环境可用再独立资格执行。

完成一部分时提交相应设计条款、实现、具名测试及执行证据；源码经 rebase 变化后重算受影响输入并补验，不改写旧 receipt。是否提交/推送由当次任务授权决定，本清单本身不授予发布权限。

## 运行入口与证据模板

清单不创建第二套 runner，现有 suite 唯一入口仍是 [check-all.sh](../../scripts/ci/check-all.sh)，features 来自 [.config/rust-tests.toml](../../.config/rust-tests.toml)。新用例应进入所属现有测试 target；完整断言绑定后再接入既有选取器，不能仅凭 JSON 的名字启动资格运行。

以下为计划和受影响检查示例，不是本次已执行命令。基准应取本次变更的真实基线，不能在变更后用相同 HEAD 制造空选择。

```sh
python scripts/ci/rust_test_plan.py --base <actual-base>
python scripts/ci/rust_test_plan.py --base <actual-base> --run
bash scripts/ci/test.sh -p awaken-environment-application
bash scripts/ci/test.sh -p awaken-memory-store
cargo test --locked -p <owner> --doc
bash scripts/ci/check-docs.sh
```

Windows 应通过绝对 Git Bash 路径调用原 shell 入口，设置同一个 CARGO_TARGET_DIR/CARGO_BUILD_BUILD_DIR，并保留原生 MSVC target；数据库使用已配置真实 fixture。数据库、companion 和 ignored 必需例的运行要求沿用原 owner，不使用文档中的示例绕过 host admission、期限和资源门槛。T30 真实服务用仓库锁定的 SDK：当前 TS 0.132.1、oldest 0.105.0，Python 环境锁为 anthropic 1.12.1；其他锚点及 Python 矩阵以各原始 lock/oracle 为准。这些是本源码的 pins，不宣称外部当前最新版。

最小验收记录如下；完整可执行命令与日志作为 artifact 存档，不写入明文凭据。

```json
{
  "case_id": "T16-C01",
  "source": {"head": "<sha>", "tree": "<tree>", "dirty_digest": "<digest>"},
  "facet": {"mode": "native-http", "os": "windows", "target": "x86_64-pc-windows-msvc", "features": [], "backend": "<actual>"},
  "inputs": {"compiler": "<version>", "linker": "<version/hash>", "fixture": "<identity>", "companion": "<hash>", "input_closure": "<manifest/hash>"},
  "execution": {"argv": [], "listed": [], "executed": [], "exit": 0, "seconds": 0, "skips": [], "log_sha256": "<hash>"},
  "assertions": [{"clause": "pre-IO responsibility", "test": "<actual-name>", "negative_control": "<actual-result>"}],
  "acceptance": {"status": "pending", "owner": "<actual-maintainer>", "missing_facets": []}
}
```

本次文档校准会检查 42 个需求、127 个场景、源码位置、现有 receipt/日志摘要及引用一致性；这只是清单完整性检查。所有新增负控、平台和病例的实施/执行状态保持独立。
