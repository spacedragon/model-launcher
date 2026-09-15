# model-serving

`model-serving` 是运行在 WSL2 中的本地模型控制面和推理网关。它负责发现本地模型、启动和监管 `llama.cpp` / `NInfer` 推理进程、提供 OpenAI 兼容接口、提供部分 LM Studio REST API，以及向远端浏览器提供管理 UI。

当前仓库处于设计阶段，尚未开始实现。

## 已确认范围

- 只支持 WSL2，不负责安装或升级推理引擎。
- Rust 后端；TypeScript + React Web UI，构建产物由 Rust 服务托管。
- 本地模型，不提供模型下载：`llama.cpp` 使用 `*.gguf`，NInfer 使用 `*.ninfer`。
- 支持多个已加载模型；显存不足时，显式 load 操作可以切换模型。
- inference 请求只路由到已加载模型，不触发隐式加载。
- OpenAI Chat Completions、Completions、Models，以及后端支持时的 Embeddings。
- LM Studio v1 REST API 的 models/list/load/unload 子集。
- SQLite 持久化配置和运行事件，默认不保存聊天正文。

## 文档

- [产品需求](docs/product-requirements.md)
- [总体架构](docs/architecture.md)
- [API 设计](docs/api.md)
- [安全与部署](docs/security-and-deployment.md)
- [开发计划](docs/development-plan.md)

