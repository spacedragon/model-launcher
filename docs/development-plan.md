# 开发计划

## 1. 交付策略

先打通单模型、单 runtime 的可靠纵切，再扩展到第二 runtime、多模型切换和兼容层。每个里程碑必须有可运行 demo 和自动化验收，避免先做完整 UI 后才发现子进程/streaming 边界错误。

## 2. 里程碑

### M0：工程基线与风险 spike

目标：验证三项最高风险技术点，产出可丢弃实验代码。

- 初始化 Cargo workspace、React/Vite、lint/format/test CI。
- spike：Axum -> 本地假 upstream 的 SSE 透明转发和客户端中断取消。
- spike：Tokio 子进程 process group、stdout/stderr 持续采集、TERM/KILL。
- spike：空闲端口选择与 child bind 竞态的可接受方案。
- 记录现场 `ninfer-serve --help` capability fixture 和 `/health`、`/v1/models` fixture。
- 选择并记录 llama.cpp 最低支持版本及对应 fixture。

完成条件：三个 spike 在 WSL 中通过；架构决策记录端口策略和最低 runtime 版本。

### M1：领域模型、SQLite 与模型扫描

- 定义 Model、Runtime、Instance、Operation、LoadConfig 和错误码。
- 建立 migration 与 repository；SQLite WAL、事务和测试数据库 fixture。
- model root CRUD 和安全扫描，识别 `.gguf` / `.ninfer`。
- model key 冲突规则、mtime 增量扫描和已删除标记。
- runtime CRUD 与 executable probe。
- 实现 `doctor` 初版。

完成条件：在真实 `/mnt/...` root 扫描模型；重启后索引一致；路径逃逸测试通过。

### M2：llama.cpp 单实例生命周期

- 实现 `RuntimeAdapter` 和 llama.cpp adapter。
- process supervisor、日志 ring、health deadline 和 exit classification。
- load/unload operation 及状态机。
- 原生 management API：models、instances、operations、events。
- 正常退出时 drain/terminate child。

完成条件：真实 GGUF 连续 load/inference/unload 20 次无孤儿进程；非法模型、端口冲突、启动超时均进入确定终态。

### M3：OpenAI inference gateway

- `/v1/models`、chat completions、completions。
- 按 `model` 路由，普通 JSON/SSE 透明转发。
- request lease、backpressure、客户端取消、超时和错误映射。
- 可选 inference API key、body/concurrency limit。
- 无正文 metrics 和 tracing correlation。
- embeddings capability gate。

完成条件：OpenAI 客户端可以同步和流式调用；断流测试无泄漏；未加载模型不会触发 load。

### M4：NInfer adapter

- `.ninfer` artifact validation。
- 映射公共参数和 NInfer 专属参数。
- 使用 `/health` readiness、`/v1/models` identity 校验。
- 适配现场参数：max context、KV capacity/dtype、concurrency、prefill、spec、vision/thinking。
- 真实 NInfer 集成测试及 crash/timeout/OOM fixture。

完成条件：用现场 qwen `.ninfer` artifact 完成 load、streaming chat、unload；daemon 不直接暴露 1234 child port。

### M5：多实例和显存切换

- global resource lock、instance lease 和 LRU idle 排序。
- `deny`、`auto_evict_idle`、`replace_instances`。
- `nvidia-smi` GPU snapshot parser；无该命令时安全降级。
- OOM likely 分类和至多一次驱逐重试。
- 并发 load/unload/request race tests。

完成条件：资源不足时能从 idle 模型切到目标模型；活动实例不被自动驱逐；并发 load 不造成双重超分配决策。

### M6：LM Studio 子集

- `GET /api/v1/models`。
- `POST /api/v1/models/load` 同步 bridge 到 operation。
- `POST /api/v1/models/unload`。
- 参数映射、null/unknown 字段和错误 fixture。
- 对选定 LM Studio 版本做 contract tests，发布兼容矩阵。

完成条件：官方 REST 示例经最小改动可调用；不支持的字段得到明确 422；不支持的 endpoint 返回 404。

### M7：Web UI

- 登录/management token bootstrap。
- Dashboard：daemon/GPU/runtime/instance 状态。
- Models：搜索、扫描、详情、load form。
- Instances：active requests、drain/unload、日志 tail。
- Settings：model roots、runtime executable、inference auth。
- Operation SSE、错误提示、断线重连和 responsive layout。
- Rust build 嵌入带内容哈希的 frontend assets。

完成条件：远端浏览器完成扫描、加载、调用状态观察和卸载；刷新页面不丢 operation 最终状态。

### M8：加固与首个发布

- management auth、session/CSRF、secret hashing/redaction。
- rate/body/header limits、trusted proxy 配置。
- systemd user unit、示例配置、升级/备份说明。
- 故障注入：kill child、kill daemon、磁盘只读、DB busy、GPU OOM、upstream malformed SSE。
- 安全检查、性能基线、SBOM/license audit。
- WSL 安装和远程访问文档。

完成条件：所有产品验收标准通过；干净 WSL 环境按文档部署成功；发布已知限制和 runtime 兼容矩阵。

## 3. 建议 issue 拆分顺序

每项控制在一个可审查 PR：

1. Workspace/CI/tooling。
2. Domain types + error catalog。
3. SQLite migrations/repositories。
4. Secure model scanner。
5. Runtime probe and command spec。
6. Process supervisor。
7. Operation/state machine。
8. llama.cpp adapter + real smoke test。
9. Management APIs + event stream。
10. Gateway JSON proxy。
11. Gateway SSE/cancellation。
12. Auth/limits/redaction。
13. NInfer adapter + real smoke test。
14. GPU snapshot + eviction scheduler。
15. LM Studio contract layer。
16. React shell/auth/dashboard。
17. Models/load form/instances/logs。
18. Packaging/systemd/docs/hardening。

## 4. 测试策略

### 单元测试

- 状态转换表和非法转换。
- 参数 validation/adapter argv snapshot。
- model key、路径 containment 和 symlink。
- victim selection 与 active lease 保护。
- OpenAI/LM Studio error mapping。
- secret redaction。

### 组件测试

- 使用脚本化 fake inference server 模拟 health 延迟、SSE、断流、OOM stderr 和 malformed response。
- 临时 SQLite 验证 transaction 和 migration。
- 启动真实轻量 child fixture 验证 process group 与 drain/kill。

### Runtime contract 测试

- 版本化保存 `--help`、health、models、chat 和错误响应 fixture。
- llama.cpp 和 NInfer 的真实 GPU 测试使用独立标签，不阻塞无 GPU 的普通 CI。
- 每次更新最低/最高验证版本时更新兼容矩阵。

### 端到端测试

- 浏览器：bootstrap/login -> scan -> load -> observe ready -> unload。
- API：OpenAI SDK sync/stream；LM Studio REST curl fixture。
- 故障：client cancel、child crash、daemon restart、concurrent unload。

## 5. 性能与可靠性预算

首版先建立基线，建议目标：

- 非流式网关自身增加的 p50 latency 小于 10 ms（不含网络和模型）。
- SSE 首 token 不因网关额外缓冲；chunk 到达后尽快 flush。
- daemon 空闲 RSS 小于 150 MiB（不含前端构建和 child）。
- 日志 ring、operation event replay 和 response body 均有明确内存上限。
- 每个 management mutation 可重试或有明确冲突响应，不出现永久 `loading`。

这些是工程目标，不是首版对外 SLA；M3 后用实测修订。

## 6. 开发前必须固定的决策

以下不再阻塞设计，但应在对应里程碑开工时形成 ADR：

- 项目最终 binary/package 名称是否沿用 `model-serving`。
- llama.cpp 最低支持版本与 health endpoint。
- 空闲端口 reservation 策略。
- UI component library。
- management token bootstrap 的具体交互。
- LM Studio contract test 锁定的版本号和 loaded instance 完整字段。

## 7. 首个可用切片

如果优先追求尽快可用，完成 M0-M4 即可得到：一个在 WSL 运行的 Rust daemon，能管理 llama.cpp/NInfer 单实例，提供 OpenAI chat streaming 和基础管理 API。之后再做多模型显存切换、LM Studio 兼容和完整 Web UI。不要把 M5 的并发状态正确性压缩进单实例 MVP。

