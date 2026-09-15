# API 设计

状态：首版契约草案  
所有 JSON 使用 UTF-8；时间为 RFC 3339 UTC；ID 区分大小写。

## 1. API 分区

| 前缀 | 用途 | 兼容目标 | 认证 |
|---|---|---|---|
| `/v1/*` | 推理 | OpenAI 子集 | 可选 inference API key |
| `/api/v1/*` | 模型 list/load/unload | LM Studio v1 子集 | 管理认证 |
| `/admin/v1/*` | 完整管理面 | 本项目原生 | 管理认证 |
| `/health/live` | daemon liveness | 本项目原生 | 无 |
| `/health/ready` | daemon readiness | 本项目原生 | 无 |

`/api/v1` 只承诺本文列出的字段。为避免让客户端误判，不实现的 LM Studio endpoint 返回 404，而不是返回形似成功的降级结果。

## 2. 通用规则

### 2.1 Correlation ID

接受合法的 `X-Request-Id`，否则生成 UUID；所有响应返回该 header。管理 operation 另有稳定 `operation_id`。

### 2.2 错误

OpenAI 路径返回 OpenAI 风格：

```json
{
  "error": {
    "message": "Model 'qwen-local' is not loaded",
    "type": "invalid_request_error",
    "param": "model",
    "code": "model_not_loaded"
  }
}
```

原生管理路径返回 Problem Details 风格：

```json
{
  "type": "urn:model-serving:error:resource-exhausted",
  "title": "Insufficient GPU memory",
  "status": 409,
  "detail": "No idle instance can be evicted safely",
  "request_id": "...",
  "code": "gpu_memory_insufficient"
}
```

常用状态码：400 配置/请求无效，401 未认证，403 权限不足，404 不存在或模型未加载，409 状态冲突/资源不足，413 body 过大，422 runtime 不支持字段，429 请求过多，502 upstream 协议错误，503 模型不可用，504 upstream 超时。

### 2.3 认证

- 管理：`Authorization: Bearer <management-token>` 或 UI 的安全会话 cookie。
- 推理：若配置了 key，使用 `Authorization: Bearer <inference-key>`；未配置时不要求 header。
- 外部 credential 不传给 inference child；如子进程配置了私有 API key，由网关替换 header。

## 3. OpenAI 兼容子集

### `GET /v1/models`

只返回 `ready` 实例，而非磁盘上的全部模型：

```json
{
  "object": "list",
  "data": [
    {
      "id": "qwen-local",
      "object": "model",
      "created": 1789457473,
      "owned_by": "local"
    }
  ]
}
```

### `POST /v1/chat/completions`

- 必须有非空字符串 `model` 和合法 `messages`。
- `stream: true` 时转发 `text/event-stream`，以 `data: [DONE]` 结束。
- JSON 中未知字段默认透明转发。
- upstream 若不支持字段，尽量保留其 4xx 内容并包装为 OpenAI error。
- 首版支持 JSON 文本消息；多模态内容仅在 capability 明确开启时放行。

### `POST /v1/completions`

规则同上。若 runtime 没有原生 completions endpoint，返回 422；首版不把 prompt 隐式改写成 chat。

### `POST /v1/embeddings`

仅当实例 capability 声明 embeddings 时路由，否则返回 `unsupported_capability`。NInfer 现场版本没有确认 embeddings，因此不得仅凭模型类型假定支持。

### 路由错误

- model key 不存在：404 `model_not_found`。
- model 存在但未加载：404 `model_not_loaded`。
- 正在 loading/draining：503 `model_not_ready`，可返回 `Retry-After`。
- runtime crash：503 `upstream_unavailable`。

## 4. LM Studio v1 兼容子集

本项目按 LM Studio 当前官方 v1 路径实现三个 endpoint：`GET /api/v1/models`、`POST /api/v1/models/load`、`POST /api/v1/models/unload`。不支持 download 和 `/api/v1/chat`。

### `GET /api/v1/models`

列出磁盘中已索引模型和各自已加载实例：

```json
{
  "models": [
    {
      "type": "llm",
      "publisher": "local",
      "key": "qwen-local",
      "display_name": "Qwen Local",
      "architecture": null,
      "quantization": null,
      "size_bytes": 17000000000,
      "params_string": null,
      "loaded_instances": [
        {
          "id": "qwen-local",
          "status": "loaded",
          "context_length": 32768
        }
      ]
    }
  ]
}
```

只有能够可靠读取的 metadata 才填写，未知字段用 `null`，不根据文件名猜测 architecture/quantization。`loaded_instances` 的最终字段在兼容测试 fixture 中与选定 LM Studio 版本锁定。

### `POST /api/v1/models/load`

请求子集：

```json
{
  "model": "qwen-local",
  "context_length": 32768,
  "eval_batch_size": 512,
  "flash_attention": true,
  "offload_kv_cache_to_gpu": true,
  "echo_load_config": true
}
```

- `model` 必填，对应 model key。
- 通用支持 `context_length`。
- llama.cpp 尝试映射 `eval_batch_size`、`flash_attention`、`offload_kv_cache_to_gpu`；实际支持取决于 capability probe。
- NInfer 对无法等价映射的字段返回 422，不静默忽略。
- 额外接受可选扩展 header `X-Model-Serving-Eviction-Policy: deny|auto_evict_idle`；默认 `auto_evict_idle`。
- endpoint 按 LM Studio 的同步语义等待加载完成。达到 load API deadline 时返回 504，但后台 operation 继续；客户端可用原生 operation API 查询。

成功响应：

```json
{
  "type": "llm",
  "instance_id": "qwen-local",
  "load_time_seconds": 9.099,
  "status": "loaded",
  "load_config": {
    "context_length": 32768,
    "flash_attention": true
  }
}
```

同一 model key 已经 ready 且 resolved config 相同则幂等返回现有实例；配置不同返回 409，要求先 unload。首版不通过这个 endpoint 创建同模型第二实例。

### `POST /api/v1/models/unload`

请求：

```json
{ "instance_id": "qwen-local" }
```

等待 drain 和进程退出后返回：

```json
{ "instance_id": "qwen-local" }
```

未知 instance 返回 404；已经 unloaded 的历史 instance 也返回 404。默认 drain deadline 超时后结束剩余 upstream 请求，再终止子进程。

### 兼容性声明

兼容对象是 REST wire subset，不是 LM Studio SDK/WebSocket 协议，也不保证 `lms` CLI 的所有命令可用。CI 保存官方示例的 request/response fixtures；发布说明必须列出已验证的 LM Studio 版本。

官方协议参考：

- <https://lmstudio.ai/docs/developer/rest>
- <https://lmstudio.ai/docs/developer/rest/list>
- <https://lmstudio.ai/docs/developer/rest/load>
- <https://lmstudio.ai/docs/developer/rest/unload>

## 5. 原生管理 API

### Models

- `GET /admin/v1/models`：分页、按 format/state/runtime 搜索全部索引。
- `GET /admin/v1/models/{model_id}`：详情、默认配置、关联实例。
- `POST /admin/v1/model-roots`：增加受控扫描目录。
- `GET /admin/v1/model-roots`：列出扫描目录和最近扫描结果。
- `POST /admin/v1/model-roots/{id}/scan`：返回 202 operation。
- `PATCH /admin/v1/models/{model_id}`：修改 key、display name、默认 runtime/config。

### Instances

- `GET /admin/v1/instances`：列出当前及最近实例。
- `POST /admin/v1/models/{model_id}/load`：异步 load。
- `POST /admin/v1/instances/{instance_id}/unload`：异步 drain/unload。
- `GET /admin/v1/instances/{instance_id}/logs?cursor=...`：有界日志分页。

原生 load 示例：

```json
{
  "runtime_id": "ninfer-sm89",
  "eviction_policy": "auto_evict_idle",
  "load_config": {
    "context_length": 262144,
    "max_concurrency": 1,
    "engine_config": {
      "ninfer": {
        "kv_capacity": 262144,
        "prefill_chunk": 2048,
        "kv_dtype": "int8",
        "spec": "mtp",
        "draft_tokens": 3,
        "lm_head_draft": true,
        "thinking": "disabled"
      }
    }
  }
}
```

响应：

```json
{
  "operation_id": "op_...",
  "instance_id": "inst_...",
  "status": "queued"
}
```

### Operations and events

- `GET /admin/v1/operations/{id}`：状态和结构化结果/错误。
- `GET /admin/v1/events`：SSE；事件包括 operation progress、instance state、scan result、log notice。
- SSE 使用单调递增 event ID，支持 `Last-Event-ID` 的有限窗口重放。

### Runtimes and settings

- `GET /admin/v1/runtimes`
- `POST /admin/v1/runtimes`
- `PATCH /admin/v1/runtimes/{id}`
- `POST /admin/v1/runtimes/{id}/probe`
- `GET /admin/v1/settings`
- `PATCH /admin/v1/settings`

修改监听地址、数据库路径等需要重启的设置时，响应中明确返回 `restart_required: true`。

## 6. Health

- `/health/live`：进程 event loop 正常即 200，不探测 GPU 和子进程。
- `/health/ready`：数据库可用、配置已加载即 200；没有 loaded model 不影响 daemon readiness。
- 单个 instance 的 health 只出现在认证后的管理 API，不公开内部端口或可执行路径。

