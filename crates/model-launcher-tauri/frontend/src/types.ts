export type LaunchSettings = {
  context_length?: number;
  gpu_layers?: number;
  cpu_threads?: number;
  batch_size?: number;
  parallel_slots?: number;
  flash_attention?: boolean;
  kv_cache_type?: "f16" | "q8_0" | "q4_0";
  speculative_type?: "draft-mtp" | "draft-dflash";
};

export type ModelMetadata = {
  architecture?: string;
  parameter_count?: number;
  quantization?: string;
  quantization_version?: number;
  context_length?: number;
  block_count?: number;
  embedding_length?: number;
  attention_head_count?: number;
  attention_head_count_kv?: number;
  attention_key_length?: number;
  attention_value_length?: number;
  full_attention_interval?: number;
};

export type ContextEstimate = {
  model_context_limit: number;
  vram_context_limit?: number;
  recommended_context: number;
  kv_bytes_per_token?: number;
  estimated_weight_bytes: number;
  safety_reserve_bytes: number;
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
  metadata: ModelMetadata;
  contextEstimate?: ContextEstimate;
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
  gpuMemory?: { name: string; total_bytes: number; free_bytes: number };
};

export type LogRecord = {
  timestamp_ms: number;
  source: "application" | "engine_stdout" | "engine_stderr";
  level: "trace" | "debug" | "info" | "warn" | "error";
  generation?: number;
  model_id?: string;
  message: string;
  truncated?: boolean;
};

export type LoadFinished = {
  modelId: string;
  success: boolean;
  message: string;
};
