# 总体架构

## 1. 部署边界

整个应用运行在 WSL2 中。Rust daemon 是唯一对外服务，也是所有推理子进程的父进程。它直接用 Tokio process API 启动 `llama-server` 或 `ninfer-serve`，无需从 Windows 侧调用 `wsl.exe`。

```text
Remote client / Browser
          |
          | HTTP(S)
          v
+-------------------------------- WSL2 --------------------------------+
| model-serving daemon                                                  |
|                                                                      |
|  Web UI  Management API  OpenAI Gateway                              |
|       \       |             /                                        |
|        Catalog + Scheduler + Instance Registry                       |
|                    |                                                 |
|              Process Supervisor                                     |
|               /             \                                        |
| llama.cpp adapter          NInfer adapter                            |
|       |                         |                                    |
| llama-server :loopback      ninfer-serve :loopback                   |
|       |                         |                                    |
|    *.gguf                   *.ninfer                                 |
+----------------------------------------------------------------------+
                         |
                       CUDA GPU
```

生产远程访问推荐在 daemon 前放 Caddy/Nginx/Tailscale 处理 TLS。也允许 daemon 直接监听 WSL 网卡，但需显式配置并启用管理认证。

## 2. 技术选择

### Rust workspace

- `axum`：HTTP 路由、中间件、WebSocket/SSE 周边能力。
- `tokio`：异步运行时、子进程、signal 和 I/O。
- `reqwest`/`hyper`：连接内部 inference server，支持 streaming body。
- `serde`/`serde_json`：公共协议和 runtime 配置。
- `sqlx` + SQLite：迁移、配置和事件持久化。
- `tracing`：结构化日志和 request/operation correlation id。
- `tower`：认证、请求限制、超时和 body limit。
- `utoipa` 或静态 OpenAPI：管理 API 文档；是否采用由实现 spike 决定。

前端采用 React + TypeScript + Vite，建议使用 TanStack Query 管理 server state。组件库在 UI 实现阶段选择，不进入后端协议。

### 建议 workspace 结构

```text
crates/
  server/             # binary、配置、路由装配、静态 UI
  domain/             # 状态机、调度策略、领域错误
  runtime/            # RuntimeAdapter trait、进程监管
  runtime-llamacpp/   # llama.cpp 参数映射与 capability
  runtime-ninfer/     # NInfer 参数映射与 capability
  api-types/          # OpenAI/LM Studio/内部管理 DTO
  persistence/        # SQLite repositories 与 migrations
web/                  # React/Vite
docs/
```

如果早期编译成本过高，可将这些边界先实现为一个 crate 内的模块，但依赖方向仍按此设计。

## 3. 核心领域对象

### Model

- `id`：内部稳定 UUID。
- `key`：API 使用的唯一可读标识，扫描时生成，允许管理员覆盖。
- `path`：canonical absolute path。
- `artifact_kind`：`gguf | ninfer`。
- `size_bytes`、`mtime`、可选 metadata。
- `default_runtime_id`、`default_load_config`。

模型身份不能只用文件名。若不同目录产生同名 key，追加短路径哈希；移动文件会被视为新 artifact，除非以后实现内容 fingerprint 迁移。

### Runtime

- `id`、`kind`、`executable_path`。
- `enabled`、`version_text`、`capabilities`。
- runtime 固定参数模板；不允许存储任意 shell command。

### Instance

- `instance_id`：一次加载实例的唯一标识，同时用于 LM Studio unload。
- `model_id`、`runtime_id`、resolved load config。
- `state`、`pid`、`port`、`started_at`、`last_used_at`。
- `active_requests`、`health`、`failure`。

数据库保存实例记录和期望状态，但 PID/port 只有进程监管器确认后才可信。

### Operation

load/unload/rescan 是可能耗时的 operation：`queued | running | succeeded | failed | cancelled`。内部管理 API 默认返回 `202 + operation_id`，Web UI 通过 SSE 订阅进展。LM Studio 兼容 load 为满足其同步响应约定，会等待 operation 结束或到达专用长超时。

## 4. Runtime adapter

统一接口的概念形态：

```rust
trait RuntimeAdapter {
    fn kind(&self) -> RuntimeKind;
    async fn probe(&self, executable: &Path) -> Result<Capabilities>;
    fn validate(&self, model: &Model, config: &LoadConfig) -> Result<()>;
    fn command(&self, ctx: &LaunchContext) -> Result<CommandSpec>;
    async fn health(&self, endpoint: &Url) -> Result<Health>;
    fn classify_exit(&self, exit: ExitStatus, stderr_tail: &str) -> FailureClass;
}
```

`CommandSpec` 是 argv 数组和受控环境变量，不经过 shell。adapter 必须把“不支持的字段”区分为 error 或 ignored-with-warning，不能静默误配。

### llama.cpp adapter

基本命令：

```text
llama-server --model <path.gguf> --host 127.0.0.1 --port <port> ...
```

健康检查和能力会因 llama.cpp 版本变化，首次实现时通过 integration fixture 固定受支持的最低版本。常见统一字段映射包括 context length、GPU layers、batch size、flash attention 和 parallel/concurrency。

### NInfer adapter

现场确认的命令形态：

```text
ninfer-serve <path.ninfer> --host 127.0.0.1 --port <port> \
  --model-id <key> --max-context <n> --kv-capacity <n|auto> \
  --max-concurrency <n> --prefill-chunk <n> --kv-dtype <dtype> ...
```

现场版本支持 OpenAI Chat Completions/Responses、Anthropic Messages、`GET /v1/models`、`GET /health` 和 `/slots`。首版 adapter 以 `GET /health -> {"status":"ok"}` 作为 liveness/readiness 的必要条件，并通过 `/v1/models` 校验实际 model id。

NInfer 专属配置放在 `engine_config.ninfer`，包括 `kv_capacity`、`prefill_chunk`、`kv_dtype`、speculation、vision 和 thinking flags。公共 API 不假设这些参数在 llama.cpp 上存在。

## 5. 加载状态机

```text
unloaded -> queued -> loading -> ready
                       |          |
                       v          v
                     failed    draining -> unloading -> unloaded
                                  |
                                  v
                                ready   (drain 被取消/超时策略回退)

任意运行态 --unexpected exit--> crashed
crashed --explicit load---------> queued
```

加载事务：

1. 在 per-model lock 下验证 model/runtime/config。
2. 为 operation 和 instance 生成 ID，选择空闲 loopback port。
3. 做资源预检。若明显不足且策略允许，选择 idle LRU victims，依次 drain/unload。
4. 以独立 process group 启动子进程，同时采集有界 stdout/stderr ring buffer。
5. 等待 health endpoint，采用启动 deadline；同时监控子进程退出。
6. health 成功后读取 `/v1/models` 校验标识，再原子发布为 `ready`。
7. 若疑似 OOM，且尚未进行过驱逐重试，则驱逐 idle victims 后只重试一次。
8. 失败时终止残留进程、释放端口 reservation、记录结构化失败原因。

避免“端口先检查后被占用”的竞态：daemon 应持有 listener reservation，或让子进程在受控端口范围内失败重选；具体方式在实现 spike 中确定，因为现有引擎不能继承监听 socket。

## 6. 显存切换策略

- `deny`：资源不足立即失败。
- `auto_evict_idle`：默认；按 `last_used_at` 从旧到新驱逐 idle 实例。
- `replace_instances`：调用者显式给出可卸载的 instance IDs，最可预测。

active request 是硬保护条件。加载 operation 使用全局 resource lock 串行化，避免两个并发 load 各自依据相同空闲显存做决定。MVP 假设单 GPU；数据模型保留 `device_ids`。

失败识别不能只依赖 stderr 文案。建议记录：进程 exit code、stderr tail、启动前后 `nvidia-smi` snapshot、是否出现 health、deadline 阶段。只有 `FailureClass::GpuOomLikely` 才自动驱逐重试，其他错误直接失败。

## 7. 推理请求路径

1. 认证、body size、并发限制。
2. 对需要路由的请求只解析 `model`，保留原始 JSON 字段。
3. 从内存 registry 获取 `ready` instance lease；lease 增加 `active_requests`。
4. 将请求发送到 loopback upstream。必要时将外部 bearer token 替换为 runtime 私有 token。
5. 普通响应透传状态、body 和允许的 headers；SSE 以流方式转发，不缓冲完整响应。
6. 完成、错误或客户端断开均释放 lease，并更新无正文 metrics。

同一 model key 若未来存在多个实例，采用 least-active + round-robin tie-break。首版通常是一对一。

## 8. 进程恢复和退出

- daemon 启动时将数据库中非终态实例标记为 `crashed/recovery_required`。
- 首版不 attach 到未知 PID，避免 PID reuse 和错误终止其他进程。
- daemon 正常退出时停止接收管理操作，drain inference，然后终止其监管的全部子进程。
- 可选的 `restore_on_start` 后续实现；首版服务重启后由用户重新 load。
- 子进程意外退出不会无限重启；记录 crash 并等待显式 load，防止 OOM crash loop。

## 9. 一致性和并发控制

- per-model mutex：防止同一模型同时 load/unload。
- global resource mutex：串行 GPU placement/eviction/load commit。
- instance lease：保护有活动请求的实例不被自动驱逐。
- SQLite transaction：operation、instance desired state 和 audit event 同步提交。
- 内存 registry 是路由真相；SQLite 用于恢复和审计，不位于 inference 热路径。

