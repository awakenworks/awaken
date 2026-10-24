# Session 执行与资源测试验收清单：具体用例（T28–T41）

本文件是[验收清单总览](session-execution-test-acceptance.md)的具体用例部分，因仓库文件长度限制按编号拆分；编号、判定规则和语义与总览一致。

## T28 物理副作用前的授权

责任组件：各 effect executor 及当前 authority fence。入口：[源码或现有门禁](../../crates/worker/awaken-sandbox-container/src/tests.rs)。风险基准：P0。类型：行为域。

校准：细化。为每个副作用定义授权线性化点和不确定性边界，而非宣称任意时刻撤权可追溯取消已发出的请求。

相关套件证明：E03，只支持原证明声明的范围，不自动适用于下列全部场景。

### T28-C01 过期和错误身份

- 场景：在真正 runtime/mount/process/credential 效果前置入过期 fence、错误 incarnation 或 epoch。
- 必须断言：物理调用数为零；返回稳定 stale/expired 分类；不物化 mount 或材料。
- 反向控制：只在最终 receipt 提交时检查授权不足。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：B031；找到源码，未绑定本次选定运行证明。完整场景：待验收。

### T28-C02 等待与替换窗口

- 场景：在授权后到效果前插入 barrier，撤权、换 owner 或关闭 admission，再放行。
- 必须断言：在契约要求的最近 effect gate 再校验；旧缓存成功或未绑定 disposal 不能扩权。
- 反向控制：把 barrier 移到效果之后的测试不能证明前置拒绝。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T28-C03 已发出效果的未知结果

- 场景：请求已经外发后才撤权或断网，恢复器重连原身份。
- 必须断言：记录并观察原操作，不能声称零效果；无观察能力则保留 unknown，不进行不安全重发。
- 反向控制：把 RPC 失败等同效果未发生必须失败。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T29 进程 文件 资源和产物

责任组件：Native ProcessHandle、Sandbox 和 resource staging。入口：[源码或现有门禁](../../crates/worker/awaken-sandbox-memoryd/src/mounter.rs)。风险基准：P0。类型：行为域。

校准：细化。增加文件系统与 Windows 路径边界；生命周期、产物来源和数据责任分别观察。

相关套件证明：E04, E06，只支持原证明声明的范围，不自动适用于下列全部场景。

### T29-C01 原生进程树与 peer

- 场景：启动就绪子进程和后代，分别取消、timeout、restart/drop，保留无关 peer。
- 必须断言：拥有的完整进程范围退出；peer 不受影响；资源/pipe/端口释放且可再次运行。
- 反向控制：只检查进程 PID 已退出而后代活着必须失败。
- 层级与基质：L3；native Windows x86_64-pc-windows-msvc；actual owned child/descendant processes。所选 Windows 模式；不含 Linux 或未启用执行器。
- 绑定及状态：B023, B068；当前提交有部分执行证明。完整场景：待验收。

### T29-C02 Copy 数据责任

- 场景：可写 Copy 更新、删除和冲突，teardown 中断后从 surviving files 恢复。
- 必须断言：写回或冲突责任确立前不销毁源；只读不写回；恢复重复执行幂等。
- 反向控制：清空目录后再尝试写回的负控必须失败。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：B061, B005；当前提交有部分执行证明。完整场景：待验收。

### T29-C03 产物来源与路径安全

- 场景：长路径/Unicode/相对路径、symlink/reparse point、共享祖先目录，重复收集原 Run 产物。
- 必须断言：不逃出受控根或写祖先；来源绑定原 Run；材料确认完整后才清理；日志和 origin 不含凭据。
- 反向控制：用字符串前缀检查代替真实路径边界应被负控检测。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：B037, B087；当前提交有部分执行证明。完整场景：待验收。

## T30 Anthropic Managed Agents 兼容

责任组件：Managed protocol 与受支持官方 SDK。入口：[源码或现有门禁](../../packages/managed-sdk-oracle/package.json)。风险基准：P1。类型：行为域。

校准：细化。拆分类型/oracle、SDK mock transport 和真实 Awaken 服务行为；外部托管服务属于单独部署资格。

相关套件证明：E05，只支持原证明声明的范围，不自动适用于下列全部场景。

### T30-C01 锁定版本的 wire 契约

- 场景：按仓库锁定的 TS/Python current、oldest 及变更锚点检查 DTO、路径、分页、absent/null/enum/error。
- 必须断言：各受支持版本形状和 capability selection 明确；升级 diff 可复核；不借内部重命名改变 wire。
- 反向控制：仅 TypeScript 编译不能替代响应和请求行为。
- 层级与基质：L1；pinned official TS/Python SDK, oracle, request/response witnesses。wire/transport；mock fetch 不代表实际服务。
- 绑定及状态：待绑定现有用例或新增实现；只有套件级部分证明，尚不能签收完整场景。完整场景：待验收。

### T30-C02 SDK 到真实本地服务

- 场景：使用官方 TS 和 Python SDK 连接同一源码的真实 Awaken 服务，执行 Session/Environment/Memory 生命周期、分页和错误。
- 必须断言：实际请求经路由/授权/application；返回原身份、版本和错误；两种 SDK 观察一致。
- 反向控制：拦截 fetch 返回 data:[] 的测试不能算真实服务 E2E。
- 层级与基质：L3；real Awaken service/process boundary；actual client/transport；declared backend and fixture。模型可用确定性测试 Provider；授权、路由、事务、传输不可全部 mock。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

| 客户端 | 服务 | 必需操作 |
| --- | --- | --- |
| 锁定的 TS SDK | 真实本地 Awaken 服务 | Session 创建/输入/等待回复/结果/重放；Environment 创建/更新/归档；Memory 内容/history/redact |
| 锁定的 Python SDK | 相同源码与 backend 的真实服务 | 相同操作及 absent/null、分页、错误和 capability selection |
| 上述每个支持版本锚点 | 明确兼容/拒绝边界 | 不支持的新操作有原 wire 语义，不假造能力 |

### T30-C03 中断恢复行为

- 场景：客户端断流、响应丢失、服务重启后以原 ID/ticket/cursor 继续。
- 必须断言：公开身份、原回执和断连策略保持；不重启已完成效果；适配器恢复语义相同。
- 反向控制：只做 hosted canary 的 happy path 不能替代本地可控故障注入。
- 层级与基质：L3；real Awaken service/process boundary；actual client/transport；declared backend and fixture。模型可用确定性测试 Provider；授权、路由、事务、传输不可全部 mock。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T31 持久投影与公开身份

责任组件：Managed committed projector。入口：[源码或现有门禁](../../crates/server/awaken-protocol-managed/src/state/events/tests.rs)。风险基准：P0。类型：行为域。

校准：保留。区分组件级 active-active 与真实多进程，公开身份由持久坐标决定。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T31-C01 消息身份重建

- 场景：相同根 assistant 响应含 thinking/text，warm/cold/active-active 多次读取及分页。
- 必须断言：同持久坐标得到相同公开 ID、ordinal 和 cursor；不会因缓存丢失重新编号。
- 反向控制：随机新 ID 或按每次读取索引编号必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B070；当前提交有部分执行证明。完整场景：待验收。

### T31-C02 Outcome 投影

- 场景：重复、乱序或落后的投影候选，cold 重建及多 owner 观察。
- 必须断言：前缀不倒退；durable outcome 稳定；live preview 不冒充最终事实。
- 反向控制：用文本预览生成 terminal outcome 的负控必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B024；当前提交有部分执行证明。完整场景：待验收。

### T31-C03 真实多进程恢复

- 场景：两个服务实例共享真实 backend，一方投影中途退出，另一方接续并重建缓存。
- 必须断言：公开 ID、内容顺序和 cursor 与重启前一致；不产生遗漏或重复终态。
- 反向控制：同一进程的两个内存对象不能替代多进程资格。
- 层级与基质：L3；real Awaken service/process boundary；actual client/transport；declared backend and fixture。模型可用确定性测试 Provider；授权、路由、事务、传输不可全部 mock。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T32 连接与恢复策略

责任组件：Managed SSE/outbox 与 Run cancellation owner。入口：[源码或现有门禁](../../crates/server/awaken-coordinator/src/managed_lifecycle.rs)。风险基准：P0。类型：行为域。

校准：细化。断开连接不默认取消业务；分别验证部署明确选择的断连策略与显式取消。

相关套件证明：E02，只支持原证明声明的范围，不自动适用于下列全部场景。

### T32-C01 断流策略

- 场景：Run 活跃时关闭 SSE/HTTP 连接，再查询原 Run 或显式取消。
- 必须断言：按照已选 detach/cancel 策略处理；连接 owner 不夺取 Run owner；显式取消有独立回执。
- 反向控制：将 socket close 无条件映射为 Run terminal 必须失败。
- 层级与基质：L3；real Awaken service/process boundary；actual client/transport；declared backend and fixture。模型可用确定性测试 Provider；授权、路由、事务、传输不可全部 mock。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T32-C02 fanout 与重放

- 场景：打开 SSE，追加 outbox，重复 fanout，再用旧 cursor 重连。
- 必须断言：新事实可见；精确重放幂等；同一事实不产生重复公共身份。
- 反向控制：通过单次直接读取而不经过 fanout 不能覆盖开放连接。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B043；找到源码，未绑定本次选定运行证明。完整场景：待验收。

### T32-C03 分页与 preview 交错

- 场景：历史分页过程中追加 live preview、终态及服务重启。
- 必须断言：无遗漏、重复、倒退或错误终态；cursor 继续同一持久前缀。
- 反向控制：只断流不恢复或只验证消息数不够。
- 层级与基质：L3；real Awaken service/process boundary；actual client/transport；declared backend and fixture。模型可用确定性测试 Provider；授权、路由、事务、传输不可全部 mock。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T33 错误分类与重试

责任组件：应用错误和协议/执行适配。入口：[源码或现有门禁](../../crates/server/awaken-session-mcp/src/mcp/error.rs)。风险基准：P0。类型：行为域。

校准：细化。区分接纳失败、确定回滚、提交成功丢响应和副作用结果未知四类。

相关套件证明：E03，只支持原证明声明的范围，不自动适用于下列全部场景。

### T33-C01 分类不依赖文案

- 场景：对 input/auth/conflict/unavailable/unknown effect 注入误导性 message。
- 必须断言：分支只用类型/稳定 code；HTTP 与 Run 错误保留边界和可重试语义。
- 反向控制：改为 message.contains 判断的负控必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B040；找到源码，未绑定本次选定运行证明。完整场景：待验收。

### T33-C02 重试原身份

- 场景：分别制造提交前失败、成功丢响应、效果未知，再重试。
- 必须断言：确定未接纳可重新接纳；已提交回原回执；未知效果先观察原 operation。
- 反向控制：对所有 5xx 无条件创建新 operation 的实现必须失败。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T33-C03 跨适配器一致

- 场景：同一故障经本地、HTTP Worker、Native/ACP/MCP 入口传播。
- 必须断言：保留原权威、uncertainty 和 retry hint；不在某个适配器静默降级成功。
- 反向控制：只测试 error enum 构造不验证投影链不能签收。
- 层级与基质：L3；real Awaken service/process boundary；actual client/transport；declared backend and fixture。模型可用确定性测试 Provider；授权、路由、事务、传输不可全部 mock。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T34 真实后端资格门禁

责任组件：各持久化 adapter 的既有 conformance。入口：[源码或现有门禁](../../scripts/ci/pg_tests.sh)。风险基准：P0。类型：公共门禁。

校准：改为公共门禁。这是适用于相关行为的轴，不再复制一组全部业务测试；组合按事务/状态机风险选择。

相关套件证明：E01, E02，只支持原证明声明的范围，不自动适用于下列全部场景。

### T34-C01 同语义表

- 场景：对实际支持的 Volatile、文件 SQLite、PostgreSQL 运行同一行为表。
- 必须断言：相同命令、CAS、receipt、历史、时钟和回滚语义；不由各后端自定义不同 oracle。
- 反向控制：Volatile 通过不能证明 SQL 事务，in-memory SQLite 不能证明文件重启。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T34-C02 durability 与取消

- 场景：真实连接、独立客户端、子进程死亡、取消和重连后执行原操作。
- 必须断言：完整提交/回滚且连接可再用；fixture 必需时不可因不可用而跳过。
- 反向控制：catch 连接错误 return 绿色或只验证断言数量必须拒绝。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T34-C03 证明边界

- 场景：后端某条测试通过，但另一操作、迁移版本或 features 尚未运行。
- 必须断言：逐条记录已执行切片和未验收切片；不以 suite 总数推断等价。
- 反向控制：同名 mock 后端、仅 cargo check 或未执行 ignored 用例不能晋级。
- 层级与基质：L0；source/metadata/static negative controls。静态守卫；不代表运行时效果。
- 绑定及状态：`test_count_only_validator_accepts_what_name_alignment_refuses`、`test_unrun_slice_or_compile_only_run_cannot_be_promoted`；当前提交本机 Windows L0 执行（E11；rebase 到 origin/main 后复跑 E15），每条 oracle 与反向控制已具名绑定。完整场景：待验收（公共门禁 T35 未闭合）。

## T35 平台与执行器资格门禁

责任组件：现有原生 OS/substrate 门禁。入口：[源码或现有门禁](../../scripts/ci/check-trusted-host-portability.sh)。风险基准：P0。类型：公共门禁。

校准：改为公共门禁。平台是资格维度；本任务 Windows 限定不要求现在执行 Linux，但不得把范围外标为通过。

相关套件证明：E01, E04, E06, E08，只支持原证明声明的范围，不自动适用于下列全部场景。

### T35-C01 Windows MSVC

- 场景：原生 x86_64-pc-windows-msvc 实际执行受影响 Windows 路径与真实后端。
- 必须断言：保留 target、features、linker、companion 和实际病例；path/process/DLL/locking 行为实际运行。
- 反向控制：Linux 容器、交叉编译或 Windows --no-run 不能替代。
- 层级与基质：L3；native Windows x86_64-pc-windows-msvc；actual owned child/descendant processes。所选 Windows 模式；不含 Linux 或未启用执行器。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T35-C02 Linux 物理基质

- 场景：对声明支持且受变更影响的 Linux namespace/FUSE/container 执行 native gate。
- 必须断言：具有所需内核/引擎并实际发生 mount/process/effect；未具备时明确未验收或按模式不适用。
- 反向控制：Windows fuse feature 编译/Copy 测试不得报告内核 FUSE 通过。
- 层级与基质：L4；native Linux kernel/FUSE or applicable namespace/container substrate。本次 Windows 任务不执行；只可未验收或有能力拒绝证明的不适用。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T35-C03 执行器与托管边界

- 场景：按实际启用的 Native/ACP/MCP/Sandbox/OCI/Kubernetes/外部中介逐一选择。
- 必须断言：每个模式有独立资格；未启用经能力拒绝证明可记不适用；外部部署资格不冒充仓库行为证明。
- 反向控制：把 offline Kubernetes 测试、TLC 或 Stub 中介当真实集群/托管服务必须拒绝。
- 层级与基质：L3；real Awaken service/process boundary；actual client/transport；declared backend and fixture。模型可用确定性测试 Provider；授权、路由、事务、传输不可全部 mock。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T36 选取与证据有效性门禁

责任组件：canonical test plan 与 evidence validator。入口：[源码或现有门禁](../../scripts/ci/rust_test_plan.py)。风险基准：P0。类型：公共门禁。

校准：改为公共门禁。名称、执行、断言覆盖和验收四层分开；清单不是另一个可执行 suite manifest。

相关套件证明：E04, E07，只支持原证明声明的范围，不自动适用于下列全部场景。

### T36-C01 影响选择

- 场景：改变共享类型、ctor、transaction/auth、manifest、脚本和 companion；再加入未知文件。
- 必须断言：扩展到真实反向依赖和必要层级；未知信息扩大/拒绝；不静默零用例。
- 反向控制：只按文件所在 crate 选测试的负控必须失败。
- 层级与基质：L0；source/metadata/static negative controls。静态守卫；不代表运行时效果。
- 绑定及状态：`test_shared_contract_change_selects_every_real_reverse_dependant`、`test_crate_only_selection_is_detected_as_too_narrow`、`test_unknown_or_global_input_widens_and_never_selects_zero`；当前提交本机 Windows L0 执行（E11；rebase 到 origin/main 后复跑 E15），每条 oracle 与反向控制已具名绑定。完整场景：待验收（公共门禁 T35 未闭合）。

### T36-C02 列出与执行一致

- 场景：零 case、改名、同总数不同名称、ignored/fixture skip、child 非零退出、source 改变。
- 必须断言：逐名和 occurrence 对齐；每个必须用例实际通过；失败和跳过不签收；source 不变。
- 反向控制：记录总数等于期望数不能替代逐名对齐。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：`test_each_misalignment_is_refused_for_its_own_reason`、`test_count_only_validator_accepts_what_name_alignment_refuses`、`test_mutations_at_each_sequence_fence_stop_or_reject_execution`；当前提交本机 Windows L0 执行（E11、E12；rebase 后复跑 E15），每条 oracle 与反向控制已具名绑定。完整场景：待验收（公共门禁 T35 未闭合）。

### T36-C03 历史证明复用

- 场景：rebase 后源码相同部分、依赖/feature/config 变化部分、仅 cache 命中。
- 必须断言：保留原 source/命令/日志；逐输入复用审查后才能继承资格；cache 命中与编译成功不是测试证据。
- 反向控制：把历史 receipt 的 SHA 直接改为当前 HEAD 必须拒绝。
- 层级与基质：L0；source/metadata/static negative controls。静态守卫；不代表运行时效果。
- 绑定及状态：`test_unrun_slice_or_compile_only_run_cannot_be_promoted`、`test_historical_receipt_relabelled_as_current_is_refused`；当前提交本机 Windows L0 执行（E11；rebase 到 origin/main 后复跑 E15），每条 oracle 与反向控制已具名绑定。完整场景：待验收（公共门禁 T35 未闭合）。

## T37 持久格式迁移与跨版本恢复

责任组件：各 schema bundle、immutable id allocator 和 codecs。入口：[源码或现有门禁](../../crates/stores/awaken-work-store/src/schema.rs)。风险基准：P0。类型：行为域。

校准：新增。原清单未把迁移/已发布历史/高水位与非法持久行列为独立风险域。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T37-C01 已发布 schema 升级

- 场景：从每个受支持已发布 schema 启动，保留 live lease、已删最高 ID 和旧 revision，升级两次。
- 必须断言：原身份和 lease 不变；历史/command/outbox 只迁移一次；最高 ID 不复用。
- 反向控制：只从空数据库建表不能证明升级。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：B074, B048, B041；当前提交有部分执行证明。完整场景：待验收。

### T37-C02 allocator 事务与重启

- 场景：独立连接并发分配，令 work/watermark 任一写失败，再进程重启后继续分配。
- 必须断言：work 与 watermark 同事务；无 reset/重复身份；旧标识继续被 fence。
- 反向控制：仅使用内存原子计数器或同一连接串行执行不得签收。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：B073, B047, B078, B051；当前提交有部分执行证明。完整场景：待验收。

### T37-C03 非法历史与版本耗尽

- 场景：注入缺失/负数/畸形 allocator、非法历史行及版本最大值，再读取/写入。
- 必须断言：返回稳定错误且零修复性猜测/零覆盖；合法精确重放在耗尽时仍可返回原结果。
- 反向控制：自动默认零、wrap 或重建空 catalog 的实现必须失败。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：B021, B053, B082；当前提交有部分执行证明。完整场景：待验收。

## T38 容量 背压和故障隔离

责任组件：dispatch pool、inbox、registration 与 SQL adapters。入口：[源码或现有门禁](../../crates/stores/awaken-work-store/src/lib.rs)。风险基准：P0。类型：行为域。

校准：新增。正确性清单缺少积压、慢依赖、非饥饿和资源耗尽下的有界行为。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T38-C01 SQL 等待不阻塞调度

- 场景：SQLite 权威写锁/连接忙时持续 heartbeat 和 timer，独立 Session 继续工作。
- 必须断言：异步等待不阻塞 runtime timer；按既有期限失败；peer 的授权和关闭仍可进展。
- 反向控制：用阻塞数据库等待占满唯一 runtime 线程的负控必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B085, B075；当前提交有部分执行证明。完整场景：待验收。

### T38-C02 队列及慢消费者

- 场景：达到既有队列/worker 容量，消费者停读或外部依赖挂起，再取消/关闭。
- 必须断言：资源增长受既有容量约束；背压/拒绝分类正确；不静默丢已接纳责任；关闭仍在共享预算内。
- 反向控制：无界缓冲或依赖永不返回导致 drain 永不结束必须失败。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T38-C03 空间与资源失败

- 场景：生产写入/prepare 遇到只读、磁盘满、句柄/连接额度耗尽，恢复资源后重试。
- 必须断言：保留已提交/不确定责任；无 partial success/全局 panic；恢复原 operation；不以调大超时遮蔽失败。
- 反向控制：真实磁盘填满主机不是合法 fixture，应使用受控配额/注入层。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T39 Environment 镜像与软件包交付

责任组件：Environment image application、build store 和 worker。入口：[源码或现有门禁](../../crates/server/awaken-environment-image-build/src/application.rs)。风险基准：P0。类型：行为域。

校准：新增。原 T20/T21 只覆盖定义与投递，不能覆盖包到冻结镜像的异步构建状态机。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T39-C01 配方身份

- 场景：仅修改 authoring metadata，再分别改变 packages/base image/security policy。
- 必须断言：非执行字段不改变 recipe identity；执行字段改变确切配方；冻结 Session 使用原 immutable image。
- 反向控制：用 latest tag 或 mutable definition 推导已接纳 Session 必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B035, B008；当前提交有部分执行证明。完整场景：待验收。

### T39-C02 构建需求与 readiness

- 场景：registration 触发 build demand，builder 失败/退避后成功，并让旧版本历史查询继续。
- 必须断言：同需求身份幂等；未就绪不授予新接纳；失败按原状态重试且不饿死冻结历史。
- 反向控制：Stub builder 的成功只能证明编排，不能证明 OCI 构建实际发生。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B062, B036；当前提交有部分执行证明。完整场景：待验收。

### T39-C03 构建 claim 与真实产物

- 场景：两个构建 worker 竞争，旧 claim 到期；在外部 build/receipt 前后退出并恢复。
- 必须断言：只有当前 claim 发布确切 image digest；旧 owner 无权提交；支持引擎的真实产物符合配方与恢复身份。
- 反向控制：PostgreSQL claim 测试和 fake image 字符串不能代替引擎产物证明。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：B016；当前提交有部分执行证明。完整场景：待验收。

## T40 Memory 缓存与投影能力边界

责任组件：Resources invalidator 与 MemoryStoreMounter。入口：[源码或现有门禁](../../crates/contract/awaken-resource-contract/src/memory_invalidation.rs)。风险基准：P1。类型：行为域。

校准：新增。区分同进程 FUSE 失效、远端 CopyOnly 和未来分布式实时一致性；不默认要求实现新网络总线。

相关套件证明：E02, E06, E09，只支持原证明声明的范围，不自动适用于下列全部场景。

### T40-C01 原总线与本地失效

- 场景：同 backend 的多角色和 mount 共用 bus，尝试混入第二总线，再使接收方 lag/关闭。
- 必须断言：异总线构造在监听/挂载前拒绝；提交后对应 path/全 store 正确失效；lag 清全部相关缓存。
- 反向控制：只比较总线类型或把无订阅者当写失败必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B018, B034；历史提交有部分执行证明。完整场景：待验收。

### T40-C02 远端能力拒绝

- 场景：远端仅提供 CopyOnly 的 client 尝试构造 live FUSE 或独立文件系统写能力。
- 必须断言：按公开能力边界拒绝或编译失败；正常 Copy 的快照/CAS 写回仍可用。
- 反向控制：为了复用 mount 偷偷补原始全权限 backend 不得通过。
- 层级与基质：L1；independent consumer, declared minimal/default features。SDK 本身与 API；不含服务端或物理 Provider。
- 绑定及状态：B020；当前提交有部分执行证明。完整场景：待验收。

### T40-C03 分布式实时模式

- 场景：仅当部署宣称支持跨进程 live FUSE，验证通知丢失、乱序、重复、断连和缓存进程重启。
- 必须断言：按明确版本/期限/重建契约收敛且无越权读取；没有该能力时记录不适用并证明拒绝，不报告通过。
- 反向控制：本地 broadcast 两个订阅者不能证明跨进程一致性。
- 层级与基质：L3；real Awaken service/process boundary；actual client/transport；declared backend and fixture。模型可用确定性测试 Provider；授权、路由、事务、传输不可全部 mock。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T41 可诊断性与恢复操作

责任组件：各现有 readiness、健康、错误和恢复入口。入口：[源码或现有门禁](../../crates/control/awaken-control/src/registration_supervisor.rs)。风险基准：P1。类型：行为域。

校准：新增。开发者体验不仅是命名，还需要知道操作是否提交、谁负责恢复及哪些能力未就绪。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T41-C01 稳定诊断与无敏感信息

- 场景：注入缺少能力、数据库故障、未确认效果和投影退化，查询错误/health/log。
- 必须断言：包含稳定 code、原 operation/资源身份及可执行恢复方向；secret、token、credential URL 不出现。
- 反向控制：依赖英文文案或打印凭据以求可诊断的实现必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：`degraded_registration_health_is_observable_secret_free_and_leaks_are_detected`（E13 本机 Windows 执行通过，覆盖已成立的 oracle 与反向控制）；产品缺口复现 `degraded_registration_diagnostics_never_contain_the_secret_canary`、`degraded_registration_diagnostics_carry_stable_code_identity_and_recovery`（忽略，不计证据）：健康快照不含机密，但 supervisor 日志直接输出原始错误（含凭据 URL），且没有稳定 code、领域身份或恢复方向。完整场景：待验收。

### T41-C02 服务与投影就绪区分

- 场景：投影故障但管理服务仍可用，随后恢复并重建。
- 必须断言：readiness/health 与真实能力一致；不把 serving 绿色解释为全部投影就绪；恢复后退化状态消除。
- 反向控制：将所有服务状态合成一个 bool 会丢失本用例要求。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B071；当前提交有部分执行证明。完整场景：待验收。

### T41-C03 恢复扫描完整可操作

- 场景：恢复记录超过一页，第二页错误/quarantine/cursor 不进展；修复后原入口重试。
- 必须断言：不根据不完整扫描授权 purge/retirement；诊断保留失败域和原身份；修复后可接续而非人工改库。
- 反向控制：只测第一页或每次用新身份重跑无法签收。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：B015, B098；当前提交有部分执行证明。完整场景：待验收。
