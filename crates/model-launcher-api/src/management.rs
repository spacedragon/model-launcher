use axum::{Json, extract::State};
use model_launcher_core::{BatchSize, ContextLength};
use serde_json::{Value, json};
use std::time::Instant;

use crate::{ApiError, AppState, LmModel, LoadConfig, LoadRequest, LoadResponse, UnloadRequest};

pub(crate) async fn list(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"models": state.models.iter().map(LmModel::from).collect::<Vec<_>>() }))
}

pub(crate) async fn load(
    State(state): State<AppState>,
    Json(request): Json<LoadRequest>,
) -> Result<Json<LoadResponse>, ApiError> {
    let mut model = state.find_model(&request.model)?.record.clone();
    model.launch_profile.settings.context_length = request
        .context_length
        .map(ContextLength::new)
        .transpose()
        .map_err(ApiError::core)?;
    model.launch_profile.settings.batch_size = request
        .eval_batch_size
        .map(BatchSize::new)
        .transpose()
        .map_err(ApiError::core)?;
    model.launch_profile.settings.flash_attention = request.flash_attention;
    let started = Instant::now();
    state
        .lifecycle
        .load(model.clone())
        .await
        .map_err(ApiError::core)?;
    Ok(Json(LoadResponse {
        kind: "llm_instance",
        model_instance_id: model.id.as_uuid().to_string(),
        load_time_seconds: started.elapsed().as_secs_f64(),
        status: "loaded",
        config: request.echo_load_config.then(|| LoadConfig::from(&request)),
    }))
}

pub(crate) async fn unload(
    State(state): State<AppState>,
    Json(request): Json<UnloadRequest>,
) -> Result<Json<Value>, ApiError> {
    let snapshot = state.lifecycle.snapshot();
    let Some(process) = snapshot.process else {
        return Err(ApiError::not_found(
            "model_instance_not_found",
            "model instance was not found",
        ));
    };
    if process.model_id.as_uuid().to_string() != request.instance_id {
        return Err(ApiError::not_found(
            "model_instance_not_found",
            "model instance was not found",
        ));
    }
    state.lifecycle.eject().await.map_err(ApiError::core)?;
    Ok(Json(
        json!({"type":"unload_result","status":"unloaded","model_instance_id":request.instance_id}),
    ))
}

pub(crate) async fn openai_models(State(state): State<AppState>) -> Json<Value> {
    Json(
        json!({"object":"list","data":state.models.iter().map(|model| json!({"id":model.record.key,"object":"model","owned_by":model.publisher})).collect::<Vec<_>>() }),
    )
}
