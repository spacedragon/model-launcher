import { useCallback, useEffect, useMemo, useState } from "react";
import { bridge } from "./bridge";
import ChatPage from "./ChatPage";
import type { Bootstrap, ContextEstimate, LaunchSettings, LogRecord, Model } from "./types";

type Page = "models" | "chat" | "api" | "logs" | "settings" | "detail";
const nav: Array<[Page, string, string]> = [
  ["models", "模型库", "◆"], ["chat", "Chat", "✦"], ["api", "API 服务", "▦"],
  ["logs", "日志与诊断", "⌁"], ["settings", "设置", "⚙"],
];

export default function App() {
  const [data, setData] = useState<Bootstrap>();
  const [page, setPage] = useState<Page>("models");
  const [selectedId, setSelectedId] = useState<string>();
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState("all");
  const [status, setStatus] = useState("");
  const [token, setToken] = useState<string>();
  const [loading, setLoading] = useState(false);

  const refresh = useCallback(async () => {
    const next = await bridge.bootstrap();
    setData(next);
    setSelectedId(current => current ?? next.models.find(model => model.running)?.id ?? next.models[0]?.id);
  }, []);

  useEffect(() => {
    void refresh();
    const removers: Array<() => void> = [];
    void bridge.listen("state-changed", () => void refresh()).then(fn => removers.push(fn));
    void bridge.listen<string>("operation-status", message => setStatus(message)).then(fn => removers.push(fn));
    void bridge.listen<string>("token-generated", value => setToken(value)).then(fn => removers.push(fn));
    void bridge.listen<string>("close-notice", message => setStatus(message)).then(fn => removers.push(fn));
    return () => removers.forEach(remove => remove());
  }, [refresh]);

  const selected = data?.models.find(model => model.id === selectedId);
  const models = useMemo(() => (data?.models ?? []).filter(model => {
    const text = `${model.name} ${model.key} ${model.path}`.toLowerCase();
    const matchesText = text.includes(query.trim().toLowerCase());
    const matchesState = filter === "all" || (filter === "running" ? model.running : model.state === filter);
    return matchesText && matchesState;
  }), [data, query, filter]);

  const run = async (action: () => Promise<unknown>) => {
    setLoading(true);
    try { await action(); await refresh(); }
    catch (error) { setStatus(String(error)); }
    finally { setLoading(false); }
  };

  if (!data) return <div className="boot"><i className="spinner" />正在连接 Model Launcher…</div>;
  const activePage = page === "detail" ? "models" : page;

  return <div className="app">
    <header className="titlebar" data-tauri-drag-region>
      <i className="mark" /><b data-tauri-drag-region>Model Launcher</b><span className="version">v0.2.0</span>
      <div className="drag" data-tauri-drag-region />
      <span className={`service-state ${data.engineValid ? "ok" : "bad"}`}><i className="dot" />{data.engineValid ? "引擎就绪" : "引擎不可用"}</span>
      <div className="window-controls">
        <button aria-label="最小化" onClick={() => void bridge.minimize()}>—</button>
        <button aria-label="最大化" onClick={() => void bridge.maximize()}>□</button>
        <button aria-label="关闭" onClick={() => void bridge.close()}>×</button>
      </div>
    </header>

    <div className={`body ${page === "chat" || page === "logs" || page === "settings" ? "no-aside" : ""}`}>
      <nav className="sidebar" aria-label="主导航">
        <span className="kicker">导航</span>
        {nav.map(([id, label, icon]) => <button key={id} aria-current={activePage === id} onClick={() => setPage(id)}>
          <i>{icon}</i><span>{label}</span>{id === "models" && <em>{data.models.length}</em>}
        </button>)}
        <div className="grow" />
        <div className="service-card"><div><i className="dot run" />API 服务运行中</div><code>{data.baseUrl.replace("http://", "")}</code><small>{data.authenticationStatus}</small></div>
      </nav>

      <main>
        {page === "models" && <ModelsPage data={data} models={models} query={query} setQuery={setQuery} filter={filter} setFilter={setFilter}
          select={model => { setSelectedId(model.id); }} rescan={() => void run(() => bridge.rescan())}
          load={model => { setSelectedId(model.id); setPage("detail"); }}
          eject={() => void run(bridge.eject)} loading={loading} />}
        {page === "detail" && selected && <DetailPage model={selected} data={data} back={() => setPage("models")}
          save={(key, settings) => void run(() => bridge.load(selected.id, key, settings))} />}
        {page === "chat" && <ChatPage baseUrl={data.baseUrl} model={data.models.find(model => model.running)?.key} authenticationEnabled={data.serverSettings.auth_enabled} complete={bridge.chatCompletion} />}
        {page === "api" && <ApiPage data={data} generate={() => void bridge.generateToken()} />}
        {page === "logs" && <LogsPage exportLogs={() => void bridge.exportLogs()} />}
        {page === "settings" && <SettingsPage data={data} saveEngine={settings => void run(() => bridge.saveEngine(settings))}
          saveServer={settings => void run(() => bridge.saveServer(settings))} />}
      </main>

      {page !== "chat" && page !== "logs" && page !== "settings" && <aside>
        {page === "detail" ? <CapabilityPanel data={data} /> : <ContextPanel data={data} selected={selected}
          details={() => setPage("detail")} eject={() => void run(bridge.eject)} load={model => { setSelectedId(model.id); setPage("detail"); }} />}
      </aside>}
    </div>

    <footer><span>WSL: {data.engineSettings.distribution}</span><span>{data.engineSettings.executable}</span><div className="grow" /><span><i className="dot run" />{status || `${data.lifecycle.inFlight} 个活动请求`}</span></footer>
    {token && <Modal title="新的 API Token 已生成" close={() => setToken(undefined)}><div className="notice warn">此 Token 只显示一次，关闭后无法再次查看。</div><div className="token"><code>{token}</code><button className="tag blue" onClick={() => void navigator.clipboard.writeText(token)}>复制</button></div><p>配置文件只保存 Argon2id 哈希，不保存明文。</p></Modal>}
  </div>;
}

function ModelsPage({ data, models, query, setQuery, filter, setFilter, select, rescan, load, eject, loading }: {
  data: Bootstrap; models: Model[]; query: string; setQuery(v: string): void; filter: string; setFilter(v: string): void;
  select(m: Model): void; rescan(): void; load(m: Model): void; eject(): void; loading: boolean;
}) {
  const counts = { all: data.models.length, ready: data.models.filter(m => m.state === "ready").length, running: data.models.filter(m => m.running).length, missing: data.models.filter(m => m.state === "missing").length };
  return <section className="view">
    <div className="pagehead"><div className="headrow"><div><h1>模型库</h1><p><code>{data.engineSettings.model_directory || "尚未配置模型目录"}</code> · {data.models.length} 个模型</p></div><div className="grow" />
      <label className="search">⌕<input value={query} onChange={e => setQuery(e.target.value)} placeholder="搜索模型名称、API 名称或路径" /></label>
      <button className="btn" onClick={rescan} disabled={loading}>重新扫描</button></div>
      <div className="chips">{Object.entries(counts).map(([id, count]) => <button key={id} aria-pressed={filter === id} onClick={() => setFilter(id)}>{({all:"全部",ready:"已就绪",running:"运行中",missing:"文件丢失"} as Record<string,string>)[id]} {count}</button>)}</div>
    </div>
    <div className="model-list scroll">{models.length === 0 ? <Empty title="没有匹配的模型" text="调整筛选条件，或检查模型目录后重新扫描。" /> : models.map(model => <article key={model.id} className={`model-row ${model.running ? "running" : ""}`} onClick={() => select(model)}>
      <i className={`dot ${model.running ? "run" : model.state === "missing" ? "bad" : ""}`} />
      <div className="model-title"><b>{model.name}</b><code>{model.fileName}</code></div>
      <div className="model-meta"><span className="tag">{model.key}</span><span>{model.size}</span></div>
      <span className={`tag ${model.running ? "green" : model.state === "missing" ? "red" : ""}`}>{model.running ? data.lifecycle.state : model.state}</span>
      {model.running ? <button className="btn danger" onClick={e => { e.stopPropagation(); eject(); }}>卸载</button> : <button className="btn" disabled={loading || model.state !== "ready" || !data.engineValid || data.lifecycle.inFlight > 0} onClick={e => { e.stopPropagation(); load(model); }}>配置并加载</button>}
    </article>)}</div>
  </section>;
}

function ContextPanel({ data, selected, details, eject, load }: { data: Bootstrap; selected?: Model; details(): void; eject(): void; load(m: Model): void }) {
  const running = data.models.find(model => model.running);
  return <div className="panel"><span className="panel-label">当前运行</span>{running ? <div className="card"><h3><i className="dot run" />{running.name}</h3><span className="tag">{running.key}</span><dl><Kv k="上下文长度" v={String(running.settings.context_length ?? "默认")} /><Kv k="GPU 卸载层" v={String(running.settings.gpu_layers ?? "自动")} /><Kv k="并行槽位" v={String(running.settings.parallel_slots ?? "默认")} /><Kv k="活动请求" v={String(data.lifecycle.inFlight)} /></dl><div className="actions"><button className="btn danger" onClick={eject}>卸载模型</button><button className="btn" onClick={details}>运行参数</button></div></div> : <Empty title="未加载模型" text="选择一个可用模型开始本地推理。" />}
    {data.lifecycle.inFlight > 0 && <div className="notice warn"><b>{data.lifecycle.inFlight} 个请求推理中</b><span>模型切换已锁定，其他模型请求会返回 model_busy。</span></div>}
    <span className="panel-label">选中模型</span>{selected && !selected.running && <div className="card compact"><b>{selected.name}</b><code>{selected.path}</code><button className="btn primary" disabled={selected.state !== "ready"} onClick={() => load(selected)}>查看参数并加载</button></div>}
  </div>;
}

function DetailPage({ model, data, back, save }: { model: Model; data: Bootstrap; back(): void; save(key: string, settings: LaunchSettings): void }) {
  const [key, setKey] = useState(model.key);
  const [settings, setSettings] = useState<LaunchSettings>(() => {
    const initial = { ...data.engineSettings.defaults, ...model.settings };
    if (!initial.context_length) initial.context_length = model.contextEstimate?.recommended_context ?? model.metadata.context_length ?? 32768;
    return initial;
  });
  const [estimate, setEstimate] = useState<ContextEstimate | undefined>(model.contextEstimate);
  useEffect(() => {
    let cancelled = false;
    void bridge.estimateContext(model.id, settings).then(next => { if (!cancelled && next) setEstimate(next); });
    return () => { cancelled = true; };
  }, [model.id, settings.gpu_layers, settings.kv_cache_type]);
  const setNumber = (name: keyof LaunchSettings, value: string) => setSettings(current => ({ ...current, [name]: value ? Number(value) : undefined }));
  const dflashSelected = settings.speculative_type === "draft-dflash";
  const draftModels = data.models.filter(candidate => candidate.id !== model.id && candidate.state === "ready");
  const draftModelValid = !dflashSelected || draftModels.some(candidate => candidate.path === settings.draft_model);
  return <section className="view"><div className="detail-head"><div><button className="crumb" onClick={back}>← 模型库</button><h1>{model.name} <span className={`tag ${model.running ? "green" : ""}`}>{model.running ? "运行中" : "模型级参数"}</span></h1></div><div className="grow" /><button className="btn" onClick={() => setSettings(data.engineSettings.defaults)}>恢复全局默认</button><button className="btn primary" disabled={!draftModelValid} onClick={() => save(key, settings)}>保存并{model.running ? "重启" : "加载"}</button></div>
    <div className="params scroll"><div className="model-facts">
      <Kv k="架构" v={model.metadata.architecture ?? "未知"} />
      <Kv k="参数量" v={formatParameters(model.metadata.parameter_count)} />
      <Kv k="量化" v={model.metadata.quantization ?? "未知"} />
      <Kv k="模型大小" v={model.size} />
      <Kv k="原生 Context" v={model.metadata.context_length ? `${model.metadata.context_length.toLocaleString()} tokens` : "未声明"} />
    </div><div className="field full"><label>API 名称</label><div className="input"><input value={key} onChange={e => setKey(e.target.value)} /></div><small>用于 OpenAI 和 LM Studio 兼容接口中的 model 字段。</small></div><div className="form-grid">
      <ContextSlider value={settings.context_length ?? estimate?.recommended_context ?? 32768} estimate={estimate} gpu={data.gpuMemory} onChange={value => setSettings(s => ({ ...s, context_length: value }))} />
      <Field label="GPU 卸载层数" flag="--n-gpu-layers" value={settings.gpu_layers} onChange={v => setNumber("gpu_layers", v)} />
      <Field label="CPU 线程数" flag="--threads" value={settings.cpu_threads} onChange={v => setNumber("cpu_threads", v)} />
      <Field label="批处理大小" flag="--batch-size" value={settings.batch_size} onChange={v => setNumber("batch_size", v)} />
      <Field label="并行槽位" flag="--parallel" value={settings.parallel_slots} onChange={v => setNumber("parallel_slots", v)} />
      <div className="field"><label>KV Cache 类型 <span className="tag">--cache-type-k / -v</span></label><div className="input"><select value={settings.kv_cache_type ?? "f16"} onChange={e => setSettings(s => ({...s, kv_cache_type: e.target.value as LaunchSettings["kv_cache_type"]}))}><option>f16</option><option>q8_0</option><option>q4_0</option></select></div></div>
      <div className="field"><label>投机解码 <span className="tag">--spec-type</span></label><div className="input"><select aria-label="投机解码" disabled={!data.capabilities.speculative_type} value={settings.speculative_type ?? ""} onChange={e => setSettings(s => ({...s, speculative_type: (e.target.value || undefined) as LaunchSettings["speculative_type"], draft_model: e.target.value === "draft-dflash" ? s.draft_model : undefined}))}><option value="">关闭</option><option value="draft-mtp">MTP</option><option value="draft-dflash" disabled={!data.capabilities.draft_model}>DFlash 2</option></select></div><small>{data.capabilities.speculative_type ? "仅为带有兼容预测头的模型启用。" : "当前 llama-server 不支持 --spec-type。"}</small></div>
      {dflashSelected && <div className="field full"><label>DFlash 2 草稿模型 <span className="tag">--spec-draft-model</span></label><div className="input"><select aria-label="DFlash 2 草稿模型" value={settings.draft_model ?? ""} onChange={e => setSettings(s => ({...s, draft_model: e.target.value || undefined}))}><option value="">请选择单独的草稿模型</option>{draftModels.map(candidate => <option key={candidate.id} value={candidate.path}>{candidate.name} · {candidate.fileName}</option>)}</select></div><small>{draftModels.length ? "草稿模型必须与当前目标模型匹配。" : "模型库中没有其他可用的 GGUF 模型。"}</small></div>}
      <Toggle label="Flash Attention" checked={!!settings.flash_attention} onChange={checked => setSettings(s => ({...s, flash_attention: checked}))} />
    </div><div className={`notice ${estimate?.vram_context_limit && settings.context_length && settings.context_length > estimate.vram_context_limit ? "warn" : "blue"}`}>{contextEstimateMessage(estimate, data.gpuMemory, settings.context_length)}</div><div className="notice blue">确认参数后才会启动模型；对外 API 地址 {data.baseUrl} 保持不变。</div></div>
  </section>;
}

function CapabilityPanel({ data }: { data: Bootstrap }) { return <div className="panel"><span className="panel-label">引擎能力探测</span><div className="card"><h3><i className={`dot ${data.engineValid ? "run" : "bad"}`} />{data.engineValid ? "llama-server 可用" : "配置无效"}</h3><code>{data.engineSettings.executable}</code><small>{data.engineDiagnostic || "通过 --help 动态探测可用参数"}</small></div><div className="cap-list">{Object.entries(data.capabilities).map(([name, enabled]) => <div key={name} className={enabled ? "enabled" : ""}><b>{enabled ? "✓" : "×"}</b><code>{name}</code><span>{enabled ? "可用" : "已隐藏"}</span></div>)}</div></div> }

function ApiPage({ data, generate }: { data: Bootstrap; generate(): void }) { return <section className="view scroll"><div className="api-page"><h1>API 服务</h1><p>模型重启或切换时，对外地址保持不变。</p><div className="hero"><div><i className="dot run" /><code>{data.baseUrl}</code><button className="tag blue" onClick={() => void navigator.clipboard.writeText(data.baseUrl)}>复制</button><div className="grow" /><span>{data.authenticationStatus}</span></div><dl><Kv k="监听地址" v={data.serverSettings.bind_address} /><Kv k="端口" v={String(data.serverSettings.port)} /><Kv k="活动请求" v={String(data.lifecycle.inFlight)} /></dl></div><RouteGroup title="OpenAI 兼容接口" routes={[["GET","/v1/models"],["POST","/v1/chat/completions"],["POST","/v1/completions"]]} /><RouteGroup title="LM Studio 兼容接口" routes={[["GET","/api/v1/models"],["POST","/api/v1/models/load"],["POST","/api/v1/models/unload"]]} /><div className="notice warn"><b>Bearer Token</b><span>明文只在生成时显示一次，配置仅保存 Argon2id 哈希。</span><button className="btn danger" onClick={generate}>重新生成 Token</button></div></div></section> }

function LogsPage({ exportLogs }: { exportLogs(): void }) { const [logs, setLogs] = useState<LogRecord[]>([]); const [source, setSource] = useState<string>(); const [level, setLevel] = useState("info"); const [query, setQuery] = useState(""); useEffect(() => { const refresh = () => void bridge.logs(source, level).then(setLogs); refresh(); const id = window.setInterval(refresh, 1200); return () => clearInterval(id); }, [source, level]); const shown = logs.filter(log => log.message.toLowerCase().includes(query.toLowerCase())); return <section className="view logs-page"><div className="headrow"><div><h1>日志与诊断</h1><p>应用日志、引擎 stdout 与 stderr 的统一视图</p></div><div className="grow" /><button className="btn" onClick={() => void navigator.clipboard.writeText(shown.map(l => l.message).join("\n"))}>复制当前日志</button><button className="btn primary" onClick={exportLogs}>导出诊断日志</button></div><div className="log-toolbar"><select value={source ?? ""} onChange={e => setSource(e.target.value || undefined)}><option value="">全部来源</option><option value="application">应用</option><option value="engine_stdout">引擎 stdout</option><option value="engine_stderr">引擎 stderr</option></select><select value={level} onChange={e => setLevel(e.target.value)}><option value="trace">TRACE</option><option value="debug">DEBUG</option><option value="info">INFO</option><option value="warn">WARN</option><option value="error">ERROR</option></select><label className="search"><input value={query} onChange={e => setQuery(e.target.value)} placeholder="过滤日志内容" /></label><div className="grow" /><code>{shown.length} 行</code></div><div className="log-stream">{shown.map((log, i) => <div key={`${log.timestamp_ms}-${i}`} className={log.level}><time>{new Date(log.timestamp_ms).toLocaleTimeString()}</time><b>{log.level.toUpperCase()}</b><span>{log.source}</span><code>{log.message}</code></div>)}</div></section> }

function SettingsPage({ data, saveEngine, saveServer }: { data: Bootstrap; saveEngine(v: Bootstrap["engineSettings"]): void; saveServer(v: Bootstrap["serverSettings"]): void }) { const [engine, setEngine] = useState(data.engineSettings); const [server, setServer] = useState(data.serverSettings); return <section className="view scroll"><div className="settings-page"><div><h1>设置</h1><p>登记模型目录、WSL 引擎位置与默认启动参数。</p></div><SettingsSection title="模型目录" description="递归扫描 .gguf 文件并自动识别分片模型。"><div className="directory"><i className="dot run" /><code>{engine.model_directory || "尚未配置"}</code></div><label>模型目录<input value={engine.model_directory} onChange={e => setEngine({...engine, model_directory:e.target.value})} /></label></SettingsSection><SettingsSection title="WSL 与推理引擎" description="Windows 路径会在启动模型时转换为 WSL 路径。"><div className="settings-row"><label>WSL 发行版<input value={engine.distribution} onChange={e => setEngine({...engine, distribution:e.target.value})} /></label><label className="wide">llama-server 路径<input value={engine.executable} onChange={e => setEngine({...engine, executable:e.target.value})} /></label><button className="btn primary" onClick={() => saveEngine(engine)}>保存并验证</button></div></SettingsSection><SettingsSection title="API 服务" description="非本地监听且未启用认证会产生安全警告。"><div className="settings-row"><label>监听地址<input value={server.bind_address} onChange={e => setServer({...server, bind_address:e.target.value})} /></label><label>端口<input type="number" value={server.port} onChange={e => setServer({...server, port:Number(e.target.value)})} /></label><Toggle label="Bearer Token 认证" checked={server.auth_enabled} onChange={auth_enabled => setServer({...server,auth_enabled})} /><button className="btn primary" onClick={() => saveServer(server)}>保存服务设置</button></div>{data.serverWarning && <div className="notice warn">{data.serverWarning}</div>}</SettingsSection><SettingsSection title="持久化与后台运行" description="配置使用版本化 JSON 和原子替换写入。"><div className="settings-list"><Toggle label="关闭主窗口后驻留系统托盘" checked onChange={() => undefined} /><Toggle label="退出时停止 API 服务与受管模型" checked onChange={() => undefined} /><Toggle label="启动时恢复上次模型（默认关闭）" checked={false} onChange={() => undefined} /></div></SettingsSection></div></section> }

function ContextSlider({ value, estimate, gpu, onChange }: { value: number; estimate?: ContextEstimate; gpu?: Bootstrap["gpuMemory"]; onChange(v:number):void }) {
  const hardMax = estimate?.model_context_limit ?? 32768;
  const recommended = estimate?.recommended_context ?? Math.min(hardMax, 32768);
  return <div className="field full context-slider"><label>上下文长度 <span className="tag">--ctx-size</span></label><div className="range-head"><b>{value.toLocaleString()} tokens</b><span>推荐 {recommended.toLocaleString()} · 模型上限 {hardMax.toLocaleString()}</span></div><input aria-label="上下文长度" type="range" min="512" max={hardMax} step="512" value={Math.min(value, hardMax)} onChange={e => onChange(Number(e.target.value))} /><div className="range-foot"><span>512</span><button type="button" className="tag blue" onClick={() => onChange(recommended)}>使用推荐值</button><span>{hardMax.toLocaleString()}</span></div>{gpu && <small>显存估算值只作为推荐和警告，不再锁死 slider；模型声明的 context 才是硬上限。</small>}</div>;
}

function contextEstimateMessage(estimate?: ContextEstimate, gpu?: Bootstrap["gpuMemory"], selected?: number) {
  if (!gpu) return "未检测到 NVIDIA 显存；可调整到模型声明的上限，但请留意加载日志。";
  if (!estimate?.kv_bytes_per_token) return `${gpu.name} 当前可用 ${formatBytes(gpu.free_bytes)}；GGUF 缺少完整的 KV 维度，无法可靠估算显存。`;
  const vramLimit = estimate.vram_context_limit ?? estimate.model_context_limit;
  const selectedWarning = selected && selected > vramLimit ? " 当前选择超过显存估算值，仍可尝试加载，但可能回退到系统内存或加载失败。" : "";
  return `${gpu.name} 当前可用 ${formatBytes(gpu.free_bytes)} / ${formatBytes(gpu.total_bytes)}；KV Cache 约 ${formatBytes(estimate.kv_bytes_per_token * 1024)} / 1K tokens，显存估算 ${vramLimit.toLocaleString()} tokens。${selectedWarning}`;
}

function formatParameters(value?: number) { if (!value) return "未知"; return value >= 1e9 ? `${(value / 1e9).toFixed(1)}B` : value >= 1e6 ? `${(value / 1e6).toFixed(1)}M` : value.toLocaleString(); }
function formatBytes(value: number) { return `${(value / 1024 ** 3).toFixed(1)} GiB`; }

function Field({ label, flag, value, onChange, suffix }: { label: string; flag: string; value?: number; onChange(v:string):void; suffix?:string }) { return <div className="field"><label>{label} <span className="tag">{flag}</span></label><div className="input"><input type="number" min="0" value={value ?? ""} onChange={e => onChange(e.target.value)} />{suffix && <small>{suffix}</small>}</div></div> }
function Toggle({ label, checked, onChange }: { label:string; checked:boolean; onChange(v:boolean):void }) { return <label className="toggle-row"><span>{label}</span><button type="button" className="toggle" aria-pressed={checked} onClick={() => onChange(!checked)}><i /></button></label> }
function Kv({ k, v }: { k:string; v:string }) { return <div><dt>{k}</dt><dd>{v}</dd></div> }
function Empty({ title, text }: { title:string; text:string }) { return <div className="empty"><i>◇</i><b>{title}</b><span>{text}</span></div> }
function RouteGroup({ title, routes }: { title:string; routes:string[][] }) { return <div className="route-group"><h2>{title}</h2><div>{routes.map(([method,path]) => <p key={path}><span className={`tag ${method === "GET" ? "green" : "blue"}`}>{method}</span><code>{path}</code></p>)}</div></div> }
function SettingsSection({ title, description, children }: React.PropsWithChildren<{title:string;description:string}>) { return <section className="settings-section"><h2>{title}</h2><p>{description}</p>{children}</section> }
function Modal({ title, close, children }: React.PropsWithChildren<{title:string;close():void}>) { return <div className="scrim" role="presentation" onMouseDown={e => e.target === e.currentTarget && close()}><section className="modal" role="dialog" aria-modal="true"><h2>{title}</h2>{children}<div className="modal-actions"><button className="btn primary" onClick={close}>我已保存</button></div></section></div> }
