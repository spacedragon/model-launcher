# Job Tracker（按 development-plan.md §3 + M0 spike 拆分）

状态：`pending` → `implementing` → `reviewing` → `passed/committed` 或 `fixing`（codex review 未通过，回炉）

| # | Job | 状态 | 说明 / codex review 结论 |
|---|-----|------|--------------------------|
| 1 | Workspace/CI/tooling | passed/committed | 分支 job/1-workspace-ci-tooling @ 19ec93a（main 受环境 hook 保护，合入待人工）。codex review 两轮通过（P2: 端口配置严格校验、tracing subscriber、workspace 元数据继承、代理端口一致）。注意：本机 npm 走 npmmirror（user-level），lockfile 已改回 npmjs |
| S1 | M0 风险 spike + fixtures + ADR | in-progress | ✅ Spike A (SSE 透明转发/中断取消，crates/spike，5 tests，ADR-0004，codex 两轮通过)；⏳ Spike B 子进程 TERM/KILL、Spike C 端口竞态、ADR-0001/0002/0003、fixtures/ |
| 2 | Domain types + error catalog | pending | Model/Runtime/Instance/Operation/LoadConfig + 错误码（docs/api.md） |
| 3 | SQLite migrations/repositories | pending | WAL、事务、测试 fixture |
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