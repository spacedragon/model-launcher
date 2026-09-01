import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { BenchmarkRequest, BenchmarkResult, Bootstrap, ContextEstimate, LaunchSettings, LogRecord } from "./types";

export const bridge = {
  bootstrap: () => invoke<Bootstrap>("get_bootstrap"),
  chatCompletion: (request: { model: string; messages: Array<{ role: "user" | "assistant"; content: string }>; token?: string }) =>
    invoke<string>("chat_completion", { request }),
  estimateContext: (id: string, settings: LaunchSettings) =>
    invoke<ContextEstimate | undefined>("estimate_model_context", { request: { id, settings } }),
  load: (id: string, key: string, settings: LaunchSettings) =>
    invoke("load_model", { request: { id, key, settings } }),
  eject: () => invoke("eject_model"),
  rescan: () => invoke("rescan_models"),
  logs: (source?: string, minimumLevel?: string) =>
    invoke<LogRecord[]>("get_logs", { query: { source, minimumLevel } }),
  exportLogs: () => invoke("export_logs"),
  saveEngine: (settings: Bootstrap["engineSettings"]) =>
    invoke("save_engine_settings", { settings }),
  saveServer: (settings: Bootstrap["serverSettings"]) =>
    invoke("save_server_settings", { settings }),
  generateToken: () => invoke("generate_token"),
  runBenchmark: (request: BenchmarkRequest) => invoke<BenchmarkResult>("run_benchmark", { request }),
  cancelBenchmark: (id: string) => invoke("cancel_benchmark", { id }),
  minimize: () => invoke("minimize"),
  maximize: () => invoke("toggle_maximize"),
  close: () => invoke("close_window"),
  listen: <T>(event: string, callback: (payload: T) => void): Promise<UnlistenFn> =>
    listen<T>(event, ({ payload }) => callback(payload)),
};
