# local-llm-proxy

面向个人本地 LLM 的 Responses API 兼容代理。它把固定的公开模型名映射到不同的 provider 部署，并适配上游请求/响应格式（含 SSE 流式响应）。

## 快速开始

```bash
cp config.example.toml config.toml   # 填入真实 base_url / api_key
./start.sh                           # 构建并以 127.0.0.1:8787 启动
./llpx list                          # 查看 provider 与当前路由
./stop.sh                            # 停止并清理 exchange 日志
```

## 配置

`config.toml`（本地、gitignored）定义静态 provider 目录与默认 provider。以下是一个最小示例（完整模板见 `config.example.toml`）：

```toml
bind_addr = "127.0.0.1:8787"
exchange_log_dir = ".run/exchanges"
default_provider = "mapped"

[[providers]]
name = "mapped"
base_url = "https://provider-a.example/v1"
api_key = "replace-me"
supports_compact = false

[[providers.models]]
upstream_model = "upstream-flash"
response_adapter = "deepseek"
```

- `providers`：静态 provider 定义，只声明连接信息、支持的 upstream 模型及各自 `response_adapter`。
- `response_adapter`：`standard` | `deepseek` | `glm`，决定请求/响应的上游适配方式。
- `default_provider`：启动时用于补缺的默认 provider。

## 模型路由

项目把静态 provider 目录与运行时的 public-model 路由表彻底解耦：

- 公开模型固定为 `gpt-5.6-luna`、`gpt-5.6-terra`、`gpt-5.6-sol`，由 `/v1/models` 和请求路由统一接受。
- 动态路由表持久化在 `routes.json`，每个公开模型指向 `provider + upstream_model`：

```json
{
  "routes": {
    "gpt-5.6-luna": {
      "provider": "mapped",
      "upstream_model": "upstream-flash"
    }
  }
}
```

- 启动时对缺少显式路由的公开模型做同名 self 路由补缺：仅当 `default_provider` 声明了同名 upstream model 时才写入，已有路由不受影响。
- provider 只负责连接信息与 adapter，模型到 provider 的映射可运行时调整，无需重启。

## 管理 CLI

`llpx` 支持交互式选择，`ESC` 可逐级回退：

```bash
./start.sh                  # 构建后会在仓库根目录生成 ./llpx
./llpx list
./llpx set gpt-5.6-luna mapped upstream-flash
./llpx unset gpt-5.6-luna
./llpx                        # 交互式向导
./llpx --base <url> list      # 指定代理地址
```

## HTTP 接口

- `GET /v1/models`：返回当前已配置路由的公开模型列表。
- `POST /v1/responses`：Responses API 转发入口。
- `POST /v1/responses/compact`、`POST /compact`：compact 透传入口；provider 不支持 compact 时返回 404。
- `GET /v1/admin/providers`：查看 provider 目录。
- `GET /v1/admin/routes`：查看当前动态路由。
- `PUT /v1/admin/routes/{model}`：设置某个公开模型的 provider 与 upstream model。
- `DELETE /v1/admin/routes/{model}`：删除某个公开模型的路由。

## 环境变量

- `CONFIG_PATH`：配置文件路径（默认 `config.toml`）。
- `BIND_ADDR`：监听地址，覆盖配置中的 `bind_addr`。
- `EXCHANGE_LOG_DIR`：exchange 日志目录（默认 `.run/exchanges`）。
- `ROUTES_PATH`：动态路由表路径（默认 `.run/routes.json`）。

## 开发

```bash
cargo build                 # 构建 debug 二进制
cargo test                  # 运行单元测试
cargo fmt --check           # 检查格式化
cargo clippy --all-targets  # 运行 lint
```

运行期文件统一放在 `.run/` 下，已由 Git 忽略。
