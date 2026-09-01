use model_launcher_core::{
    CatalogMetadata, ContextEstimate, EngineCapabilities, LaunchSettings, LifecycleSnapshot,
    LogFilter, LogLevel, LogRecord, LogSource, ModelId, ModelRecord, ModelState, estimate_context,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, OnceLock, RwLock},
};
use tauri::{Emitter, Manager, WebviewWindow};

#[derive(Clone, Debug)]
pub struct AppSnapshot {
    pub models: Vec<ModelRecord>,
    pub recent_models: Vec<ModelRecord>,
    pub lifecycle: LifecycleSnapshot,
    pub capabilities: EngineCapabilities,
    pub authentication_status: String,
    pub server_warning: String,
    pub engine_valid: bool,
    pub engine_diagnostic: Option<String>,
    pub configuration_diagnostic: Option<String>,
    pub gpu_memory: Option<GpuMemoryInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuMemoryInfo {
    pub name: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Clone)]
pub struct UiActions {
    pub load: Arc<dyn Fn(UiLoadRequest) + Send + Sync>,
    pub eject: Arc<dyn Fn() + Send + Sync>,
    pub rescan: Arc<dyn Fn() + Send + Sync>,
    pub snapshot: Arc<dyn Fn() -> AppSnapshot + Send + Sync>,
    pub quit: Arc<dyn Fn() + Send + Sync>,
    pub close_notice: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    pub save_settings: Arc<dyn Fn(EngineSettings) + Send + Sync>,
    pub logs: Arc<dyn Fn(LogFilter) -> Vec<LogRecord> + Send + Sync>,
    pub export_logs: Arc<dyn Fn() + Send + Sync>,
    pub generate_token: Arc<dyn Fn() + Send + Sync>,
    pub engine_settings: Arc<dyn Fn() -> EngineSettings + Send + Sync>,
    pub server_settings: Arc<dyn Fn() -> UiServerSettings + Send + Sync>,
    pub save_server_settings: Arc<dyn Fn(UiServerSettings) + Send + Sync>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiLoadRequest {
    pub id: ModelId,
    pub key: String,
    pub settings: LaunchSettings,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSettings {
    pub distribution: String,
    pub executable: String,
    pub model_directory: String,
    pub defaults: LaunchSettings,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiServerSettings {
    pub bind_address: String,
    pub port: u16,
    pub auth_enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    models: Vec<ModelDto>,
    recent_models: Vec<ModelDto>,
    lifecycle: LifecycleDto,
    capabilities: EngineCapabilities,
    authentication_status: String,
    server_warning: String,
    engine_valid: bool,
    engine_diagnostic: Option<String>,
    configuration_diagnostic: Option<String>,
    engine_settings: EngineSettings,
    server_settings: UiServerSettings,
    base_url: String,
    gpu_memory: Option<GpuMemoryInfo>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelDto {
    id: ModelId,
    key: String,
    name: String,
    path: String,
    file_name: String,
    size_bytes: u64,
    size: String,
    state: String,
    running: bool,
    settings: LaunchSettings,
    metadata: CatalogMetadata,
    context_estimate: Option<ContextEstimate>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LifecycleDto {
    state: String,
    desired_model: Option<ModelId>,
    in_flight: usize,
    diagnostic: Option<String>,
}

struct DesktopState {
    address: String,
    actions: UiActions,
    http: reqwest::Client,
}

fn bootstrap(state: &DesktopState) -> Bootstrap {
    let snapshot = (state.actions.snapshot)();
    let running = snapshot.lifecycle.desired_model;
    let convert = |model: &ModelRecord| ModelDto {
        id: model.id,
        key: model.key.to_string(),
        name: model.display_name.clone(),
        path: model.path.display().to_string(),
        file_name: model
            .path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        size_bytes: model.size_bytes,
        size: format_bytes(model.size_bytes),
        state: match &model.state {
            ModelState::Available => "ready".into(),
            ModelState::Missing => "missing".into(),
            ModelState::Unlaunchable { .. } => "unlaunchable".into(),
        },
        running: running == Some(model.id),
        settings: model.launch_profile.settings.clone(),
        metadata: model.metadata.clone(),
        context_estimate: snapshot.gpu_memory.as_ref().map(|gpu| {
            estimate_context(
                &model.metadata,
                model.size_bytes,
                &model.launch_profile.settings,
                gpu.total_bytes,
                gpu.free_bytes,
            )
        }),
    };
    Bootstrap {
        models: snapshot.models.iter().map(convert).collect(),
        recent_models: snapshot.recent_models.iter().map(convert).collect(),
        lifecycle: LifecycleDto {
            state: format!("{:?}", snapshot.lifecycle.state).to_lowercase(),
            desired_model: snapshot.lifecycle.desired_model,
            in_flight: snapshot.lifecycle.in_flight,
            diagnostic: snapshot.lifecycle.diagnostic,
        },
        capabilities: snapshot.capabilities,
        authentication_status: snapshot.authentication_status,
        server_warning: snapshot.server_warning,
        engine_valid: snapshot.engine_valid,
        engine_diagnostic: snapshot.engine_diagnostic,
        configuration_diagnostic: snapshot.configuration_diagnostic,
        engine_settings: (state.actions.engine_settings)(),
        server_settings: (state.actions.server_settings)(),
        base_url: format!("http://{}", state.address),
        gpu_memory: snapshot.gpu_memory,
    }
}

#[tauri::command]
fn get_bootstrap(state: tauri::State<'_, DesktopState>) -> Bootstrap {
    bootstrap(&state)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    token: Option<String>,
}

#[derive(Serialize)]
struct OpenAiChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
}

#[tauri::command]
async fn chat_completion(
    state: tauri::State<'_, DesktopState>,
    request: ChatCompletionRequest,
) -> Result<String, String> {
    post_chat_completion(&state.http, &state.address, request).await
}

async fn post_chat_completion(
    client: &reqwest::Client,
    address: &str,
    request: ChatCompletionRequest,
) -> Result<String, String> {
    let url = format!("http://{address}/v1/chat/completions");
    let body = OpenAiChatRequest {
        model: &request.model,
        messages: &request.messages,
        stream: false,
    };
    let mut outgoing = client.post(&url).json(&body);
    if let Some(token) = request.token.as_deref().filter(|token| !token.is_empty()) {
        outgoing = outgoing.bearer_auth(token);
    }
    let response = outgoing.send().await.map_err(|_| {
        format!(
            "Could not reach the local API at http://{address}. Make sure Model Launcher is running and try again."
        )
    })?;
    let status = response.status();
    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "The local API returned an invalid response.".to_owned())?;
    if !status.is_success() {
        let detail = payload
            .pointer("/error/message")
            .or_else(|| payload.get("error"))
            .and_then(serde_json::Value::as_str);
        return Err(detail.map_or_else(
            || match status.as_u16() {
                401 => "The API token was rejected. Paste a valid Bearer token and try again.".to_owned(),
                503 => "The local model server is unavailable. Check that the model is running and try again.".to_owned(),
                code => format!("The local API returned HTTP {code}."),
            },
            str::to_owned,
        ));
    }
    payload
        .pointer("/choices/0/message/content")
        .and_then(serde_json::Value::as_str)
        .filter(|content| !content.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "The server returned an empty assistant response.".to_owned())
}

#[derive(Clone, Debug, Deserialize)]
struct ContextEstimateRequest {
    id: ModelId,
    settings: LaunchSettings,
}

#[tauri::command]
fn estimate_model_context(
    state: tauri::State<'_, DesktopState>,
    request: ContextEstimateRequest,
) -> Option<ContextEstimate> {
    let snapshot = (state.actions.snapshot)();
    let gpu = snapshot.gpu_memory?;
    let model = snapshot
        .models
        .iter()
        .find(|model| model.id == request.id)?;
    Some(estimate_context(
        &model.metadata,
        model.size_bytes,
        &request.settings,
        gpu.total_bytes,
        gpu.free_bytes,
    ))
}

#[tauri::command]
fn load_model(state: tauri::State<'_, DesktopState>, request: UiLoadRequest) {
    (state.actions.load)(request);
}

#[tauri::command]
fn eject_model(state: tauri::State<'_, DesktopState>) {
    (state.actions.eject)();
}

#[tauri::command]
fn rescan_models(state: tauri::State<'_, DesktopState>) {
    (state.actions.rescan)();
}

#[tauri::command]
fn save_engine_settings(state: tauri::State<'_, DesktopState>, settings: EngineSettings) {
    (state.actions.save_settings)(settings);
}

#[tauri::command]
fn save_server_settings(
    state: tauri::State<'_, DesktopState>,
    settings: UiServerSettings,
) -> Result<(), String> {
    parse_server_settings(
        settings.bind_address.clone(),
        i32::from(settings.port),
        settings.auth_enabled,
    )?;
    (state.actions.save_server_settings)(settings);
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogQuery {
    source: Option<String>,
    minimum_level: Option<String>,
}

#[tauri::command]
fn get_logs(state: tauri::State<'_, DesktopState>, query: LogQuery) -> Vec<LogRecord> {
    let source = match query.source.as_deref() {
        Some("application") => Some(LogSource::Application),
        Some("engine_stdout") => Some(LogSource::EngineStdout),
        Some("engine_stderr") => Some(LogSource::EngineStderr),
        _ => None,
    };
    let minimum_level = match query.minimum_level.as_deref() {
        Some("trace") => Some(LogLevel::Trace),
        Some("debug") => Some(LogLevel::Debug),
        Some("info") => Some(LogLevel::Info),
        Some("warn") => Some(LogLevel::Warn),
        Some("error") => Some(LogLevel::Error),
        _ => None,
    };
    (state.actions.logs)(LogFilter {
        source,
        minimum_level,
    })
}

#[tauri::command]
fn export_logs(state: tauri::State<'_, DesktopState>) {
    (state.actions.export_logs)();
}

#[tauri::command]
fn generate_token(state: tauri::State<'_, DesktopState>) {
    (state.actions.generate_token)();
}

#[tauri::command]
fn minimize(window: WebviewWindow) -> Result<(), String> {
    window.minimize().map_err(|error| error.to_string())
}

#[tauri::command]
fn toggle_maximize(window: WebviewWindow) -> Result<(), String> {
    let maximized = window.is_maximized().map_err(|error| error.to_string())?;
    if maximized {
        window.unmaximize()
    } else {
        window.maximize()
    }
    .map_err(|error| error.to_string())
}

#[tauri::command]
fn close_window(window: WebviewWindow) -> Result<(), String> {
    window.close().map_err(|error| error.to_string())
}

static APP: OnceLock<RwLock<Option<tauri::AppHandle>>> = OnceLock::new();

fn app_slot() -> &'static RwLock<Option<tauri::AppHandle>> {
    APP.get_or_init(|| RwLock::new(None))
}

fn emit(event: &str, payload: impl Serialize + Clone) {
    if let Some(app) = app_slot()
        .read()
        .expect("app handle lock poisoned")
        .as_ref()
    {
        let _ = app.emit(event, payload);
    }
}

pub fn request_refresh() {
    emit("state-changed", ());
}

pub fn report_status(message: impl Into<String>) {
    emit("operation-status", message.into());
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LoadFinished {
    model_id: ModelId,
    success: bool,
    message: String,
}

pub fn report_load_finished(model_id: ModelId, result: Result<(), impl std::fmt::Display>) {
    let (success, message) = match result {
        Ok(()) => (true, "模型已加载并可以使用。".to_owned()),
        Err(error) => (false, error.to_string()),
    };
    emit(
        "load-finished",
        LoadFinished {
            model_id,
            success,
            message,
        },
    );
}

pub fn report_settings_saved() {
    report_status("设置已保存");
    request_refresh();
}

pub fn report_generated_token(token: String) {
    emit("token-generated", token);
}

pub fn quit_event_loop() {
    if let Some(app) = app_slot()
        .read()
        .expect("app handle lock poisoned")
        .as_ref()
    {
        app.exit(0);
    }
}

pub fn run_desktop(
    _snapshot: AppSnapshot,
    address: String,
    actions: UiActions,
) -> Result<(), tauri::Error> {
    let state = DesktopState {
        address,
        actions: actions.clone(),
        http: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .expect("chat HTTP client configuration is valid"),
    };
    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_bootstrap,
            chat_completion,
            estimate_model_context,
            load_model,
            eject_model,
            rescan_models,
            save_engine_settings,
            save_server_settings,
            get_logs,
            export_logs,
            generate_token,
            minimize,
            toggle_maximize,
            close_window
        ])
        .setup(move |app| {
            *app_slot().write().expect("app handle lock poisoned") = Some(app.handle().clone());
            setup_tray(app, actions.clone())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                if let Some(state) = window.try_state::<DesktopState>()
                    && let Some(message) = (state.actions.close_notice)()
                {
                    emit("close-notice", message);
                }
            }
        })
        .run(tauri::generate_context!())?;
    *app_slot().write().expect("app handle lock poisoned") = None;
    Ok(())
}

fn setup_tray(app: &mut tauri::App, actions: UiActions) -> tauri::Result<()> {
    use tauri::{
        menu::{Menu, MenuItem},
        tray::TrayIconBuilder,
    };
    let open = MenuItem::with_id(app, "open", "打开 Model Launcher", true, None::<&str>)?;
    let eject = MenuItem::with_id(app, "eject", "卸载当前模型", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &eject, &quit])?;
    TrayIconBuilder::new()
        .menu(&menu)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "open" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "eject" => (actions.eject)(),
            "quit" => (actions.quit)(),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

pub fn configuration_diagnostic_status(
    diagnostic: Option<&model_launcher_core::ConfigDiagnostic>,
) -> Option<String> {
    diagnostic.map(|diagnostic| match diagnostic.kind {
        model_launcher_core::ConfigDiagnosticKind::Corrupt => {
            "Configuration was corrupt and has been quarantined.".to_owned()
        }
        model_launcher_core::ConfigDiagnosticKind::UnsupportedVersion { version } => {
            format!("Configuration version {version} is unsupported and has been quarantined.")
        }
    })
}

pub fn parse_server_settings(
    bind_address: String,
    port: i32,
    auth_enabled: bool,
) -> Result<UiServerSettings, String> {
    bind_address
        .parse::<IpAddr>()
        .map_err(|_| "监听地址必须是有效的 IPv4 或 IPv6 地址".to_string())?;
    let port = u16::try_from(port)
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| "端口必须介于 1 和 65535 之间".to_string())?;
    Ok(UiServerSettings {
        bind_address,
        port,
        auth_enabled,
    })
}

pub fn server_authentication_status(settings: &UiServerSettings) -> &'static str {
    if settings.auth_enabled {
        "Bearer Token 已启用"
    } else {
        "认证未启用"
    }
}

pub fn server_base_url(settings: &UiServerSettings) -> String {
    format!("http://{}:{}", settings.bind_address, settings.port)
}

pub fn server_lan_warning(settings: &UiServerSettings) -> &'static str {
    match settings.bind_address.parse::<IpAddr>() {
        Ok(address) if !address.is_loopback() && !settings.auth_enabled => {
            "当前监听地址可被其他设备访问，且未启用身份验证。"
        }
        _ => "",
    }
}

pub struct CloseNoticeStore {
    path: PathBuf,
}

impl CloseNoticeStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> CloseNotice {
        CloseNotice {
            pending: !self.path.exists(),
        }
    }

    pub fn save(&self, notice: &CloseNotice) -> std::io::Result<()> {
        if !notice.pending {
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&self.path, b"shown")?;
        }
        Ok(())
    }
}

pub struct CloseNotice {
    pending: bool,
}

impl CloseNotice {
    pub fn take(&mut self) -> Option<&'static str> {
        self.pending.then(|| {
            self.pending = false;
            "Model Launcher 将继续在系统托盘中运行。"
        })
    }
}

fn format_bytes(value: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if value as f64 >= GB {
        format!("{:.1} GB", value as f64 / GB)
    } else {
        format!("{:.0} MB", value as f64 / MB)
    }
}

#[cfg(test)]
mod chat_tests {
    use super::{ChatCompletionRequest, ChatMessage, post_chat_completion};
    use axum::{Json, Router, http::HeaderMap, routing::post};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn chat_command_posts_to_the_configured_local_gateway() {
        let received = Arc::new(Mutex::new(None));
        let captured = received.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let captured = captured.clone();
                async move {
                    *captured.lock().unwrap() = Some((headers, body));
                    Json(json!({"choices":[{"message":{"content":"Local reply"}}]}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let reply = post_chat_completion(
            &reqwest::Client::new(),
            &address.to_string(),
            ChatCompletionRequest {
                model: "qwen".to_owned(),
                messages: vec![ChatMessage {
                    role: "user".to_owned(),
                    content: "Hello".to_owned(),
                }],
                token: Some("secret".to_owned()),
            },
        )
        .await
        .unwrap();

        assert_eq!(reply, "Local reply");
        let (headers, body) = received.lock().unwrap().take().unwrap();
        assert_eq!(headers["authorization"], "Bearer secret");
        assert_eq!(body["model"], "qwen");
        assert_eq!(body["messages"][0]["content"], "Hello");
        assert_eq!(body["stream"], false);
        server.abort();
    }

    #[tokio::test]
    async fn chat_command_reports_an_unavailable_gateway() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(1))
            .build()
            .unwrap();

        let error = post_chat_completion(
            &client,
            &address.to_string(),
            ChatCompletionRequest {
                model: "qwen".to_owned(),
                messages: vec![],
                token: None,
            },
        )
        .await
        .unwrap_err();

        assert!(error.contains("Could not reach the local API"));
        assert!(error.contains(&address.to_string()));
    }
}
