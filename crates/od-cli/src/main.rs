use std::path::PathBuf;
use std::sync::Arc;

use od_api::{ApiState, router};
use od_core::{
    APP_PORT, AppConfig, CancellationToken, DownloadSpec, Event, FetcherRegistry, FileTaskStore,
    FinishReason, ResolveRequest, Scheduler, SpeedTestConfig, SpeedTestEvent, TaskManager,
    ThemeMode, auth_token, auth_token_path, filename_from_url, run_speedtest,
};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-V" | "--version" | "version") => {
            println!("od {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("resolve") => run_resolve(&args[1..]).await,
        Some("serve") => run_serve(&args[1..]).await,
        Some("speedtest") => run_speedtest_cli(&args[1..]).await,
        Some("config") => run_config_cli(&args[1..]),
        Some("batch") => run_batch_cli(&args[1..]).await,
        Some("verify") => run_verify_cli(&args[1..]).await,
        Some("doctor") => run_doctor_cli().await,
        Some("status") => run_status_cli().await,
        Some("conflict") => run_conflict_cli(&args[1..]),
        Some("autostart") => run_autostart(&args[1..]),
        Some("-h" | "--help") | None => usage(),
        Some(_) => run_download(args).await,
    }
}

async fn run_speedtest_cli(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut cfg = SpeedTestConfig::default();
    let mut json_out = false;
    for arg in args {
        match arg.as_str() {
            "--no-upload" => cfg.include_upload = false,
            "--json" => json_out = true,
            _ => {
                eprintln!("usage: od speedtest [--no-upload] [--json]");
                std::process::exit(2);
            }
        }
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<SpeedTestEvent>();
    let printer = if !json_out {
        Some(tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    SpeedTestEvent::PhaseChanged { phase } => {
                        eprintln!("  [{phase:?}]...");
                    }
                    SpeedTestEvent::Sample { phase, mbps, .. } => {
                        eprint!("\r  {phase:?}: {mbps:.1} Mbps   ");
                    }
                    SpeedTestEvent::Completed { .. } => {
                        eprintln!();
                    }
                }
            }
        }))
    } else {
        None
    };

    let cancel = CancellationToken::new();
    let report = run_speedtest(
        &reqwest::Client::new(),
        &cfg,
        cancel,
        if json_out { None } else { Some(tx) },
    )
    .await?;
    if let Some(h) = printer {
        let _ = h.await;
    }

    if json_out {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Connection Quality : {}", report.quality.label());
        println!(
            "Ping / Jitter      : {:.1} ms (±{:.1} ms)",
            report.ping_ms, report.jitter_ms
        );
        println!(
            "Download           : {:.2} Mbps ({:.1} MB/s)",
            report.download_mbps,
            report.download_bps as f64 / (1024.0 * 1024.0)
        );
        if let Some(up) = report.upload_mbps {
            println!("Upload             : {up:.2} Mbps");
        }
        println!("Recommended Conns  : {}", report.recommended_connections);
    }
    Ok(())
}

async fn run_doctor_cli() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = AppConfig::load();
    let cfg_path = AppConfig::default_path();
    let token_path = auth_token_path();
    let store_path = token_path.with_file_name("tasks.json");
    let exe = std::env::current_exe().ok();
    let auto = od_core::autostart::status().ok();

    let health_url = format!("http://127.0.0.1:{APP_PORT}/api/v1/health");
    let daemon_online = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(1))
        .build()
        .ok()
        .is_some_and(|c| {
            // Checked asynchronously right below.
            let _ = c;
            true
        });
    let daemon_live = if daemon_online {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .ok()
            .map(|c| c.get(&health_url).send())
    } else {
        None
    };
    let daemon_status = match daemon_live {
        Some(fut) => match fut.await {
            Ok(r) if r.status().is_success() => format!("online (127.0.0.1:{APP_PORT})"),
            _ => "offline (run `od serve` to start)".into(),
        },
        None => "offline".into(),
    };

    let dir_writable = std::fs::create_dir_all(&cfg.downloads.download_dir).is_ok();
    println!("Open Downloader Doctor — v{}", env!("CARGO_PKG_VERSION"));
    println!("  OS / Arch       : {} / {}", std::env::consts::OS, std::env::consts::ARCH);
    println!(
        "  Binary          : {}",
        exe.map(|p| p.display().to_string()).unwrap_or_else(|| "unknown".into())
    );
    println!(
        "  Config          : {} ({})",
        cfg_path.display(),
        if cfg_path.exists() { "present" } else { "defaults" }
    );
    println!(
        "  Auth Token      : {} ({})",
        token_path.display(),
        if token_path.exists() { "present" } else { "not generated yet" }
    );
    println!(
        "  Task Store      : {} ({})",
        store_path.display(),
        if store_path.exists() { "present" } else { "empty" }
    );
    println!(
        "  Download Dir    : {} ({})",
        cfg.downloads.download_dir.display(),
        if dir_writable { "writable" } else { "NOT WRITABLE" }
    );
    println!("  Local Daemon    : {daemon_status}");
    if let Some(a) = auto {
        println!(
            "  Auto-Start      : {} ({} · {})",
            if a.enabled { "enabled" } else { "disabled" },
            a.mechanism,
            a.target
        );
    }
    let others: Vec<String> = od_core::scan_downloaders()
        .into_iter()
        .filter(|d| d.running)
        .map(|d| format!("{} (PID {:?})", d.name, d.pids))
        .collect();
    println!(
        "  Other Apps      : {}",
        if others.is_empty() {
            "none running".into()
        } else {
            others.join(", ")
        }
    );
    Ok(())
}

fn run_conflict_cli(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    match args.first().map(String::as_str) {
        Some("scan") | None => {
            let list = od_core::scan_downloaders();
            let running: Vec<_> = list.iter().filter(|d| d.running).collect();
            if running.is_empty() {
                println!("No competing download managers currently running.");
            } else {
                println!("Running download managers:");
                for d in running {
                    println!("  - {:<12} {:<32} PIDs: {:?}", d.id, d.name, d.pids);
                }
            }
            Ok(())
        }
        Some("stop") if args.len() >= 2 => {
            let id = &args[1];
            let force = args.iter().any(|a| a == "--force");
            let n = od_core::stop_downloader(id, force)?;
            println!("Signaled {n} process(es) for '{id}'.");
            Ok(())
        }
        _ => {
            eprintln!("usage: od conflict <scan | stop <id> [--force]>");
            std::process::exit(2);
        }
    }
}

async fn run_status_cli() -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?;
    let health_url = format!("http://127.0.0.1:{APP_PORT}/api/v1/health");
    let health: serde_json::Value = match client.get(&health_url).send().await {
        Ok(r) if r.status().is_success() => r.json().await?,
        _ => {
            eprintln!("daemon is not running on 127.0.0.1:{APP_PORT} (start with `od serve`)");
            std::process::exit(1);
        }
    };
    println!(
        "Daemon: online (v{}, protocols: {})",
        health["version"].as_str().unwrap_or("?"),
        health["protocols"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    );
    if let Ok(tok) = std::fs::read_to_string(auth_token_path()) {
        let tasks_url = format!("http://127.0.0.1:{APP_PORT}/api/v1/tasks");
        if let Ok(resp) = client
            .get(&tasks_url)
            .header("x-od-token", tok.trim())
            .send()
            .await
            && resp.status().is_success()
            && let Ok(list) = resp.json::<Vec<od_core::TaskRecord>>().await
        {
            println!("Tasks : {} total", list.len());
            for t in list.iter().take(10) {
                println!(
                    "  [#{:<3}] {:<10?}  {} ({} bytes)",
                    t.id, t.status, t.resource.name, t.progress.downloaded
                );
            }
        }
    }
    Ok(())
}

async fn run_batch_cli(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let Some(file) = args.first() else {
        eprintln!("usage: od batch <urls.txt>");
        std::process::exit(2);
    };
    let text = tokio::fs::read_to_string(file).await?;
    let urls = od_core::extractors::parse_batch_urls(&text);
    if urls.is_empty() {
        eprintln!("no URLs found in {file}");
        return Ok(());
    }
    eprintln!("Queueing {} URLs from {file}...", urls.len());
    run_download(urls).await
}

async fn run_verify_cli(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let Some(file) = args.first() else {
        eprintln!("usage: od verify <file> [expected-sha256]");
        std::process::exit(2);
    };
    let path = PathBuf::from(file);
    match args.get(1) {
        Some(expected) => {
            let digest = od_core::verify_file_sha256(&path, expected).await?;
            println!("OK  {digest}  {}", path.display());
        }
        None => {
            let digest = od_core::sha256_file(&path).await?;
            println!("{digest}  {}", path.display());
        }
    }
    Ok(())
}

fn run_config_cli(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    match args.first().map(String::as_str) {
        Some("show") | None => {
            let cfg = AppConfig::load();
            eprintln!("# {}", AppConfig::default_path().display());
            println!("{}", serde_json::to_string_pretty(&cfg)?);
            Ok(())
        }
        Some("reset") => {
            let cfg = AppConfig::default();
            cfg.save()?;
            println!("Reset configuration to defaults ({})", AppConfig::default_path().display());
            Ok(())
        }
        Some("set") if args.len() >= 3 => {
            let mut cfg = AppConfig::load();
            let key = args[1].as_str();
            let val = args[2].as_str();
            match key {
                "download_dir" => cfg.downloads.download_dir = PathBuf::from(val),
                "organize" => cfg.downloads.organize_by_category = val.parse()?,
                "connections" => cfg.downloads.connections_per_download = val.parse()?,
                "concurrency" => cfg.downloads.max_concurrent_downloads = val.parse()?,
                "rate_limit" => {
                    cfg.downloads.rate_limit_bps = if val == "none" || val == "0" {
                        None
                    } else {
                        Some(val.parse()?)
                    };
                }
                "auto_start" => {
                    let on: bool = val.parse()?;
                    cfg.startup.auto_start = on;
                    if on {
                        let exe = std::env::current_exe()?;
                        let _ = od_core::autostart::enable(&od_core::autostart::AutoStartConfig::daemon(exe));
                    } else {
                        let _ = od_core::autostart::disable();
                    }
                }
                "require_token" => cfg.security.require_api_token = val.parse()?,
                "verify_tls" => cfg.security.verify_tls = val.parse()?,
                "proxy" => {
                    cfg.security.proxy_url = if val == "none" || val.is_empty() {
                        None
                    } else {
                        Some(val.to_string())
                    };
                }
                "theme" => {
                    cfg.appearance.theme = match val.to_ascii_lowercase().as_str() {
                        "dark" => ThemeMode::Dark,
                        "light" => ThemeMode::Light,
                        "system" => ThemeMode::System,
                        _ => return Err("theme must be system, dark, or light".into()),
                    };
                }
                "ninja.mode" => {
                    cfg.ninja.mode = match val.to_ascii_lowercase().as_str() {
                        "off" => od_core::NinjaMode::Off,
                        "global" => od_core::NinjaMode::Global,
                        "focused" => od_core::NinjaMode::Focused,
                        _ => return Err("ninja.mode must be off, global, or focused".into()),
                    };
                }
                "ninja.action" => {
                    cfg.ninja.action = match val.to_ascii_lowercase().as_str() {
                        "grabber" | "grabber_queue" => od_core::NinjaAction::GrabberQueue,
                        "auto" | "auto_download" => od_core::NinjaAction::AutoDownload,
                        _ => return Err("ninja.action must be grabber or auto".into()),
                    };
                }
                "ninja.domains" => {
                    cfg.ninja.focused_domains = val
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                k if k.starts_with("category.") => {
                    let cat_name = k.trim_start_matches("category.");
                    if val == "default" || val.is_empty() {
                        cfg.downloads.category_dirs.remove(cat_name);
                    } else {
                        cfg.downloads
                            .category_dirs
                            .insert(cat_name.to_string(), PathBuf::from(val));
                    }
                }
                other => {
                    return Err(format!(
                        "unknown config key '{other}' (valid: download_dir, organize, connections, concurrency, rate_limit, auto_start, require_token, verify_tls, proxy, theme, category.<Name>)"
                    )
                    .into());
                }
            }
            cfg.save()?;
            println!("Saved {key} = {val}");
            Ok(())
        }
        _ => {
            eprintln!("usage: od config <show|reset|set <key> <value>>");
            std::process::exit(2);
        }
    }
}

fn run_autostart(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use od_core::autostart::{AutoStartConfig, disable, enable, status};
    match args.first().map(String::as_str) {
        Some("enable") => {
            let exe = std::env::current_exe()?;
            let st = enable(&AutoStartConfig::daemon(exe))?;
            println!("{}", serde_json::to_string_pretty(&st)?);
            Ok(())
        }
        Some("disable") => {
            let st = disable()?;
            println!("{}", serde_json::to_string_pretty(&st)?);
            Ok(())
        }
        Some("status") | None => {
            let st = status()?;
            println!("{}", serde_json::to_string_pretty(&st)?);
            Ok(())
        }
        _ => {
            eprintln!("usage: od autostart <enable|disable|status>");
            std::process::exit(2);
        }
    }
}

async fn run_resolve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let Some(url) = args.first() else {
        eprintln!("usage: od resolve <url>");
        std::process::exit(2);
    };
    let registry = FetcherRegistry::with_defaults(reqwest::Client::new());
    let (_, resource) = registry.resolve(&ResolveRequest::new(url)).await?;
    println!("{}", serde_json::to_string_pretty(&resource)?);
    Ok(())
}

async fn run_serve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut addr = format!("127.0.0.1:{APP_PORT}");
    let mut dir = default_downloads_dir();
    let mut concurrency: usize = 3;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--addr" if i + 1 < args.len() => {
                addr = args[i + 1].clone();
                i += 2;
            }
            "--dir" if i + 1 < args.len() => {
                dir = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--concurrency" if i + 1 < args.len() => {
                concurrency = args[i + 1].parse().unwrap_or(3).max(1);
                i += 2;
            }
            _ => {
                eprintln!(
                    "usage: od serve [--addr 127.0.0.1:19799] [--dir <path>] [--concurrency N]"
                );
                std::process::exit(2);
            }
        }
    }

    let token = auth_token()?;
    let store_path = auth_token_path().with_file_name("tasks.json");
    let registry = FetcherRegistry::with_defaults(reqwest::Client::new());
    let protocols: Vec<String> = registry.protocols().into_iter().map(String::from).collect();
    let store = Arc::new(FileTaskStore::new(&store_path));
    let manager = TaskManager::new(registry, store, concurrency)?;

    let cfg = AppConfig::load();
    let state = ApiState::new(manager, &dir)
        .with_config(cfg, Some(AppConfig::default_path()))
        .with_token(token)
        .with_protocols(protocols);
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("open-downloader daemon listening on http://{addr}");
    eprintln!("  downloads dir : {}", dir.display());
    eprintln!("  task store    : {}", store_path.display());
    eprintln!("  REST API      : /api/v1/{{health,resolve,tasks,events}}");
    eprintln!("  MCP endpoint  : POST /mcp");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("\nshutting down daemon...");
        })
        .await?;
    Ok(())
}

async fn run_download(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let (output, urls) = match args.first().map(String::as_str) {
        Some("-o") => {
            if args.len() != 3 {
                usage();
            }
            (Some(PathBuf::from(&args[1])), vec![args[2].clone()])
        }
        Some(_) => (None, args),
        None => usage(),
    };
    if output.is_some() && urls.len() != 1 {
        usage();
    }
    if let Some(bad) = urls
        .iter()
        .find(|u| !(u.starts_with("http://") || u.starts_with("https://")))
    {
        eprintln!("error: not a URL: {bad}");
        std::process::exit(2);
    }

    let client = reqwest::Client::new();
    let mut specs = Vec::with_capacity(urls.len());
    for (i, url) in urls.iter().enumerate() {
        let dest = match &output {
            Some(p) => p.clone(),
            None => PathBuf::from(filename_for(&client, url).await),
        };
        specs.push(DownloadSpec {
            id: i as u64,
            url: url.clone(),
            dest,
            headers: vec![],
        });
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    let multi = specs.len() > 1;
    let reporter = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                Event::Progress {
                    downloaded, total, ..
                } if !multi => match total {
                    Some(t) if t > 0 => eprint!(
                        "\r{:.1}% ({} / {} bytes)",
                        downloaded as f64 / t as f64 * 100.0,
                        downloaded,
                        t
                    ),
                    _ => eprint!("\r{} bytes", downloaded),
                },
                Event::Progress { .. } => {}
                Event::Finished { id, reason } => match reason {
                    FinishReason::Completed(o) => {
                        let tag = if o.resumed { " (resumed)" } else { "" };
                        eprintln!("[{id}] done{tag}: {}", o.path.display());
                    }
                    FinishReason::Cancelled => eprintln!("[{id}] cancelled"),
                    FinishReason::Failed(e) => eprintln!("[{id}] failed: {e}"),
                },
            }
        }
        if !multi {
            eprintln!();
        }
    });

    let cancel = od_core::CancellationToken::new();
    let opts = od_core::DownloadOptions {
        cancel: Some(cancel.clone()),
        // `-o` is an explicit path — never relocate it into a type folder.
        organize: output.is_none(),
        ..Default::default()
    };
    let scheduler = Scheduler::new(opts, 2);
    let run = scheduler.run(&client, specs, tx);
    tokio::pin!(run);
    let results = tokio::select! {
        r = &mut run => r,
        _ = tokio::signal::ctrl_c() => {
            cancel.cancel();
            eprintln!("\ncancelling — progress is kept, rerun to resume");
            run.await
        }
    };
    reporter.await?;

    if results.iter().any(|r| r.is_err()) {
        std::process::exit(1);
    }
    Ok(())
}

async fn filename_for(client: &reqwest::Client, url: &str) -> String {
    od_core::probe(client, url)
        .await
        .ok()
        .and_then(|i| i.filename)
        .unwrap_or_else(|| filename_from_url(url))
}

fn default_downloads_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| PathBuf::from(h).join("Downloads"))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn usage() -> ! {
    eprintln!("open-downloader v{}", env!("CARGO_PKG_VERSION"));
    eprintln!("usage:");
    eprintln!("  od [-o output] <url> [url...]");
    eprintln!("  od resolve <url>");
    eprintln!("  od serve [--addr 127.0.0.1:19799] [--dir <path>] [--concurrency N]");
    eprintln!("  od speedtest [--no-upload] [--json]");
    eprintln!("  od batch <urls.txt>");
    eprintln!("  od verify <file> [expected-sha256]");
    eprintln!("  od config <show|reset|set <key> <value>>");
    eprintln!("  od doctor");
    eprintln!("  od status");
    eprintln!("  od conflict <scan | stop <id> [--force]>");
    eprintln!("  od autostart <enable|disable|status>");
    eprintln!("  od --version");
    std::process::exit(2);
}
