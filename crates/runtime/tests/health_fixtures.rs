//! Validation of the pinned `fixtures/runtimes/health-and-models.json` fixture.
//!
//! The fixture is a synthetic pin, not a live capture. These tests lock its
//! schema, truthful provenance, engine-version linkage to `manifest.json`, and
//! the shape of the `/health` and `/v1/models` bodies for both runtimes.

use serde_json::Value;

const HEALTH_AND_MODELS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/runtimes/health-and-models.json"
);
const MANIFEST: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/runtimes/manifest.json"
);

fn json_fixture(path: &str) -> Value {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn case<'a>(fixture: &'a Value, runtime: &str, kind: &str) -> &'a Value {
    fixture["cases"]
        .as_array()
        .expect("cases must be an array")
        .iter()
        .find(|entry| entry["runtime"] == runtime && entry["kind"] == kind)
        .unwrap_or_else(|| panic!("fixture must contain a {runtime}/{kind} case"))
}

fn body_json(case: &Value) -> Value {
    let body = case["body"].as_str().expect("body must be a JSON string");
    serde_json::from_str(body).expect("body must parse as JSON")
}

fn models_ids(body: &Value) -> Vec<&str> {
    assert_eq!(body["object"].as_str(), Some("list"), "models object");
    let data = body["data"].as_array().expect("data must be an array");
    assert!(!data.is_empty(), "models list must not be empty");
    for model in data {
        assert_eq!(model["object"].as_str(), Some("model"), "model object");
        assert!(
            model["id"].as_str().is_some_and(|id| !id.is_empty()),
            "model id must be a non-empty string"
        );
    }
    data.iter()
        .map(|model| model["id"].as_str().expect("model id"))
        .collect()
}

#[test]
fn health_and_models_fixture_is_schema_versioned_and_truthful() {
    let fixture = json_fixture(HEALTH_AND_MODELS);
    assert_eq!(fixture["schema_version"].as_u64(), Some(1));
    assert_eq!(
        fixture["provenance"]["kind"].as_str(),
        Some("synthetic-pinned")
    );
    assert_eq!(
        fixture["provenance"]["captured_live"].as_bool(),
        Some(false),
        "the fixture must not claim a live capture"
    );

    for runtime in ["llamacpp", "ninfer"] {
        for kind in ["health", "models"] {
            assert_eq!(
                case(&fixture, runtime, kind)["provenance"].as_str(),
                Some("synthetic-pinned"),
                "{runtime}/{kind} case provenance"
            );
        }
    }
}

#[test]
fn health_and_models_fixture_links_to_the_manifest_engine_versions() {
    let fixture = json_fixture(HEALTH_AND_MODELS);
    let manifest = json_fixture(MANIFEST);

    for (runtime, manifest_key) in [("llamacpp", "llamacpp"), ("ninfer", "ninfer")] {
        assert_eq!(
            fixture["engine_versions"][runtime].as_str(),
            manifest["engines"][manifest_key]["fixture_version"].as_str(),
            "{runtime} engine version must match the manifest pin"
        );
    }
}

#[test]
fn llamacpp_health_fixture_reports_ok() {
    let fixture = json_fixture(HEALTH_AND_MODELS);
    let body = body_json(case(&fixture, "llamacpp", "health"));
    assert_eq!(body["status"].as_str(), Some("ok"));
}

#[test]
fn llamacpp_models_fixture_has_the_expected_identity_and_list_shape() {
    let fixture = json_fixture(HEALTH_AND_MODELS);
    let body = body_json(case(&fixture, "llamacpp", "models"));
    assert_eq!(models_ids(&body), vec!["qwen2.5-7b-instruct-q4_k_m"]);
}

#[test]
fn ninfer_health_fixture_reports_ok() {
    let fixture = json_fixture(HEALTH_AND_MODELS);
    let body = body_json(case(&fixture, "ninfer", "health"));
    assert_eq!(body["status"].as_str(), Some("ok"));
}

#[test]
fn ninfer_models_fixture_has_the_expected_identity_and_list_shape() {
    let fixture = json_fixture(HEALTH_AND_MODELS);
    let body = body_json(case(&fixture, "ninfer", "models"));
    assert_eq!(models_ids(&body), vec!["qwen-2.5-7b-instruct"]);
}
