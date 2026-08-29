use model_launcher_api::{LoadRequest, TokenStore};

#[test]
fn load_contract_rejects_unknown_fields_and_distinguishes_omitted_config() {
    let request: LoadRequest = serde_json::from_str(r#"{"model":"acme/tiny"}"#).unwrap();
    assert_eq!(request.model, "acme/tiny");
    assert!(request.context_length.is_none());
    assert!(serde_json::from_str::<LoadRequest>(r#"{"model":"x","gpu":99}"#).is_err());
}

#[test]
fn generated_tokens_are_only_returned_at_creation_and_verify_uniformly() {
    let mut store = TokenStore::default();
    let created = store.create().unwrap();
    assert!(created.plaintext.starts_with("ml_"));
    assert!(store.verify(&created.plaintext));
    assert!(!store.verify("ml_wrong"));
    assert!(!format!("{store:?}").contains(&created.plaintext));
}
