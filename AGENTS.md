# Repository Guidelines

## Project Structure & Module Organization

This repository is a Rust 2024 Codex-oriented local LLM proxy. It takes over Codex `base_url` + `auth.json` while running, forwards `/v1/responses` (+ compact), and bridges three upstream API formats back to the OpenAI Responses shape Codex expects. Entry point and routes are in `src/main.rs`; library surface is `src/lib.rs`.

- `src/config.rs`: TOML config (`active_provider`, `[[providers]]` with `api_format` / `upstream_model`).
- `src/codex_live.rs`: apply/restore of Codex `base_url`, forced `wire_api = "responses"`, and `OPENAI_API_KEY` placeholder.
- `src/proxy/`: protocol bridges ported from cc-switch (Responses / Chat Completions / Anthropic Messages, SSE rewriting).
- `src/exchange.rs`: request/response exchange logging under `.run/`.
- `config.toml`: local credentials (gitignored; copy from `config.example.toml`).

## Architecture & Provider Configuration

Configure one active provider and a list of providers in TOML. Codex only sees the local proxy (Responses wire). The proxy selects the real upstream via `active_provider` (hot-switchable). Supported `api_format` values:

- `openai_responses` — passthrough Responses upstream (`/responses`, `/responses/compact`)
- `openai_chat` — Responses ⇄ Chat Completions (including Compact → `/chat/completions`)
- `anthropic` — Responses ⇄ Anthropic Messages (including Compact → `/v1/messages`)

Compact is always accepted on the local entry (same as cc-switch) and follows the active provider's native Responses passthrough or protocol bridge.

Hot-switch without restarting Codex:

```bash
./llpx providers
./llpx use mmkg-chat
```

or `POST /v1/admin/active` with `{"name":"..."}` (also persists the active provider in the JSON store).

Runtime files belong under `.run/` (gitignored). `./start.sh` applies Codex live takeover (`base_url` + `wire_api=responses` + auth placeholder); `./stop.sh` restores it. Set `LLPX_SKIP_CODEX_LIVE=1` to skip takeover.

## Build, Test, and Development Commands

```bash
cargo build                 # Build the debug proxy binary
cargo test                  # Run unit tests
cargo fmt --check           # Verify Rust formatting
cargo clippy --all-targets  # Run Rust lints
./start.sh                  # Build, apply Codex live config, listen on 127.0.0.1:8787
./stop.sh                   # Restore Codex config and stop the proxy
./llpx status               # Show proxy health / active provider
./llpx providers            # List configured providers
./llpx use <name>           # Hot-switch active upstream provider
```

Load `CONFIG_PATH` (default `config.toml`). Optional `BIND_ADDR` / `EXCHANGE_LOG_DIR` override TOML. Some upstreams (e.g. Cloudflare) require a Codex-like `User-Agent`; the proxy sets a default when missing.

## Coding Style & Naming Conventions

Use `rustfmt` defaults, four-space indentation, idiomatic Rust naming, and small helpers for JSON transforms. Keep protocol quirks documented near the bridge code. Prefer extending `src/proxy/` over reintroducing the old channel/adapter route table.

## Testing Guidelines

Use `#[cfg(test)]` modules colocated with the code they exercise. Name tests by behavior. Add regression coverage for each request/response/SSE bridge change, then run `cargo test`.

## Commit & Pull Request Guidelines

Follow Conventional Commits (`feat:`, `fix:`, …). Do not commit API keys, `config.toml`, `.env`, `.run/`, or generated logs.
