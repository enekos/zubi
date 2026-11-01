use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::Notify;

use crate::plan::Launch;

const LOG_LINES_PER_SERVICE: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub enum Phase {
    Starting,
    Listening,
    Ready,
    Restarting,
    Exited { status: String },
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Phase::Starting => f.write_str("starting"),
            Phase::Listening => f.write_str("listening"),
            Phase::Ready => f.write_str("ready"),
            Phase::Restarting => f.write_str("restarting"),
            Phase::Exited { status } => write!(f, "exited ({status})"),
        }
    }
}

#[derive(Debug)]
pub struct ServiceState {
    pub name: String,
    pub kind: String,
    pub command: String,
    pub port: Option<u16>,
    pub pid: Option<u32>,
    pub started_at: Instant,
    pub phase: Phase,
    pub listening_after: Option<Duration>,
    pub ready_after: Option<Duration>,
    pub restarts: u32,
    pub next_seq: u64,
    pub logs: VecDeque<(u64, String)>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceView {
    pub name: String,
    pub kind: String,
    pub command: String,
    pub port: Option<u16>,
    pub url: Option<String>,
    pub pid: Option<u32>,
    #[serde(flatten)]
    pub phase: Phase,
    pub listening_after_ms: Option<u128>,
    pub ready_after_ms: Option<u128>,
    pub restarts: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogPage {
    pub since: u64,
    pub next: u64,
    pub lines: Vec<String>,
}

#[derive(Debug)]
pub struct Store {
    epoch: Instant,
    services: Mutex<Vec<ServiceState>>,
    restarts: HashMap<String, Arc<Notify>>,
}

impl Store {
    pub fn new(launches: &[Launch], epoch: Instant) -> Arc<Self> {
        let services = launches
            .iter()
            .map(|l| ServiceState {
                name: l.name.clone(),
                kind: l.kind.clone(),
                command: l.command_line(),
                port: l.port,
                pid: None,
                started_at: epoch,
                phase: Phase::Starting,
                listening_after: None,
                ready_after: None,
                restarts: 0,
                next_seq: 0,
                logs: VecDeque::with_capacity(64),
            })
            .collect();
        let restarts = launches
            .iter()
            .map(|l| (l.name.clone(), Arc::new(Notify::new())))
            .collect();
        Arc::new(Store {
            epoch,
            services: Mutex::new(services),
            restarts,
        })
    }

    pub fn empty(epoch: Instant) -> Arc<Self> {
        Arc::new(Store {
            epoch,
            services: Mutex::new(Vec::new()),
            restarts: HashMap::new(),
        })
    }

    pub fn uptime(&self) -> Duration {
        self.epoch.elapsed()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<ServiceState>> {
        self.services.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn restart_handle(&self, name: &str) -> Option<Arc<Notify>> {
        self.restarts.get(name).map(Arc::clone)
    }

    pub fn request_restart(&self, name: &str) -> bool {
        match self.restarts.get(name) {
            Some(notify) => {
                notify.notify_one();
                true
            }
            None => false,
        }
    }

    pub fn update(&self, name: &str, change: impl FnOnce(&mut ServiceState)) {
        let mut services = self.lock();
        if let Some(service) = services.iter_mut().find(|s| s.name == name) {
            change(service);
        }
    }

    pub fn mark_listening(&self, name: &str) -> Option<Duration> {
        let mut first = None;
        self.update(name, |s| {
            let elapsed = s.started_at.elapsed();
            if s.listening_after.is_none() {
                s.listening_after = Some(elapsed);
                first = Some(elapsed);
            }
            if s.phase == Phase::Starting {
                s.phase = Phase::Listening;
            }
        });
        first
    }

    pub fn mark_ready(&self, name: &str) -> Option<Duration> {
        let mut first = None;
        self.update(name, |s| {
            let elapsed = s.started_at.elapsed();
            if s.ready_after.is_none() {
                s.ready_after = Some(elapsed);
                first = Some(elapsed);
            }
            if matches!(s.phase, Phase::Starting | Phase::Listening) {
                s.phase = Phase::Ready;
            }
        });
        first
    }

    pub fn push_log(&self, name: &str, line: &str) {
        self.update(name, |s| {
            if s.logs.len() >= LOG_LINES_PER_SERVICE {
                s.logs.pop_front();
            }
            s.logs.push_back((s.next_seq, line.to_owned()));
            s.next_seq += 1;
        });
    }

    pub fn snapshot(&self) -> Vec<ServiceView> {
        self.lock()
            .iter()
            .map(|s| ServiceView {
                name: s.name.clone(),
                kind: s.kind.clone(),
                command: s.command.clone(),
                port: s.port,
                url: s.port.map(|p| format!("http://localhost:{p}")),
                pid: s.pid,
                phase: s.phase.clone(),
                listening_after_ms: s.listening_after.map(|d| d.as_millis()),
                ready_after_ms: s.ready_after.map(|d| d.as_millis()),
                restarts: s.restarts,
            })
            .collect()
    }

    pub fn phase_of(&self, name: &str) -> Option<Phase> {
        self.lock()
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.phase.clone())
    }

    pub fn all_ready(&self) -> bool {
        let services = self.lock();
        !services.is_empty() && services.iter().all(|s| s.phase == Phase::Ready)
    }

    pub fn logs(&self, name: &str, since: u64, limit: usize) -> Option<LogPage> {
        let services = self.lock();
        let service = services.iter().find(|s| s.name == name)?;
        let lines: Vec<String> = service
            .logs
            .iter()
            .filter(|(seq, _)| *seq >= since)
            .map(|(_, line)| line.clone())
            .collect();
        let skip = lines.len().saturating_sub(limit);
        let lines: Vec<String> = lines.into_iter().skip(skip).collect();
        Some(LogPage {
            since,
            next: service.next_seq,
            lines,
        })
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::indexing_slicing,
        reason = "a test may panic on a missing index; that is the failure it reports"
    )]
    use super::*;
    use crate::plan::ReadyCheck;
    use std::path::PathBuf;

    fn launch(name: &str, port: u16) -> Launch {
        Launch {
            name: name.into(),
            dir: PathBuf::from("."),
            port: Some(port),
            program: PathBuf::from("next"),
            args: vec!["dev".into()],
            env: Vec::new(),
            env_file: None,
            ready: ReadyCheck::Port,
            warm: Vec::new(),
            before: None,
            kind: "next".into(),
        }
    }

    #[test]
    fn phases_and_timings() {
        let store = Store::new(&[launch("auth", 3001)], Instant::now());
        assert_eq!(store.phase_of("auth"), Some(Phase::Starting));
        assert!(store.mark_listening("auth").is_some());
        assert!(store.mark_listening("auth").is_none());
        assert_eq!(store.phase_of("auth"), Some(Phase::Listening));
        assert!(!store.all_ready());
        store.mark_ready("auth");
        assert!(store.all_ready());
        assert!(store.request_restart("auth"));
        assert!(!store.request_restart("nope"));
        assert_eq!(
            store.snapshot()[0].url.as_deref(),
            Some("http://localhost:3001")
        );
    }

    #[test]
    fn log_ring_is_bounded_and_cursorable() {
        let store = Store::new(&[launch("a", 1)], Instant::now());
        for i in 0..(LOG_LINES_PER_SERVICE + 5) {
            store.push_log("a", &format!("line {i}"));
        }
        let page = store.logs("a", 0, 10_000).unwrap();
        assert_eq!(page.lines.len(), LOG_LINES_PER_SERVICE);
        assert_eq!(page.lines.first().map(String::as_str), Some("line 5"));
        assert_eq!(page.next, (LOG_LINES_PER_SERVICE + 5) as u64);
        let tail = store.logs("a", page.next - 2, 100).unwrap();
        assert_eq!(tail.lines, vec!["line 1003", "line 1004"]);
        assert_eq!(store.logs("a", 0, 3).unwrap().lines.len(), 3);
        assert!(store.logs("missing", 0, 1).is_none());
    }
}
