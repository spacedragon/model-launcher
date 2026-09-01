import { useMemo, useRef, useState } from "react";
import { bridge } from "./bridge";
import type { BenchmarkResult, Model } from "./types";
import "./BenchmarkPage.css";

type RunRow = Partial<BenchmarkResult> & { id: string; number: number; error?: string };

export function BenchmarkPage({ baseUrl, model, authEnabled }: {
  baseUrl: string;
  model?: Model;
  authEnabled: boolean;
}) {
  const [prompt, setPrompt] = useState("Explain in two sentences why local language models are useful.");
  const [runCount, setRunCount] = useState(3);
  const [maxTokens, setMaxTokens] = useState(64);
  const [token, setToken] = useState("");
  const [rows, setRows] = useState<RunRow[]>([]);
  const [phase, setPhase] = useState<"idle" | "running" | "complete" | "cancelled">("idle");
  const activeId = useRef<string | undefined>(undefined);

  const summary = useMemo(() => {
    const successful = rows.filter(row => !row.error && row.latencyMs !== undefined);
    const ttft = successful.filter(row => row.ttftMs !== undefined);
    const throughput = successful.filter(row => row.tokensPerSecond !== undefined);
    const average = (values: number[]) => values.length ? values.reduce((sum, value) => sum + value, 0) / values.length : undefined;
    return {
      latency: average(successful.map(row => row.latencyMs!)),
      ttft: average(ttft.map(row => row.ttftMs!)),
      throughput: average(throughput.map(row => row.tokensPerSecond!)),
      errors: rows.filter(row => row.error).length,
    };
  }, [rows]);

  const start = async () => {
    if (!model || phase === "running") return;
    setRows([]);
    setPhase("running");
    for (let index = 0; index < runCount; index += 1) {
      const id = `${Date.now()}-${index}`;
      activeId.current = id;
      try {
        const result = await bridge.runBenchmark({ id, model: model.key, prompt, maxTokens, token: token.trim() || undefined });
        setRows(current => [...current, { ...result, id, number: index + 1 }]);
      } catch (error) {
        const message = String(error).replace(/^Error:\s*/, "");
        if (message.toLowerCase().includes("cancelled")) {
          setPhase("cancelled");
          activeId.current = undefined;
          return;
        }
        setRows(current => [...current, { id, number: index + 1, error: message }]);
      }
    }
    activeId.current = undefined;
    setPhase("complete");
  };

  const cancel = async () => {
    if (activeId.current) await bridge.cancelBenchmark(activeId.current);
  };

  const buttonLabel = phase === "running" ? "运行中…" : rows.length || phase === "cancelled" ? "重新运行" : "开始基准测试";
  return <section className="view scroll benchmark-page">
    <div className="benchmark-head">
      <div><h1>基准测试</h1><p>通过当前 OpenAI 兼容 API 运行小型、顺序推理测试。</p></div>
      <div className="benchmark-endpoint"><span className="dot run" /><code>{baseUrl}/v1/chat/completions</code></div>
    </div>
    {!model && <div className="notice warn"><b>没有正在运行的模型</b><span>请先从模型库加载一个模型，再运行基准测试。</span></div>}
    <div className="benchmark-layout">
      <div className="benchmark-config">
        <div className="section-title"><h2>测试配置</h2><span className="tag">顺序执行</span></div>
        <label className="field"><span>模型</span><div className="input"><code>{model?.key ?? "未加载"}</code></div></label>
        <label className="field"><span>提示词</span><textarea value={prompt} maxLength={8192} disabled={phase === "running"} onChange={event => setPrompt(event.target.value)} /></label>
        <div className="benchmark-inputs">
          <label className="field"><span>运行次数</span><div className="input"><input aria-label="运行次数" type="number" min="1" max="5" value={runCount} disabled={phase === "running"} onChange={event => setRunCount(clamp(event.target.value, 1, 5))} /></div></label>
          <label className="field"><span>最大输出 Tokens</span><div className="input"><input aria-label="最大输出 Tokens" type="number" min="1" max="512" value={maxTokens} disabled={phase === "running"} onChange={event => setMaxTokens(clamp(event.target.value, 1, 512))} /></div></label>
        </div>
        {authEnabled && <label className="field"><span>Bearer Token <small>仅用于本次页面会话</small></span><div className="input"><input aria-label="Bearer Token" type="password" autoComplete="off" value={token} disabled={phase === "running"} onChange={event => setToken(event.target.value)} placeholder="ml_…" /></div></label>}
        <div className="benchmark-actions">
          <button className="btn primary" disabled={!model || !prompt.trim() || phase === "running"} onClick={() => void start()}>{buttonLabel}</button>
          {phase === "running" && <button className="btn danger" onClick={() => void cancel()}>取消</button>}
          {phase === "cancelled" && <span className="benchmark-state">测试已取消，已完成的结果仍保留。</span>}
        </div>
      </div>
      <div className="benchmark-results">
        <div className="section-title"><h2>结果</h2><span>{rows.length}/{runCount} 次完成</span></div>
        <div className="metric-grid">
          <Metric label="平均延迟" value={formatMs(summary.latency)} />
          <Metric label="平均 TTFT" value={formatMs(summary.ttft)} />
          <Metric label="Token 吞吐" value={summary.throughput === undefined ? "—" : `${summary.throughput.toFixed(1)} tok/s`} />
          <Metric label="错误" value={String(summary.errors)} bad={summary.errors > 0} />
        </div>
        {rows.length === 0 ? <div className="benchmark-empty"><i>◫</i><b>等待测试结果</b><span>TTFT 来自流式响应的首个内容片段。</span></div> : <div className="benchmark-table">
          <div className="benchmark-row header"><span>运行</span><span>延迟</span><span>TTFT</span><span>输出</span><span>吞吐</span></div>
          {rows.map(row => row.error ? <div className="benchmark-row error" key={row.id}><span>#{row.number}</span><span className="error-message" title={row.error}>{row.error}</span></div> : <div className="benchmark-row" key={row.id}>
            <span>#{row.number}</span><span>{formatMs(row.latencyMs)}</span><span>{formatMs(row.ttftMs)}</span>
            <span>{row.completionTokens ?? "—"}{row.tokenCountEstimated ? "~" : ""}</span>
            <span>{row.tokensPerSecond === undefined ? "—" : `${row.tokensPerSecond.toFixed(1)} tok/s`}</span>
          </div>)}
        </div>}
        <p className="benchmark-note">吞吐按首个内容片段到响应结束计算；若 API 未返回 token usage，则以流式内容事件数估算并标记 “~”。</p>
      </div>
    </div>
  </section>;
}

function Metric({ label, value, bad = false }: { label: string; value: string; bad?: boolean }) {
  return <div className={bad ? "bad" : ""}><span>{label}</span><b>{value}</b></div>;
}

function formatMs(value?: number) { return value === undefined ? "—" : `${Math.round(value)} ms`; }
function clamp(value: string, minimum: number, maximum: number) {
  const parsed = Number.parseInt(value, 10);
  return Number.isFinite(parsed) ? Math.min(maximum, Math.max(minimum, parsed)) : minimum;
}
