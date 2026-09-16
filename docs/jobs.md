# Job Tracker（按 development-plan.md §3 + M0 spike 拆分）

状态：`pending` → `implementing` → `reviewing` → `passed/committed` 或 `fixing`（codex review 未通过，回炉）

| # | Job | 状态 | 说明 / codex review 结论 |
|---|-----|------|--------------------------|
| 1 | Workspace/CI/tooling | passed/committed | 分支 job/1-workspace-ci-tooling @ 19ec93a（main 受环境 hook 保护，合入待人工）。codex review 两轮通过（P2: 端口配置严格校验、tracing subscriber、workspace 元数据继承、代理端口一致）。注意：本机 npm 走 npmmirror（user-level），lockfile 已改回 npmjs |
| S1 | M0 风险 spike + fixtures + ADR | passed/committed | ✅ Spike A (SSE 转发/中断取消, ADR-0004, codex 两轮通过)；✅ Spike B 子进程 (subprocess.rs: stdout+stderr 双路 drain / TERM→KILL 升级+双路 EOF / unix 组杀 / in-terminate in-loop pump, 4 常规 + 1 unix 测试)；✅ Spike C 端口竞态 (port_race.rs: A 预分配 100/100 失败 vs B child 自选 100/100 成功)；✅ ADR-0002(accepted)；ADR-0001/0003 为 proposed——真实 runtime ready-line 契约与 b5555 基线待 WSL 现场 fixture（本 WSL 无 llama.cpp/NInfer/cargo，M2 llama adapter 开工门槛）；fixtures/upstream ✅ 字节级核对，fixtures/runtimes 为占位（PENDING WSL 现场采集）。codex review 通过（分支 job/s1-m0-spikes, 5bba80a→bbfeaf3） |
| 2 | Domain types + error catalog | passed/committed | 分支 job/2-domain。crates/domain: Model/Runtime/Instance/Operation/LoadConfig(全部公开数据字段+with_state 转换校验)/Capabilities/NInferConfig/DevicePolicy/ErrorCode(24 码, 5 元组 meta)/DomainError+映射(OpenAI+RFC7807, problem_type 类别 URN 与 code_str 分离)。状态机: Instance(8态, draining→crashed, 所有非终态可达 crashed 供 §8 恢复) + Operation(5态)。44 测试, 双平台 clippy 0 警告。codex 三轮: P1(公开字段)/P2(状态机语义+problem_type URN)/P3(device_ids+fixed_args+result) 全修, 最终无发现 |
| 3 | SQLite migrations/repositories | fixing | WAL、事务、测试 fixture。codex R1 报 5×P1+3×P2（SQL注入插值、repo层无状态机校验、DB文件0600、损坏pid/port静默None、非状态CHECK错误映射错；3条vacuous测试）。回炉中 |
| 4 | Secure model scanner | pending | `.gguf`/`.ninfer` 扫描、key 冲突、mtime 增量、删除标记、路径逃逸防护 |
| 5 | Runtime probe and command spec | pending | executable probe + 每 runtime 的 argv 命令规范 |
| 6 | Process supervisor | pending | process group、stdout/stderr ring、health deadline、exit classification |
| 7 | Operation/state machine | pending | load/unload operation、状态表、非法转换测试 |
| 8 | llama.cpp adapter + real smoke test | pending | RuntimeAdapter 实现 + 真实/fixture 连续 load/inference/unload |
| 9 | Management APIs + event stream | pending | models/instances/operations/events 原生 API + SSE |
| 10 | Gateway JSON proxy | pending | /v1/models、chat、completions 同步转发 + 路由 + lease/backpressure/限制 |
| 11 | Gateway SSE/cancellation | pending | 流式透明转发、客户端取消、断流无泄漏、错误映射 |
| 12 | Auth/limits/redaction | pending | management token、inference API key、rate/body/header limits、redaction |
| 13 | NInfer adapter + real smoke test | pending | .ninfer 校验、参数映射、health/models 校验、crash/timeout/OOM fixture |
| 14 | GPU snapshot + eviction scheduler | pending | nvidia-smi parser、global lock/lease/LRU、三种策略、OOM 重试 |
| 15 | LM Studio contract layer | pending | /api/v1/models list/load/unload + 422/404 + contract tests |
| 16 | React shell/auth/dashboard | pending | 登录/token bootstrap + daemon/GPU/runtime/instance 状态面板 |
| 17 | Models/load form/instances/logs | pending | 搜索/扫描/详情/load form、instances、日志 tail、SSE 重连 |
| 18 | Packaging/systemd/docs/hardening | pending | systemd unit、示例配置、故障注入、安全/性能/SBOM、WSL 部署文档 |