export type LaunchSettings = {
  context_length?: number;
  gpu_layers?: number;
  cpu_threads?: number;
  batch_size?: number;
  parallel_slots?: number;
  flash_attention?: boolean;
  kv_cache_type?: "f16" | "q8_0" | "q4_0";
};

export type Model = {
  id: string;
  key: string;
  name: string;
  path: string;
  fileName: string;
  sizeBytes: number;
  size: string;
  state: "ready" | "missing" | "unlaunchable";
  running: boolean;
  settings: LaunchSettings;
};

export type Bootstrap = {
  models: Model[];
  recentModels: Model[];
  lifecycle: { state: string; desiredModel?: string; inFlight: number; diagnostic?: string };
  capabilities: Record<string, boolean>;
  authenticationStatus: string;
  serverWarning: string;
  engineValid: boolean;
  engineDiagnostic?: string;
  configurationDiagnostic?: string;
  engineSettings: {
    distribution: string;
    executable: string;
    model_directory: string;
    defaults: LaunchSettings;
  };
  serverSettings: { bind_address: string; port: number; auth_enabled: boolean };
  baseUrl: string;
};

export type LogRecord = {
  timestamp_ms: number;
  source: "application" | "engine_stdout" | "engine_stderr";
  level: "trace" | "debug" | "info" | "warn" | "error";
  message: string;
};
