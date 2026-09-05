//! agent-proxy — Hierarchical TUI + headless CLI for AgentProxy.

mod tui_nav;

use cliclack::spinner;
use agent_proxy::{
    ApiFormat, AgentProxyStore, StoredProvider, codex_live, default_store_path, load_runtime,
    models_fetch::fetch_model_ids,
};
use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{self, Command, Stdio},
    thread,
    time::Duration,
};
use tui_nav::{Flow, Menu, input_flow};

const USAGE: &str = "\n\
Usage:\n\
  agent-proxy                      Interactive hierarchical TUI\n\
  agent-proxy configure            Open the same interactive TUI\n\
  agent-proxy status               Proxy health\n\
  agent-proxy providers            List providers (store + live)\n\
  agent-proxy use <name>           Hot-switch active provider\n\
  agent-proxy start                Start proxy (Codex takeover when ACTIVE)\n\
  agent-proxy stop                 Stop proxy + restore Codex\n\
  agent-proxy provider upsert ...  Non-interactive provider write\n\
  agent-proxy models sync <name>   Fetch /v1/models + identity mappings\n\
  agent-proxy mapping set ...      Set a client model -> upstream model mapping\n\
  agent-proxy --base <url> ...     Override proxy base for live admin calls\n\
\n\
provider upsert flags:\n\
  --name <id> --base-url <url> --api-key <key>\n\
  --format responses|chat|anthropic [--default-model <id>]\n\
\n\
mapping set arguments:\n\
  <provider> <client-model> <upstream-model>\n\
\n\
Env:\n\
  AGENT_PROXY_STORE   JSON store path (default ~/.agent-proxy/store.json)\n\
  CONFIG_PATH  TOML to migrate from when store is missing\n\
  AGENT_PROXY_BASE / PROXY_BASE  live proxy base (default http://127.0.0.1:8787)\n";

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
            use_provider(&base, &name, false).await
        }
        Some("start") => start_proxy(&root, false),
        Some("stop") => stop_proxy(&root, false),
        Some("provider") => match args.get(1).map(String::as_str) {
            Some("upsert") => provider_upsert(&args[2..]),
            _ => {
                eprint!(
                    "usage: agent-proxy provider upsert --name .. --base-url .. --api-key .. --format ..\n"
                );
                process::exit(1);
            }
        },
        Some("models") => match args.get(1).map(String::as_str) {
            Some("sync") => {
                let name = args.get(2).cloned().unwrap_or_default();
                if name.is_empty() {
                    eprint!("usage: agent-proxy models sync <provider-name>\n");
                    process::exit(1);
                }
                match models_sync(&name, false).await {
                    Ok(()) => refresh_live_provider_if_active(&base, &name, false).await,
                    Err(err) => Err(err),
                }
            }
            _ => {
                eprint!("usage: agent-proxy models sync <provider-name>\n");
                process::exit(1);
            }
        },
        Some("mapping") => match args.get(1).map(String::as_str) {
            Some("set") if args.len() == 5 => {
                match mapping_set(&args[2], &args[3], &args[4], false) {
                    Ok(()) => refresh_live_provider_if_active(&base, &args[2], false).await,
                    Err(err) => Err(err),
                }
            }
            _ => {
                eprint!("usage: agent-proxy mapping set <provider> <client-model> <upstream-model>\n");
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
    let store = env::var_os("AGENT_PROXY_STORE")
        .map(PathBuf::from)
        .unwrap_or_else(default_store_path);
    let toml = env::var_os("CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("config.toml"));
    (store, toml)
}

fn load_or_migrate(root: &PathBuf) -> Result<(AgentProxyStore, PathBuf), String> {
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
    let base = env::var("AGENT_PROXY_BASE")
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
            "codex_active": store.codex_active,
            "providers": store.providers.iter().map(|p| serde_json::json!({
                "name": p.name,
                "api_format": p.api_format.as_str(),
                "base_url": p.base_url,
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

async fn use_provider(base: &str, name: &str, quiet: bool) -> Result<(), String> {
    let root = repo_root();
    let (mut store, path) = load_or_migrate(&root)?;
    store.set_active(name).map_err(|e| e.to_string())?;
    store.save(&path).map_err(|e| e.to_string())?;
    if !quiet {
        println!("store active → {name} ({})", path.display());
    }

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
            if !quiet {
                println!("live: {status}");
                println!("{body}");
            }
            if !status.is_success() {
                return Err(format!("live provider switch failed: {status}"));
            }
        }
        Err(err) => {
            if !quiet {
                println!("note: store updated; proxy not reachable ({err})");
            }
        }
    }
    Ok(())
}

fn start_proxy(root: &PathBuf, quiet: bool) -> Result<(), String> {
    let (store, store_path) = load_or_migrate(root)?;
    let run_dir = root.join(".run");
    fs::create_dir_all(&run_dir).map_err(|e| format!("create {}: {e}", run_dir.display()))?;
    let pid_path = run_dir.join("agent-proxy-server.pid");
    if let Some(pid) = read_pid(&pid_path)? {
        if process_alive(pid) {
            if !quiet {
                println!("already running pid={pid}");
            }
            return Ok(());
        }
        let _ = fs::remove_file(&pid_path);
    }

    let exchange_dir = env::var_os("EXCHANGE_LOG_DIR")
        .map(PathBuf::from)
        .or_else(|| store.exchange_log_dir.clone().map(PathBuf::from))
        .unwrap_or_else(|| run_dir.join("exchanges"));
    let bind_addr = env::var("BIND_ADDR")
        .ok()
        .or(store.bind_addr.clone())
        .unwrap_or_else(|| "127.0.0.1:8787".into());
    let socket_addr: SocketAddr = bind_addr
        .parse()
        .map_err(|e| format!("invalid bind address {bind_addr}: {e}"))?;
    clear_dir(&exchange_dir)?;
    fs::create_dir_all(&exchange_dir)
        .map_err(|e| format!("create {}: {e}", exchange_dir.display()))?;

    let log_path = run_dir.join("agent-proxy-server.log");
    let log = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&log_path)
        .map_err(|e| format!("open {}: {e}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("clone {}: {e}", log_path.display()))?;

    let binary = root.join("target/debug/agent-proxy-server");
    ensure_proxy_binary(root, &binary)?;

    let backup_path = env::var_os("AGENT_PROXY_CODEX_BACKUP")
        .map(PathBuf::from)
        .unwrap_or_else(|| codex_live::default_backup_path(&run_dir));
    if store.codex_active && env::var("AGENT_PROXY_SKIP_CODEX_LIVE").as_deref() != Ok("1") {
        codex_live::apply_takeover(&format!("http://{bind_addr}/v1"), &backup_path)
            .map_err(|e| format!("apply Codex live config failed: {e}"))?;
    }
    let child = Command::new("nohup")
        .arg(&binary)
        .current_dir(root)
        .env("AGENT_PROXY_STORE", &store_path)
        .env("EXCHANGE_LOG_DIR", &exchange_dir)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|e| format!("start {}: {e}", binary.display()))?;
    let pid = child.id();
    fs::write(&pid_path, format!("{pid}\n"))
        .map_err(|e| format!("write {}: {e}", pid_path.display()))?;

    for _ in 0..40 {
        if TcpStream::connect_timeout(&socket_addr, Duration::from_secs(1)).is_ok() {
            if !quiet {
                println!(
                    "started pid={pid} bind={bind_addr} log={} exchanges={}",
                    log_path.display(),
                    exchange_dir.display()
                );
            }
            return Ok(());
        }
        thread::sleep(Duration::from_millis(150));
    }
    Err(format!(
        "started but proxy is not listening on {bind_addr}; pid={pid} log={}",
        log_path.display()
    ))
}

fn stop_proxy(root: &PathBuf, quiet: bool) -> Result<(), String> {
    let run_dir = root.join(".run");
    let pid_path = run_dir.join("agent-proxy-server.pid");
    let runtime_store = load_or_migrate(root).ok().map(|(store, _)| store);
    let mut stopped = false;
    if let Some(pid) = read_pid(&pid_path)? {
        if process_alive(pid) {
            terminate(pid);
            if !quiet {
                println!("stopped pid={pid}");
            }
            stopped = true;
        }
        let _ = fs::remove_file(&pid_path);
    }

    if let Some(bind_addr) = env::var("BIND_ADDR")
        .ok()
        .or_else(|| runtime_store.as_ref().and_then(|s| s.bind_addr.clone()))
    {
        if let Ok(port) = bind_addr
            .rsplit(':')
            .next()
            .unwrap_or_default()
            .parse::<u16>()
        {
            if let Ok(output) = Command::new("lsof")
                .args(["-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
                .output()
            {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    if let Ok(pid) = line.trim().parse::<u32>() {
                        terminate(pid);
                        if !quiet {
                            println!("stopped listener pid={pid} port={port}");
                        }
                        stopped = true;
                    }
                }
            }
        }
    }

    let backup_path = env::var_os("AGENT_PROXY_CODEX_BACKUP")
        .map(PathBuf::from)
        .unwrap_or_else(|| codex_live::default_backup_path(&run_dir));
    codex_live::restore_takeover(&backup_path)
        .map_err(|e| format!("restore Codex live config failed: {e}"))?;

    let exchange_dir = env::var_os("EXCHANGE_LOG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            runtime_store
                .as_ref()
                .and_then(|s| s.exchange_log_dir.clone().map(PathBuf::from))
        })
        .unwrap_or_else(|| run_dir.join("exchanges"));
    clear_dir(&exchange_dir)?;
    if !stopped {
        if !quiet {
            println!("not running");
        }
    }
    if !quiet {
        println!("cleared {}", exchange_dir.display());
    }
    Ok(())
}

fn ensure_proxy_binary(root: &Path, binary: &Path) -> Result<(), String> {
    let status = Command::new("cargo")
        .current_dir(root)
        .args(["build", "-q", "--bin", "agent-proxy-server"])
        .status()
        .map_err(|e| format!("build agent-proxy-server: {e}"))?;
    if status.success() && binary.exists() {
        Ok(())
    } else {
        Err(format!("build agent-proxy-server exited {status}"))
    }
}

fn clear_dir(path: &Path) -> Result<(), String> {
    if path.exists() {
        fs::remove_dir_all(path).map_err(|e| format!("clear {}: {e}", path.display()))?;
    }
    Ok(())
}

fn proxy_is_running(root: &Path) -> bool {
    let pid_path = root.join(".run").join("agent-proxy-server.pid");
    match read_pid(&pid_path) {
        Ok(Some(pid)) => process_alive(pid),
        _ => false,
    }
}

fn read_pid(path: &Path) -> Result<Option<u32>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    text.trim()
        .parse::<u32>()
        .map(Some)
        .map_err(|e| format!("invalid pid file {}: {e}", path.display()))
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn terminate(pid: u32) {
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .stderr(Stdio::null())
        .status();
    for _ in 0..20 {
        if !process_alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .stderr(Stdio::null())
        .status();
}

fn provider_upsert(args: &[String]) -> Result<(), String> {
    let mut name = None;
    let mut base_url = None;
    let mut api_key = None;
    let mut format = None;
    let mut default_model = None;
    let mut is_full_url = false;
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
            "--full-url" => {
                is_full_url = true;
                i += 1;
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
            let path = env::var_os("AGENT_PROXY_STORE")
                .map(PathBuf::from)
                .unwrap_or_else(default_store_path);
            (AgentProxyStore::empty(&name), path)
        }
    };

    let mut mappings = BTreeMap::new();
    if let Some(m) = default_model.as_ref() {
        mappings.insert(m.clone(), m.clone());
    }
    store.upsert_provider(StoredProvider {
        name: name.clone(),
        base_url,
        is_full_url,
        api_key,
        api_format,
        model_mappings: mappings,
        max_output_tokens: None,
        upstream_model: default_model.clone(),
        codex_chat_reasoning: None,
        model_catalog: None,
    });
    store.set_active(&name).map_err(|e| e.to_string())?;
    store.save(&path).map_err(|e| e.to_string())?;
    println!("upserted provider '{name}' → {}", path.display());
    Ok(())
}

async fn models_sync(name: &str, quiet: bool) -> Result<(), String> {
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
    if quiet {
        spin.clear();
    } else {
        spin.stop(format!("found {} models", ids.len()));
    }
    let p = store
        .get_mut(name)
        .ok_or_else(|| format!("provider '{name}' missing"))?;
    p.apply_identity_mappings_from_models(&ids);
    store.save(&path).map_err(|e| e.to_string())?;
    if !quiet {
        println!(
            "synced identity mappings for '{name}' ({} entries) → {}",
            store.get(name).map(|p| p.model_mappings.len()).unwrap_or(0),
            path.display()
        );
    }
    Ok(())
}

async fn refresh_live_provider_if_active(
    base: &str,
    name: &str,
    quiet: bool,
) -> Result<(), String> {
    let root = repo_root();
    let (store, _) = load_or_migrate(&root)?;
    if store.active_provider == name {
        use_provider(base, name, quiet).await?;
    }
    Ok(())
}

fn mapping_set(
    provider_name: &str,
    client_model: &str,
    upstream_model: &str,
    quiet: bool,
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
    if !quiet {
        println!(
            "mapped {client_model} -> {upstream_model} for '{provider_name}' ({})",
            path.display()
        );
    }
    Ok(())
}

async fn run_wizard(base: &str, root: &PathBuf) -> Result<(), String> {
    let (mut store, store_path) = match load_or_migrate(root) {
        Ok(v) => v,
        Err(_) => {
            let path = env::var_os("AGENT_PROXY_STORE")
                .map(PathBuf::from)
                .unwrap_or_else(default_store_path);
            (AgentProxyStore::empty("default"), path)
        }
    };

    loop {
        clear_tui();
        let codex_status = if store.codex_active {
            "ACTIVE"
        } else {
            "INACTIVE"
        };
        let proxy_running = proxy_is_running(root);
        let proxy_status = if proxy_running { "ON" } else { "OFF" };
        let action = match Menu::new("LLPX")
            .item(
                "codex",
                "Codex",
                format!("{codex_status} · provider {}", store.active_provider),
            )
            .item("claude", "Claude Code", "")
            .item("proxy", format!("Proxy · {proxy_status}"), "")
            .interact()?
        {
            Flow::Value(action) => action,
            Flow::Back | Flow::Exit => break,
        };

        match action {
            "codex" => {
                if matches!(
                    codex_menu(base, root, &mut store, &store_path).await?,
                    Flow::Exit
                ) {
                    break;
                }
            }
            "claude" => {
                clear_tui();
                if matches!(
                    Menu::new("Claude Code\nIntegration is not configured")
                        .item("stay", "Not configured", "")
                        .interact()?,
                    Flow::Exit
                ) {
                    break;
                }
            }
            "proxy" => {
                if proxy_running {
                    stop_proxy(root, true)?;
                } else {
                    start_proxy(root, true)?;
                }
            }
            _ => {}
        }
        if let Ok((fresh, _)) = load_or_migrate(root) {
            store = fresh;
        }
    }

    clear_tui();
    Ok(())
}

async fn codex_menu(
    base: &str,
    root: &PathBuf,
    store: &mut AgentProxyStore,
    store_path: &Path,
) -> Result<Flow<()>, String> {
    loop {
        clear_tui();
        let status = if store.codex_active {
            "ACTIVE"
        } else {
            "INACTIVE"
        };
        let action = match Menu::new(format!(
            "Codex\nConfiguration: {status} · Provider: {}",
            store.active_provider
        ))
        .item("activation", format!("Configuration · {status}"), "")
        .item(
            "providers",
            "Providers",
            format!("{} configured", store.providers.len()),
        )
        .interact()?
        {
            Flow::Value(action) => action,
            Flow::Back => return Ok(Flow::Back),
            Flow::Exit => return Ok(Flow::Exit),
        };

        match action {
            "activation" => {
                set_codex_active(root, store, store_path, !store.codex_active)?;
            }
            "providers" => {
                if matches!(
                    providers_menu(base, root, store, store_path).await?,
                    Flow::Exit
                ) {
                    return Ok(Flow::Exit);
                }
            }
            _ => {}
        }
        if let Ok((fresh, _)) = load_or_migrate(root) {
            *store = fresh;
        }
    }
}

async fn providers_menu(
    base: &str,
    root: &PathBuf,
    store: &mut AgentProxyStore,
    store_path: &Path,
) -> Result<Flow<()>, String> {
    loop {
        clear_tui();
        let mut menu = Menu::new(format!(
            "Codex / Providers\nActive provider: {}",
            store.active_provider
        ));
        for provider in &store.providers {
            let marker = if provider.name == store.active_provider {
                "●"
            } else {
                " "
            };
            menu = menu.item(
                format!("provider:{}", provider.name),
                format!("{marker} {}", provider.name),
                format!("{} · {}", provider.api_format.as_str(), provider.base_url),
            );
        }
        menu = menu.item("add".to_string(), "Add Provider", "");
        let action = match menu.interact()? {
            Flow::Value(action) => action,
            Flow::Back => return Ok(Flow::Back),
            Flow::Exit => return Ok(Flow::Exit),
        };

        if let Some(name) = action.strip_prefix("provider:") {
            if matches!(
                provider_detail_menu(base, root, store, store_path, name).await?,
                Flow::Exit
            ) {
                return Ok(Flow::Exit);
            }
        } else if action == "add" {
            match prompt_provider(None)? {
                Flow::Value(provider) => {
                    store.upsert_provider(provider);
                    store.save(store_path).map_err(|e| e.to_string())?;
                }
                Flow::Back => {}
                Flow::Exit => return Ok(Flow::Exit),
            }
        }
        if let Ok((fresh, _)) = load_or_migrate(root) {
            *store = fresh;
        }
    }
}

async fn provider_detail_menu(
    base: &str,
    root: &PathBuf,
    store: &mut AgentProxyStore,
    store_path: &Path,
    name: &str,
) -> Result<Flow<()>, String> {
    let mut current = name.to_string();
    loop {
        clear_tui();
        let Some(provider) = store.get(&current).cloned() else {
            return Ok(Flow::Back);
        };
        let active = provider.name == store.active_provider;
        let action = match Menu::new(format!(
            "{}\n{} · {}",
            provider.name,
            provider.api_format.as_str(),
            provider.base_url
        ))
        .item(
            "active",
            format!("Active · {}", if active { "ON" } else { "OFF" }),
            "",
        )
        .item(
            "mapping",
            format!("Model Mapping · {}", provider.model_mappings.len()),
            "",
        )
        .item("edit", "Edit", "")
        .item("sync", "Sync models", "")
        .interact()?
        {
            Flow::Value(action) => action,
            Flow::Back => return Ok(Flow::Back),
            Flow::Exit => return Ok(Flow::Exit),
        };

        match action {
            "active" if !active => {
                store.set_active(&current).map_err(|e| e.to_string())?;
                store.save(store_path).map_err(|e| e.to_string())?;
                use_provider(base, &current, true).await?;
            }
            "mapping" => {
                if matches!(
                    mappings_menu(base, root, store, &current).await?,
                    Flow::Exit
                ) {
                    return Ok(Flow::Exit);
                }
            }
            "edit" => {
                let existing = store.get(&current).cloned().unwrap();
                match prompt_provider(Some(&existing))? {
                    Flow::Value(provider) => {
                        let updated_name = provider.name.clone();
                        store
                            .rename_provider(&current, provider)
                            .map_err(|e| e.to_string())?;
                        store.save(store_path).map_err(|e| e.to_string())?;
                        if store.active_provider == updated_name {
                            use_provider(base, &updated_name, true).await?;
                        }
                        current = updated_name;
                    }
                    Flow::Back => {}
                    Flow::Exit => return Ok(Flow::Exit),
                }
            }
            "sync" => {
                models_sync(&current, true).await?;
                if let Ok((fresh, _)) = load_or_migrate(root) {
                    *store = fresh;
                }
                refresh_live_provider_if_active(base, &current, true).await?;
            }
            _ => {}
        }
        if let Ok((fresh, _)) = load_or_migrate(root) {
            *store = fresh;
        }
    }
}

async fn mappings_menu(
    base: &str,
    root: &PathBuf,
    store: &mut AgentProxyStore,
    provider_name: &str,
) -> Result<Flow<()>, String> {
    loop {
        clear_tui();
        let Some(provider) = store.get(provider_name) else {
            return Ok(Flow::Back);
        };
        let entries: Vec<(String, String)> = provider
            .model_mappings
            .iter()
            .map(|(client, upstream)| (client.clone(), upstream.clone()))
            .collect();
        let mut menu = Menu::new(format!(
            "Model Mapping / {provider_name}\n{} mapping(s)",
            entries.len()
        ));
        for (index, (client, upstream)) in entries.iter().enumerate() {
            menu = menu.item(index, format!("{client} → {upstream}"), "");
        }
        let add_index = entries.len();
        menu = menu.item(add_index, "Add mapping", "");
        let action = match menu.interact()? {
            Flow::Value(action) => action,
            Flow::Back => return Ok(Flow::Back),
            Flow::Exit => return Ok(Flow::Exit),
        };

        let (client_model, default_upstream) = if action == add_index {
            let client = match input_flow("Codex/client model", "", |value| {
                if value.trim().is_empty() {
                    Err("required".into())
                } else {
                    Ok(())
                }
            })? {
                Flow::Value(client) => client,
                Flow::Back => continue,
                Flow::Exit => return Ok(Flow::Exit),
            };
            let default = client.trim().to_string();
            (client, default)
        } else if let Some((client, upstream)) = entries.get(action) {
            (client.clone(), upstream.clone())
        } else {
            continue;
        };
        let upstream_model = match input_flow("Upstream model", &default_upstream, |value| {
            if value.trim().is_empty() {
                Err("required".into())
            } else {
                Ok(())
            }
        })? {
            Flow::Value(model) => model,
            Flow::Back => continue,
            Flow::Exit => return Ok(Flow::Exit),
        };
        mapping_set(provider_name, &client_model, &upstream_model, true)?;
        if let Ok((fresh, _)) = load_or_migrate(root) {
            *store = fresh;
        }
        refresh_live_provider_if_active(base, provider_name, true).await?;
    }
}

fn set_codex_active(
    root: &Path,
    store: &mut AgentProxyStore,
    store_path: &Path,
    active: bool,
) -> Result<(), String> {
    let run_dir = root.join(".run");
    let backup_path = env::var_os("AGENT_PROXY_CODEX_BACKUP")
        .map(PathBuf::from)
        .unwrap_or_else(|| codex_live::default_backup_path(&run_dir));
    if active {
        if env::var("AGENT_PROXY_SKIP_CODEX_LIVE").as_deref() != Ok("1") {
            let bind_addr = env::var("BIND_ADDR")
                .ok()
                .or(store.bind_addr.clone())
                .unwrap_or_else(|| "127.0.0.1:8787".into());
            codex_live::apply_takeover(&format!("http://{bind_addr}/v1"), &backup_path)
                .map_err(|e| format!("apply Codex live config failed: {e}"))?;
        }
    } else {
        codex_live::restore_takeover(&backup_path)
            .map_err(|e| format!("restore Codex live config failed: {e}"))?;
    }
    store.codex_active = active;
    store.save(store_path).map_err(|e| e.to_string())
}

fn clear_tui() {
    let _ = cliclack::clear_screen();
}

fn prompt_provider(existing: Option<&StoredProvider>) -> Result<Flow<StoredProvider>, String> {
    let default_name = existing.map(|p| p.name.as_str()).unwrap_or("");
    let name = match input_flow("Provider name", default_name, |v| {
        if v.trim().is_empty() {
            Err("required".into())
        } else {
            Ok(())
        }
    })? {
        Flow::Value(name) => name,
        Flow::Back => return Ok(Flow::Back),
        Flow::Exit => return Ok(Flow::Exit),
    };

    let base_url = match input_flow(
        "Base URL",
        existing.map(|p| p.base_url.as_str()).unwrap_or("https://"),
        |v| {
            if !(v.starts_with("http://") || v.starts_with("https://")) {
                Err("must start with http:// or https://".into())
            } else {
                Ok(())
            }
        },
    )? {
        Flow::Value(base_url) => base_url,
        Flow::Back => return Ok(Flow::Back),
        Flow::Exit => return Ok(Flow::Exit),
    };

    let api_key = match input_flow(
        "API key",
        existing.map(|p| p.api_key.as_str()).unwrap_or(""),
        |v| {
            if v.trim().is_empty() {
                Err("required".into())
            } else {
                Ok(())
            }
        },
    )? {
        Flow::Value(api_key) => api_key,
        Flow::Back => return Ok(Flow::Back),
        Flow::Exit => return Ok(Flow::Exit),
    };

    let format = match Menu::new("Upstream protocol")
        .initial_value(match existing.map(|p| &p.api_format) {
            Some(ApiFormat::OpenaiChat) => "chat",
            Some(ApiFormat::Anthropic) => "anthropic",
            _ => "responses",
        })
        .item("responses", "OpenAI Responses (passthrough, default)", "")
        .item("chat", "OpenAI Chat Completions", "")
        .item("anthropic", "Anthropic Messages", "")
        .interact()?
    {
        Flow::Value(format) => format,
        Flow::Back => return Ok(Flow::Back),
        Flow::Exit => return Ok(Flow::Exit),
    };
    let api_format = ApiFormat::parse(format).unwrap_or_default();

    Ok(Flow::Value(StoredProvider {
        name: name.trim().to_string(),
        base_url: base_url.trim().trim_end_matches('/').to_string(),
        is_full_url: existing.is_some_and(|p| p.is_full_url),
        api_key: api_key.trim().to_string(),
        api_format,
        model_mappings: existing
            .map(|p| p.model_mappings.clone())
            .unwrap_or_default(),
        max_output_tokens: existing.and_then(|p| p.max_output_tokens),
        upstream_model: existing.and_then(|p| p.upstream_model.clone()),
        codex_chat_reasoning: existing.and_then(|p| p.codex_chat_reasoning.clone()),
        model_catalog: existing.and_then(|p| p.model_catalog.clone()),
    }))
}
