# 安全与部署

## 1. 威胁边界

系统可以启动本地二进制、读取大模型文件并将管理页面暴露到网络，因此管理 API 等同于本机高权限控制面。首版假设单一可信管理员；不能把管理端直接无认证地暴露到不可信网络。

## 2. 默认安全设置

- 默认监听 `127.0.0.1`。
- 首次启动生成高熵 management token，只在终端显示一次；数据库仅保存带 salt 的密码学哈希。
- inference API key 可选；若配置，也只保存哈希。
- UI 登录用 `HttpOnly`、`SameSite=Strict` cookie；启用 TLS 时设置 `Secure`。
- 管理 mutation 做 Origin/CSRF 防护；bearer-only API 仍校验合理的 content type。
- request body、并发、header 和启动/推理超时均有限制。
- 日志自动遮蔽 `Authorization`、cookie、token 和模型请求正文。

若用户显式监听 `0.0.0.0` 且没有管理认证，服务必须拒绝启动。inference auth 可以关闭，但管理认证不能关闭；仅在 compile-time development profile 下允许例外。

## 3. 文件系统安全

- model root 必须是 WSL 内可 canonicalize 的绝对目录。
- 扫描不跟随跨 root 的 symlink；打开模型前再次 canonicalize，防止 TOCTOU 路径逃逸。
- API 永不接受一个任意文件路径直接 load，只接受已索引的 model ID。
- runtime executable path 只能由管理员配置，必须为 regular executable file。
- load config 是 typed allowlist，禁止 arbitrary args、shell snippet 和任意环境变量。
- 日志和 SQLite 文件采用仅服务用户可读写的权限。

共享 `/mnt/c`、`/mnt/e` 上的权限语义可能弱于 WSL ext4。部署文档会建议把 token、数据库和日志放在 WSL 用户目录，将 `/mnt/...` 仅用于只读模型文件。

## 4. 子进程安全与监管

- 使用 argv 直接执行，不调用 `sh -c`。
- 每个实例使用新的 process group，记录启动 nonce、PID 和 executable identity。
- 子进程绑定 `127.0.0.1`，使用随机私有 API key，避免同机其他进程绕过网关策略。
- 继承最小环境变量集合；明确设置工作目录。
- stdout/stderr 持续消费并写入有界 ring buffer，避免 pipe 填满造成死锁。
- unload 顺序：停止新 lease、drain、SIGTERM、等待、SIGKILL。
- daemon 只向自身创建且 identity 匹配的 process group 发信号。

首版不承诺容器级沙箱。runtime 和模型文件被视为管理员信任的本地内容。

## 5. 网络与 TLS

推荐部署选项，按优先顺序：

1. Tailscale/同类私网访问，daemon 保持 loopback 或可信接口监听。
2. Caddy/Nginx 在 WSL 中终止 TLS，反向代理到 loopback daemon。
3. 可信 LAN 内直接监听 WSL 地址，仍必须启用管理认证和主机防火墙。

若通过 Windows 端口转发或 WSL mirrored networking 暴露端口，需要同时检查 Windows Defender Firewall 和 WSL 内监听地址。服务本身不自动修改 Windows 防火墙或 portproxy。

必须配置受信代理网段后才读取 `X-Forwarded-For` / `Forwarded`；否则使用直接 peer IP，防止伪造审计来源。

## 6. 建议目录

遵循 XDG，允许配置覆盖：

```text
~/.config/model-serving/config.toml
~/.local/share/model-serving/model-serving.db
~/.local/state/model-serving/logs/
~/.local/state/model-serving/instances/
```

示意配置：

```toml
[server]
listen = "127.0.0.1:12340"
trusted_proxies = []

[auth]
inference_required = false

[storage]
database = "/home/user/.local/share/model-serving/model-serving.db"

[[model_roots]]
path = "/mnt/e/models"

[[runtimes]]
id = "llama-default"
kind = "llama_cpp"
executable = "/home/user/llama.cpp/build/bin/llama-server"

[[runtimes]]
id = "ninfer-sm89"
kind = "ninfer"
executable = "/home/user/Workspace/ninfer-4080-32G/build-sm89/apps/ninfer-serve"
```

敏感 token 不写入此普通配置文件；由初始化命令生成并存入数据库哈希，或从权限受控的 secret file/environment 注入。

## 7. 安装与运行形态

首版交付一个 Rust binary 和内嵌/同目录 Web UI assets。支持两种运行方式：

- 前台：`model-serving serve --config ...`，适合开发和诊断。
- systemd user service：适合 WSL 已启用 systemd 的长期运行环境。

systemd unit 应配置 restart-on-failure，但要设置退避；daemon 自身不会无限重启 OOM inference child。安装脚本不安装 inference runtime。

## 8. 数据与隐私

默认记录：model/instance ID、runtime、状态、latency、HTTP status、粗粒度 token usage、时间、request ID。默认不记录：messages、prompt、completion、上传图像、Authorization、cookie、完整请求 JSON。

管理员显式开启 debug payload logging 时必须看到隐私警告、配置最大保留期，并把日志写入单独受限文件；该能力不进入 MVP。

## 9. 运维检查

- `model-serving doctor`：检查数据库目录、runtime executable、model roots、loopback port、`nvidia-smi` 和 CUDA 可见性。
- readiness 不依赖任何模型已加载。
- UI 显示 daemon 版本、runtime probe 结果、GPU snapshot 和数据库 migration version。
- SQLite 采用 WAL、busy timeout 和周期 checkpoint；升级前建议备份数据库。
- schema migration 只向前；降级需使用对应备份。

