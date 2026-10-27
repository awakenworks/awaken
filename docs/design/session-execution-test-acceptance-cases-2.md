# Session 执行与资源测试验收清单：具体用例（T14–T27）

本文件是[验收清单总览](session-execution-test-acceptance.md)的具体用例部分，因仓库文件长度限制按编号拆分；编号、判定规则和语义与总览一致。

## T14 资源替换与引用修复

责任组件：Session aggregate 与 resource reconciliation。入口：[源码或现有门禁](../../crates/server/awaken-session-application/src/resource_references/tests.rs)。风险基准：P0。类型：行为域。

校准：保留。保留确切 manifest 身份和原子 root；索引是可修复投影。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T14-C01 部分准备拒绝

- 场景：新旧 manifest 交叉，第二个资源准备失败或版本不匹配。
- 必须断言：无部分 root；原 manifest 仍有效；准备责任保留可恢复身份。
- 反向控制：只比较返回错误不检查原 root 和准备责任不能通过。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：`failed_second_input_or_stale_version_keeps_the_original_manifest`、`error_only_oracle_cannot_accept_partial_or_forgotten_preparation`；当前提交本机 Windows L1 执行（E13），每条 oracle 与反向控制已具名绑定。完整场景：待验收（公共门禁 T35 未闭合）。

### T14-C02 并发替换

- 场景：两客户端替换资源，赢家提交后令输家索引更新/回收失败。
- 必须断言：只有 root 权威决定当前引用；索引可修复；输家不能回收赢家资源。
- 反向控制：把引用索引作为独立真相会使负控失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B067；当前提交有部分执行证明。完整场景：待验收。

### T14-C03 替换重放与崩溃

- 场景：root 提交后、引用投影完成前终止子进程，再重放替换。
- 必须断言：恢复原版本和 intent；不重新准备不相同资源；不释放仍受保护的物理责任。
- 反向控制：用 current head 替代原 pinned version 必须失败。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T15 精确凭据与明文边界

责任组件：Credential application 与 materializer。入口：[源码或现有门禁](../../crates/server/awaken-credential-materializer/src/platform_egress.rs)。风险基准：P0。类型：行为域。

校准：细化。保留身份精确性，同时显式区分宿主内凭据、工作负载凭据和外部中介契约。

相关套件证明：E03，只支持原证明声明的范围，不自动适用于下列全部场景。

### T15-C01 绑定矩阵

- 场景：逐一更换 credential revision、holder、target、scope 或 runtime incarnation。
- 必须断言：在获取明文和外部效果前拒绝；错误分类稳定；拒绝次数与物理效果可观测。
- 反向控制：只换 credential id 而不覆盖 revision/holder 不足。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：`binding_matrix_rejects_each_dimension_before_plaintext_with_stable_class`（E13 本机 Windows 执行通过，覆盖 oracle 与反向控制）、`binding_matrix_holder_dimension_has_its_recipient_classification`（E40 本机 Windows 执行通过：公开 resolve 把 holder 不符归为 RecipientMismatch，零明文读取）；MCP attachment 另按 revision/recipient/其余拒绝分别分类。当前提交 oracle 与反向控制均已具名绑定。完整场景：待验收（公共门禁 T35 未闭合）。

### T15-C02 等待后撤销

- 场景：凭据 resolve/refresh/provision 挂在 barrier，期间撤销授权或更换 owner。
- 必须断言：按当前权威拒绝后续效果；原不确定外部请求有观察/过期责任。
- 反向控制：重放缓存的成功材料绕过撤销必须失败。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：E42 在本机 Linux 执行 Provider/A2A 的四个具名等待用例，覆盖调用前撤销、读取期间换主/撤销/不可用、原读取的完成观察，以及回执已提交但确认延迟时不释放材料；原忽略复现已启用。E16/E17 保留原始 Windows 历史。当前为 `current_subset`：未把模型调用、所有 refresh/provision 传输或公共门禁计作完成；完整场景仍待验收。

### T15-C03 明文边界

- 场景：选择 Platform、Worker、Workload holder，分别缺少和安装允许的适配器。
- 必须断言：持久化、日志及响应无明文；Platform 路径不在 Host 解析上游凭据；缺失适配器拒绝。
- 反向控制：用仅断言返回 URL 的 Stub 不能证明真实中介强制撤销。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B046；历史提交有部分执行证明。完整场景：待验收。

## T16 MCP generation 生命周期

责任组件：McpSessions 与 generation root authority。入口：[源码或现有门禁](../../crates/server/awaken-session-mcp/src/mcp/attachment.rs)。风险基准：P0。类型：行为域。

校准：细化。现有 WorkerRelay 保护不能泛化为所有 Native、PlatformRelay、stdio 分支已等价。

相关套件证明：E03，只支持原证明声明的范围，不自动适用于下列全部场景。

### T16-C01 统一准备窗口

- 场景：对无凭据 HTTP、WorkerRelay、PlatformRelay、sandbox-stdio 和 ACP 适用分支，在外部准备/连接等待期间 drain 或替换 root。
- 必须断言：I/O 前有确切 staging 责任；旧任务不能装入新 allocation；等待后副作用受当前 fence 控制。
- 反向控制：删掉预 I/O staging reservation 或只检查最后插入时 root，应使测试失败。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：`t16_c01_native_http_preparation_is_staged_before_io_and_fenced_after_the_wait`、`t16_c01_io_before_staging_reservation_is_detected`、`t16_c01_platform_relay_provision_is_staged_before_io_and_never_published_late`、`t16_c01_native_worker_held_resolution_is_fenced_before_connect`、`t16_c01_acp_worker_relay_resolution_is_fenced_before_relay_start`（E16 本机 Windows 执行通过，覆盖已成立部分）；产品缺口复现 `t16_c01_platform_relay_route_minted_in_the_window_keeps_a_cleanup_owner`（忽略，不计证据；E17 为预期失败）：窗口内铸造的 PlatformRelay 路由没有清理所有者（无 revoke、过期被丢弃）。完整场景：待验收。

| 维度 | 必须覆盖值 | 适用规则 |
| --- | --- | --- |
| 分支 | Native HTTP 无凭据；Native Worker-held；ACP WorkerRelay；PlatformRelay；sandbox-stdio | 只实例化合法且声明支持的组合；未支持模式验证提前拒绝 |
| 并发事件 | close admission；root 相同逻辑 ID 换 allocation；lease 到期 | 每次事件由 barrier 确认发生，不能靠 sleep 猜测 |
| 窗口 | 准备前；准备等待中；效果后回执前 | F01/F07 要求 effect gate 拒绝；F05 要求原责任观察，不谎称零效果 |
| 观察 | operation/generation/allocation、route/process、凭据解析次数、cleanup owner | 错误返回和最终投影为空不足以证明外部责任已清理 |

### T16-C02 热替换与失败清理

- 场景：旧 generation 忙调用，新 generation 发布；drain、route/remove/reap 分别失败并重试。
- 必须断言：不混用授权、凭据和 manager；忙调用结束前无 Removed 证明；失败 owner 可再次选中。
- 反向控制：超时即删 owner 或用新 generation 清理旧句柄必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B042, B038, B039；历史提交有部分执行证明。完整场景：待验收。

### T16-C03 取消与重建

- 场景：取消 staging/drain 等待者或关闭服务，再恢复同一责任。
- 必须断言：已启动效果有监督者或可恢复意图；未完成清理不丢 owner；不存在强引用关闭环。
- 反向控制：join waiter 被 drop 就丢任务责任不得通过。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：B099, B072；历史提交有部分执行证明。完整场景：待验收。

## T17 prepare 与补偿责任

责任组件：Repository、MCP 与 Session creation 各既有事务所有者。入口：[源码或现有门禁](../../crates/server/awaken-session-application/src/tests/creation.rs)。风险基准：P0。类型：行为域。

校准：细化。不能依赖尚未创建的 Session root 作为准备资源的唯一清理入口。

相关套件证明：E03，只支持原证明声明的范围，不自动适用于下列全部场景。

### T17-C01 准备前后崩溃

- 场景：在原 intent 持久化后、外部准备前后分别终止子进程。
- 必须断言：无授权前效果；已开始准备可凭稳定 intent 找回；可发现性不依赖 Session root 存在。
- 反向控制：只在局部变量保存资源身份的实现必须失败。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：`crash_before_or_after_prepare_keeps_identity_for_the_original_intent`（E16 本机 Windows 执行通过，覆盖已成立部分）；产品缺口复现 `crash_after_prepare_is_recoverable_without_the_original_caller`（忽略，不计证据；E17 为预期失败）：没有 root 之前的持久 intent，崩溃后只有原调用者能找回已开始的准备。完整场景：待验收。

### T17-C02 补偿失败

- 场景：准备成功但 root 插入失败，清理接口失败/挂起/响应丢失。
- 必须断言：持久责任保持 pending/uncertain；幂等重试原补偿；不报告完成或泄露材料。
- 反向控制：best-effort cleanup 后丢弃错误不能签收。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：`compensation_failure_hang_or_lost_response_never_reports_completion`、`failing_compensation_reports_incompleteness_to_its_caller`（E16 本机 Windows 执行通过，覆盖已成立部分）；产品缺口复现 `failed_compensation_is_retried_by_a_rebuilt_service`（忽略，不计证据；E17 为预期失败）：补偿失败只记日志，不留下持久的待处理责任。完整场景：待验收。

### T17-C03 并发重试

- 场景：同一 prepare identity 被原调用和恢复器同时继续。
- 必须断言：不创建第二个外部对象；只有原确切对象可发布/清理；冲突重试拒绝。
- 反向控制：仅验证同一对象上的第二次函数调用不足。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：`concurrent_continuations_share_one_external_object`（E16 本机 Windows 执行通过，覆盖已成立部分）；产品缺口复现 `compensation_cannot_retire_a_participant_adopted_concurrently`（忽略，不计证据；E17 为预期失败）：补偿先检查后退役，缺少针对并发 adopt 的 fence。完整场景：待验收。

## T18 archive 与 delete 竞争

责任组件：Session lifecycle aggregate 与 resource retention。入口：[源码或现有门禁](../../crates/server/awaken-session-application/src/tests/realization/worker_retirement.rs)。风险基准：P0。类型：行为域。

校准：保留。终态与物理静止分开，retirement 资格观察不得消费 cleanup 责任。

相关套件证明：E01, E08，只支持原证明声明的范围，不自动适用于下列全部场景。

### T18-C01 归档竞争

- 场景：archive/delete 与新 Run 接纳、资源替换及恢复器在 barrier 竞争。
- 必须断言：终态不能复活；只有合法 CAS 顺序胜出；仍受保护引用不被回收。
- 反向控制：仅顺序调用 archive 再 run 不足以证明竞争。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：`sqlite_archive_races_admission_replacement_and_recovery_through_root_cas`、`postgres_archive_races_admission_replacement_and_recovery_through_root_cas`、`sequential_archive_then_admission_has_no_race_witness`；当前提交本机 Windows 执行（E16），每条 oracle 与反向控制已具名绑定。完整场景：待验收（公共门禁 T35 未闭合）。

### T18-C02 活跃来源保留

- 场景：来源仍 resident，Run 为 idle/running/rescheduling，读取 retirement 资格。
- 必须断言：保留原 Session/root/lease/Run/pending 事实；不通过破坏性 ack 制造可退出状态。
- 反向控制：为了取得资格先清理或置终态应失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B006；当前提交有部分执行证明。完整场景：待验收。

### T18-C03 全部来源独立证明

- 场景：同时存在当前及保留来源、已准备与未准备 Memory 或未知结算。
- 必须断言：每个确切来源单独满足规则；失败不得以成功前缀替代全集；未知保留。
- 反向控制：只检查最后来源或仅统计来源总数必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B100；当前提交有部分执行证明。完整场景：待验收。

## T19 checkpoint 与续接

责任组件：Session continuation aggregate 与现有 provider。入口：[源码或现有门禁](../../crates/server/awaken-session-application/src/tests/continuation.rs)。风险基准：P0。类型：行为域。

校准：细化。每种 Provider 声明支持的恢复能力；无能力时提前拒绝，不伪造通用恢复保证。

相关套件证明：E01, E03，只支持原证明声明的范围，不自动适用于下列全部场景。

### T19-C01 源端数据责任

- 场景：checkpoint prepare/上传/确认/源销毁分别失败、丢响应和崩溃。
- 必须断言：数据和清理责任建立前不销毁源；重试保留原 checkpoint 和 source operation。
- 反向控制：只模拟 effect trait 返回 Err 不证明生产阶段崩溃。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：B058, B022；当前提交有部分执行证明。完整场景：待验收。

### T19-C02 目标恢复重试

- 场景：恢复效果完成但 receipt/root 投影失败，重建并重试。
- 必须断言：观察或接续同一 restore operation；不恢复两次；源和目标身份严格匹配。
- 反向控制：从当前 Provider 任选新目标不能算恢复。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：B069, B066；当前提交有部分执行证明。完整场景：待验收。

### T19-C03 并发接管及保留

- 场景：恢复期间旧 epoch 再提交，checkpoint 到期删除失败。
- 必须断言：旧 owner 拒绝；失效 checkpoint 删除失败保留引用和原责任；真实 backend 状态一致。
- 反向控制：把删除失败当作已清理必须失败。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：B017, B029；当前提交有部分执行证明。完整场景：待验收。

## T20 Environment 定义事务

责任组件：EnvironmentDefinitionStore 与 registration outbox。入口：[源码或现有门禁](../../crates/stores/awaken-env-store/src/lib.rs)。风险基准：P0。类型：行为域。

校准：保留。定义、不可变 revision 和 intent 是同一事务，不能由 mutable current row 重建历史。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T20-C01 完整 revision 与重放

- 场景：create/update/archive、相同 command 重试、改变 fingerprint 和 stale revision，使用所有支持后端。
- 必须断言：定义、不可变版本和正确 Register/Withdraw intent 成对提交；相同重试不增事实；历史不跟随 current row 漂移。
- 反向控制：只检查 current row 存在而不核对 immutable revision/intent 不足。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：B028, B054；当前提交有部分执行证明。完整场景：待验收。

| 操作规则 | 必须出现的事实 | 不得出现 |
| --- | --- | --- |
| 首次 create | 一个 root/revision/Register intent 和 command receipt | 孤立 root 或 intent |
| 同 command 同 fingerprint | 原身份及回执 | 新 revision/healthcheck |
| 同 command 改 fingerprint | typed conflict | 任何新事实 |
| update current revision 且改变内容 | 唯一后继不可变 revision 与 intent | 覆盖旧 immutable revision |
| update no-op | 原事实 | 新增 revision/intent |
| update stale revision | typed conflict | 输家事实 |
| archive | 终态 revision 与 Withdraw intent | 后续更新或旧 register 恢复新接纳 |

### T20-C02 create 末步 SQL 故障

- 场景：生产 create_once 执行到 intent 插入时，SQLite trigger 与 PG trigger 分别拒绝。
- 必须断言：返回存储失败；current/revision/command/intent 全部保持提交前状态。
- 反向控制：改为单独提交 current row 的负控必须失败。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：B033, B054；当前提交有部分执行证明。完整场景：待验收。
- 已核对的现有断言子范围：Windows SQLite/PG，生产 create_once 的末步 trigger 错误；不含 update/archive/kill。完整四表回滚已检查；PostgreSQL parity case 内含同一失败表。

### T20-C03 update/archive 失败与竞争

- 场景：独立客户端并发 update/archive，并在生产各中途写点故障或杀死子进程。
- 必须断言：单一合法 revision 序；终态不可再改；同事务全回滚；重启保留完整历史。
- 反向控制：create 的回滚测试不能直接代替这两个生产写路径。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：`sqlite_file_update_archive_race_faults_and_reopen_keep_one_legal_history`、`postgres_update_archive_race_faults_and_reopen_keep_one_legal_history`、`sqlite_file_update_child_killed_inside_its_transaction_rolls_back_and_retries_once`、`sqlite_file_archive_child_killed_inside_its_transaction_rolls_back_and_retries_once`、`postgres_archive_child_killed_inside_its_transaction_rolls_back_and_retries_once`；当前提交本机 Windows 执行（E16），每条 oracle 与反向控制已具名绑定。完整场景：待验收（公共门禁 T35 未闭合）。

## T21 Environment 注册投递

责任组件：EnvironmentRegistrationDelivery、catalog、Work 投影和监督者。入口：[源码或现有门禁](../../crates/control/awaken-environment-application/tests/registration_replay.rs)。风险基准：P0。类型：行为域。

校准：保留。明确 at-least-once 投递、单调确切身份及可恢复责任，不要求网络恰好一次。

相关套件证明：E01，只支持原证明声明的范围，不自动适用于下列全部场景。

### T21-C01 迟到撤销与重连

- 场景：先 register，再 withdraw，数据库 reopen/reconnect 后让原 register 迟到。
- 必须断言：三后端均不能恢复已撤销 Work；原历史仍可读但不重新授予接纳。
- 反向控制：没有 reconnect 的纯顺序 Stub 不能覆盖 durable 行为。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：B092, B076, B049；当前提交有部分执行证明。完整场景：待验收。
- 已核对的现有断言子范围：Windows Volatile/SQLite reopen/PostgreSQL reconnect。三个 adapter 的迟到 register 在 withdraw 后不能恢复 Work。

### T21-C02 历史与新投影隔离

- 场景：catalog 历史恢复成功但新 queue registration 失败，交叉 pending/all 和丢 ack。
- 必须断言：历史成功不得消费未应用新 intent；失败责任保留；重试使同一新投影收敛。
- 反向控制：只比较 catalog 高水位不比较 Work 效果的实现应失败。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：B093, B077, B050, B096, B083, B055；当前提交有部分执行证明。完整场景：待验收。
- 已核对的现有断言子范围：Windows 三后端，catalog 历史与尚未应用的新 Work 注册。六个实际病例绑定历史/新意图隔离；不等同真实分布式服务故障。

### T21-C03 监督恢复状态

- 场景：初始 All 全量重建失败，再成功，随后 wake；投影异常时读 serving readiness。
- 必须断言：首次完整成功前保持 All；之后 Pending；投影退化与服务 readiness 按既有契约分离。
- 反向控制：第一次部分成功就切 Pending 或无限全历史重放必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B088, B071；当前提交有部分执行证明。完整场景：待验收。
- 已核对的现有断言子范围：Windows 组件，注入 registrar 失败与 supervisor wake。All 至首次完整成功后转 Pending，以及 readiness 与投影退化分离。

### T21-C04 真实服务断连

- 场景：Control 与 Coordinator 分开进程，commit 后断网/丢 ack/重启并归档，原 Session 仍执行。
- 必须断言：旧 Session 保持冻结版本；新接纳服从 archive；恢复原 intent 且不产生第二 healthcheck/Work。
- 反向控制：组件 registrar Stub 不能替代网络及进程边界。
- 层级与基质：L3；real Awaken service/process boundary；actual client/transport；declared backend and fixture。模型可用确定性测试 Provider；授权、路由、事务、传输不可全部 mock。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T22 Memory 内容事务与作者

责任组件：Memory backend 与 Resources 装饰器。入口：[源码或现有门禁](../../crates/resources/awaken-memory-store/tests/common/memory_repository.rs)。风险基准：P0。类型：行为域。

校准：保留。写入、路径覆盖和历史原子；装饰器只在提交后通知，作者不在中间层丢失。

相关套件证明：E02，只支持原证明声明的范围，不自动适用于下列全部场景。

### T22-C01 写入与作者

- 场景：带不同 actor 执行 create/update/rename-overwrite/delete/no-op，经各真实 backend 与装饰器。
- 必须断言：内容、路径、被覆盖目标及 history 原子；created_by/redacted_by 不丢；no-op 不增版本。
- 反向控制：只在 raw backend 测 actor、不经过装饰器不能证明透传。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：B027, B095, B081, B052；历史提交有部分执行证明。完整场景：待验收。

### T22-C02 生产写路径崩溃

- 场景：生产 update_head/rename 在目标删除、内容写入、history 插入及 commit 前后终止子进程或触发 SQL 错误。
- 必须断言：重开只见完整旧/新状态；目标、作者、history 一致；无半覆盖。
- 反向控制：手写测试事务并 drop 不得替代生产命令崩溃。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：B063；历史提交有部分执行证明。完整场景：待验收。

| 生产写点 | 故障 | 重开后的合法状态 |
| --- | --- | --- |
| 覆盖目标删除后 | SQL 故障/取消/子进程死亡 | 完整旧状态 |
| 源内容/路径改变后、history 前 | SQL 故障/取消/子进程死亡 | 完整旧状态 |
| history 后、commit 前 | SQL 故障/子进程死亡 | 完整旧状态 |
| commit 后、响应前 | 终止请求进程/丢响应 | 完整新状态，原操作观察/回执，禁止半覆盖 |

### T22-C03 通知失败与无订阅者

- 场景：成功/失败/no-op 写入分别遇到失效通知无订阅者、lag 和监听器关闭。
- 必须断言：通知不逆转已提交事实；失败写不发布 head 变更；缓存恢复遵循 T40 的一致性契约。
- 反向控制：把通知失败回滚声明为写入未发生必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B001, B003；历史提交有部分执行证明。完整场景：待验收。

## T23 Memory 一致快照

责任组件：MemorySnapshotSource、Recall、Dream 与 Copy。入口：[源码或现有门禁](../../crates/resources/awaken-memory-store/tests/common/memory_repository.rs)。风险基准：P0。类型：行为域。

校准：细化。快照保证按声明 store/事务范围定义，不承诺跨独立 store 或任意多次请求全局快照。

相关套件证明：E02, E03, E06，只支持原证明声明的范围，不自动适用于下列全部场景。

### T23-C01 并发单 store 快照

- 场景：两个文件交替提交且并发 snapshot_heads，独立连接观察历史线性化前缀。
- 必须断言：每次结果对应一个完整已提交前缀；无 list+get 混合版本；隔离 foreign store 且排序稳定。
- 反向控制：用 list 后逐项 get 的负控应出现不合法混合并失败。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：B097, B084, B056；历史提交有部分执行证明。完整场景：待验收。

### T23-C02 消费者只取一次

- 场景：使用仅支持原子 snapshot 的窄适配器运行 Recall、Dream 初始化、Copy materialization。
- 必须断言：各入口使用一个 snapshot 原语；不回落到额外 list/get；授权在合适边界执行。
- 反向控制：让 list/get 故意失败以检出分裂读取。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B060, B019, B059；当前提交有部分执行证明。完整场景：待验收。

### T23-C03 重建期间快照

- 场景：初始化中断、清理失败、重启并重新准备私有 Dream/Copy 输入。
- 必须断言：保留现有受保护输出；成功重建来自一个新完整快照；失败不会发布半快照。
- 反向控制：只在同一内存对象重复初始化不证明持久恢复。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：`interrupted_snapshot_clone_rebuilds_one_complete_snapshot_and_keeps_protected_output`、`crashed_snapshot_clone_rebuilds_from_durable_state_in_a_new_application`（E16 本机 Windows 执行通过，覆盖已成立部分）；产品缺口复现 `interrupted_snapshot_clone_never_exposes_a_half_snapshot`、`failed_snapshot_cleanup_is_reported_or_leaves_no_private_content`（忽略，不计证据；E17 为预期失败）：Dream prepare 在克隆完成前就把快照条目登记为 Active；release_result 吞掉清理错误。完整场景：待验收。

## T24 Copy 写回与 ABA

责任组件：MemoryCopyWriteback、Copy harvest 与 FUSE 文件身份。入口：[源码或现有门禁](../../crates/worker/awaken-sandbox-memoryd/src/copy.rs)。风险基准：P0。类型：行为域。

校准：细化。Copy 按原路径和完整 generation 比较；FUSE 打开句柄跟随 ID，二者不能混为一条规则。

相关套件证明：E02, E06，只支持原证明声明的范围，不自动适用于下列全部场景。

### T24-C01 完整条件与同内容 ABA

- 场景：独立改变 source path/id/version/hash，或删除重建同内容后写回旧 Copy。
- 必须断言：全部条件在同事务检查；旧 Copy 不覆盖或删除新 head；路径错误先于幂等处理。
- 反向控制：只比较 bytes/hash 的 CAS 负控必须失败。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：B094, B080, B014；历史提交有部分执行证明。完整场景：待验收。

| 原 Copy 条件漂移 | 字节是否相同 | 预期 |
| --- | --- | --- |
| path 改变 | 相同或不同 | 旧 Copy 条件写拒绝；不覆盖新位置 |
| id 删除后重建 | 相同 | 旧 Copy 拒绝；新 head 保留 |
| version 改变 | 相同 | 旧 generation 拒绝 |
| hash 改变 | 不同 | 拒绝或按既有 CAS 冲突规则保存 proposal |
| 条件全部匹配 | 改变/不变 | 一次更新/合法 no-op，history 不重复 |

### T24-C02 冲突保存失败

- 场景：CAS 冲突时令 conflict preservation 失败、丢响应，随后重试 teardown/recovery。
- 必须断言：未成功持久保存冲突前保留原文件和数据责任；成功重试保存一个原身份 proposal。
- 反向控制：先删除 projection 再保存冲突或用新 proposal id 重试必须失败。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：`lost_conflict_preservation_response_retries_with_the_original_proposal_identity`、`nc_projection_removed_before_conflict_preservation_is_detected`；当前提交本机 Windows 执行（E16），每条 oracle 与反向控制已具名绑定。完整场景：待验收（公共门禁 T35 未闭合）。

### T24-C03 FUSE fd 及 rename

- 场景：Linux 真挂载下打开 fd，rename/覆盖目标/删除重建并继续读写原 fd。
- 必须断言：原 fd 跟随原 ID；目标身份不被复用；不把 Copy 的原路径 CAS 语义套给 fd。
- 反向控制：Windows copy 路径或仅内存 FS 对象不能算内核挂载证明。
- 层级与基质：L4；native Linux kernel/FUSE or applicable namespace/container substrate。本次 Windows 任务不执行；只可未验收或有能力拒绝证明的不适用。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

## T25 Memory 历史脱敏

责任组件：MemoryVersionRedaction 与 Managed 授权入口。入口：[源码或现有门禁](../../crates/server/awaken-protocol-managed/tests/memory_store_api_http.rs)。风险基准：P0。类型：行为域。

校准：保留。历史脱敏、普通删除和物理 purge 是三个不同授权操作。

相关套件证明：E02，只支持原证明声明的范围，不自动适用于下列全部场景。

### T25-C01 归属与权限

- 场景：跨 store 使用 version id、只授予内容写权限或错误租户请求 redact。
- 必须断言：无法读取/脱敏外部版本；错误不泄露外部资源存在性；拒绝零变更。
- 反向控制：仅由 version id 查找不限制所属 store 必须失败。
- 层级与基质：L3；real Awaken service/process boundary；actual client/transport；declared backend and fixture。模型可用确定性测试 Provider；授权、路由、事务、传输不可全部 mock。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。

### T25-C02 幂等脱敏与作者

- 场景：对当前和历史版本重复 redact，再重新打开 backend。
- 必须断言：保留 created_by；redacted_by/时间和内容清除规则稳定；普通 delete 保留历史。
- 反向控制：把删除 head 当作删除/脱敏历史必须失败。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：B095, B081, B052；历史提交有部分执行证明。完整场景：待验收。

### T25-C03 中途故障

- 场景：脱敏写入中途故障、commit 后丢响应，再重试原操作。
- 必须断言：不暴露部分脱敏；成功重放不恢复被清除内容、不改写原脱敏身份。
- 反向控制：只重复成功请求而不测试故障窗口不足。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：`sqlite_file_redaction_survives_trigger_failure_and_lost_response`、`postgres_redaction_survives_trigger_failure_and_lost_response`；当前提交本机 Windows 执行（E16），每条 oracle 与反向控制已具名绑定。完整场景：待验收（公共门禁 T35 未闭合）。

## T26 内容回收与保护

责任组件：Resource reclamation application 与 physical cleanup。入口：[源码或现有门禁](../../crates/server/awaken-resource-reclaimer/src/lib.rs)。风险基准：P0。类型：行为域。

校准：保留。删除内容权限不包含物理 purge；检查保护事实的读取失败必须保守拒绝。

相关套件证明：E02，只支持原证明声明的范围，不自动适用于下列全部场景。

### T26-C01 保护条件表

- 场景：交叉 active Session、引用、extraction、retention 和查询失败。
- 必须断言：任一保护事实或未知阻止 purge；所有允许条件满足才授予确切清理权限。
- 反向控制：某个依赖读取失败被当作零引用必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：B045, B032；历史提交有部分执行证明。完整场景：待验收。

### T26-C02 清理失败与租约接管

- 场景：物理清理部分成功后失败，旧 claim 到期由新 owner 接管。
- 必须断言：保留可恢复责任；旧 owner 不提交；重试 outcome 稳定、不会误删受保护对象。
- 反向控制：只以空返回值视作成功不能签收。
- 层级与基质：L2；file SQLite；live PostgreSQL；independent clients/processes where required。需要持久性时真实文件和重开；仅 in-memory SQLite 不足。
- 绑定及状态：B030, B004；历史提交有部分执行证明。完整场景：待验收。

### T26-C03 实际全内容清理

- 场景：真实 backend 含 head/history/conflict proposal，允许 purge 后重开。
- 必须断言：范围内数据全部清理且其他 store 不变；重复清理幂等；普通 writer 没有 purge 能力。
- 反向控制：只删除 head 并留敏感 history 或冲突数据必须失败。
- 层级与基质：L1/L2；Volatile logical model；file SQLite for persistence clauses；live PostgreSQL for supported SQL clauses。只覆盖该操作真实支持的 backend；不凭空要求 Volatile 持久性。
- 绑定及状态：`volatile_purge_removes_exactly_one_store_and_heads_only_purge_is_detected`、`sqlite_file_purge_removes_exactly_one_store_across_handles_and_reopen`、`postgres_purge_removes_exactly_one_store_across_pools_and_reopen`、`memory_writer`；当前提交本机 Windows 执行（E16、E18），每条 oracle 与反向控制已具名绑定。完整场景：待验收（公共门禁 T35 未闭合）。

## T27 Sandbox 能力与同一物理实例

责任组件：SessionEnvironmentProvider 与 Sandbox 能力构造。入口：[源码或现有门禁](../../crates/contract/awaken-provisioning-contract/src/sandbox/runtime.rs)。风险基准：P0。类型：行为域。

校准：细化。按消费者角色约束能力，保留同一物理实例；不要按方法机械拆 trait。

相关套件证明：E03, E09，只支持原证明声明的范围，不自动适用于下列全部场景。

### T27-C01 缺少恢复能力

- 场景：启用 checkpoint/restore 但选中不支持的 Provider。
- 必须断言：在创建外部环境或提交不可恢复生命周期前拒绝；普通执行模式仍可用。
- 反向控制：运行到半途才从 Option 得知缺能力不能作为构造完成。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：`restore_capability_pairing_and_ordinary_mode_hold_and_late_option_discovery_is_detected`（E13 本机 Windows 执行通过，覆盖普通模式与反向控制）、`restore_enabled_worker_with_unsupported_restoration_is_refused_at_construction`（E40 本机 Windows 执行通过：构造阶段检查 restore 能力，零外部创建）。当前提交 oracle 与反向控制均已具名绑定。完整场景：待验收（公共门禁 T35 未闭合）。

### T27-C02 混入其他实例

- 场景：组合 A 的执行句柄、B 的停止/文件/restore 能力，并使用相同逻辑 id。
- 必须断言：构造或 exact identity 校验拒绝；能力来自同一受控物理来源而非仅同字符串。
- 反向控制：只比较逻辑 id 的负控必须失败。
- 层级与基质：L1；owned clocks/barriers, component public entrypoint。组件语义；若用真实 adapter，其属性另按 L2 签收；不代表进程崩溃。
- 绑定及状态：`foreign_instance_with_differing_identity_is_refused_and_logical_id_check_is_insufficient`（E13 本机 Windows 执行通过，覆盖反向控制）、`foreign_instance_with_equal_handle_cannot_rebind_another_providers_environment`（E40 本机 Windows 执行通过：Environment 记录实现它的 Provider 实例，相同 handle 的另一实例被 Conflict 拒绝）。当前提交 oracle 与反向控制均已具名绑定。完整场景：待验收（公共门禁 T35 未闭合）。

### T27-C03 create/adopt 证据

- 场景：create/adopt 丢响应、旧实例仍在，再恢复原准备身份。
- 必须断言：观察同一候选物理实例；成功 adoption 回执与 root 一致；不因重试创建第二环境。
- 反向控制：用新 wrapper/新容器替代原实例却宣称恢复必须失败。
- 层级与基质：L3；actual selected native executor/provider；barrier-controlled fault or child death；effect/cleanup observer。实际启用模式；内核、引擎或物理恢复额外需要 L4。
- 绑定及状态：待绑定现有用例或新增实现；待绑定或新增，不能据此断言仓库完全无测试。完整场景：待验收。
