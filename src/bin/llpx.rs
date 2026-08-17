use dialoguer::Select;
use serde_json::{Value, json};
use std::{env, process};

const USAGE: &str = "\n\
Usage:\n\
  llpx                         # interactive wizard\n\
  llpx wizard                  # interactive wizard\n\
  llpx list\n\
  llpx set                     # interactive: choose model/provider/upstream model\n\
  llpx set <public_model>      # interactive: choose provider/upstream model\n\
  llpx set <model> <provider> <upstream_model>\n\
  llpx unset                   # interactive: choose a routed model to remove\n\
  llpx unset <public_model>\n\
  llpx --base <url> <command>\n\
\n\
Dynamic model routing for local-llm-proxy.\n\
\n\
Base URL resolution (first match wins):\n\
  1. --base <url>\n\
  2. LLPX_BASE\n\
  3. PROXY_BASE\n\
  4. http://127.0.0.1:8787\n";

const PUBLIC_MODELS: [&str; 3] = ["gpt-5.6-luna", "gpt-5.6-terra", "gpt-5.6-sol"];

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let (base, args) = parse_base(args);
    let base = base.trim_end_matches('/').to_string();

    let result = if args.is_empty() {
        wizard(&base).await
    } else {
        let command = args[0].as_str();
        match command {
            "list" => list(&base).await,
            "wizard" => wizard(&base).await,
            "set" => set_command(&base, &args[1..]).await,
            "unset" => unset_command(&base, &args[1..]).await,
            "-h" | "--help" | "help" => {
                print!("{USAGE}");
                return;
            }
            _ => {
                eprint!("unknown command: {command}\n{USAGE}");
                process::exit(1);
            }
        }
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        process::exit(1);
    }
}

fn parse_base(mut args: Vec<String>) -> (String, Vec<String>) {
    if args.first().map(String::as_str) == Some("--base") {
        if args.len() < 3 {
            eprint!("missing value for --base\n{USAGE}");
            process::exit(1);
        }
        let base = args[1].clone();
        args.drain(0..2);
        return (base, args);
    }
    let base = env::var("LLPX_BASE")
        .or_else(|_| env::var("PROXY_BASE"))
        .unwrap_or_else(|_| "http://127.0.0.1:8787".to_string());
    (base, args)
}

async fn list(base: &str) -> Result<(), String> {
    let providers = get_json(base, "/v1/admin/providers").await?;
    let routes = get_json(base, "/v1/admin/routes").await?;

    println!("providers:");
    for provider in providers["providers"].as_array().unwrap_or(&vec![]) {
        let name = provider["name"].as_str().unwrap_or("");
        let compact = provider["supports_compact"].as_bool().unwrap_or(false);
        println!("  {name}  compact={compact}");
        for model in provider["models"].as_array().unwrap_or(&vec![]) {
            let upstream = model["upstream_model"].as_str().unwrap_or("");
            let adapter = model["response_adapter"].as_str().unwrap_or("");
            println!("    - {upstream}  adapter={adapter}");
        }
    }

    println!("routes:");
    let routes_obj = routes["routes"].as_object();
    if routes_obj.map(|m| m.is_empty()).unwrap_or(true) {
        println!("  (empty)");
        return Ok(());
    }
    for (model, route) in routes_obj.unwrap() {
        let provider = route["provider"].as_str().unwrap_or("");
        let upstream = route["upstream_model"].as_str().unwrap_or("");
        println!("  {model} -> {provider}/{upstream}");
    }
    Ok(())
}

async fn set_command(base: &str, args: &[String]) -> Result<(), String> {
    match args.len() {
        0 => wizard(base).await,
        1 => {
            let model = &args[0];
            ensure_public_model(model)?;
            let providers = get_json(base, "/v1/admin/providers").await?;
            let routes = get_json(base, "/v1/admin/routes").await?;
            let Some((model, provider, upstream)) = choose_route(
                &providers,
                &routes,
                InteractiveStep::Provider {
                    public_model: model.clone(),
                    can_return_to_model: false,
                },
            )?
            else {
                return Ok(());
            };
            set_route(base, &model, &provider, &upstream).await
        }
        3 => {
            let model = &args[0];
            ensure_public_model(model)?;
            set_route(base, model, &args[1], &args[2]).await
        }
        _ => {
            eprint!("invalid arguments for set\n{USAGE}");
            process::exit(1);
        }
    }
}

async fn unset_command(base: &str, args: &[String]) -> Result<(), String> {
    match args.len() {
        0 => {
            let routes = get_json(base, "/v1/admin/routes").await?;
            let Some(model) = select_routed_model(&routes)? else {
                return Ok(());
            };
            unset_route(base, &model).await
        }
        1 => {
            ensure_public_model(&args[0])?;
            unset_route(base, &args[0]).await
        }
        _ => {
            eprint!("invalid arguments for unset\n{USAGE}");
            process::exit(1);
        }
    }
}

async fn wizard(base: &str) -> Result<(), String> {
    let providers = get_json(base, "/v1/admin/providers").await?;
    let routes = get_json(base, "/v1/admin/routes").await?;
    let Some((model, provider, upstream)) =
        choose_route(&providers, &routes, InteractiveStep::PublicModel)?
    else {
        return Ok(());
    };
    set_route(base, &model, &provider, &upstream).await
}

async fn set_route(base: &str, model: &str, provider: &str, upstream: &str) -> Result<(), String> {
    let body = json!({ "provider": provider, "upstream_model": upstream });
    let resp = put_json(base, &format!("/v1/admin/routes/{model}"), body).await?;
    print_routes(&resp);
    Ok(())
}

async fn unset_route(base: &str, model: &str) -> Result<(), String> {
    let resp = delete_json(base, &format!("/v1/admin/routes/{model}")).await?;
    print_routes(&resp);
    Ok(())
}

fn ensure_public_model(model: &str) -> Result<(), String> {
    if PUBLIC_MODELS.contains(&model) {
        Ok(())
    } else {
        Err(format!("unsupported public model '{model}'"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum InteractiveStep {
    PublicModel,
    Provider {
        public_model: String,
        can_return_to_model: bool,
    },
    UpstreamModel {
        public_model: String,
        provider: String,
        provider_can_return_to_model: bool,
    },
}

fn previous_step(step: InteractiveStep) -> Option<InteractiveStep> {
    match step {
        InteractiveStep::PublicModel => None,
        InteractiveStep::Provider {
            can_return_to_model: true,
            ..
        } => Some(InteractiveStep::PublicModel),
        InteractiveStep::Provider {
            can_return_to_model: false,
            ..
        } => None,
        InteractiveStep::UpstreamModel {
            public_model,
            provider: _,
            provider_can_return_to_model,
        } => Some(InteractiveStep::Provider {
            public_model,
            can_return_to_model: provider_can_return_to_model,
        }),
    }
}

fn choose_route(
    providers: &Value,
    routes: &Value,
    mut step: InteractiveStep,
) -> Result<Option<(String, String, String)>, String> {
    loop {
        step = match step {
            InteractiveStep::PublicModel => match select_public_model()? {
                Some(public_model) => InteractiveStep::Provider {
                    public_model,
                    can_return_to_model: true,
                },
                None => return Ok(None),
            },
            InteractiveStep::Provider {
                public_model,
                can_return_to_model,
            } => match select_provider(providers)? {
                Some(provider) => InteractiveStep::UpstreamModel {
                    public_model,
                    provider,
                    provider_can_return_to_model: can_return_to_model,
                },
                None => {
                    let current = InteractiveStep::Provider {
                        public_model,
                        can_return_to_model,
                    };
                    let Some(previous) = previous_step(current) else {
                        return Ok(None);
                    };
                    previous
                }
            },
            InteractiveStep::UpstreamModel {
                public_model,
                provider,
                provider_can_return_to_model,
            } => {
                let default = current_upstream(routes, &public_model);
                match select_upstream_model(providers, &provider, Some(&default))? {
                    Some(upstream_model) => {
                        return Ok(Some((public_model, provider, upstream_model)));
                    }
                    None => previous_step(InteractiveStep::UpstreamModel {
                        public_model,
                        provider,
                        provider_can_return_to_model,
                    })
                    .expect("upstream model always has a provider parent"),
                }
            }
        };
    }
}

fn select_public_model() -> Result<Option<String>, String> {
    let models: Vec<String> = PUBLIC_MODELS.iter().map(|m| m.to_string()).collect();
    select_items("Select public model", &models, None)
}

fn select_provider(providers: &Value) -> Result<Option<String>, String> {
    let names: Vec<String> = providers["providers"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|p| p["name"].as_str())
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        return Err("no providers configured".to_string());
    }
    select_items("Select provider", &names, None)
}

fn select_upstream_model(
    providers: &Value,
    provider: &str,
    default: Option<&str>,
) -> Result<Option<String>, String> {
    let models: Vec<String> = providers["providers"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .find(|p| p["name"].as_str() == Some(provider))
        .map(|p| {
            p["models"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|m| m["upstream_model"].as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if models.is_empty() {
        return Err(format!("provider '{provider}' has no models"));
    }
    let default_idx = default.and_then(|d| models.iter().position(|m| m == d));
    select_items("Select upstream model", &models, default_idx)
}

fn select_routed_model(routes: &Value) -> Result<Option<String>, String> {
    let mut models: Vec<String> = routes["routes"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    if models.is_empty() {
        return Err("no routes configured".to_string());
    }
    models.sort();
    select_items("Select model to remove", &models, None)
}

fn current_upstream(routes: &Value, model: &str) -> String {
    routes["routes"][model]["upstream_model"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

fn select_items(
    prompt: &str,
    items: &[String],
    default: Option<usize>,
) -> Result<Option<String>, String> {
    let mut select = Select::new().with_prompt(prompt).items(items);
    if let Some(idx) = default {
        select = select.default(idx);
    }
    let idx = select.interact_opt().map_err(|e| e.to_string())?;
    Ok(idx.and_then(|idx| items.get(idx).cloned()))
}

fn print_routes(resp: &Value) {
    println!("routes:");
    let routes = resp["routes"].as_object();
    if routes.map(|m| m.is_empty()).unwrap_or(true) {
        println!("  (empty)");
        return;
    }
    for (model, route) in routes.unwrap() {
        let provider = route["provider"].as_str().unwrap_or("");
        let upstream = route["upstream_model"].as_str().unwrap_or("");
        println!("  {model} -> {provider}/{upstream}");
    }
}

async fn get_json(base: &str, path: &str) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base}{path}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = ensure_status(resp).await?;
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

async fn put_json(base: &str, path: &str, body: Value) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{base}{path}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = ensure_status(resp).await?;
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

async fn delete_json(base: &str, path: &str) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let resp = client
        .delete(format!("{base}{path}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = ensure_status(resp).await?;
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

async fn ensure_status(resp: reqwest::Response) -> Result<reqwest::Response, String> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(format!("HTTP {status}: {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_from_upstream_model_returns_to_provider() {
        let step = InteractiveStep::UpstreamModel {
            public_model: "gpt-5.6-luna".to_string(),
            provider: "ada".to_string(),
            provider_can_return_to_model: true,
        };

        assert_eq!(
            previous_step(step),
            Some(InteractiveStep::Provider {
                public_model: "gpt-5.6-luna".to_string(),
                can_return_to_model: true,
            })
        );
    }

    #[test]
    fn esc_from_provider_in_wizard_returns_to_public_model() {
        let step = InteractiveStep::Provider {
            public_model: "gpt-5.6-terra".to_string(),
            can_return_to_model: true,
        };

        assert_eq!(previous_step(step), Some(InteractiveStep::PublicModel));
    }

    #[test]
    fn esc_from_public_model_exits_wizard() {
        assert_eq!(previous_step(InteractiveStep::PublicModel), None);
    }

    #[test]
    fn esc_from_provider_for_fixed_public_model_exits_command() {
        let step = InteractiveStep::Provider {
            public_model: "gpt-5.6-sol".to_string(),
            can_return_to_model: false,
        };

        assert_eq!(previous_step(step), None);
    }
}
