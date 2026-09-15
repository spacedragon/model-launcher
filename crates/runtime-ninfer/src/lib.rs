//! `NInfer` runtime adapter (placeholder).
//!
//! Maps public load config to `ninfer-serve` argv; `NInfer`-specific knobs
//! (`kv_capacity`, `prefill_chunk`, `kv_dtype`, speculation, vision,
//! thinking) live under `engine_config.ninfer` and are never exposed as
//! generic parameters (`docs/architecture.md` §4).

#[cfg(test)]
mod tests {
    #[test]
    fn ninfer_crate_is_wired_into_workspace() {
        let _ = module_path!();
    }
}
