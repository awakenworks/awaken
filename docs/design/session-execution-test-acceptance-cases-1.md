# Session 执行与资源测试验收清单：具体用例（T00–T13）

本文件是[验收清单总览](session-execution-test-acceptance.md)的具体用例部分，因仓库文件长度限制按编号拆分；编号、判定规则和语义与总览一致。


每项列出源码责任组件、原入口和可观察 oracle。B 编号指向文末现有测试绑定，完整 argv、backend、features、原始日志哈希见 JSON 的 execution_witnesses 与 evidence_registry。绑定只支持其明确覆盖的子范围；设计中的新增行为不是实际测试函数名。

## T00 清点与边界门禁

责任组件：各状态机现有所有者。入口：[源码或现有门禁](../../scripts/ci/_contract_map_fitness.py)。风险基准：P1。类型：公共门禁。

校准：改为公共门禁。治理要求应作为每次变更的公共门禁，不与业务用例混算。

### T00-C01 声明与调用方

- 场景：修改或移动 Host、MCP、Environment、Memory 的声明、别名和包装。
- 必须断言：唯一状态所有者、调用角色、事务和恢复入口可追溯；移动文件或别名不会隐藏对宽对象的访问。
- 反向控制：故意加入越界依赖或通过包装恢复宽对象，守卫或审查必须拒绝。
- 层级与基质：L0；source/metadata/static negative controls。静态守卫；不代表运行时效果。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T00-C02 需求到断言

- 场景：某用例存在同名函数，但断言只覆盖成功路径。
- 必须断言：每个验收条款绑定具体断言和适用切片；仅命中测试索引不得标记已验收。
- 反向控制：只有测试名称、零断言或仅构造对象的绑定不能签收。
- 层级与基质：L0；source/metadata/static negative controls。静态守卫；不代表运行时效果。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T00-C03 变更影响

- 场景：修改共享契约、构造器、事务、授权或测试选取输入。
- 必须断言：列出所有受影响调用方及资格变化；未知归属扩大或拒绝选取。
- 反向控制：新增未映射文件不能得到空测试通过。
- 层级与基质：L0；source/metadata/static negative controls。静态守卫；不代表运行时效果。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T01 构造与关闭

责任组件：SessionExecutorBuilder 与进程 composition root。入口：[源码或现有门禁](../../crates/server/awaken-session-executor/src/host/dependencies/environment.rs)。风险基准：P0。类型：行为域。

校准：细化。能力是否必需取决于启用模式；明确构造顺序、实例及关闭责任。

相关套件证明：E01, E03, E09，只支持原证明声明的范围，不自动适用于下列全部场景。

### T01-C01 构造顺序无关

- 场景：分别先安装 Secret Broker 后选 Provider、先选 Provider 后安装 Broker，并替换 fallback Provider。
- 必须断言：最终 Provider 使用同一 Broker、Memory mounter 和时钟；不因配置调用顺序丢失能力。
- 反向控制：移除最终绑定步骤或保留旧 Provider，测试必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

| 调用顺序 | 选中实例 | 预期能力 |
| --- | --- | --- |
| Broker → Provider A | A | A 使用原 Broker |
| Provider A → Broker | A | 与前一行相同 |
| fallback A → Broker → Provider B | B | B 使用原 Broker，A 不获得新执行权 |
| Provider A → mounter → Provider B → Broker | B | B 的 Broker 与 mounter 均为原受控实例 |

### T01-C02 按模式要求能力

- 场景：选择 sandbox-stdio MCP、checkpoint、cloud-native 等模式，分别遗漏对应必需能力。
- 必须断言：构造或 readiness 在物理效果前返回稳定错误；普通不启用该能力的模式仍可构造。
- 反向控制：不启用 checkpoint 的简单 SDK consumer 不应被错误地拒绝。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B012；当前提交有部分执行证明。完整场景：待验收。

### T01-C03 实例与关闭

- 场景：混入外来 Store/clock/应用实例、重复绑定，随后取消启动或关闭接收方。
- 必须断言：拒绝异实例且零越界读取；重复绑定拒绝；无强引用环、遗留任务和伪造关闭成功。
- 反向控制：留下原强引用或漏 join 的负控应暴露未关闭资源。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B011, B072；当前提交有部分执行证明。完整场景：待验收。

## T02 SDK 独立使用

责任组件：awaken-runtime 与独立 consumer。入口：[源码或现有门禁](../../scripts/ci/check_sdk_consumer.py)。风险基准：P1。类型：行为域。

校准：保留。SDK 独立性仍是发布边界，不能由服务端集成测试替代。

相关套件证明：E07，只支持原证明声明的范围，不自动适用于下列全部场景。

### T02-C01 最小与默认 consumer

- 场景：在仓库外独立 crate 分别选择最小和默认 features，执行单次与多轮 Run。
- 必须断言：不链接服务端 Session 管理权威；实际产生并读取 Run 结果；依赖闭包符合 SDK 边界。
- 反向控制：加入服务端依赖或仅 cargo check 不得满足本用例。
- 层级与基质：L1；independent consumer, declared minimal/default features。SDK 本身与 API；不含服务端或物理 Provider。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T02-C02 重建与恢复

- 场景：保存执行状态和 ticket，销毁 consumer，再以原 Run 身份重建。
- 必须断言：恢复原等待点和原 Run；不因重建新建 Run 或重复已提交步骤。
- 反向控制：只在同一对象上 resume 不能替代进程/对象重建。
- 层级与基质：L1；independent consumer, declared minimal/default features。SDK 本身与 API；不含服务端或物理 Provider。
- 绑定及状态：B007；找到源码，未绑定本次选定运行证明。完整场景：待验收。

### T02-C03 API 兼容

- 场景：改变公开类型、features 或示例，验证受支持旧入口及预期拒绝入口。
- 必须断言：API snapshot、示例运行及 compile-fail 分别通过；兼容范围和迁移差异明确。
- 反向控制：删除公开能力、诊断因无关 import 错误而失败必须被识别。
- 层级与基质：L1；independent consumer, declared minimal/default features。SDK 本身与 API；不含服务端或物理 Provider。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T03 SDK 等待和结果语义

责任组件：Runtime run 与 resume。入口：[源码或现有门禁](../../crates/runtime/awaken-runtime/tests/sdk_api.rs)。风险基准：P0。类型：行为域。

校准：细化。区分等待返回、恢复命令接纳及 Run 终态，避免把协议等待当业务完成。

相关套件证明：E07，只支持原证明声明的范围，不自动适用于下列全部场景。

### T03-C01 首次等待返回

- 场景：Run 达到 Awaiting，调用者在首次返回后不提交回复。
- 必须断言：返回原 Run 和未消费 ticket；返回等待不会自动批准或推进工具。
- 反向控制：自动消耗 ticket 或阻塞到最终终态必须失败。
- 层级与基质：L1；independent consumer, declared minimal/default features。SDK 本身与 API；不含服务端或物理 Provider。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T03-C02 审批超时后恢复

- 场景：审批超时或拒绝后重建 Runtime，再向原 ticket 回复。
- 必须断言：保留定义的等待/终态语义；有效恢复最多接纳一次并可取得原 Run。
- 反向控制：重建后新建 Run、重复驱动原工具或丢失 ticket 必须失败。
- 层级与基质：L1；independent consumer, declared minimal/default features。SDK 本身与 API；不含服务端或物理 Provider。
- 绑定及状态：B007；找到源码，未绑定本次选定运行证明。完整场景：待验收。

### T03-C03 取消与回调失败

- 场景：在接纳前、接纳后等待期间分别取消或令回调失败。
- 必须断言：按已提交事实返回原身份和可恢复状态；不得把连接或回调失败推断为业务成功。
- 反向控制：仅检查 error.is_err 不足，必须检查持久状态与调用计数。
- 层级与基质：L1；independent consumer, declared minimal/default features。SDK 本身与 API；不含服务端或物理 Provider。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T04 创建与错误顺序

责任组件：Session application 与协议创建适配。入口：[源码或现有门禁](../../crates/server/awaken-session-application/src/tests/creation.rs)。风险基准：P0。类型：行为域。

校准：细化。要求已定义的错误优先级；不要为无关错误任意规定全局顺序。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T04-C01 早期拒绝

- 场景：组合非法输入、越权、缺少能力和不可用依赖，按入口决策表调用。
- 必须断言：返回契约规定的首个错误；后续准备、root 写入和模型调用计数为零。
- 反向控制：改换无关错误文案不能改变分类或优先级。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B065；当前提交有部分执行证明。完整场景：待验收。

### T04-C02 投影先于提交

- 场景：令协议所需字段或资源最终化失败，再尝试两条创建入口。
- 必须断言：root 插入前完成必需转换；拒绝不留下半成品 root/运行效果。
- 反向控制：先写 root 后转换的负控必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B009；当前提交有部分执行证明。完整场景：待验收。

### T04-C03 准备后写入失败

- 场景：外部准备成功后令 root CAS/插入失败或响应丢失，再重建服务。
- 必须断言：已准备责任可凭原意图发现并补偿/观察；精确重试保留原身份。
- 反向控制：仅在同一调用栈执行 best-effort cleanup 不能满足崩溃恢复。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T05 Session CAS 与回执重放

责任组件：Session aggregate 和 Session repository。入口：[源码或现有门禁](../../crates/server/awaken-session-application/src/tests/observed_commit.rs)。风险基准：P0。类型：行为域。

校准：保留。保留原子 root 与历史回执语义，分开逻辑竞争和持久崩溃。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T05-C01 并发赢家

- 场景：两个独立客户端以同一预期 revision 提交不同命令。
- 必须断言：仅一个合法赢家；root、事实、outbox 和回执一致；输家不产生事实。
- 反向控制：两个客户端共用测试锁串行化不能证明数据库竞争。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T05-C02 回原提交回执

- 场景：命令成功或丢响应后，另一个命令更新 root，再重放前一命令。
- 必须断言：返回前一命令原 committed_revision 和原回执；改变 fingerprint 的重试拒绝。
- 反向控制：从当前 root 临时重建回执的实现必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B091；当前提交有部分执行证明。完整场景：待验收。
- 已核对的现有断言子范围：Windows 组件，SQLite 内存连接；不含真实进程死亡或所有后端 CAS。原回执与当前 root 分离，不能据此签收 T05 全部条款。

| 原操作 | 当前 root | 重试 | 预期 |
| --- | --- | --- | --- |
| 已提交但响应丢失 | 未变化 | 相同 key/fingerprint | 原 receipt/revision |
| 已提交 | 后来已更新 | 相同 key/fingerprint | 仍是原 receipt/revision |
| 已提交 | 任意较新 root | 相同 key、不同 fingerprint | Conflict、零额外事实 |

### T05-C03 事务与进程死亡

- 场景：在各根写入口的 root/事实/outbox/回执写入之间注入 SQL 故障并终止子进程。
- 必须断言：同事务全回滚或暴露完整提交；重启只继续原意图，不出现部分事实。
- 反向控制：只手动回滚一段测试 SQL 不得代替生产命令路径。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：B079；找到源码，未绑定本次选定运行证明。完整场景：待验收。

## T06 归属与当前授权

责任组件：各受控 application 和执行入口。入口：[源码或现有门禁](../../crates/server/awaken-session-application/src/tests/authority/file_authorization.rs)。风险基准：P0。类型：行为域。

校准：细化。冻结配置身份与动态撤销权分开验证，减少不必要的重复目录读取。

相关套件证明：E01, E10，只支持原证明声明的范围，不自动适用于下列全部场景。

### T06-C01 归属拒绝

- 场景：替换 Workspace、Session、Thread、Resource 所属方或复用外来绑定对象。
- 必须断言：在未获准的数据读取/物理效果前拒绝；错误响应不泄露另一个租户的存在性。
- 反向控制：只在 HTTP 层校验而内部入口可绕过必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T06-C02 等待后撤权

- 场景：在锁等待、凭据解析或网络准备 barrier 后撤销当前权利。
- 必须断言：在契约规定的物理授权线性化点再次校验；不使用缓存成功绕过新撤权。
- 反向控制：把再次校验移到长等待之前的负控必须被检测。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T06-C03 冻结与动态事实

- 场景：运行中更新发布头及动态权限，恢复同一已接纳 Run。
- 必须断言：冻结版本、价格和模型身份保持原值；动态撤销按当前权威判定。
- 反向控制：一律重新解析当前版本或一律相信冻结授权都应失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B064；当前提交有部分执行证明。完整场景：待验收。

## T07 Run 接纳与激活

责任组件：SessionRun application 与 dispatch。入口：[源码或现有门禁](../../crates/server/awaken-session-application/src/tests/run_admission.rs)。风险基准：P0。类型：行为域。

校准：细化。分别约束权威提交和外部效果，不承诺任意网络副作用恰好一次。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T07-C01 激活前零执行

- 场景：在 reserve 后、activity CAS 失败或 activate 之前暂停。
- 必须断言：未激活不调用模型/工具；未接纳 reserve 有明确释放责任。
- 反向控制：用成功返回码代替模型/工具计数不能签收。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T07-C02 丢响应重放

- 场景：activity 或 activation 提交后丢响应，再用原 operation id 重试。
- 必须断言：同 reserve、Run 和原回执；不创建第二次接纳；root 是 epoch 唯一权威。
- 反向控制：以较新的独立 epoch 覆盖原 root 回执必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B044；当前提交有部分执行证明。完整场景：待验收。

### T07-C03 崩溃后观察

- 场景：在提交后效果前、效果后回执前终止执行器并恢复。
- 必须断言：可观察操作以原身份恢复；无法确认的外部效果保留 uncertainty 并拒绝不安全重启。
- 反向控制：外部超时后一律重试执行不得通过。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T08 Work 与 Run 桥接

责任组件：Session work projection 与 Run dispatch。入口：[源码或现有门禁](../../crates/server/awaken-session-application/src/tests/authority/work_projection.rs)。风险基准：P0。类型：行为域。

校准：细化。Work 是领取/租约协议事项；ack 不等于 Run 成功，healthcheck 不强制创建 Run。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T08-C01 唯一投影路径

- 场景：self-hosted/nonterminal 与其他 Session 交叉，重复 reconciliation。
- 必须断言：只有符合条件的 Session 产生一个稳定 Work 身份；重复不新增。
- 反向控制：另一入口偷偷 enqueue 或 terminal 仍投递必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B025；当前提交有部分执行证明。完整场景：待验收。

### T08-C02 终态后 ack 失败

- 场景：Run 已终态，令 Work ack/响应失败并重连。
- 必须断言：只重试原 ack；不重新执行模型/工具；保留独立 Run 结果。
- 反向控制：以 Work ack 成功合成 Run 成功必须失败。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T08-C03 两侧租约与 healthcheck

- 场景：Work 与 dispatch 租约分别过期、接管，并领取 healthcheck。
- 必须断言：不得互用 epoch 或 lease；旧领取者拒绝；healthcheck 可独立完成。
- 反向控制：仅测试单侧 lease 或用同一个 epoch 变量不能证明本项。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T09 租约边界与竞争

责任组件：Work、dispatch、realization 各自权威。入口：[源码或现有门禁](../../crates/stores/awaken-work-store/tests/lease_book_properties.rs)。风险基准：P0。类型：行为域。

校准：保留。不同租约各有状态机；共用时钟和边界规则，不合并 epoch。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T09-C01 精确边界

- 场景：用注入时钟分别采样 expiry-1、expiry、expiry+1 并续租。
- 必须断言：严格采用各既有契约的有效边界；跨真实后端结果一致。
- 反向控制：测试替换为实际 sleep 导致未命中精确边界不能签收。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：B002；当前提交有部分执行证明。完整场景：待验收。

### T09-C02 并发接管

- 场景：两客户端在 barrier 同时 renew/reclaim/commit，旧 owner 继续提交。
- 必须断言：仅当前 owner 可提交；stale receipt 拒绝；失败不扩权。
- 反向控制：测试内串行锁掩盖数据库竞争必须拒绝。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T09-C03 身份跨重建

- 场景：删除最高序号项、重开/重启、换 incarnation 后重试旧身份。
- 必须断言：身份不复用；lease epoch 和 runtime incarnation 保持各自含义。
- 反向控制：仅比较队列长度不够，必须比较原身份和拒绝效果。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：B057, B086；当前提交有部分执行证明。完整场景：待验收。

## T10 物理停止与未知结果

责任组件：Native、ACP、Sandbox 的现有停止观察者。入口：[源码或现有门禁](../../crates/runtime/awaken-ext-builtin-tools/tests/hand/windows_process.rs)。风险基准：P0。类型：行为域。

校准：细化。停止证明必须按适配器能力声明，future 被 drop 不等于物理静止。

相关套件证明：E03, E04，只支持原证明声明的范围，不自动适用于下列全部场景。

### T10-C01 Windows 进程树停止

- 场景：先证明真实后代进程就绪，再取消、超时、restart 或 drop Session。
- 必须断言：已拥有进程树退出后才报告完成；peer 不受影响；再次执行可复用环境。
- 反向控制：只等待父进程退出或只检查 future 完成必须失败。
- 层级与基质：L3；native Windows x86_64-pc-windows-msvc；actual owned child/descendant processes。所选 Windows 模式；不含 Linux 或未启用执行器。
- 绑定及状态：B013, B090；当前提交有部分执行证明。完整场景：待验收。

### T10-C02 ACP 与 Sandbox 证明

- 场景：构造父进程已退出但后代/远程操作仍存活或状态未知。
- 必须断言：不伪造 stopped/quiesced；保留原操作观察责任和稳定身份。
- 反向控制：unknown 映射为 stopped 的负控必须失败。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T10-C03 恢复原操作

- 场景：重建具有恢复能力的执行器，再提交相同操作身份。
- 必须断言：仅有能力的适配器重连原操作；不具备能力时给出可操作的拒绝。
- 反向控制：适配器无证据时启动新操作不能算恢复。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T11 工具回复接纳与重放

责任组件：Session event application。入口：[源码或现有门禁](../../crates/server/awaken-protocol-managed/src/state/events/tests.rs)。风险基准：P0。类型：行为域。

校准：保留。保持 ticket/version/Run/Thread 坐标约束及消费语义。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T11-C01 终态精确重放

- 场景：终态后提交新回复、原精确重试和改变内容重试。
- 必须断言：拒绝新接纳；精确重放原回执；不再驱动 Run 或工具。
- 反向控制：以全部终态回复一律拒绝会破坏合法回执重放。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B089；当前提交有部分执行证明。完整场景：待验收。

### T11-C02 坐标与授权

- 场景：混用 ticket、版本、跨 Run/Thread 坐标和旧投影 ID。
- 必须断言：只接受确切原坐标且不扩张权限；拒绝时零驱动和零事实追加。
- 反向控制：只校验公开 ID 的前缀不能通过。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T11-C03 并发消费

- 场景：两个客户端并发提交同 ticket 的相同/不同回复并丢响应。
- 必须断言：合法消费最多一次；相同重试回原回执；冲突结果跨 SQL 后端一致。
- 反向控制：只在单个进程 Mutex 内串行化不能替代根 CAS。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T12 事件批次

责任组件：Session aggregate 与事件适配。入口：[源码或现有门禁](../../crates/server/awaken-session-application/src/tests/event_batches.rs)。风险基准：P0。类型：行为域。

校准：保留。协议语法验证与 aggregate 业务判定保持分层。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T12-C01 批次边界与拒绝

- 场景：空/最大/超限批次、错误顺序、归档 child、无效目标和合法 no-op。
- 必须断言：稳定分类；非法目标零效果；合法 no-op 不追加事实；错误优先级遵循入口表。
- 反向控制：仅验证 JSON grammar 不得替代业务接纳。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T12-C02 批次原回执

- 场景：提交批次后丢响应、重建服务，重放原 fingerprint 或改动批次。
- 必须断言：精确重放原 root 回执；改变 fingerprint 冲突；不重复追加/执行。
- 反向控制：用当前 root revision 代替原回执必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B026；当前提交有部分执行证明。完整场景：待验收。

### T12-C03 暂停与事务失败

- 场景：预算暂停时交叉 interrupt-only/普通批次，并在写入中途故障。
- 必须断言：只接纳既有策略允许动作；同事务记录一起回滚。
- 反向控制：把暂停处理放入协议 parser 或允许半批次提交必须失败。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T13 预算与动态策略

责任组件：BudgetAccounting、Session policy 与 effect gate。入口：[源码或现有门禁](../../crates/server/awaken-session-application/src/tests/budget_settlement.rs)。风险基准：P0。类型：行为域。

校准：细化。明确冻结配置与动态限制；不把模型价格变化追溯应用到已接纳操作。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T13-C01 结算回执

- 场景：两个 Thread 结算、CAS 失败、提交响应丢失后重试。
- 必须断言：原 root receipt 和 usage 一致；不重复扣费；失败不丢账。
- 反向控制：只检查累计总数不够，应检查每笔 operation 身份。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B010；当前提交有部分执行证明。完整场景：待验收。

### T13-C02 执行中收紧

- 场景：在模型或外部工具效果前 barrier 收紧预算/egress 动态限制。
- 必须断言：原冻结模型和计价保持；下一受控效果按当前动态权威拒绝或等待。
- 反向控制：仅创建时做一次预算/网络检查不得通过。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T13-C03 恢复预算

- 场景：预算达到边界后提高允许额度并恢复原等待。
- 必须断言：只恢复允许的原 Run；无重复已结算步骤；边界值和消息身份稳定。
- 反向控制：通过新建 Run 绕过预算等待必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。
