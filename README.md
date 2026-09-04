# AgentProxy

Local Codex Responses proxy (formerly local-llm-proxy / llpx).

面向 Codex 的本地 Responses API 代理。Codex 始终只连接本地入口，代理按当前 provider 将请求透传到 Responses，或转换为 Chat Completions / Anthropic Messages，并保留 SSE 与工具调用能力。

## 快速开始

```bash
cp config.example.toml config.toml   # 填入 provider 连接信息
cargo build --bins
./agent-proxy configure                     # 层级式交互 TUI
./agent-proxy start                         # 启动代理；Codex 为 ACTIVE 时接管连接配置
./agent-proxy providers                     # 查看 JSON store 与运行时 provider
./agent-proxy use <provider>                # 运行中热切换 provider
./agent-proxy stop                          # 停止并还原 Codex 配置
```

## 配置

`config.toml`（本地、gitignored）用于首次迁移。后续 provider 和模型映射持久化在 `~/.agent-proxy/store.json`（可用 `AGENT_PROXY_STORE` 覆盖）。以下是一个最小示例（完整模板见 `config.example.toml`）：

```toml
bind_addr = "127.0.0.1:8787"
exchange_log_dir = ".run/exchanges"
active_provider = "mapped"

[[providers]]
name = "mapped"
base_url = "https://provider-a.example/v1"
api_key = "replace-me"
api_format = "openai_responses"
upstream_model = "gpt-5.4"
```

- `active_provider`：启动时使用的 provider；运行中可通过 `agent-proxy use` 或管理 API 热切换。
- `api_format`：`openai_responses`（默认透传）、`openai_chat`（Chat Completions bridge）或 `anthropic`（Messages bridge）。
- `upstream_model`：没有模型映射时使用的上游模型。
- TUI 中 Codex 配置为 `ACTIVE` 时，启动会改写 Codex 的 `base_url`、`wire_api` 和 `auth.json` 中的 `OPENAI_API_KEY`；`INACTIVE` 时代理仍可启动，但不会改写这些文件。

## 模型路由

`~/.agent-proxy/store.json` 中的 `model_mappings` 保存 Codex/client model → upstream model 映射。新增 provider 时可设置默认模型；`agent-proxy models sync <provider>` 会调用 provider 的模型列表接口，并为返回的模型建立原名映射。也可以直接设置映射：

```bash
./agent-proxy mapping set <provider> <client-model> <upstream-model>
```

`GET /v1/models` 返回当前 provider 的 client model 映射键，Codex 的请求模型按映射后再转发。

## 管理 CLI

`agent-proxy` 使用 `cliclack` 提供实时刷新的层级式 TUI：入口先选择 Codex 或 Claude Code；进入 Codex 后可管理配置激活状态、Providers、模型 Mapping，以及启动/停止代理。Provider 列表和 Mapping 会在返回菜单时立即刷新，操作本身不会留下日志行：

```bash
./agent-proxy configure
./agent-proxy provider upsert --name mmkg --base-url https://example/v1 --api-key "$KEY" --format responses --default-model gpt-5.4
./agent-proxy models sync mmkg
./agent-proxy mapping set mmkg gpt-5.6-luna upstream-model
./agent-proxy --base http://127.0.0.1:8787 use mmkg
```

## HTTP 接口

- `POST /v1/responses`：Responses API 转发入口（Codex 统一入口）。
- `POST /v1/responses/compact`、`POST /responses/compact`、`POST /compact`（及 `/v1/v1/...`、`/codex/v1/...` 别名）：与 cc-switch 一致——`openai_responses` 透传上游 `/responses/compact`；`openai_chat` / `anthropic` 走与 `/responses` 相同的协议桥（分别打到 `/chat/completions` 与 `/v1/messages`）。
- `GET /health`：健康检查与当前 active provider。
- `GET /v1/admin/providers`：查看 provider 目录与当前 active。
- `POST /v1/admin/active`：热切换 active provider（`{"name":"..."}`）。

## 环境变量

- `CONFIG_PATH`：配置文件路径（默认 `config.toml`）。
- `BIND_ADDR`：监听地址，覆盖配置中的 `bind_addr`。
- `EXCHANGE_LOG_DIR`：exchange 日志目录（默认 `.run/exchanges`）。
- `AGENT_PROXY_STORE`：JSON store 路径（默认 `~/.agent-proxy/store.json`）。
- `AGENT_PROXY_CODEX_BACKUP`：Codex live backup 路径（默认 `.run/codex-live-backup.json`）。
- `AGENT_PROXY_SKIP_CODEX_LIVE=1`：启动代理时跳过 Codex 配置接管。

## 开发

```bash
cargo build                 # 构建 debug 二进制
cargo test                  # 运行单元测试
cargo fmt --check           # 检查格式化
cargo clippy --all-targets  # 运行 lint
```

运行期文件统一放在 `.run/` 下，已由 Git 忽略。
