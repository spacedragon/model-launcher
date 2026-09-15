//! SQLite persistence (placeholder).
//!
//! Migrations, repositories (models, runtimes, instance desired state,
//! operations, audit events) and the in-memory registry/SQLite split
//! described in `docs/architecture.md` §9. The `sqlx` dependency is added
//! when the "SQLite migrations/repositories" job starts; this crate is
//! intentionally dependency-free until then.

#[cfg(test)]
mod tests {
    #[test]
    fn persistence_crate_is_wired_into_workspace() {
        let _ = module_path!();
    }
}
