# TOML Provider Configuration Design

**Goal:** Replace the single hard-coded upstream with a TOML-defined registry of providers, public model mappings, and built-in response adapters.

## Configuration

The application loads `CONFIG_PATH`, defaulting to `config.toml` in the repository root. Runtime settings remain top-level fields:

```toml
bind_addr = "127.0.0.1:8787"
exchange_log_dir = ".run/exchanges"

[[providers]]
name = "ada"
base_url = "http://ada-cli-golang.ctripcorp.com/coding-plan/openai/v1"
api_key_env = "ADA_API_KEY"
supports_compact = false

[[providers.models]]
public_model = "gpt-5.6-luna"
upstream_model = "DeepSeek-V4-Flash-0731"
response_adapter = "deepseek"
```

Each provider must have a unique non-empty name, base URL, API-key environment variable, and explicit `supports_compact` flag. Each model must have a public name, upstream name, and built-in adapter name. A provider may define one or more models; public model names must be globally unique. Credentials are never stored in TOML.

## Runtime Design

`config.rs` owns TOML deserialization and validation. A provider registry resolves a public model to a route containing the provider name, upstream model, and adapter kind. `main.rs` uses that route to select the provider base URL and environment-resolved API key for `/v1/responses` and `/v1/responses/compact`.

The existing DeepSeek and Standard channel implementations remain built-in adapters. Each model selects one by stable name (`deepseek` or `standard`); unknown names fail during startup. The selected adapter handles request normalization, JSON response normalization, and SSE response normalization. Compact fallback is provider-level and uses the selected model through the normal Responses endpoint.

When `supports_compact = false`, the proxy skips the upstream compact request and uses the selected upstream model through `/responses`. When it is `true`, the proxy tries `/responses/compact` first and falls back to that same model-level `/responses` path on transport errors, `404`, other non-success responses, or invalid compact JSON. The fallback adds the compaction prompt, disables tools, aggregates JSON or SSE output, and returns the compact endpoint's `{model, output}` envelope.

`/v1/models` returns the union of all configured public models. Missing config, missing API keys, duplicate names, invalid URLs, empty model lists, and unknown adapters are startup errors with actionable messages.

## Testing

Unit tests cover valid multi-provider TOML, each validation failure, route/provider lookup, adapter selection, public model aggregation, the model fallback request body, compact envelope conversion, and streamed model output aggregation.
