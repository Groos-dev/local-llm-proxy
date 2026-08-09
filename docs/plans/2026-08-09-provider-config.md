# Provider Configuration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use executing-plans to implement this plan task-by-task with review checkpoints.

**Goal:** Load multiple LLM providers and model mappings from TOML, select built-in adapters by name, and provide configured compact fallback behavior.

**Architecture:** Add `config.rs` for TOML parsing, validation, API-key resolution, and a runtime provider registry. Provider entries contain shared connection settings and compact capability; each model entry owns its adapter kind. `ModelRoute` becomes an owned route value containing public/upstream model names, provider name, and adapter settings. `main.rs` resolves each request to a provider route. `supports_compact = false` skips upstream compact and calls the selected model through `/responses`; otherwise the proxy tries `/responses/compact` first and falls back to the same model on transport errors, `404`, and other non-success statuses.

**Tech Stack:** Rust 2024, Serde, TOML, Axum, Reqwest, Tokio.

**Spec:** `docs/specs/2026-08-09-provider-config-design.md`

## Global Constraints

- Provider credentials stay in environment variables named by `api_key_env`; never store secrets in TOML.
- Supported built-in adapters remain `deepseek` and `standard`.
- `response_adapter` belongs to each `providers.models` entry; `supports_compact` belongs to `providers`.
- Public model names must be globally unique across providers.
- Do not modify unrelated `.idea` files already present in the worktree.

### Task 1: Add TOML Schema and Provider Registry

**Files:**
- Create: `src/config.rs`
- Modify: `src/channel/mod.rs`, `src/model.rs`, `src/lib.rs`, `Cargo.toml`
- Test: `tests/provider_config.rs`

- [x] Write integration tests for parsing the confirmed TOML shape, two-provider route lookup, public model aggregation, duplicate model rejection, missing model rejection, invalid URL rejection, and unknown adapter rejection.
- [x] Run `cargo test --test provider_config`; the first run was blocked by the existing registry lock mismatch, then passed after dependency resolution.
- [x] Add the pinned `toml` dependency and implement `AppConfig`, `ProviderConfig`, `ModelConfig`, `ModelRoute`, and `ProviderRegistry`.
- [x] Make `ChannelKind` deserialize from stable names `deepseek` and `standard`; validate provider fields, URLs, uniqueness, and provider-level `supports_compact` during load.
- [x] Replace hard-coded `MODEL_ROUTES` lookup with registry-backed route lookup while keeping model transformation helpers focused on `ModelRoute`.
- [x] Run the provider configuration test target and confirm all new registry tests pass.

### Task 2: Preserve Adapter and Compact Semantics

**Files:**
- Modify: `src/channel/mod.rs`, `src/compact.rs`, `src/request.rs`, `src/response.rs`, `src/sse.rs`
- Test: existing colocated module tests and `tests/provider_config.rs`

- [x] Update tests and helper signatures to borrow owned routes instead of relying on `Copy` static routes.
- [x] Add tests for the normal Responses fallback request body and the compact response envelope.
- [x] Keep request normalization, JSON response normalization, and SSE model restoration unchanged while making compact fallback provider-level.
- [x] Run all library tests after the route ownership and adapter changes.

### Task 3: Integrate Provider Selection and Compact Fallback

**Files:**
- Modify: `src/main.rs`, `start.sh`, `stop.sh`
- Create: `config.toml`

- [x] Load `CONFIG_PATH` (default `config.toml`), resolve configured API-key environment variables, and build shared provider state at startup.
- [x] Route `/v1/responses`, `/v1/responses/compact`, and `/v1/models` through the registry; use the selected provider base URL and API key.
- [x] For compact, skip upstream when `supports_compact` is false; otherwise fall back through the selected model's normal `/responses` endpoint on transport errors, `404`, other non-success statuses, or invalid compact JSON. Preserve the upstream error when the model fallback also fails.
- [x] Update shell startup to use `CONFIG_PATH` and remove the hard-coded single-provider endpoint. Keep bind/log environment overrides.
- [x] Run formatting, unit/integration tests, and a build.

### Task 4: Update Contributor Guidance

**Files:**
- Modify: `AGENTS.md`, `docs/specs/2026-08-09-provider-config-design.md`

- [x] Document the final `supports_compact` semantics and the `config.toml` startup path.
- [x] Run `git diff --check` and verify unrelated `.idea` changes remain unstaged.
