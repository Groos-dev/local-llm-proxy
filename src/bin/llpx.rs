//! llpx — Clack-style wizard + headless CLI for local-llm-proxy.

use cliclack::{confirm, input, intro, outro, select, spinner};
use local_llm_proxy::{
    ApiFormat, LlpxStore, StoredProvider, default_store_path, load_runtime,
    models_fetch::fetch_model_ids,
};
use std::{collections::BTreeMap, env, path::PathBuf, process, process::Command};

const USAGE: &str = "\n\
Usage:\n\
  llpx                      Interactive configure wizard (cliclack)\n\
  llpx configure            Same as interactive wizard\n\
  llpx status               Proxy health\n\
  llpx providers            List providers (store + live)\n\
  llpx use <name>           Hot-switch active provider\n\
  llpx start                Start proxy + Codex live takeover\n\
  llpx stop                 Stop proxy + restore Codex\n\
  llpx provider upsert ...  Non-interactive provider write\n\
  llpx models sync <name>   Fetch /v1/models + identity mappings\n\
  llpx mapping set ...      Set a client model -> upstream model mapping\n\
  llpx --base <url> ...     Override proxy base for live admin calls\n\
\n\
provider upsert flags:\n\
  --name <id> --base-url <url> --api-key <key>\n\
  --format responses|chat|anthropic [--default-model <id>]\n\
\n\
mapping set arguments:\n\
  <provider> <client-model> <upstream-model>\n\
\n\
Env:\n\
  LLPX_STORE   JSON store path (default ~/.llpx/store.json)\n\
  CONFIG_PATH  TOML to migrate from when store is missing\n\
  LLPX_BASE / PROXY_BASE  live proxy base (default http://127.0.0.1:8787)\n";

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let (base, args) = parse_base(args);
    let base = base.trim_end_matches('/').to_string();
    let root = repo_root();

    let result = match args.first().map(String::as_str) {
        None | Some("configure") => run_wizard(&base, &root).await,
        Some("status") => status(&base).await,
        Some("providers") => providers_cmd(&base).await,
        Some("use") => {
            let name = args.get(1).cloned().unwrap_or_default();
            if name.is_empty() {
                eprint!("missing provider name\n{USAGE}");
                process::exit(1);
            }
            use_provider(&base, &name).await
        }
        Some("start") => start_proxy(&root),
        Some("stop") => stop_proxy(&root),
        Some("provider") => match args.get(1).map(String::as_str) {
            Some("upsert") => provider_upsert(&args[2..]),
            _ => {
                eprint!(
                    "usage: llpx provider upsert --name .. --base-url .. --api-key .. --format ..\n"
                );
                process::exit(1);
            }
        },
        Some("models") => match args.get(1).map(String::as_str) {
            Some("sync") => {
                let name = args.get(2).cloned().unwrap_or_default();
                if name.is_empty() {
                    eprint!("usage: llpx models sync <provider-name>\n");
                    process::exit(1);
                }
                match models_sync(&name).await {
                    Ok(()) => refresh_live_provider_if_active(&base, &name).await,
                    Err(err) => Err(err),
                }
            }
            _ => {
                eprint!("usage: llpx models sync <provider-name>\n");
                process::exit(1);
            }
        },
        Some("mapping") => match args.get(1).map(String::as_str) {
            Some("set") if args.len() == 5 => match mapping_set(&args[2], &args[3], &args[4]) {
                Ok(()) => refresh_live_provider_if_active(&base, &args[2]).await,
                Err(err) => Err(err),
            },
            _ => {
                eprint!("usage: llpx mapping set <provider> <client-model> <upstream-model>\n");
                process::exit(1);
            }
        },
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            return;
        }
        Some(other) => {
            eprint!("unknown command: {other}\n{USAGE}");
            process::exit(1);
        }
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        process::exit(1);
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn store_and_toml(root: &PathBuf) -> (PathBuf, PathBuf) {
    let store = env::var_os("LLPX_STORE")
        .map(PathBuf::from)
        .unwrap_or_else(default_store_path);
    let toml = env::var_os("CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("config.toml"));
    (store, toml)
}

fn load_or_migrate(root: &PathBuf) -> Result<(LlpxStore, PathBuf), String> {
    let (store_path, toml) = store_and_toml(root);
    load_runtime(&store_path, Some(&toml)).map_err(|e| e.to_string())
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

async fn status(base: &str) -> Result<(), String> {
    let client = reqwest::Client::new();
    let health = client
        .get(format!("{base}/health"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    println!("health: {}", health.status());
    println!("{}", health.text().await.map_err(|e| e.to_string())?);
    Ok(())
}

async fn providers_cmd(base: &str) -> Result<(), String> {
    let root = repo_root();
    let (store, path) = load_or_migrate(&root)?;
    println!("store: {}", path.display());
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "active": store.active_provider,
            "providers": store.providers.iter().map(|p| serde_json::json!({
                "name": p.name,
                "api_format": p.api_format.as_str(),
                "base_url": p.base_url,
                "default_upstream_model": p.default_upstream_model,
                "mappings": p.model_mappings.len(),
            })).collect::<Vec<_>>(),
        }))
        .map_err(|e| e.to_string())?
    );
    // Best-effort live view
    let client = reqwest::Client::new();
    if let Ok(resp) = client
        .get(format!("{base}/v1/admin/providers"))
        .send()
        .await
    {
        if resp.status().is_success() {
            println!("live: {}", resp.text().await.unwrap_or_default());
        }
    }
    Ok(())
}

async fn use_provider(base: &str, name: &str) -> Result<(), String> {
    let root = repo_root();
    let (mut store, path) = load_or_migrate(&root)?;
    store.set_active(name).map_err(|e| e.to_string())?;
    store.save(&path).map_err(|e| e.to_string())?;
    println!("store active → {name} ({})", path.display());

    let client = reqwest::Client::new();
    match client
        .post(format!("{base}/v1/admin/active"))
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            println!("live: {status}");
            println!("{body}");
            if !status.is_success() {
                return Err(format!("live provider switch failed: {status}"));
            }
        }
        Err(err) => {
            println!("note: store updated; proxy not reachable ({err})");
        }
    }
    Ok(())
}

fn start_proxy(root: &PathBuf) -> Result<(), String> {
    // Ensure store exists (migrate toml if needed) before start.sh loads config.
    let (_, store_path) = load_or_migrate(root)?;
    println!("using store {}", store_path.display());
    // Point proxy at JSON store.
    let status = Command::new(root.join("start.sh"))
        .current_dir(root)
        .env("LLPX_STORE", &store_path)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("start.sh exited {status}"));
    }
    Ok(())
}

fn stop_proxy(root: &PathBuf) -> Result<(), String> {
    let status = Command::new(root.join("stop.sh"))
        .current_dir(root)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("stop.sh exited {status}"));
    }
    Ok(())
}

fn provider_upsert(args: &[String]) -> Result<(), String> {
    let mut name = None;
    let mut base_url = None;
    let mut api_key = None;
    let mut format = None;
    let mut default_model = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                name = Some(args.get(i + 1).cloned().ok_or("missing --name value")?);
                i += 2;
            }
            "--base-url" => {
                base_url = Some(args.get(i + 1).cloned().ok_or("missing --base-url value")?);
                i += 2;
            }
            "--api-key" => {
                api_key = Some(args.get(i + 1).cloned().ok_or("missing --api-key value")?);
                i += 2;
            }
            "--format" => {
                format = Some(args.get(i + 1).cloned().ok_or("missing --format value")?);
                i += 2;
            }
            "--default-model" => {
                default_model = Some(
                    args.get(i + 1)
                        .cloned()
                        .ok_or("missing --default-model value")?,
                );
                i += 2;
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    let name = name.ok_or("missing --name")?;
    let base_url = base_url.ok_or("missing --base-url")?;
    let api_key = api_key.ok_or("missing --api-key")?;
    let format_raw = format.unwrap_or_else(|| "responses".into());
    let api_format =
        ApiFormat::parse(&format_raw).ok_or_else(|| format!("invalid --format {format_raw}"))?;

    let root = repo_root();
    let (mut store, path) = match load_or_migrate(&root) {
        Ok(v) => v,
        Err(_) => {
            let path = env::var_os("LLPX_STORE")
                .map(PathBuf::from)
                .unwrap_or_else(default_store_path);
            (LlpxStore::empty(&name), path)
        }
    };

    let mut mappings = BTreeMap::new();
    if let Some(m) = default_model.as_ref() {
        mappings.insert(m.clone(), m.clone());
    }
    store.upsert_provider(StoredProvider {
        name: name.clone(),
        base_url,
        api_key,
        api_format,
        default_upstream_model: default_model,
        model_mappings: mappings,
        max_output_tokens: None,
    });
    store.set_active(&name).map_err(|e| e.to_string())?;
    store.save(&path).map_err(|e| e.to_string())?;
    println!("upserted provider '{name}' → {}", path.display());
    Ok(())
}

async fn models_sync(name: &str) -> Result<(), String> {
    let root = repo_root();
    let (mut store, path) = load_or_migrate(&root)?;
    let provider = store
        .get(name)
        .cloned()
        .ok_or_else(|| format!("provider '{name}' not found"))?;
    let spin = spinner();
    spin.start(format!("fetching models from {} …", provider.base_url));
    let ids = fetch_model_ids(&provider)
        .await
        .map_err(|e| e.to_string())?;
    spin.stop(format!("found {} models", ids.len()));
    let p = store
        .get_mut(name)
        .ok_or_else(|| format!("provider '{name}' missing"))?;
    p.apply_identity_mappings_from_models(&ids);
    store.save(&path).map_err(|e| e.to_string())?;
    println!(
        "synced identity mappings for '{name}' ({} entries) → {}",
        store.get(name).map(|p| p.model_mappings.len()).unwrap_or(0),
        path.display()
    );
    Ok(())
}

async fn refresh_live_provider_if_active(base: &str, name: &str) -> Result<(), String> {
    let root = repo_root();
    let (store, _) = load_or_migrate(&root)?;
    if store.active_provider == name {
        use_provider(base, name).await?;
    }
    Ok(())
}

fn mapping_set(
    provider_name: &str,
    client_model: &str,
    upstream_model: &str,
) -> Result<(), String> {
    if client_model.trim().is_empty() || upstream_model.trim().is_empty() {
        return Err("model names must not be empty".into());
    }
    let root = repo_root();
    let (mut store, path) = load_or_migrate(&root)?;
    let provider = store
        .get_mut(provider_name)
        .ok_or_else(|| format!("provider '{provider_name}' not found"))?;
    provider
        .model_mappings
        .insert(client_model.trim().into(), upstream_model.trim().into());
    store.save(&path).map_err(|e| e.to_string())?;
    println!(
        "mapped {client_model} -> {upstream_model} for '{provider_name}' ({})",
        path.display()
    );
    Ok(())
}

async fn run_wizard(base: &str, root: &PathBuf) -> Result<(), String> {
    intro("local-llm-proxy").map_err(|e| e.to_string())?;
    let (mut store, store_path) = match load_or_migrate(root) {
        Ok(v) => v,
        Err(_) => {
            let path = env::var_os("LLPX_STORE")
                .map(PathBuf::from)
                .unwrap_or_else(default_store_path);
            (LlpxStore::empty("default"), path)
        }
    };

    loop {
        let action = select("What do you want to do?")
            .item("start", "Start proxy (Codex live takeover)", "")
            .item("stop", "Stop proxy (restore Codex)", "")
            .item("switch", "Hot-switch active provider", "")
            .item("add", "Add provider", "")
            .item("edit", "Edit provider", "")
            .item("sync", "Sync models (/v1/models → identity map)", "")
            .item("mapping", "Edit model mapping", "")
            .item("status", "Show status", "")
            .item("quit", "Quit", "")
            .interact()
            .map_err(|e| e.to_string())?;

        match action {
            "start" => {
                store.save(&store_path).map_err(|e| e.to_string())?;
                start_proxy(root)?;
            }
            "stop" => stop_proxy(root)?,
            "switch" => {
                if store.providers.is_empty() {
                    cliclack::log::warning("No providers yet — add one first.")
                        .map_err(|e| e.to_string())?;
                    continue;
                }
                let mut sel = select("Active provider");
                for p in &store.providers {
                    let hint = format!("{} · {}", p.api_format.as_str(), p.base_url);
                    sel = sel.item(p.name.as_str(), p.name.as_str(), hint);
                }
                let name = sel.interact().map_err(|e| e.to_string())?.to_string();
                store.set_active(&name).map_err(|e| e.to_string())?;
                store.save(&store_path).map_err(|e| e.to_string())?;
                use_provider(base, &name).await?;
            }
            "add" => {
                let provider = prompt_provider(None)?;
                let name = provider.name.clone();
                store.upsert_provider(provider);
                if confirm(format!("Set '{name}' as active provider?"))
                    .initial_value(true)
                    .interact()
                    .map_err(|e| e.to_string())?
                {
                    store.set_active(&name).map_err(|e| e.to_string())?;
                }
                store.save(&store_path).map_err(|e| e.to_string())?;
                if store.active_provider == name {
                    use_provider(base, &name).await?;
                }
                cliclack::log::success(format!("Saved {name} → {}", store_path.display()))
                    .map_err(|e| e.to_string())?;

                if confirm("Fetch /v1/models and seed identity mappings now?")
                    .initial_value(true)
                    .interact()
                    .map_err(|e| e.to_string())?
                {
                    let _ = models_sync(&name).await.map_err(|e| {
                        let _ = cliclack::log::warning(format!("models sync failed: {e}"));
                        e
                    });
                    // reload after sync
                    if let Ok((s, _)) = load_or_migrate(root) {
                        store = s;
                    }
                }
            }
            "edit" => {
                if store.providers.is_empty() {
                    cliclack::log::warning("No providers yet — add one first.")
                        .map_err(|e| e.to_string())?;
                    continue;
                }
                let mut sel = select("Provider to edit");
                for p in &store.providers {
                    let hint = format!("{} · {}", p.api_format.as_str(), p.base_url);
                    sel = sel.item(p.name.as_str(), p.name.as_str(), hint);
                }
                let selected = sel.interact().map_err(|e| e.to_string())?.to_string();
                let existing = store.get(&selected).cloned().unwrap();
                let provider = prompt_provider(Some(&existing))?;
                let name = provider.name.clone();
                store
                    .rename_provider(&selected, provider)
                    .map_err(|e| e.to_string())?;
                store.save(&store_path).map_err(|e| e.to_string())?;
                if store.active_provider == name {
                    use_provider(base, &name).await?;
                }
                cliclack::log::success(format!("Saved {name} → {}", store_path.display()))
                    .map_err(|e| e.to_string())?;
            }
            "sync" => {
                if store.providers.is_empty() {
                    cliclack::log::warning("No providers yet.").map_err(|e| e.to_string())?;
                    continue;
                }
                let mut sel = select("Provider to sync");
                for p in &store.providers {
                    sel = sel.item(p.name.as_str(), p.name.as_str(), p.base_url.as_str());
                }
                let name = sel.interact().map_err(|e| e.to_string())?.to_string();
                models_sync(&name).await?;
                if let Ok((s, _)) = load_or_migrate(root) {
                    store = s;
                }
                refresh_live_provider_if_active(base, &name).await?;
            }
            "mapping" => {
                if store.providers.is_empty() {
                    cliclack::log::warning("No providers yet.").map_err(|e| e.to_string())?;
                    continue;
                }
                let mut sel = select("Provider mapping");
                for p in &store.providers {
                    sel = sel.item(p.name.as_str(), p.name.as_str(), p.base_url.as_str());
                }
                let provider_name = sel.interact().map_err(|e| e.to_string())?.to_string();
                let provider = store.get(&provider_name).unwrap();
                let client_model: String = input("Codex/client model")
                    .validate(|v: &String| {
                        if v.trim().is_empty() {
                            Err("required")
                        } else {
                            Ok(())
                        }
                    })
                    .interact()
                    .map_err(|e| e.to_string())?;
                let default_upstream = provider
                    .model_mappings
                    .get(client_model.trim())
                    .map(String::as_str)
                    .or(provider.default_upstream_model.as_deref())
                    .unwrap_or("");
                let upstream_model: String = input("Upstream model")
                    .default_input(default_upstream)
                    .validate(|v: &String| {
                        if v.trim().is_empty() {
                            Err("required")
                        } else {
                            Ok(())
                        }
                    })
                    .interact()
                    .map_err(|e| e.to_string())?;
                mapping_set(&provider_name, &client_model, &upstream_model)?;
                if let Ok((s, _)) = load_or_migrate(root) {
                    store = s;
                }
                refresh_live_provider_if_active(base, &provider_name).await?;
            }
            "status" => {
                cliclack::log::info(format!(
                    "store={} active={}",
                    store_path.display(),
                    store.active_provider
                ))
                .map_err(|e| e.to_string())?;
                let _ = status(base).await;
            }
            "quit" => break,
            _ => {}
        }
    }

    outro("Done.").map_err(|e| e.to_string())?;
    Ok(())
}

fn prompt_provider(existing: Option<&StoredProvider>) -> Result<StoredProvider, String> {
    let default_name = existing.map(|p| p.name.as_str()).unwrap_or("");
    let name: String = input("Provider name")
        .default_input(default_name)
        .validate(|v: &String| {
            if v.trim().is_empty() {
                Err("required")
            } else {
                Ok(())
            }
        })
        .interact()
        .map_err(|e| e.to_string())?;

    let base_url: String = input("Base URL")
        .default_input(existing.map(|p| p.base_url.as_str()).unwrap_or("https://"))
        .validate(|v: &String| {
            if !(v.starts_with("http://") || v.starts_with("https://")) {
                Err("must start with http:// or https://")
            } else {
                Ok(())
            }
        })
        .interact()
        .map_err(|e| e.to_string())?;

    let api_key: String = input("API key")
        .default_input(existing.map(|p| p.api_key.as_str()).unwrap_or(""))
        .validate(|v: &String| {
            if v.trim().is_empty() {
                Err("required")
            } else {
                Ok(())
            }
        })
        .interact()
        .map_err(|e| e.to_string())?;

    let format = select("Upstream protocol")
        .initial_value(match existing.map(|p| &p.api_format) {
            Some(ApiFormat::OpenaiChat) => "chat",
            Some(ApiFormat::Anthropic) => "anthropic",
            _ => "responses",
        })
        .item(
            "responses",
            "OpenAI Responses (passthrough, default)",
            "no conversion",
        )
        .item("chat", "OpenAI Chat Completions", "Responses ⇄ Chat bridge")
        .item(
            "anthropic",
            "Anthropic Messages",
            "Responses ⇄ Anthropic bridge",
        )
        .interact()
        .map_err(|e| e.to_string())?;
    let api_format = ApiFormat::parse(format).unwrap_or_default();

    let default_model: String = input("Default upstream model (optional)")
        .default_input(
            existing
                .and_then(|p| p.default_upstream_model.as_deref())
                .unwrap_or(""),
        )
        .interact()
        .map_err(|e| e.to_string())?;
    let default_upstream_model = {
        let t = default_model.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    };

    let mut model_mappings = existing
        .map(|p| p.model_mappings.clone())
        .unwrap_or_default();
    if let Some(m) = default_upstream_model.as_ref() {
        model_mappings.entry(m.clone()).or_insert_with(|| m.clone());
    }

    Ok(StoredProvider {
        name: name.trim().to_string(),
        base_url: base_url.trim().trim_end_matches('/').to_string(),
        api_key: api_key.trim().to_string(),
        api_format,
        default_upstream_model,
        model_mappings,
        max_output_tokens: existing.and_then(|p| p.max_output_tokens),
    })
}
