use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
};
use serde_json::{Value, json};
use std::time::Instant;

use crate::{ApiError, AppState, LmModel, LoadConfig, LoadRequest, LoadResponse, UnloadRequest};

pub(crate) async fn list(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"models": state.models.iter().map(LmModel::from).collect::<Vec<_>>() }))
}

pub(crate) async fn load(
    State(state): State<AppState>,
    request: Result<Json<LoadRequest>, JsonRejection>,
) -> Result<Json<LoadResponse>, ApiError> {
    let Json(request) = request.map_err(|_| {
        ApiError::bad_request(
            "invalid_request",
            "request body does not match the API contract",
        )
    })?;
    let resolved = state
        .management
        .resolve(&request.model)
        .ok_or_else(|| ApiError::not_found("model_not_found", "model was not found"))?;
    let model = state
        .profiles
        .apply(resolved, &request)
        .map_err(ApiError::core)?;
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
        load_config: request.echo_load_config.then(|| LoadConfig::from(&request)),
    }))
}

pub(crate) async fn unload(
    State(state): State<AppState>,
    request: Result<Json<UnloadRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(request) = request.map_err(|_| {
        ApiError::bad_request(
            "invalid_request",
            "request body does not match the API contract",
        )
    })?;
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
    Ok(Json(json!({"instance_id":request.instance_id})))
}

pub(crate) async fn openai_models(State(state): State<AppState>) -> Json<Value> {
    Json(
        json!({"object":"list","data":state.models.iter().map(|model| json!({"id":model.record.key,"object":"model","owned_by":model.publisher})).collect::<Vec<_>>() }),
    )
}
