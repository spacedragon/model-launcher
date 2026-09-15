# ADR-0001: Runtime child port allocation — child self-selects, reports via stdout

- Status: proposed（机制已由 Spike C 合成 child 验证；**M2 开工门槛**：WSL 现场用真实 `llama-server` / `ninfer-serve` 启动（`--port 0`）验证 ready-line 究竟出现在 stdout 还是 stderr、格式是否匹配 `PORT=<n>` 解析规则，验证通过后才可将本 ADR 置为 accepted）
- Date: 2026-09-15
- Spike: S1 / Spike C (`crates/spike/src/port_race.rs`)
- Tests: `cargo test -p model-serving-spike`（`race_b_child_self_selects_port` 常规执行；`race_a_preallocated_port_failure_rate` 标记 `#[ignore]`，需 `--ignored --nocapture` 手动跑，100 轮约 80s）

## Context

`llama-server` / `ninfer-serve` 是独立子进程，**不能继承 daemon 的监听 socket**（跨进程 SO_INHERITED 在 WSL2/Windows 上均不可行）。daemon 必须把"用哪个端口"告诉 child，有两种候选：

- **A（daemon 预探测）**：daemon `bind(127.0.0.1:0)` 拿到空闲端口 P → 释放 → 把 P 传给 child。P 的"空闲"只在探测瞬间成立，child 真正 bind 之前（引擎启动、加载模型，`llama-server` 在几秒后才 bind）任何竞争者都可以抢占 P。
- **B（child 自选）**：child 自己 `bind(127.0.0.1:0)` 由 OS 保证无竞态，启动完成后向 stdout 打印 ready 行 `PORT=<n>`，daemon 解析后才知道实际端口并发起流量。

Spike C 实测（N=100，Windows 与 WSL Ubuntu 2.2 均复现；探针用 `powershell -File <.ps1>`（Windows）/ `python3`（Linux），探针脚本以 **stdout token** `BIND=ok` / `BIND=conflict` 报告结果而非退出码——早期 inline PowerShell `-Command` 探针在 CreateProcess 参数引号下出现 ParserError，导致探针自身失败无法与"绑定被拒"区分，是跨 OS 测量不可复现的根源；.ps1 文件 + token 契约修复了测量，之后两 OS 结果一致；stealer 在 +20ms bind 并**持有**端口 500ms，child 在 +150ms bind 模拟引擎启动延迟）：

| 方案 | 结果（Windows / WSL） | 说明 |
| --- | --- | --- |
| A | child bind 失败 **100/100**（两 OS 一致） | 竞争者持有端口期间 child 必败——失败率 100% 是模拟延迟结构（child 恒定晚于 stealer）的确定性结果，但证明了"先检查后使用"的窗口完全可被抢占；真实 WSL 环境的失败率取决于竞争者时序，无法靠重试消除 |
| B | 成功率 **100/100**（两 OS 一致，ready 行解析 + 真实 TCP connect 验证） | OS 分配的端口无竞争者 |

## Decision

运行时端口策略采用 **B**：

1. daemon 以 `--port 0`（或等价参数）启动 child。
2. child 启动完成后必须向 **stdout** 输出一行 `PORT=<n>`（llama.cpp：`--port 0` 时打印实际端口；NInfer 现场是否支持 0 端口需在 M4 前确认，若不支持则 adapter 用 daemon 从 `--help` fixture 推导的能力判断并回退策略，见 Consequences）。
3. daemon 的 process supervisor 必须**持续 drain stdout**（见 ADR-0002）并从 ready 行解析端口，端口在解析成功前对该 runtime 不可用。
4. daemon **不做** bind-probe 预分配；`/health`、inference 流量的目标地址一律以 ready 行为准。

## Consequences

- M2 的 `RuntimeAdapter::command()` 必须产出"0 端口 + ready 行"形态的 argv，supervisor 必须实现 ready-line 解析协议（每 runtime 一个 ready-line 形态，存入 adapter）。
- daemon 无法提前告诉客户端/网关"模型将在端口 X"——M3 gateway 的路由表以 supervisor 上报的端口为准。
- 若 NInfer 现场确认不支持 `--port 0`：adapter 回退到"daemon 分配高位端口段（如 48000-49000 内随机取号）+ 抢占失败时换号重启 child 至多一次"，该回退本身即 spike 证明的 A 方案风险受控版本。此回退不得用于 llama.cpp。
- `fixtures/runtimes/llama-server-help.txt` / `ninfer-serve-help.txt` 采集后需复核 `--port 0` 行为并更新本 ADR 的确认状态。
- 本开发环境的 WSL 发行版未安装 llama.cpp / NInfer（`command -v llama-server ninfer-serve` 无结果），现场验证需在有 runtime 的 WSL 机器上完成；在此之前 M2 的 llama.cpp adapter 实现不得开工。