# Job Tracker（按 development-plan.md §3 + M0 spike 拆分）

状态：`pending` → `implementing` → `reviewing` → `passed/committed` 或 `fixing`（codex review 未通过，回炉）

| # | Job | 状态 | 说明 / codex review 结论 |
|---|-----|------|--------------------------|
| 1 | Workspace/CI/tooling | passed/committed | 分支 job/1-workspace-ci-tooling @ 19ec93a（main 受环境 hook 保护，合入待人工）。codex review 两轮通过（P2: 端口配置严格校验、tracing subscriber、workspace 元数据继承、代理端口一致）。注意：本机 npm 走 npmmirror（user-level），lockfile 已改回 npmjs |
| S1 | M0 风险 spike + fixtures + ADR | passed/committed | ✅ Spike A (SSE 转发/中断取消, ADR-0004, codex 两轮通过)；✅ Spike B 子进程 (subprocess.rs: stdout+stderr 双路 drain / TERM→KILL 升级+双路 EOF / unix 组杀 / in-terminate in-loop pump, 4 常规 + 1 unix 测试)；✅ Spike C 端口竞态 (port_race.rs: A 预分配 100/100 失败 vs B child 自选 100/100 成功)；✅ ADR-0002(accepted)；ADR-0001/0003 为 proposed——真实 runtime ready-line 契约与 b5555 基线待 WSL 现场 fixture（本 WSL 无 llama.cpp/NInfer/cargo，M2 llama adapter 开工门槛）；fixtures/upstream ✅ 字节级核对，fixtures/runtimes 为占位（PENDING WSL 现场采集）。codex review 通过（分支 job/s1-m0-spikes, 5bba80a→bbfeaf3） |
| 2 | Domain types + error catalog | passed/committed | 分支 job/2-domain。crates/domain: Model/Runtime/Instance/Operation/LoadConfig(全部公开数据字段+with_state 转换校验)/Capabilities/NInferConfig/DevicePolicy/ErrorCode(24 码, 5 元组 meta)/DomainError+映射(OpenAI+RFC7807, problem_type 类别 URN 与 code_str 分离)。状态机: Instance(8态, draining→crashed, 所有非终态可达 crashed 供 §8 恢复) + Operation(5态)。44 测试, 双平台 clippy 0 警告。codex 三轮: P1(公开字段)/P2(状态机语义+problem_type URN)/P3(device_ids+fixed_args+result) 全修, 最终无发现 |
| 3 | SQLite migrations/repositories | passed/committed | 分支 job/3-persistence @ f6c1f20。crates/persistence: 0001 迁移（instances/operations/model_roots/audit_events，CHECK fence + 部分索引）、存储层（WAL、foreign_keys、busy_timeout 10s、secure_db_file 0600、重启恢复 recover_non_terminal_instances）、repos 全部状态写经冻结域状态机 validate_state + (state, revision) 单调 CAS 守卫（revision = revision + 1 于 UPDATE 内原子递增，require_state_write 将 0 行映射 InvalidStateTransition），同态写为幂等数据更新；审计事件与写同事务。codex 六轮：R1 5×P1+3×P2 → R2 8 项全修确认 → R3 同态 ABA P1（updated_at 非单调）→ R4 单调 revision 计数器（7a3cfa1）+ WSL 新 clippy（9209113）→ R5 报 truncate 竞态 P1 → R6 create_new(O_EXCL) 原子创建（f6c1f20）APPROVE。双平台 fmt/clippy -D warnings/全量 test 绿（含 ABA 回归测试 stale_same_state_write_is_rejected_by_the_revision_guard + 正向对照） |
| 4 | Secure model scanner | passed/committed | crates/scanner: 递归扫描 `.gguf`/`.ninfer`（仅常规文件，拒绝符号链接/Windows reparse point，canonical 包含性校验，遍历错误与深度/目录预算均 fail-closed）；确定性 key/ID（碰撞按规范化路径字典序分配且不抢占已有 key）；mtime+size 毫秒精度增量扫描；单事务对账、缺失软删与重现恢复、root CAS 和匹配行 preflight，失败整体回滚；保留 admin 字段。scanner 测试覆盖 restart、碰撞顺序、增量更新、删除/重现、路径/软链逃逸、跨 root key、NULL root、mtime 精度、不完整遍历与并发 stale snapshot。codex 四轮 review：R1 修复不完整 I/O 误删和跨 root deleted-key 劫持；R2 修复 NULL root、mtime 精度与 admin key 分配；R3 修复遍历预算误删和 stale snapshot；R4 APPROVE。Windows 与 Linux 双平台 fmt/clippy -D warnings/test 全绿（Windows 155 passed，Linux 160 passed，1 个预存 ignored） |
| 5 | Runtime probe and command spec | passed/committed | crates/runtime: 绝对路径 executable probe（version/help 双阶段、全程 deadline、stdout/stderr 有界并行采集、超时/溢出 kill+reap、阶段化错误）与 adapter-aware doctor；llama.cpp/NInfer adapter 提供 runtime/artifact/字段校验、最低版本/capability fixture 解析、loopback-only 确定性 argv，拒绝 fixed_args 覆盖 adapter 管理参数（含 llama 短别名）；schema v1 的 version/help/health/models synthetic-pinned fixtures 带真实 provenance。精确 golden argv、缺失/非 executable/超时/后代持管道/旧版本/错 binary/不健康与 fixture 契约测试齐全。codex 三轮 review：R1 修复 probe 无界 pipe join、fixed_args 安全覆盖、adapter-aware doctor、NInfer capability 与 health/models 版本化；R2 修复 llama 短别名绕过和相对 executable PATH 解析；R3 APPROVE。Windows + WSL fmt/clippy -D warnings/workspace tests 全绿（仅预存 port-race 压测 ignored） |
| 6 | Process supervisor | passed/committed | 分支 job/6-process-supervisor。crates/runtime: `supervisor` ByteTailRing 有界尾环（stdout/stderr 保留容量 + 含淘汰字节的 total_bytes 精确计数）；`managed_process` ManagedProcess（no-shell spawn + kill_on_drop + unix 进程组 pgid=pid、双管道后台 pump 入环、try_wait/wait、wait_ready 就绪 deadline——超时自动 ADR-0002 shutdown（TERM→宽限→KILL 组升级+双管道 EOF 结算）、子进程提前退出与 leader 退出后的已验证后代均结算，防失败启动孤儿进程、probe_health 单探针 deadline、cancel/startup_timed_out 报告、exit classification 及 domain FailureClass 映射）；Windows 明确为单进程 TerminateProcess 语义。fixture 覆盖双路 >64KiB、ring 上限、clean/crash/OOM、readiness/health timeout、cancel、TERM 升级、leader 退出后代、Drop 收尸与双路 EOF。codex 三轮 review：R1 修复 leader 退出后代泄漏与 Drop PGID 授权；R2 修复 ready/exit 竞态、pipe join 重试及零间隔忙循环；R3 补真实 health deadline、domain 映射与 Drop 回归后 APPROVE。Windows + WSL fmt/clippy -D warnings/workspace tests 全绿（仅预存 port-race 压测 ignored） |
| 7 | Operation/state machine | passed/committed | `crates/ops`：durable load/unload coordinator；`BEGIN IMMEDIATE` 内原子推进 operation + instance + audit；双 revision handle 拒绝 stale/ABA writer；partial unique index 保证每 instance 最多一个 active operation；queued/running 的成功、失败、取消均进入确定终态；restart 原子恢复且幂等。测试覆盖完整 domain transition matrix、事务回滚、stale CAS、concurrent unload 与 start/restart race；独立 review、Windows/WSL 门禁及 CI 均通过，已由 PR #31 合并。 |
| 8 | llama.cpp adapter + real smoke test | reviewing | `crates/runtime-llamacpp`：完成 `LlamaCppLifecycle` 编排器实现，打通 `LlamaCppAdapter`（参数校验、loopback 确定性 argv、`classify_exit`/`classify_stderr` 故障分类、`launch` 便捷方法）与 `ManagedProcess`（无 shell spawn、独立进程组、有界日志 ring、ADR-0002 TERM→grace→KILL 卸载与无孤儿保证）；实现就绪探测（`GET /health`）、模型标识校验（`GET /v1/models` 对齐 `--alias`）、最小生成验证（`POST /v1/chat/completions` `max_tokens: 1`）；各错误路径及取消均主动清理子进程，确保零孤儿。覆盖确定性非法模型（启动前校验拦截与子进程加载失败 `InvalidModel`）、端口冲突（`PortConflict`）、启动超时（`StartupTimeout`）终态分类；声明最低支持版本 `b5555`+（ADR-0003 基线）；通过 `fixtures/runtimes/health-and-models.json` 契约集成测试与 `fake_llama_server` 全周期生命周期测试；实现 opt-in 真实冒烟测试（`smoke_test_real_runtime_20_cycles`，ignored）与独立 CLI runner（`llama_smoke_runner`）。诚实说明：当前开发/CI环境未运行真实 runtime 与 GGUF 模型，详细 opt-in 真实冒烟指南及预期断言见下方专节。 |
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

## Job 8: llama.cpp Adapter & 真实冒烟测试说明

### 1. 支持版本 (Supported llama.cpp Version)
- **最低支持版本**: `llama.cpp` build `b5555`+（ADR-0003 基线，代码常量 `MIN_SUPPORTED_BUILD = 5555`）。
- **契约要求**: 必须支持 `--host 127.0.0.1` 仅回环绑定、`--port`、`--alias`、`GET /health` (`{"status":"ok"}`)、`GET /v1/models` 以及 `POST /v1/chat/completions`。

### 2. 真实环境执行状态 (Real Runtime Execution Note)
- **诚实说明**: 在当前本地开发与 CI 环境中，**未执行**真实的 `llama-server` 二进制程序与真实 `.gguf` 权重文件（当前环境未安装真实 runtime 二进制，亦无下载的大模型权重）。
- **验证手段**: 本地与 CI 自动化回归测试由可控测试二进制 `fake_llama_server`（覆盖 healthy、slow_start、crash_early、invalid_model、port_conflict、never_ready 等多种行为模式）以及 synthetic-pinned 契约 fixtures (`fixtures/runtimes/health-and-models.json`) 完整保障；真实运行测试作为 opt-in 机制落地。

### 3. Opt-in 真实冒烟执行指南 (20 Cycles Real Smoke Instructions)

真实冒烟测试要求在连续 20 轮 load → readiness → identity → minimal inference → unload 循环中全部通过，且每轮结束后不留任何孤儿进程。

#### 环境变量 (Environment Variables)
- `LLAMACPP_SMOKE_EXECUTABLE`: 真实 `llama-server` 可执行文件的绝对路径。
- `LLAMACPP_SMOKE_MODEL`: 真实 `.gguf` 模型权重文件的绝对路径。
- `LLAMACPP_SMOKE_CYCLES`: 循环轮次，默认为 `20`。

#### 执行命令 (Commands)

**方式一：Cargo Ignored 集成测试**
```bash
LLAMACPP_SMOKE_EXECUTABLE=/path/to/llama-server \
LLAMACPP_SMOKE_MODEL=/path/to/model.gguf \
cargo test --package model-serving-runtime-llamacpp \
  --test lifecycle smoke_test_real_runtime_20_cycles -- --ignored --nocapture
```

**方式二：独立可执行 Runner (`llama_smoke_runner`)**
```bash
cargo run --package model-serving-runtime-llamacpp --bin llama_smoke_runner -- \
  --executable /path/to/llama-server \
  --model /path/to/model.gguf \
  --cycles 20
```
*(Windows PowerShell 下可使用 `$env:LLAMACPP_SMOKE_EXECUTABLE="C:\path\to\llama-server.exe"; $env:LLAMACPP_SMOKE_MODEL="C:\path\to\model.gguf"; cargo test --package model-serving-runtime-llamacpp --test lifecycle smoke_test_real_runtime_20_cycles -- --ignored --nocapture`)*

#### 预期断言 (Expected Assertions per Cycle)
在全部 20 轮循环中，每一轮均严格执行并断言以下条件：
1. **进程拉起 (Spawn)**: 子进程在独立进程组中创建成功，获得有效 PID（`pid > 0`）。
2. **就绪探测 (Readiness)**: 轮询 loopback `GET /health` 探针，在 `startup_timeout` 内必须返回 HTTP 200 且响应体为 `{"status":"ok"}`。
3. **模型标识校验 (Model Identity)**: 请求 `GET /v1/models`，断言 `data` 数组中包含启动参数 `--alias` 所指定的 `model_key`。
4. **最小推理验证 (Minimal Inference)**: 发送 `POST /v1/chat/completions` 请求（`messages: [{"role": "user", "content": "hi"}]`，`max_tokens: 1`），断言返回 HTTP 200 且响应中包含有效补全 token。
5. **优雅卸载 (Clean Unload)**: 调用 `unload()` 执行 ADR-0002 规范停机（SIGTERM → 宽限期 → SIGKILL，Windows 下为直接终止），并双管道 EOF 结算退出状态。
6. **无孤儿进程断言 (No Orphan Assertion)**: 卸载后等待 150-200ms，调用跨平台 `assert_process_dead(pid)` / `is_process_alive(pid)`（Unix 下经 `libc::kill(pid, 0)` 检验，Windows 下经 `tasklist` 进程枚举检验），断言该 PID 已从系统进程表中彻底消除。
