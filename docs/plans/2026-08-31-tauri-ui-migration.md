# Tauri UI Migration Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the Slint desktop presentation with a Tauri 2 + React + TypeScript + Vite interface matching `.orca/drops/model-launcher.html` while preserving the existing Rust service behavior.

**Architecture:** Add a new `model-launcher-ui` package backed by Tauri and remove the legacy Slint crate from the active workspace. The Rust UI crate owns serializable DTOs, Tauri commands, window/tray lifecycle, and frontend event delivery; React owns navigation, filtering, dialogs, forms, and rendering. Existing `UiActions` closures remain the application-service boundary so the core, WSL engine, and HTTP gateway do not depend on Tauri.

**Tech Stack:** Rust 1.89, Tauri 2, React 19, TypeScript, Vite, Vitest, Testing Library, CSS.

---

### Task 1: Establish the Tauri shell and typed bridge

**Files:**
- Create: `crates/model-launcher-tauri/Cargo.toml`
- Create: `crates/model-launcher-tauri/build.rs`
- Create: `crates/model-launcher-tauri/tauri.conf.json`
- Create: `crates/model-launcher-tauri/src/lib.rs`
- Modify: `Cargo.toml`
- Modify: `apps/model-launcher/Cargo.toml`
- Modify: `apps/model-launcher/src/main.rs`

**Steps:**
1. Define serializable UI snapshot, model, capability, settings, and log DTOs.
2. Add Tauri commands for snapshot refresh, load/eject/rescan, settings, logs, export, clipboard, and token generation.
3. Keep the existing `UiActions` callback contract and report functions used by asynchronous service tasks.
4. Implement close-to-tray, native tray actions, window reopening, and orderly quit.
5. Run `cargo +1.89 check -p model-launcher-ui` and resolve all errors.

### Task 2: Create the React/Vite frontend

**Files:**
- Create: `crates/model-launcher-tauri/frontend/package.json`
- Create: `crates/model-launcher-tauri/frontend/vite.config.ts`
- Create: `crates/model-launcher-tauri/frontend/tsconfig.json`
- Create: `crates/model-launcher-tauri/frontend/index.html`
- Create: `crates/model-launcher-tauri/frontend/src/main.tsx`
- Create: `crates/model-launcher-tauri/frontend/src/App.tsx`
- Create: `crates/model-launcher-tauri/frontend/src/types.ts`
- Create: `crates/model-launcher-tauri/frontend/src/bridge.ts`
- Create: `crates/model-launcher-tauri/frontend/src/styles.css`

**Steps:**
1. Encode the design tokens from `.orca/drops/model-launcher.html` as CSS variables.
2. Implement the 40px title bar, 212px navigation, responsive context panel, and 30px status bar.
3. Implement Models, Parameters, API, Logs, and Settings views with real Tauri commands.
4. Implement loading, empty, error, token reveal, and close-to-tray states.
5. Add keyboard focus states, semantic controls, labels, and reduced-motion behavior.

### Task 3: Add frontend behavior tests

**Files:**
- Create: `crates/model-launcher-tauri/frontend/src/App.test.tsx`
- Create: `crates/model-launcher-tauri/frontend/src/test/setup.ts`

**Steps:**
1. Mock the Tauri invoke/listen boundary.
2. Test model filtering, page navigation, details selection, and LAN warning behavior.
3. Run `npm test -- --run` and confirm all tests pass.
4. Run `npm run build` and confirm Vite emits `dist/`.

### Task 4: Verify the integrated desktop build

**Files:**
- Modify: `README.md`

**Steps:**
1. Update build and development instructions for the Tauri frontend.
2. Run `cargo +1.89 fmt --all --check`.
3. Run `cargo +1.89 check --workspace --all-targets`.
4. Run focused core/API tests and document any environment-only Windows acceptance work.
5. Launch the Tauri application where WebView2 is available and compare against the local HTML design at desktop and narrow widths.
