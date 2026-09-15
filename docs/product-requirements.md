# 产品需求

状态：设计基线  
日期：2026-09-15

## 1. 产品目标

在一台 Windows + WSL2 工作站上提供一个可远程访问的本地模型服务。用户通过 Web UI 或管理 API 管理本地模型的加载状态，通过统一的 OpenAI 兼容地址调用实际运行在 `llama.cpp` 或 NInfer 上的模型。

系统的核心价值是把“模型文件、引擎启动参数、子进程生命周期、显存切换和稳定 API 地址”封装成一个长期运行的服务。

## 2. 用户角色

- 管理员：配置模型目录与 runtime、加载/卸载模型、查看状态与日志、管理访问密钥。
- API 使用者：通过 OpenAI 兼容接口调用已加载模型。
- 只读观察者暂不作为独立角色；首版不实现多用户和 RBAC。

## 3. 首版功能范围

### 3.1 本地模型目录

- 管理一个或多个 WSL 内可访问的绝对目录，包括 `/home/...` 和 `/mnt/...`。
- 扫描 `*.gguf` 和 `*.ninfer`；不递归跟随符号链接。
- GGUF 只能关联 `llama.cpp` runtime，`.ninfer` 只能关联 NInfer runtime。
- 记录文件路径、大小、mtime、格式、可选显示名称和默认加载配置。
- 首版不下载、不转换、不修改模型文件。

### 3.2 Runtime 配置

- 用户配置已经存在的 `llama-server` 与 `ninfer-serve` 可执行文件绝对路径。
- 启动前验证文件存在、可执行，并读取 `--help` 或版本信息形成 capability snapshot。
- runtime adapter 将统一加载配置翻译成各引擎命令行参数。
- 子进程仅监听 loopback 动态端口，不直接暴露给远端。

### 3.3 模型加载与切换

- 同时运行多个模型实例，前提是资源允许。
- 模型状态：`unloaded`、`queued`、`loading`、`ready`、`draining`、`unloading`、`failed`、`crashed`。
- `load` 是显式操作；普通 inference 请求不会自动加载模型。
- 当加载因 GPU 显存不足而失败时，可按请求中的切换策略卸载其他空闲实例并重试一次。
- 默认切换策略为 `auto_evict_idle`：只驱逐无活动请求、最久未使用的实例。
- 不强杀正在服务请求的实例。若没有可安全驱逐的实例，load 返回资源不足。
- 同一 model key 默认只有一个实例；管理 API 保留 `instance_id`，以后可支持同模型多实例。

这里的“显存不足”首版采用两层判断：加载前基于 `nvidia-smi` 的保守检查；加载失败后根据进程退出、stderr 和 GPU 状态分类。估算不保证精确，引擎进程成功进入健康状态才是加载成功的最终依据。

### 3.4 推理代理

- 一个固定对外地址代理所有已加载实例。
- 根据请求 JSON 的 `model` 字段查找 `ready` 实例。
- 支持普通 JSON 响应和 SSE 流式响应；传递 backpressure 和客户端断开。
- 首版接口：
  - `GET /v1/models`
  - `POST /v1/chat/completions`
  - `POST /v1/completions`
  - `POST /v1/embeddings`，仅 runtime 声明支持时
- tools/function calling 等字段尽量透明传递，是否生效取决于后端；首版不承诺跨引擎语义归一化。

### 3.5 管理 API 和 Web UI

- 模型目录扫描、模型列表和详情。
- load、unload、切换策略和运行参数。
- runtime 配置、健康状态、最近日志。
- 当前请求数、启动时间、最近使用时间和基础 token/耗时统计。
- Web UI 至少包含 Dashboard、Models、Model detail/load form、Instances、Settings、Logs。
- Web UI 使用 TypeScript + React，构建为静态资源并由 Rust 服务提供。

### 3.6 认证与数据

- 管理 API 和 UI 必须认证，首次启动生成管理 token；后续可增加账号登录。
- inference API key 是可选配置。启用时接受 `Authorization: Bearer <key>`。
- 默认只监听 `127.0.0.1`；远端访问必须显式改为非 loopback 地址。
- SQLite 保存配置、模型索引、实例期望状态、操作任务和审计事件。
- 默认不保存 prompt、message、completion 或完整请求体。

## 4. 明确不做

- Windows 原生和非 WSL Linux 的正式支持。
- 自动安装/升级 llama.cpp、NInfer、CUDA 驱动或 WSL。
- Hugging Face 或其他远端模型下载。
- GGUF 与 `.ninfer` 格式转换。
- 多节点调度、跨机器集群、训练或微调。
- 多租户、计费、配额和细粒度 RBAC。
- 首版不实现 LM Studio `/api/v1/chat`、download 和 stateful chat/MCP。

## 5. 关键行为与验收标准

1. 服务重启后能恢复配置与模型目录索引，但不会盲目认领孤儿进程。
2. 管理员加载一个有效 GGUF 后，实例最终进入 `ready`，并可通过统一 `/v1/chat/completions` 流式调用。
3. NInfer artifact 能用现场 `ninfer-serve <model.ninfer> ...` 形式启动，`GET /health` 成功后才标记为 `ready`。
4. 指定未知或未加载模型时返回 OpenAI 风格 404，不隐式加载。
5. unload 先停止接收新请求，等待活动请求在超时内结束，再发送 SIGTERM，最后才使用 SIGKILL。
6. `auto_evict_idle` 不会驱逐有活动请求的实例；资源仍不足时给出可诊断错误。
7. 客户端中断 SSE 后，网关取消上游请求并正确减少实例活动计数。
8. 未认证用户无法使用管理 API；inference auth 可按配置启用或关闭。
9. 外部请求日志不包含消息正文和 bearer token。
10. `GET /api/v1/models`、load、unload 的约定字段满足本文定义的 LM Studio 子集。

## 6. 后续版本候选

- idle TTL 自动卸载、按请求自动加载。
- 模型下载和校验。
- OpenAI Responses、Anthropic Messages、vision 与 rerank。
- 账号登录、RBAC、调用配额和更完整的 metrics。
- 运行时显存估算插件、模型放置策略和多 GPU 调度。

