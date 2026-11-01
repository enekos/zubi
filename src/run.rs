use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::config::PROXY_NAME;
use crate::control::{self, Control};
use crate::log::{self, Logger};
use crate::plan::Plan;
use crate::proxy;
use crate::state::Store;
use crate::supervisor::{self, ServiceContext};

#[derive(Debug, Clone, Default)]
pub struct UpOptions {
    pub ignore_busy_ports: bool,
    pub open: bool,
    pub skip_before: bool,
}

fn prefix_width(plan: &Plan) -> usize {
    plan.launches
        .iter()
        .map(|l| l.name.len())
        .chain([PROXY_NAME.len(), "zubi".len()])
        .max()
        .unwrap_or(8)
}

fn make_control(plan: &Plan, store: Arc<Store>) -> Arc<Control> {
    let proxy_base = plan.proxy.as_ref().map_or_else(
        || http::Uri::from_static("http://localhost"),
        |p| p.base.clone(),
    );
    Arc::new(Control {
        store,
        proxy_base,
        root: plan.root.display().to_string(),
    })
}

pub async fn proxy_only(plan: Plan) -> Result<()> {
    let Some(cfg) = plan.proxy.clone() else {
        bail!("the proxy is disabled in this configuration (proxy.autostart = false)");
    };
    let busy = supervisor::busy_ports(&[(PROXY_NAME.to_owned(), cfg.listen.port())]).await;
    if !busy.is_empty() {
        bail!("port already in use: {}", busy.join("; "));
    }
    let (logger, writer) = log::start(prefix_width(&plan));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    spawn_ctrl_c(shutdown_tx, logger.clone());
    let started = Instant::now();
    let control = make_control(&plan, Store::empty(started));
    let base = cfg.base.clone();
    let proxy_task = tokio::spawn(proxy::serve(cfg, control, logger.clone(), shutdown_rx));
    logger.line(
        PROXY_NAME,
        &format!(
            "listening on {base} after {}ms · dashboard {base}__zubi",
            started.elapsed().as_millis()
        ),
    );
    proxy_task.await.context("proxy task")??;
    drop(logger);
    let _flushed = writer.await;
    Ok(())
}

pub async fn up(plan: Plan, options: UpOptions) -> Result<()> {
    if plan.launches.is_empty() && plan.proxy.is_none() {
        bail!("nothing to start: no service selected and the proxy is off");
    }
    let busy = supervisor::busy_ports(&plan.ports()).await;
    if !busy.is_empty() && !options.ignore_busy_ports {
        bail!(
            "ports already in use: {}. Stop those processes or pass --ignore-busy-ports",
            busy.join("; ")
        );
    }
    let (logger, writer) = log::start(prefix_width(&plan));
    if !busy.is_empty() {
        logger.line(
            "zubi",
            &format!(
                "ports already in use: {} (continuing because of --ignore-busy-ports)",
                busy.join("; ")
            ),
        );
    }
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    spawn_ctrl_c(shutdown_tx.clone(), logger.clone());
    let epoch = Instant::now();
    let store = Store::new(&plan.launches, epoch);
    let control = make_control(&plan, Arc::clone(&store));

    let mut tasks: JoinSet<Result<()>> = JoinSet::new();
    if let Some(cfg) = plan.proxy.clone() {
        let log = logger.clone();
        let rx = shutdown_rx.clone();
        let ctl = Arc::clone(&control);
        if cfg.router.is_empty() {
            log.line(
                PROXY_NAME,
                "no routes configured: every request will answer 404 until [[routes]] has an entry",
            );
        }
        tasks.spawn(async move {
            let base = cfg.base.clone();
            let server = proxy::serve(cfg, ctl, log.clone(), rx);
            log.line(
                PROXY_NAME,
                &format!(
                    "listening on {base} after {}ms · dashboard {base}__zubi",
                    epoch.elapsed().as_millis()
                ),
            );
            server.await
        });
    }

    if !options.skip_before {
        for launch in &plan.launches {
            if *shutdown_rx.borrow() {
                break;
            }
            supervisor::run_before(launch, &logger).await?;
        }
    }

    let expected = plan.launches.len();
    let (listening_tx, mut listening_rx) = mpsc::channel(expected.max(1));
    for launch in plan.launches.clone() {
        let ctx = ServiceContext {
            log: logger.clone(),
            store: Arc::clone(&store),
            shutdown: shutdown_rx.clone(),
            listening: listening_tx.clone(),
            epoch,
        };
        tasks.spawn(supervisor::run(launch, ctx));
    }
    drop(listening_tx);

    let summary_log = logger.clone();
    let summary_store = Arc::clone(&store);
    let names: Vec<String> = plan.launches.iter().map(|l| l.name.clone()).collect();
    let open_url = options
        .open
        .then(|| plan.proxy.as_ref().map(|p| p.base.to_string()))
        .flatten();
    let summary = tokio::spawn(async move {
        let mut seen = Vec::with_capacity(expected);
        while seen.len() < expected {
            match listening_rx.recv().await {
                Some(item) => seen.push(item),
                None => return,
            }
        }
        if expected > 0 {
            let slowest = seen.iter().map(|r| r.elapsed).max().unwrap_or_default();
            let parts: Vec<String> = names
                .iter()
                .filter_map(|n| seen.iter().find(|r| &r.name == n))
                .filter(|r| r.port.is_some())
                .map(|r| format!("{} {:.1}s", r.name, r.elapsed.as_secs_f64()))
                .collect();
            summary_log.line(
                "zubi",
                &format!(
                    "all {} services listening after {:.1}s ({})",
                    seen.len(),
                    slowest.as_secs_f64(),
                    parts.join(", ")
                ),
            );
            while !summary_store.all_ready() {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            summary_log.line(
                "zubi",
                &format!(
                    "all services ready after {:.1}s",
                    epoch.elapsed().as_secs_f64()
                ),
            );
        }
        if let Some(url) = open_url {
            open_browser(&url, &summary_log);
        }
    });

    while let Some(finished) = tasks.join_next().await {
        match finished {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                logger.line("zubi", &format!("{err:#}"));
                let _sent = shutdown_tx.send(true);
            }
            Err(err) if err.is_panic() => {
                logger.line("zubi", &format!("task panicked: {err}"));
                let _sent = shutdown_tx.send(true);
            }
            Err(_) => {}
        }
    }
    summary.abort();
    if logger.dropped() > 0 {
        logger.line(
            "zubi",
            &format!("{} log lines were dropped under load", logger.dropped()),
        );
    }
    drop(logger);
    let _flushed = writer.await;
    Ok(())
}

fn open_browser(url: &str, log: &Logger) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    match std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_child) => log.dim("zubi", &format!("opened {url}")),
        Err(err) => log.line("zubi", &format!("could not open {url}: {err}")),
    }
}

fn spawn_ctrl_c(shutdown: watch::Sender<bool>, log: Logger) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            log.line("zubi", "stopping (Ctrl-C)");
            let _sent = shutdown.send(true);
        }
    });
}

static NULL: serde_json::Value = serde_json::Value::Null;

fn field<'a>(value: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
    value.get(key).unwrap_or(&NULL)
}

pub async fn status(base: &str) -> Result<()> {
    let client = control::client();
    let (code, body, _) = control::get(
        &client,
        &format!("{base}/__zubi/api/status"),
        Duration::from_secs(5),
    )
    .await
    .with_context(|| format!("is zubi running? nothing answered at {base}/__zubi"))?;
    if !code.is_success() {
        bail!("{base}/__zubi/api/status answered {code}");
    }
    let view: serde_json::Value = serde_json::from_str(&body).context("parsing status")?;
    println!(
        "zubi at {}  root {}  up {:.0}s{}",
        field(&view, "proxy").as_str().unwrap_or("?"),
        field(&view, "root").as_str().unwrap_or("?"),
        field(&view, "uptimeMs").as_f64().unwrap_or(0.0) / 1000.0,
        if field(&view, "allReady").as_bool().unwrap_or(false) {
            "  all ready"
        } else {
            ""
        }
    );
    println!();
    println!(
        "{:<18} {:>5}  {:<22} {:>9} {:>9} {:>6} {:>8}  command",
        "service", "port", "state", "listening", "ready", "pid", "restarts"
    );
    for service in field(&view, "services").as_array().into_iter().flatten() {
        let secs = |key: &str| {
            field(service, key)
                .as_f64()
                .map_or("—".to_owned(), |ms| format!("{:.1}s", ms / 1000.0))
        };
        let mut state = field(service, "phase").as_str().unwrap_or("?").to_owned();
        if let Some(status) = field(service, "status").as_str() {
            state.push(' ');
            state.push_str(status);
        }
        println!(
            "{:<18} {:>5}  {:<22} {:>9} {:>9} {:>6} {:>8}  {}",
            field(service, "name").as_str().unwrap_or("?"),
            field(service, "port")
                .as_u64()
                .map_or("—".to_owned(), |p| p.to_string()),
            state,
            secs("listeningAfterMs"),
            secs("readyAfterMs"),
            field(service, "pid")
                .as_u64()
                .map_or("—".to_owned(), |p| p.to_string()),
            field(service, "restarts").as_u64().unwrap_or(0),
            field(service, "command").as_str().unwrap_or(""),
        );
    }
    Ok(())
}

pub async fn logs(base: &str, service: &str, lines: usize, follow: bool) -> Result<()> {
    let client = control::client();
    let mut since = 0u64;
    let mut first = true;
    loop {
        let url = format!(
            "{base}/__zubi/api/logs/{service}?since={since}&lines={}",
            if first { lines } else { 1000 }
        );
        let (code, body, _) = control::get(&client, &url, Duration::from_secs(5))
            .await
            .with_context(|| format!("is zubi running? nothing answered at {base}/__zubi"))?;
        if code == http::StatusCode::NOT_FOUND {
            bail!("zubi does not manage a service called {service:?}");
        }
        if !code.is_success() {
            bail!("{url} answered {code}");
        }
        let page: serde_json::Value = serde_json::from_str(&body).context("parsing logs")?;
        for line in field(&page, "lines").as_array().into_iter().flatten() {
            if let Some(text) = line.as_str() {
                println!("{text}");
            }
        }
        since = field(&page, "next").as_u64().unwrap_or(since);
        first = false;
        if !follow {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

pub async fn restart(base: &str, service: &str) -> Result<()> {
    let client = control::client();
    let (code, body) = control::post(&client, &format!("{base}/__zubi/api/restart/{service}"))
        .await
        .with_context(|| format!("is zubi running? nothing answered at {base}/__zubi"))?;
    if code == http::StatusCode::NOT_FOUND {
        bail!("zubi does not manage a service called {service:?}");
    }
    if !code.is_success() {
        bail!("restart answered {code}: {body}");
    }
    println!("restarting {service}");
    Ok(())
}
