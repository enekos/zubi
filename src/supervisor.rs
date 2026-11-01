use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rustix::process::{Pid, Signal};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{Notify, mpsc, oneshot, watch};

use crate::control;
use crate::log::Logger;
use crate::plan::{Launch, ReadyCheck};
use crate::state::{Phase, Store};

const MAX_RESTARTS: u32 = 5;
const RESTART_DELAY: Duration = Duration::from_secs(2);
const TERM_GRACE: Duration = Duration::from_secs(8);
const PORT_POLL: Duration = Duration::from_millis(100);
const HTTP_READY_POLL: Duration = Duration::from_millis(250);
const WARM_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone)]
pub struct Listening {
    pub name: String,
    pub port: Option<u16>,
    pub elapsed: Duration,
}

#[derive(Debug, Clone)]
pub struct ServiceContext {
    pub log: Logger,
    pub store: Arc<Store>,
    pub shutdown: watch::Receiver<bool>,
    pub listening: mpsc::Sender<Listening>,
    pub epoch: Instant,
}

pub async fn run(launch: Launch, ctx: ServiceContext) -> Result<()> {
    let ServiceContext {
        log,
        store,
        mut shutdown,
        listening,
        epoch,
    } = ctx;
    let restart = store
        .restart_handle(&launch.name)
        .unwrap_or_else(|| Arc::new(Notify::new()));
    let mut crashes = 0u32;
    loop {
        let mut child = spawn(&launch).with_context(|| {
            format!(
                "starting {} ({} in {})",
                launch.name,
                launch.command_line(),
                launch.dir.display()
            )
        })?;
        let pid = child.id();
        store.update(&launch.name, |s| {
            s.pid = pid;
            s.started_at = Instant::now();
            s.phase = Phase::Starting;
            s.listening_after = None;
            s.ready_after = None;
        });
        log.dim(
            &launch.name,
            &format!(
                "{} (pid {}) in {}",
                launch.command_line(),
                pid.unwrap_or(0),
                launch.dir.display()
            ),
        );

        let (ready_tx, ready_rx) = oneshot::channel::<()>();
        let (listen_tx, listen_rx) = oneshot::channel::<()>();
        let ready_signal: ReadySignal = Arc::new(std::sync::Mutex::new(Some(ready_tx)));
        let pumps = pump_output(&mut child, &launch, &log, &store, &ready_signal);
        let watcher = tokio::spawn(wait_for_port(
            launch.clone(),
            epoch,
            listening.clone(),
            listen_tx,
            log.clone(),
            Arc::clone(&store),
            Arc::clone(&ready_signal),
        ));
        let warmer = tokio::spawn(warm(launch.clone(), ready_rx, log.clone()));
        let http_ready = tokio::spawn(http_ready(
            launch.clone(),
            listen_rx,
            log.clone(),
            Arc::clone(&store),
            Arc::clone(&ready_signal),
        ));
        let helpers = [watcher, warmer, http_ready];

        tokio::select! {
            status = child.wait() => {
                abort_all(&helpers);
                let status = status.with_context(|| format!("waiting for {}", launch.name))?;
                for pump in pumps { let _joined = pump.await; }
                if *shutdown.borrow() {
                    break;
                }
                store.update(&launch.name, |s| { s.phase = Phase::Exited { status: status.to_string() }; s.pid = None; });
                crashes += 1;
                if crashes > MAX_RESTARTS {
                    log.line(&launch.name, &format!("exited with {status}; giving up after {MAX_RESTARTS} restarts"));
                    break;
                }
                log.line(&launch.name, &format!("exited with {status}; restarting in {}s ({crashes}/{MAX_RESTARTS})", RESTART_DELAY.as_secs()));
                store.update(&launch.name, |s| s.restarts += 1);
                tokio::time::sleep(RESTART_DELAY).await;
            }
            _ = restart.notified() => {
                abort_all(&helpers);
                log.line(&launch.name, "restart requested");
                store.update(&launch.name, |s| { s.phase = Phase::Restarting; s.restarts += 1; });
                terminate(&launch.name, pid, &mut child, &log).await;
                for pump in pumps { let _joined = pump.await; }
                crashes = 0;
            }
            _ = shutdown.changed() => {
                abort_all(&helpers);
                terminate(&launch.name, pid, &mut child, &log).await;
                for pump in pumps { let _joined = pump.await; }
                break;
            }
        }
    }
    Ok(())
}

fn abort_all(handles: &[tokio::task::JoinHandle<()>]) {
    for handle in handles {
        handle.abort();
    }
}

fn spawn(launch: &Launch) -> std::io::Result<Child> {
    let mut cmd = Command::new(&launch.program);
    cmd.args(&launch.args)
        .current_dir(&launch.dir)
        .envs(launch.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    cmd.spawn()
}

pub async fn run_before(launch: &Launch, log: &Logger) -> Result<()> {
    let Some(words) = &launch.before else {
        return Ok(());
    };
    let Some((program, args)) = words.split_first() else {
        return Ok(());
    };
    log.dim(
        &launch.name,
        &format!("before: {} in {}", words.join(" "), launch.dir.display()),
    );
    let status = Command::new(program)
        .args(args)
        .current_dir(&launch.dir)
        .envs(launch.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("{}: running before command {program}", launch.name))?;
    if !status.success() {
        log.line(
            &launch.name,
            &format!(
                "before command exited with {status}; starting anyway (pass --no-before to skip it)"
            ),
        );
    }
    Ok(())
}

type ReadySignal = Arc<std::sync::Mutex<Option<oneshot::Sender<()>>>>;

fn signal_ready(name: &str, store: &Store, log: &Logger, signal: &ReadySignal) {
    if let Some(elapsed) = store.mark_ready(name) {
        log.line(name, &format!("ready after {:.1}s", elapsed.as_secs_f64()));
    }
    let sender = signal
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(sender) = sender {
        let _sent = sender.send(());
    }
}

fn pump_output(
    child: &mut Child,
    launch: &Launch,
    log: &Logger,
    store: &Arc<Store>,
    ready: &ReadySignal,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut pumps = Vec::with_capacity(2);
    if let Some(stdout) = child.stdout.take() {
        pumps.push(tokio::spawn(pump(
            stdout,
            launch.clone(),
            log.clone(),
            Arc::clone(store),
            Arc::clone(ready),
        )));
    }
    if let Some(stderr) = child.stderr.take() {
        pumps.push(tokio::spawn(pump(
            stderr,
            launch.clone(),
            log.clone(),
            Arc::clone(store),
            Arc::clone(ready),
        )));
    }
    pumps
}

async fn pump<R: AsyncRead + Unpin>(
    reader: R,
    launch: Launch,
    log: Logger,
    store: Arc<Store>,
    ready: ReadySignal,
) {
    let mut lines = BufReader::with_capacity(64 << 10, reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        log.line(&launch.name, &line);
        store.push_log(&launch.name, &line);
        if let ReadyCheck::Log(pattern) = &launch.ready
            && pattern.is_match(&line)
        {
            signal_ready(&launch.name, &store, &log, &ready);
        }
    }
}

async fn wait_for_port(
    launch: Launch,
    epoch: Instant,
    listening: mpsc::Sender<Listening>,
    listen_tx: oneshot::Sender<()>,
    log: Logger,
    store: Arc<Store>,
    ready: ReadySignal,
) {
    let Some(port) = launch.port else {
        if matches!(launch.ready, ReadyCheck::Port) {
            signal_ready(&launch.name, &store, &log, &ready);
        }
        let _sent = listening
            .send(Listening {
                name: launch.name,
                port: None,
                elapsed: epoch.elapsed(),
            })
            .await;
        return;
    };
    loop {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            let elapsed = epoch.elapsed();
            let since_start = store.mark_listening(&launch.name).unwrap_or(elapsed);
            log.line(
                &launch.name,
                &format!(
                    "listening on http://localhost:{port} after {:.1}s",
                    since_start.as_secs_f64()
                ),
            );
            if matches!(launch.ready, ReadyCheck::Port) {
                signal_ready(&launch.name, &store, &log, &ready);
            }
            let _sent = listen_tx.send(());
            let _sent = listening
                .send(Listening {
                    name: launch.name,
                    port: Some(port),
                    elapsed,
                })
                .await;
            return;
        }
        tokio::time::sleep(PORT_POLL).await;
    }
}

async fn http_ready(
    launch: Launch,
    listening: oneshot::Receiver<()>,
    log: Logger,
    store: Arc<Store>,
    ready: ReadySignal,
) {
    let (ReadyCheck::Http(path), Some(port)) = (&launch.ready, launch.port) else {
        return;
    };
    if listening.await.is_err() {
        return;
    }
    let client = control::client();
    let url = format!("http://127.0.0.1:{port}{path}");
    loop {
        if let Ok((status, _, _)) = control::get(&client, &url, Duration::from_secs(30)).await
            && !status.is_server_error()
        {
            log.dim(&launch.name, &format!("{path} answered {status}"));
            signal_ready(&launch.name, &store, &log, &ready);
            return;
        }
        tokio::time::sleep(HTTP_READY_POLL).await;
    }
}

async fn warm(launch: Launch, ready: oneshot::Receiver<()>, log: Logger) {
    let Some(port) = launch.port else {
        return;
    };
    if launch.warm.is_empty() || ready.await.is_err() {
        return;
    }
    let client = control::client();
    for path in &launch.warm {
        let url = format!("http://127.0.0.1:{port}{path}");
        match control::get(&client, &url, WARM_TIMEOUT).await {
            Ok((status, _, took)) => log.dim(
                &launch.name,
                &format!("warmed {path} ({status}) in {:.1}s", took.as_secs_f64()),
            ),
            Err(err) => log.line(&launch.name, &format!("warming {path} failed: {err:#}")),
        }
    }
}

async fn terminate(name: &str, pid: Option<u32>, child: &mut Child, log: &Logger) {
    let group = pid
        .and_then(|p| i32::try_from(p).ok())
        .and_then(Pid::from_raw);
    if let Some(group) = group {
        let _sent = rustix::process::kill_process_group(group, Signal::TERM);
    } else {
        let _sent = child.start_kill();
    }
    match tokio::time::timeout(TERM_GRACE, child.wait()).await {
        Ok(Ok(status)) => log.dim(name, &format!("stopped ({status})")),
        Ok(Err(err)) => log.line(name, &format!("wait failed: {err}")),
        Err(_) => {
            log.line(
                name,
                &format!("still running after {}s, killing", TERM_GRACE.as_secs()),
            );
            if let Some(group) = group {
                let _sent = rustix::process::kill_process_group(group, Signal::KILL);
            }
            let _sent = child.start_kill();
            let _waited = child.wait().await;
        }
    }
}

pub async fn busy_ports(ports: &[(String, u16)]) -> Vec<String> {
    let mut busy = Vec::new();
    for (name, port) in ports {
        if TcpStream::connect(("127.0.0.1", *port)).await.is_ok() {
            busy.push(format!("{name} :{port}{}", owner_of(*port).await));
        }
    }
    busy
}

async fn owner_of(port: u16) -> String {
    let output = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"])
        .stdin(Stdio::null())
        .output()
        .await;
    let Ok(output) = output else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let (mut pid, mut command) = (None, None);
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid.get_or_insert(p.to_owned());
        } else if let Some(c) = line.strip_prefix('c') {
            command.get_or_insert(c.to_owned());
        }
    }
    match (pid, command) {
        (Some(pid), Some(command)) => format!(" (pid {pid}, {command})"),
        (Some(pid), None) => format!(" (pid {pid})"),
        _ => String::new(),
    }
}
