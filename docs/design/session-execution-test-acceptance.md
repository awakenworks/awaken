# Session execution and resource acceptance

This source document owns scenario requirements and acceptance rules. The existing ledger owns named source/assertion bindings and evidence identity; run progress and machine execution packets are kept outside tracked source.


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


## Scenario coverage

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


## Source and execution evidence

本节记录原 `design_only` 场景的补测批次。场景的条款、判定规则和基线仍以上文和[机器可读清单](session-execution-test-acceptance/catalog.json)为准；这里只说明每批新增的测试、执行证据、产品缺口和平台待验原因，不改变基线，也不据此把任何组或场景记为已验收。

### 新增证据状态与守卫

[台账守卫](../../scripts/ci/check_session_acceptance.py)由 [check-docs.sh](../../scripts/ci/check-docs.sh) 执行，拒绝以下情况：计数与场景不符；绑定的文件或函数不存在；条款绑定没有指向断言行；见证被跳过、仅编译或不在 receipt 的已执行列表中；见证与 receipt 的源码身份不一致；历史证据未经输入审查即被继承；已登记证据被改写；`current_clause_complete` 场景有未绑定的 oracle、反向控制或切片；公共门禁未闭合时声明验收；产品缺口没有原因、后续事项或被忽略的复现测试；`not_applicable` 没有原因或已执行的能力拒绝证明。每条拒绝由守卫自测对变异台账实际执行。

| 状态 | 含义 |
| --- | --- |
| `current_clause_complete` | 每条 oracle 和反向控制都有当前提交本机执行通过的具名断言绑定，并列出已执行和未验收切片；不等于场景验收 |
| `product_gap` | 产品尚不满足某条 oracle；复现测试按正确行为断言并以 `#[ignore = "product gap: …"]` 忽略，不计为执行证据；已成立的 oracle 仍可另有执行绑定 |
| `platform_pending` | 只能在当前授权范围外的平台验证（例如 Linux 内核 FUSE），写明原因；不报告通过 |
| `not_applicable` | 本部署不提供该模式；写明原因，并以已执行的能力拒绝测试证明，只证明拒绝而非行为通过 |


### 批次

| 批次 | 场景 | 状态 | 证据 |
| --- | --- | --- | --- |
| 静态门禁 | T00-C01、T00-C02、T00-C03、T34-C03、T36-C01、T36-C02、T36-C03 | `current_clause_complete` | E11、E12、E15 |
| 组件 | T01-C01、T14-C01 | `current_clause_complete` | E13 |
| 组件 | T06-C01、T07-C01、T11-C02、T12-C01、T13-C03、T15-C01、T27-C01、T27-C02、T41-C01 | `current_clause_complete` | E13、E40、E41 |
| SQL、全后端与原生效果 | T05-C01、T11-C03、T18-C01、T26-C03、T25-C03、T24-C02、T20-C03、T34-C01、T34-C02、T07-C03、T28-C03、T28-C02 | `current_clause_complete` | E16（T26-C03 另有 E18） |
| SQL、全后端与原生效果 | T04-C03、T17-C01、T17-C02、T17-C03、T12-C03、T33-C02、T38-C03、T23-C03、T08-C02、T08-C03、T09-C02、T10-C03、T38-C02、T06-C02、T16-C01 | `product_gap` | E16（已成立部分）；E17 为复现测试的预期失败 |
| 凭据读取与回执等待 | T15-C02 | `current_subset` | E42（Linux Provider/A2A 的具名等待断言）；完整场景未验收 |
| 设计场景补测（SDK、组件、真实服务） | T02-C01、T03-C01、T03-C03、T14-C03、T27-C03、T30-C02、T30-C03、T32-C03 | `current_clause_complete` | E-DOL-RUST/SDK/SDKPY/E2E-*-20261031 |
| 设计场景补测（SDK、组件、真实服务） | T02-C03、T13-C02、T25-C01、T31-C03、T32-C01、T33-C03 | `product_gap` | 已成立部分同上；E-DOL-GAPS、E-DOL-SDKPYGAP、E-DOL-E2E-MULTIPROCGAP 为复现测试的预期失败 |
| 设计场景补测（SDK、组件、真实服务） | T21-C04、T35-C01、T35-C03 | `current_subset` | E-DOL-E2E-CTRLCOORD、E-DOL-RUST；未覆盖切片见各场景 |
| 设计场景补测（平台与能力） | T24-C03、T35-C02 | `platform_pending` | 需原生 Linux 内核 FUSE/namespace，本任务不执行 Linux |
| 设计场景补测（平台与能力） | T40-C03 | `not_applicable` | E-DOL-RUST 的能力拒绝测试；无部署提供跨进程 live FUSE |

#### 静态门禁

[门禁测试](../../scripts/ci/test_session_acceptance.py)以真实工作区和台账为输入，先证明未修改的输入被接受，再注入一个反例并要求生产守卫按自己的规则拒绝：越界依赖进入 Host/MCP/Environment/Memory、通过改名或移动文件的再导出暴露宽对象、在所有者外直接写 Session 状态、移动组入口、共享契约变更的真实反向依赖、只按文件所在 crate 选取的变异规划器、未知或全局输入、按名称和次数对齐的执行验证与只比较总数的变异验证器，以及台账的名称、断言、跳过、仅编译、改写历史 receipt 等规则。T36-C02 的“源码不变”条款绑定既有的执行围栏自测。

已知限制：依赖守卫按自身表格允许部分跨上下文基础设施依赖（例如 memory-store 到 session-executor），本批只证明其规则禁止的边会被拒绝；影响选择只证明 Cargo 反向依赖，L2/L3 层级扩展由各门禁脚本负责。

E11 是 13 个门禁测试在本机 Windows 的执行，E15 是 rebase 到 `f3ae320728` 后的同一组复跑；E12 是 Rust 必需执行验证器自测（27 例通过，10 例为 POSIX/Linux 专属固件而跳过，未绑定）。原始 receipt 和日志在 `target/session-acceptance-evidence/`，哈希登记在证据注册表。

#### 组件

各测试调用生产入口，以副作用计数、持久 root 与确切身份断言，并在测试内执行反向控制（变异检查器或注入故障）。E13 为 23 个非忽略用例在本机 Windows 的 nextest 执行，全部通过；E14 以 `--run-ignored only` 执行 14 个产品缺口复现测试，全部按预期失败，只用于证明缺口存在。E40 是修复后在本机 Windows 的 nextest 复跑：其中 9 个原复现测试取消忽略并通过，改为条款绑定；其余 5 个仍为忽略的复现测试。E41 是 [ADR-0094](../adr/0094-session-access-and-run-admission-outcomes.md) 落地后的复跑：其余 5 个复现测试取消忽略并通过（T07 复现按 ADR 改为断言“结果未定、修复恰好接纳一次”），本批组件缺口全部关闭。

T01-C01 在本批修复了一个小缺口：Secret Broker 原先只安装到当时已选的 Provider，之后选择、替换或迁移的 Provider 不会获得它；现在 builder 保存 Broker，并与 Memory mounter 一起重新安装。

本批 9 个组件缺口均已关闭：T12-C01、T15-C01、T27-C01、T27-C02、T41-C01 见 E40；T06-C01、T07-C01、T11-C02、T13-C03 按 [ADR-0094](../adr/0094-session-access-and-run-admission-outcomes.md) 修复，见 E41。

本节新增的绑定从 B101 起登记在 `bindings-4.json` 及后续分片，每条带断言行、覆盖的 oracle 编号与反向控制标记；下文“现有测试绑定索引”只列基线时的 100 个绑定。



### Recording and retrieval

The existing recorder and source/assertion binder own execution packets and
their registry entries. The methods and retrieval boundary are described in
[test validation](test-validation.md#integrated-source-qualification-record).
Keep original bytes outside tracked source and retain their source identity,
hash and scope in the existing registry. A missing packet cannot establish a
new result. Source-linked scenarios and actual execution retain distinct states.


## Named bindings

The existing [catalog](session-execution-test-acceptance/catalog.json) composes the scenario, binding and evidence-registry parts. Keep those identities in that one ledger rather than copying their execution tables here.
