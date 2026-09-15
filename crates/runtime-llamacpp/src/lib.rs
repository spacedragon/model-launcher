//! llama.cpp runtime adapter (placeholder).
//!
//! Maps public load config to `llama-server` argv and records the
//! version/capability fixture (minimum supported version is pinned in an
//! ADR, see `docs/development-plan.md` M0/M2).

#[cfg(test)]
mod tests {
    #[test]
    fn llamacpp_crate_is_wired_into_workspace() {
        let _ = module_path!();
    }
}
