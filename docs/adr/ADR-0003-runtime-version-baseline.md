# ADR-0003: Runtime version baseline — llama.cpp minimum version, health/models fixture 约定

- Status: proposed（**b5555 为候选值**；WSL 现场 fixture 实测确认后才可 accepted——本开发环境 WSL 未安装 llama.cpp，`command -v llama-server` 无结果）
- Date: 2026-09-15
- Spike: S1 / M0 fixture 采集（`fixtures/runtimes/`，`--help` 采集待现场完成）
- 关联：docs/development-plan.md M0「记录现场 capability fixture 和 `/health`、`/v1/models` fixture」「选择并记录 llama.cpp 最低支持版本及对应 fixture」

## Context

daemon 不安装/升级推理引擎（docs/product-requirements.md：只支持 WSL2 现成环境），但必须声明**最低支持版本**，以便 `doctor`（M1）在版本过低时给出明确错误而不是运行时玄学失败。依赖的引擎行为有三处：

1. `llama-server` 提供 `GET /health`（`{"status":"ok"}` 作为 liveness/readiness 必要条件，docs/architecture.md §4）。
2. `llama-server` 提供 OpenAI 兼容 `GET /v1/models`（identity 校验，发布 `ready` 前比对 model id）。
3. `llama-server --port 0` 时打印实际端口（ADR-0001 的 ready 行协议）。

本机（Windows 开发机 / WSL）未安装 llama.cpp，无法现场采集 `--help` 与真实响应；`ninfer-serve` 同理。

## Decision

- **llama.cpp 最低支持版本（候选）：b5555**（2025 中期稳定线，公开知识确认同时具备 `llama-server` 的 `/health`、OpenAI 兼容 `/v1/models` 与 `--port 0`）——**未经现场验证**。WSL 现场采集 `fixtures/runtimes/llama-server-help.txt` 后，若现场版本能力不符（如 `/health` 语义变化），以现场实测为准更新本 ADR 的版本号，并同步 `doctor` 的版本解析规则。在 fixture 采集完成前，M1 的 `doctor` 只能报"未检测到 runtime"，不得按 b5555 基线拒绝/放行真实版本。
- **NInfer 最低版本**：以 WSL 现场 `ninfer-serve --version` / `--help` 为准（现场版本能力见 docs/architecture.md §4 NInfer adapter 一节），M4 开工前完成 `fixtures/runtimes/ninfer-serve-help.txt` 采集并回填本 ADR。
- **fixture 约定**（`fixtures/runtimes/health-and-models.json`）：数组元素 `{label, endpoint, runtime, body}`；`body` 保存**字节级原样**的 HTTP 响应体（以 JSON 字符串存储，保留空白、键序、转义与结尾换行；不得存解析后的 JSON 对象——反序列化再序列化会丢失字节信息）。当前文件为占位样例，现场采集后以真实响应体覆盖。
- 兼容矩阵（最低/最高已验证版本 × runtime × 引擎版本）在 M0 初始化（本 fixture 目录），每次更新版本基线时同步更新（docs/development-plan.md §4 runtime contract 测试策略）。

## Consequences

- M1 `doctor` 的输出包含：引擎可执行文件是否存在、`--help` 能否解析、版本是否 ≥ 基线、`/health`/`/v1/models` 是否与 fixture 结构一致。
- fixture 文件进入版本控制且**更新必须伴随 ADR 变更**（防止静默漂移）；真实 GPU 相关字段（如 `/v1/models` 的 capabilities）在 M2/M4 真实集成测试时补充。
- 若现场 llama.cpp 版本低于最低支持版本（以本 ADR 最终确认值为准）：`doctor` 报 `runtime_below_minimum` 错误码（M1 错误目录中定义），daemon 拒绝用该引擎 load，而不是降级运行。