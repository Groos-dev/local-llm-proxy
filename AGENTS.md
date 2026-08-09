# Repository Guidelines

## Project Structure & Module Organization

This repository is a Rust 2024 compatibility layer for a personal LLM provider. It maps public models to provider deployments and adapts provider request/response formats, including SSE. The entry point and routes are in `src/main.rs`; reusable adaptation logic is exposed from `src/lib.rs`. Keep related behavior in focused modules:

- `src/model.rs`: public-to-upstream model routing and model list generation.
- `src/request.rs` and `src/response.rs`: request/response normalization.
- `src/channel/`: upstream-specific adaptations, including DeepSeek and standard channels.
- `src/sse.rs`: streaming response rewriting.
- `src/compact.rs`: compact-response fallback behavior.
- `src/exchange.rs`: request/response exchange logging.
- `config.toml`: default provider, model mapping, adapter, and compact capability configuration.

## Architecture & Provider Configuration

Load provider definitions from TOML rather than hard-coding deployment settings. Support multiple providers and one or more models per provider; each model names a built-in Rust response adapter, while compact capability belongs to the provider. Reference adapters by stable names; do not add executable configuration hooks. Runtime files belong under `.run/`, which is ignored by Git.

## Build, Test, and Development Commands

Run these from the repository root:

```bash
cargo build                 # Build the debug proxy binary
cargo test                  # Run unit tests embedded in src modules
cargo fmt --check           # Verify Rust formatting
cargo clippy --all-targets  # Run Rust lints
./start.sh                  # Build and start locally on 127.0.0.1:8787
./stop.sh                   # Stop the proxy and clear exchange logs
```

`./start.sh` loads `CONFIG_PATH` (default `config.toml`) and `.env`; each configured `api_key_env` must be set. Keep endpoints, model mappings, adapter names, and `supports_compact` in TOML; keep credentials in environment variables or another ignored secret store. Override `BIND_ADDR` and `EXCHANGE_LOG_DIR` as needed.

## Coding Style & Naming Conventions

Use `rustfmt` defaults, four-space indentation, idiomatic Rust naming (`snake_case` functions/modules, `UpperCamelCase` types, `SCREAMING_SNAKE_CASE` constants), and explicit small helpers for JSON transformations. Preserve channel-specific behavior behind the existing `ChannelKind`/`UpstreamChannel` boundary. Keep comments focused on non-obvious protocol quirks.

## Testing Guidelines

Use Rust's built-in test framework with `#[cfg(test)]` modules colocated beside the code they exercise. Name tests by behavior, such as `restores_internal_deployment_ids_in_split_sse_events`. Add regression tests for each request, response, compact fallback, or SSE transformation change, then run `cargo test`.

## Commit & Pull Request Guidelines

Follow the concise Conventional Commit style already present in history, for example `feat: add ...` or `fix: handle ...`. Keep commits focused. Pull requests should explain the protocol or configuration impact, list validation commands run, link the relevant issue or task, and include request/response examples when API behavior changes. Do not commit API keys, `.env`, `.run/`, or generated logs.
