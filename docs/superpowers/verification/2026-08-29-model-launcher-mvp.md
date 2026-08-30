# Model Launcher MVP verification evidence

Verified 2026-08-30 on commit `068d09adb09a87e1c2d84ae1463db27cc47047ff` before this evidence update.

## Status vocabulary

- **Automated PASS**: the named test ran in the fresh full suite and passed.
- **Compile PASS**: the named build/check command exited zero; this is not runtime acceptance.
- **NOT RUN**: the required hardware, operating system, model, or manual observation was unavailable.
- **BLOCKED**: an optional attempted check could not run for the stated environmental reason.

Passing automated tests do not substitute for the real Windows/WSL and manual resource checks below.

## Fresh clean verification

Host: macOS 26.5.2 (25F84), Apple Silicon; local time zone UTC+08. Toolchain: `rustc 1.89.0 (29483883e 2025-08-04)`, host `aarch64-apple-darwin`, LLVM 20.1.7; `cargo 1.89.0`; Ruby 2.6.10.

| Command | Result | Duration / count | Notes |
|---|---|---|---|
| `cargo clean` | Compile PASS, exit 0 | Removed 39,616 files / 13.1 GiB | Cargo metadata identified the sole target directory as repository-local `target`; no source/user path was targeted. |
| `cargo fmt --all --check` | Compile PASS, exit 0 | not timed | No diff. |
| `cargo clippy --workspace --all-targets -- -D warnings` | Compile PASS, exit 0 | about 52 s | Clean rebuild; warnings denied. |
| `cargo test --workspace --all-targets` | Automated PASS, exit 0 | 239 passed; 0 failed | Final rerun outside the restricted socket sandbox included 36/36 headless, 32/32 API contracts, 1/1 proxy unit, 18/18 UI view-model, and all remaining workspace tests. An earlier restricted run was denied loopback/socket operations (`Operation not permitted`); it was environmental and is not counted as a pass. Windows-gated native-window tests compiled but ran 0 tests on macOS. |
| `cargo build -p model-launcher --release` | Compile PASS, exit 0 | 2 min 21 s | Release profile completed. |
| `cargo check --workspace --all-targets --target x86_64-pc-windows-msvc` | Compile PASS, exit 0 | 1 min 52 s | Cross-target compilation only; no Windows execution. |
| `cargo check --workspace --all-targets --target x86_64-pc-windows-gnu` | BLOCKED, exit 101 | 1.11 s | Target not installed. The installed MSVC check above is the relevant Windows cross-target evidence. |
| `ruby tests/windows-wsl/validate.rb` | Automated PASS, exit 0 | not timed | Printed `Windows packaging contracts passed`. |

## Requirements traceability matrix

### §§1–2 — product scope and architecture

| Requirement | Implementation | Test evidence | Status |
|---|---|---|---|
| Native Windows/Slint launcher with long-lived core, tray, and replaceable UI window | `apps/model-launcher/src/{main,service}.rs`; `crates/model-launcher-ui/src/{lib,tray}.rs`; `ui/app.slint` | `apps/model-launcher/tests/headless.rs::scan_http_stream_eject_restart_and_idempotent_shutdown`; `crates/model-launcher-ui/tests/view_model.rs::tray_maps_commands_without_opening_a_real_window_and_drops_windows` | Automated PASS; real desktop lifecycle NOT RUN |
| UI-independent typed core and application-service snapshots/commands | `crates/model-launcher-core/src/lib.rs`; `apps/model-launcher/src/service.rs`; `crates/model-launcher-ui/src/lib.rs` | `service_change_subscription_observes_catalog_and_profile_mutations`; `snapshot_maps_to_compact_rows_and_prioritizes_narrow_metadata` | Automated PASS |
| Replaceable `InferenceEngine` boundary | `crates/model-launcher-core/src/engine.rs`; `crates/model-launcher-wsl/src/engine.rs` | `engine::tests::inference_engine_is_object_safe`; lifecycle fake-engine suite | Automated PASS |
| One loaded model now, typed architecture for lifecycle replacement/JIT | `crates/model-launcher-core/src/lifecycle.rs`; `crates/model-launcher-api/src/management.rs` | `replacement_stops_a_and_loads_b_without_restoring_a_on_failure`; `concurrent_same_model_http_jit_shares_one_spawn` | Automated PASS |
| Stable public listener and separately allocated internal loopback port | `crates/model-launcher-api/src/lib.rs`; `crates/model-launcher-wsl/src/process.rs` | `server_handle_keeps_stable_address_and_graceful_stop_releases_listener`; `internal_port_allocator_reserves_loopback_ephemeral_port`; `internal_retry_arguments_do_not_contain_or_mutate_a_public_gateway_port` | Automated PASS |
| Only the exact owned Linux PID is stopped; never kill by executable name | `crates/model-launcher-wsl/src/{process,engine}.rs` | `owned_pid_identity_uses_structured_proc_stat_and_guarded_signal_script`; `three_stolen_ports_exhaust_and_terminate_each_owned_pid_only`; `kill_failure_blocks_cleanup_and_retry` | Automated PASS; real WSL observation NOT RUN |

### §§3–5 — WSL integration, catalog, persistence, capabilities

| Requirement | Implementation | Test evidence | Status |
|---|---|---|---|
| Typed Windows-drive-to-WSL conversion; reject UNC, traversal, invalid/reserved/ADS paths | `crates/model-launcher-wsl/src/path.rs` | `converts_drive_paths_preserving_spaces_unicode_and_normalizing_case`; `rejects_relative_unc_traversal_and_invalid_roots`; `rejects_ads_reserved_dos_names_and_trailing_dot_or_space`; `rejects_windows_illegal_and_control_characters` | Automated PASS |
| Structured process argv, with no user-controlled shell interpolation | `crates/model-launcher-wsl/src/{process,probe}.rs` | `launch_uses_fixed_script_and_positional_arguments`; `fake_runner_probes_without_wsl_and_keeps_values_as_argv`; `probes_use_direct_argv_and_capture_version_and_known_help_aliases` | Automated PASS |
| Probe version/help on save or executable identity change; retain last valid cache on failure | `crates/model-launcher-wsl/src/{probe,engine}.rs`; `apps/model-launcher/src/service.rs` | `prober_revalidates_identity_and_reuses_only_valid_snapshot`; `cache_requires_distribution_path_and_identity_match`; `failed_reprobe_preserves_previous_snapshot_on_disk`; `save_engine_settings_validates_identity_then_reprobes` | Automated PASS |
| Recursive case-insensitive GGUF discovery, explicit/debounced rescan, filesystem watcher | `crates/model-launcher-core/src/catalog.rs`; `apps/model-launcher/src/service.rs` | `scan_recurses_and_matches_extension_case_insensitively`; `debounce_coalesces_changes_deterministically`; `real_watcher_drives_service_and_persists_discovery`; `catalog_watcher_follows_a_saved_root_switch` | Automated PASS |
| Complete recognized shards group; incomplete/excessive groups are safe | `crates/model-launcher-core/src/catalog.rs` | `scan_groups_only_complete_recognized_shard_sets`; `mixed_case_shard_extensions_group_using_actual_paths`; `excessive_declared_shard_total_is_rejected_without_expansion` | Automated PASS |
| Metadata extraction with degraded visible fallback | `crates/model-launcher-core/src/catalog.rs` | `valid_gguf_extracts_metadata_and_missing_name_uses_filename`; `malformed_metadata_falls_back_to_filename_with_visible_diagnostic`; `wrong_type_name_falls_back_with_metadata_diagnostic` | Automated PASS |
| Stable UUID/key/profile identity; unique URL-safe editable keys; moved/missing reconciliation | `crates/model-launcher-core/src/{model,catalog,config}.rs` | `reconciliation_generates_unique_url_safe_keys_for_duplicates`; `reconciliation_retains_user_key_and_reconnects_a_moved_file`; `model_uuid_and_key_survive_persistence`; `missing_records_are_preserved_until_explicitly_removed`; `profile_load_validates_persists_key_and_profile_before_lifecycle_load` | Automated PASS |
| Curated typed launch settings, range validation, capability-gated argv, retained unsupported values | `crates/model-launcher-core/src/capability.rs`; `crates/model-launcher-wsl/src/probe.rs` | `positive_settings_validate_during_deserialization`; `launch_arguments_are_gated_by_engine_capabilities`; `launch_render_reports_retained_unsupported_settings`; `capability_visibility_retains_and_reports_unsupported_values` | Automated PASS |
| Probe failure disables loading with original diagnostic and later recovery | `apps/model-launcher/src/service.rs`; `crates/model-launcher-ui/src/lib.rs` | `initial_probe_failure_starts_disabled_and_valid_settings_recover_loading`; `invalid_engine_disables_load_with_probe_diagnostic` | Automated PASS |

### §6 — lifecycle state machine

| Requirement | Implementation | Test evidence | Status |
|---|---|---|---|
| Serialized stopped/starting/running/stopping/backoff/validation flow; readiness gates success | `crates/model-launcher-core/src/lifecycle.rs` | `load_transitions_stopped_starting_running_and_waits_for_readiness`; `readiness_failure_stops_process_before_reporting_failure`; `validation_and_spawn_have_actor_enforced_timeouts` | Automated PASS |
| Replacement stops A, starts B, and never restores A after B failure | same | `replacement_stops_a_and_loads_b_without_restoring_a_on_failure` | Automated PASS |
| Active inference makes replacement busy; eject remains destructive and cancels leases | same; `crates/model-launcher-api/src/proxy.rs` | `replacement_is_busy_while_inference_lease_is_active`; `explicit_eject_cancels_leases_and_clears_desired_model`; `eject_active_stream_cancels_upstream_and_releases_lease_once`; UI `busy_disables_other_load_with_explanation_but_keeps_eject_enabled` | Automated PASS |
| Crash restarts at capped 1/2/4/8/16/30 s; five healthy minutes reset; eject cancels backoff | `crates/model-launcher-core/src/lifecycle.rs` | `unexpected_crashes_restart_with_capped_exponential_backoff`; `five_healthy_minutes_reset_crash_backoff`; `stale_generation_timer_cannot_restart_after_eject`; headless `shutdown_cancels_backoff_and_prevents_restart` | Automated PASS; wall-clock real-engine observation NOT RUN |
| Bounded graceful stop then exact owned-process force stop; stale/cancelled work cannot revive | core lifecycle and WSL engine | `graceful_stop_timeout_forces_process_and_allows_replacement`; `force_timeout_aggregates_stop_requests_and_blocks_replacement`; `cancelled_spawn_future_cannot_create_a_late_process`; `bounded_termination_aborts_and_joins_actor_without_detaching` | Automated PASS |

### §7 — HTTP API, compatibility, JIT, authentication

| Requirement | Implementation | Test evidence | Status |
|---|---|---|---|
| Default loopback public gateway; configurable LAN binding independently of auth with warning | `crates/model-launcher-api/src/lib.rs`; `apps/model-launcher/src/service.rs` | `lan_without_auth_is_allowed_with_typed_warning`; `server_handle_keeps_stable_address_and_graceful_stop_releases_listener` | Automated PASS |
| Live bind/port/auth changes replace the gateway safely, reject port zero, and preserve the old listener/settings on failure | `apps/model-launcher/src/service.rs`; `crates/model-launcher-ui/src/lib.rs`; `ui/app.slint` | `loopback_to_wildcard_rebind_can_keep_the_same_port`; `server_settings_reject_zero_port_without_changing_listener`; `handoff_rollback_keeps_auth_policy_attached_to_restored_listener`; `shutdown_waits_for_in_progress_handoff_and_stops_replacement_listener` | Automated PASS |
| Gateway catalog/model routing follows rescans and persisted missing identities survive invalid-root startup/recovery | `apps/model-launcher/src/service.rs` | `file_at_default_catalog_root_starts_with_diagnostic_and_can_recover`; headless dynamic gateway model-list and routing coverage | Automated PASS |
| `GET /v1/models` lists discovered unloaded/loaded models | `crates/model-launcher-api/src/{lib,models}.rs` | `lists_match_pinned_semantics`; headless vertical slice | Automated PASS |
| Chat/completions proxy preserves payload bytes, safe response headers, and byte-correct SSE | `crates/model-launcher-api/src/proxy.rs` | `proxy_preserves_raw_bytes_and_safe_headers`; `controllable_fake_upstream_preserves_split_non_utf8_sse_bytes`; `bounded_request_spool_preserves_incoming_bytes`; `upstream_redirect_is_returned_without_following_or_forwarding_again` | Automated PASS |
| LM Studio list/load/unload pinned fields, nullable semantics, supported overrides, unknown-field rejection | `crates/model-launcher-api/src/{management,models}.rs`; JSON fixtures under `tests/fixtures` | `management_load_echo_unload_and_errors_match_contracts`; `load_contract_rejects_unknown_fields_and_distinguishes_omitted_and_null`; `nullable_lm_metadata_is_present_as_null_not_omitted`; `supported_management_overrides_are_typed_and_applied_before_load` | Automated PASS |
| JIT: absent/running/idle switch/busy/unknown/starting/load-failed and shared same-model start | API management + core lifecycle | `lifecycle_http_matrix_jit_same_running_busy_and_unknown`; `concurrent_same_model_http_jit_shares_one_spawn`; `zero_startup_budget_returns_model_starting_with_retry_after`; `lifecycle_load_failure_is_stable_503`; `same_model_start_waiters_are_capped_without_duplicate_spawn` | Automated PASS |
| Bounded body, header, connections, startup; safe cancellation and exact in-flight decrement | `crates/model-launcher-api/src/{lib,proxy}.rs` | `unknown_and_limits_have_stable_statuses`; `active_response_holds_connection_permit_and_overload_is_stable`; `listener_connection_cap_covers_idle_prebody_connections`; `stalled_upstream_headers_timeout_releases_lease_and_request_permit`; `client_disconnect_cancels_upstream_and_decrements_in_flight` | Automated PASS |
| Random one-time Bearer token; persist Argon2 hash only; uniform failure; authorization redacted | `crates/model-launcher-api/src/auth.rs`; core/UI token and log stores | `generated_tokens_persist_only_argon2_phc_hashes`; `authentication_failure_is_uniform`; `token_store_rejects_malicious_phc_inputs_and_excessive_counts`; `service_persists_live_tokens_and_exposes_redacted_logs`; `authorization_and_bearer_secrets_are_redacted_before_storage_and_broadcast` | Automated PASS |

### §§8–9 — UI, tray, persistence, observability

| Requirement | Implementation | Test evidence | Status |
|---|---|---|---|
| Quiet Native top title/navigation layout and full-width responsive Models page | `crates/model-launcher-ui/ui/{app.slint,components/*.slint}` | `modal_layout_stays_inside_compact_and_narrow_viewports`; `snapshot_maps_to_compact_rows_and_prioritizes_narrow_metadata`; `model_search_matches_name_key_and_path_case_insensitively` | Automated PASS for view model/compile; visual Windows inspection NOT RUN |
| Load modal exposes editable key and supported settings, defaults from global profile, retains unsupported saved values visibly/read-only | UI Slint + adapter; core capability/profile | `load_dialog_adapter_hydrates_every_saved_profile_value`; `load_dialog_adapter_keeps_unsupported_saved_fields_visible_and_read_only`; `newly_discovered_models_inherit_global_launch_defaults` | Automated PASS |
| Server/log/settings actions, filtered bounded redacted logs, save performs validation/probe | UI adapter + service + core log/config | `log_commands_use_bounded_filtered_redacted_snapshots`; `filters_use_typed_source_and_minimum_level`; `engine_settings_validate_persist_then_apply_exact_inputs`; `launcher_settings_saves_are_serialized_across_async_validation`; `launcher_settings_save_aborts_after_validation_when_shutdown_starts` | Automated PASS |
| Server form preserves unsaved drafts during refresh while live auth/token state stays current | `crates/model-launcher-ui/src/lib.rs`; `crates/model-launcher-ui/ui/app.slint` | UI view-model suite 18/18, including server-setting hydration and refresh behavior | Automated PASS |
| Tray status/open/eject/recent/quit; reactive stable recent IDs; ordered shutdown | `crates/model-launcher-ui/src/tray.rs`; `apps/model-launcher/src/{main,service}.rs` | `tray_maps_commands_without_opening_a_real_window_and_drops_windows`; `tray_recent_request_resolves_stable_id_after_catalog_reordering`; `recent_models_are_successful_mru_entries_with_stable_ids`; headless shutdown tests | Automated PASS; real tray NOT RUN |
| Close destroys/recreates window, one-time continued-in-tray notice, snapshots hydrate fresh state | `crates/model-launcher-ui/src/lib.rs` | `close_notice_and_plaintext_token_are_each_consumed_once`; `close_notice_consumption_is_persisted`; Windows test `real_main_window_weak_reference_dies_for_fifty_recreate_cycles` compiled but did not run | Automated PASS for state logic; native lifecycle NOT RUN |
| Versioned JSON config; atomic replace/backup; migrations; quarantine corrupt/unsupported; failures do not claim persistence | `crates/model-launcher-core/src/config.rs` | config tests `configuration_round_trips`, `save_atomically_replaces_the_main_file`, `replacement_retains_the_last_valid_backup`, `migrates_a_version_zero_fixture`, `corrupt_file_is_quarantined_without_being_overwritten`, `unsupported_version_is_quarantined`, `replacement_failure_preserves_main_and_cleans_temporary_file` | Automated PASS |
| Token leaves UI after copy; clipboard clears after 60 seconds only if unchanged; expiry retains digest | `crates/model-launcher-ui/src/lib.rs` | `token_clipboard_expiry_clears_only_the_unchanged_token`; `close_notice_and_plaintext_token_are_each_consumed_once` | Automated PASS for fake clipboard/state; platform clipboard NOT RUN |
| Structured bounded logs and safe export including engine framing | `crates/model-launcher-core/src/log.rs` | `records_keep_all_structured_fields`; `retention_enforces_record_and_utf8_byte_limits`; `export_is_stable_json_lines_in_snapshot_order`; `engine_bytes_frame_crlf_split_chunks_lossy_utf8_and_eof_partial` | Automated PASS |
| Restore settings/catalog but never auto-load previous model | config/catalog/service | headless `scan_http_stream_eject_restart_and_idempotent_shutdown`; smoke harness automated restart assertion exists | Automated PASS with fake engine; real WSL restart NOT RUN |

### §§10–11 — failures, security, and resource controls

| Requirement | Implementation | Test evidence | Status |
|---|---|---|---|
| Invalid WSL/executable and unsupported model path fail validation before spawn with diagnostic | WSL path/probe/engine + service | WSL rejection/probe tests above; `initial_probe_failure_starts_disabled_and_valid_settings_recover_loading` | Automated PASS |
| Internal port conflict retries fresh ports; public conflict fails cleanly | WSL engine/process; API/service | `address_in_use_stderr_exit_terminates_owned_pid_then_retries_fresh_port`; `three_stolen_ports_exhaust_and_terminate_each_owned_pid_only`; headless `listener_bind_failure_cleans_partially_started_lifecycle` | Automated PASS |
| Metadata failure retains degraded record; incomplete scans never erase known availability/config | core catalog | `malformed_metadata_falls_back_to_filename_with_visible_diagnostic`; `incomplete_scan_preserves_existing_availability`; `service_does_not_save_an_incomplete_scan` | Automated PASS |
| Health timeout stops owned process, preserves typed failure; crash/backoff cancellation is safe | lifecycle/WSL/log | `startup_timeout_stops_owned_process_and_fails_load`; `readiness_failure_stops_process_before_reporting_failure`; restart tests above | Automated PASS |
| Config write errors preserve last valid file/in-memory outcome and are surfaced | core config/service | `replacement_failure_preserves_main_and_cleans_temporary_file`; `update_propagates_mutator_error_without_saving`; `poisoned_transaction_lock_returns_config_io_instead_of_panicking` | Automated PASS |
| Client disconnect/cancel decrements exactly once; eject terminates stream safely | API proxy/lifecycle | `client_disconnect_cancels_upstream_and_decrements_in_flight`; `eject_active_stream_cancels_upstream_and_releases_lease_once`; `burst_lease_drops_cannot_lose_release_events` | Automated PASS |
| Strict typed validation; no arbitrary extra CLI field; request/connection/log/catalog/metadata limits | capability/model/API/catalog/log modules | `positive_settings_validate_during_deserialization`; `model_key_rejects_parent_traversal`; API limit tests; `discovered_file_and_model_counts_are_globally_bounded`; `malicious_gguf_header_counts_are_rejected_by_catalog_budgets`; log retention tests | Automated PASS |
| No symlink escape/fixed-temp attack; safe file identity and reconciliation | core catalog/config | `root_and_child_symlinks_are_never_followed`; `root_swap_to_outside_symlink_is_rejected_before_outside_open`; `preplanted_fixed_temp_symlinks_are_never_followed`; identity reuse/hardlink tests | Automated PASS |
| Window recreation does not retain hidden Slint state | UI window manager | native Windows weak-reference test exists | NOT RUN on this host |

### §§12–13 — acceptance and delivery

| Acceptance / delivery requirement | Evidence | Status |
|---|---|---|
| Automated unit: help/capabilities, paths, shards/identity, lifecycle/cancellation/backoff | Named tests above; fresh workspace suite | Automated PASS |
| API contracts: list/load/unload, forwarding, errors, auth, raw SSE | `contracts.rs` 32/32 plus proxy unit 1/1 | Automated PASS |
| Fake-engine/headless lifecycle integration including readiness/crash/timeout/replacement/shared JIT | lifecycle 31/31 and headless 36/36 | Automated PASS |
| Persistence: atomic writes, migration, corruption, missing/moved | config 20/20 and catalog 37/37 | Automated PASS |
| Slint view-model responsiveness and action availability | view-model 18/18 | Automated PASS |
| Windows package metadata and harness contract | release build, MSVC cross-check, Ruby validator; `apps/model-launcher/resources/*`, workflows, README | Compile PASS / Automated PASS |
| Real Windows acceptance steps 1–10 (probe, discover/edit key, UI load + streamed chat, LM unload, JIT, busy, crash/backoff, eject-backoff, tray reopen, restart/no auto-load) | `tests/windows-wsl/smoke.ps1` and its README define executable evidence capture, but no Windows/WSL/model was available | NOT RUN |
| Resource: destroyed window/component while tray/core remain | Windows weak-reference test and `-ManualResourceChecks` harness exist | NOT RUN |
| Resource: 50 open/close cycles with no sustained working-set growth (documented 32 MiB settled tolerance) | manual harness gate | NOT RUN |
| Resource: negligible idle CPU (documented 30 s sample, <=1% of one logical CPU) | manual harness gate | NOT RUN |
| Resource: logs/catalog remain bounded under real application pressure | automated unit bounds pass; manual Windows gate not executed | Automated PASS for units; real resource observation NOT RUN |
| Vertical delivery sequence §§13.1–13.6 | Tasks 1–8 implementation and automated evidence above; corresponding commits through UI/headless work | Automated PASS |
| Delivery §13.7: Windows packaging, real WSL, resource checks, release docs | packaging/docs/harness implemented; real WSL/resource execution absent | Compile PASS for packaging; acceptance NOT RUN |

## Remaining gap and exact completion command

This host has no Windows desktop, WSL distribution, user-supplied `llama-server`, GGUF acceptance models, or Windows resource-inspection tooling. Therefore Task 9 Step 4 and the real-runtime portions of design §12 are **NOT RUN**. This prevents full plan/MVP acceptance from being claimed even though the automated suite passes.

On trusted user-supplied Windows hardware, from an interactive clean checkout, run (substituting real values):

```powershell
./tests/windows-wsl/smoke.ps1 `
  -Distro Ubuntu `
  -LlamaServer /usr/local/bin/llama-server `
  -LlamaCommit b1234 `
  -ModelRoot D:\Models\Acceptance `
  -ModelKey publisher/tiny-model `
  -ModelProvenance "publisher/repository revision; license" `
  -SecondModelKey publisher/second-model `
  -ManualResourceChecks
```

Attach the generated ignored `artifacts/windows-wsl/evidence.json` and `evidence.md`, and check Task 9 Step 4 only if every automated and strict manual gate is `PASS`.

The independent blocker-only implementation review completed after the final fixes and reported no remaining Critical or Important findings. Its targeted checks covered handoff rollback authentication, shutdown during handoff, UI view-model behavior, formatting, Clippy, and diff cleanliness.
