//! Wire-level DTOs for all external protocols (placeholder).
//!
//! `OpenAI` Chat Completions / Completions / Embeddings request-response
//! types, the LM Studio `/api/v1/*` subset, and native management API
//! payloads. Types here are serialization shells — validation and
//! mapping to domain types happen in `model-serving-domain` /
//! `model-serving-server`.

#[cfg(test)]
mod tests {
    #[test]
    fn api_types_crate_is_wired_into_workspace() {
        let _ = module_path!();
    }
}
