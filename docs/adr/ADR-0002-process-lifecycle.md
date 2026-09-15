# ADR-0002: Child process lifecycle — process group, TERM→grace→KILL, stdout drain

- Status: accepted
- Date: 2026-09-15
- Spike: S1 / Spike B (`crates/spike/src/subprocess.rs`)
- Tests: `cargo test -p model-serving-spike`（`stdout_drain_64kb_child_stays_alive`、`stderr_drain_64kb_child_stays_alive`、`terminate_escalates_to_kill_and_reaches_stdout_eof`、`drop_reaps_child_and_leaks_nothing`、`process_group_kill_reaps_grandchild` 常规执行；spike 的两路 drain 在 `terminate` 两阶段循环内内联 pump，避免 shutdown 期间大日志冲刷把 child 卡死在 `write()`）

## Context

daemon 监管的 child（`llama-server` / `ninfer-serve`）是**外部二进制**，可能 spawn 孙进程、可能忽略 TERM、可能写爆管道。Spike B 用 tokio::process 验证了三个机制（可丢弃 spike，M2 在 `crates/runtime` 重写正式版）：

1. **持续 stdout/stderr drain**：`AsyncReadExt::read` 循环不断读取管道，输出 >64KB（典型 OS 管道容量）的 chatty child 不阻塞事件循环、不死锁。测试 `stdout_drain_64kb_child_stays_alive` / `stderr_drain_64kb_child_stays_alive` 通过（本机 Windows：PowerShell 循环 child）。
2. **终止升级路径**：先 graceful 信号 → 宽限期 → 强杀 → stdout EOF。unix 用 `kill(-pgid, TERM)`，宽限（测试 300ms / 生产 5s）后 `kill(-pgid, KILL)`；测试 child `trap "" TERM` 拒死，证明只有组 KILL 能终结，stdout 随后到达 EOF（测试 `terminate_escalates_to_kill_and_reaches_stdout_eof` 通过）。
3. **process group 组杀**：spawn 前 `process_group(0)`（pre_exec setsid 等价）使 child 成为新组 leader，`sleep 100 &` 孙进程与 child 同组；`kill(-pgid)` 后验证孙进程消失（测试 `process_group_kill_reaps_grandchild`，仅 unix）。
4. **PGID 复用防护**：一旦观察到 leader 退出，其 PGID 编号即可能被内核分配给无关新进程——任何"事后对 PGID 发信号"（尤其 `Drop` 的 panic 安全网）都可能误杀无关进程。因此：组存活探测不用 `kill -0`（对纯僵尸组也成功，会把死组误判为存活 → 错误的 SIGKILL 升级），Linux 下扫 `/proc/<pid>/stat` 的 pgrp 字段、`state != "Z"` 才算活成员（`/proc` 不可用时回退 signal-0）；`Drop` 只在直接 child **仍存活**（PGID == child pid 可证明仍属本组）时对组发 KILL，已观察到退出则一律不信号（若 `terminate` 未跑过，组内后代有意保留——spike 级限制，见 Decision）。

## Decision

正式 process supervisor（M2，`crates/runtime`）采用：

- **process group**：WSL2（正式目标）下每个 runtime child 经 `process_group(0)` 进入独立进程组，所有终止信号发 `kill(-pgid)`。目的：wrapper shell 的后台作业（如 child 自己 spawn 的辅助进程）不持有 stdout 管道——否则 EOF 永不到达，supervisor 无法判定 child 退出，端口/显存资源永久泄漏。
- **升级序列**：`kill -s TERM -- -pgid` → 宽限（生产 **5s**）→ `kill -s KILL -- -pgid` → 再等 10s 硬超时仍未退出则按 `crashed/unrecoverable` 分类上报（Spike 常量 `POST_KILL_HARD_TIMEOUT`）。宽限内直接 child 退出后**不得立即跳过强杀阶段**：先短暂 settle（spike 用 100ms，settle 期间继续双路 drain，避免后代 flush 大日志卡死在 `write()`）再用 **live-member 探测**（/proc pgrp 扫描，见 Context 4）判断整组是否清空——忽略 TERM 的孙进程会持有 stdout 管道，若此时开始 EOF drain 会永久挂起（spike 的 `IgnoresTerm`/`Grandchild` 测试覆盖）。force 阶段与 leader 已退出的 `AlreadyExited` 路径的组 KILL 同样经 live-member 探测门控：组已空/纯僵尸则不发信号（PGID 可复用）。
- **Drop 安全网规则**：`Drop`（panic 安全网）只在直接 child 仍存活时发组 KILL（此时 PGID 可证明属于本组），随后**无条件**显式 `waitpid(2, WNOHANG)` 轮询收尸：未被观察到的退出（supervisor 未经 `reap`/`is_alive`/`terminate` 就 drop）会留下僵尸，而僵尸未回收前其 PGID 编号仍被占用，显式回收同时关闭了 PGID 复用窗口（tokio 只在后台尽力回收，且 `Child::kill` 后不会代等，需自等防僵尸）。已观察到 leader 退出则 `Drop` 永不信号：PGID 可能已复用；此时若 `terminate` 未跑过，组内后代有意保留（spike 级限制）。**M2 生产 supervisor 的组内成员认证规则**：(a) 直接 child 存活期间可用 /proc ppid 链谱系认证（pid + starttime + pgrp 三重匹配），但 child 退出后 Linux 立即把后代 reparent 给 PID 1，ppid 链失效，故**不得作为退出后清理的依据**；(b) 正式目标 WSL2 + systemd（M8 的 systemd user unit 前置）下，正确机制是把每个 child 放入专属 **cgroup（systemd scope，`systemd-run --scope`）**——cgroup 成员关系是所有权语义，不受 pid/PGID 复用影响，leader 退出后依然成立，对 cgroup 发 kill 只作用于当前真实成员，从根本上消除 PGID 复用问题；无 systemd 环境回退到进程组 + 存活期间谱系认证，接受残余风险并记录。任何"先探测/复核、后对组信号"的两步操作在理论上都有 TOCTOU 窗口（pid 在复核后被复用），M2 仅在 cgroup 不可用时接受该残余风险。M2 用 `libc::kill(-pgid, sig)` 或 cgroup kill 而非 shell out（spike 因依赖最小化用 coreutils `kill`）。
- **stdout/stderr 语义**：两路都持续 drain 入**有界 ring buffer**（Spike 用 4KB tail；M2 按 docs/development-plan.md 的内存上限做成可配置 ring）。EOF 是 child 死亡的信号之一（与 `try_wait` 互为印证）；EOF 前不得判定终态。
- **Windows 差异**：无 process group 语义，`Child::kill()`（TerminateProcess）只杀直接 child，孙进程成为孤儿。因正式目标仅 WSL2，Windows 分支只做单进程终止 + 文档标注（`llama-server`/`ninfer-serve` 本身是单进程，孙进程风险低）；未来若支持原生 Windows 再引入 Job Object。
- **正常退出 drain**：daemon 退出（Ctrl+C / systemd stop）时对所有 running child 执行同一升级序列，drain 完成（全部 EOF/收尸）后才退出，不留孤儿。

## Consequences

- `RuntimeAdapter` 与 supervisor 的边界：adapter 只产 argv/环境（`CommandSpec`），process group 设置、信号、ring buffer 全在 supervisor 内，adapter 不感知 OS 差异。
- exit classification（M2）以 `TerminateOutcome`（`AlreadyExited`/`Graceful`/`Escalated`/error）+ stderr tail 为输入：graceful ⇒ 预期 unload；escalated ⇒ 疑似挂死；硬超时 ⇒ 记录 `kill_timeout` 失败类。
- 宽限期 5s 是产品默认值，`LoadConfig`/runtime 配置可覆盖；健康 child 的 unload 实测应远小于宽限（TERM 后 <1s 退出）。

## 验证状态

- Windows（本开发机）：64KB drain、升级路径 + EOF 两个测试通过（PowerShell 单进程分支）。
- **WSL 现场已验证（2026-09-15）**：WSL Ubuntu 2.2 安装 stable rustup 后，`cargo test --workspace` 全部通过（11 个 spike 测试，含全部 `#[cfg(unix)]` 分支：process group / 孙进程组杀 / `Drop` 收尸回归），`race_a -- --ignored` 在 WSL 复现 **100/100**（与 Windows 一致）。此前"WSL 未装 cargo 无法本地验证 unix 分支"的状态已消除；CI 的 Linux job 继续作为第二重保障。