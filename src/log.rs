use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const QUEUE: usize = 4096;
const BATCH: usize = 1 << 16;
const PALETTE: [&str; 7] = [
    "\x1b[36m", "\x1b[32m", "\x1b[33m", "\x1b[35m", "\x1b[34m", "\x1b[91m", "\x1b[96m",
];
const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";

#[derive(Debug, Clone)]
pub struct Logger {
    tx: mpsc::Sender<String>,
    dropped: Arc<AtomicUsize>,
    color: bool,
    width: usize,
}

pub fn start(width: usize) -> (Logger, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<String>(QUEUE);
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        let mut batch = String::with_capacity(8192);
        while let Some(first) = rx.recv().await {
            batch.clear();
            batch.push_str(&first);
            while batch.len() < BATCH {
                match rx.try_recv() {
                    Ok(more) => batch.push_str(&more),
                    Err(_) => break,
                }
            }
            if out.write_all(batch.as_bytes()).await.is_err() {
                return;
            }
        }
        let _flushed = out.flush().await;
    });
    let color = std::env::var_os("NO_COLOR").is_none();
    (
        Logger {
            tx,
            dropped: Arc::new(AtomicUsize::new(0)),
            color,
            width: width.clamp(6, 32),
        },
        writer,
    )
}

impl Logger {
    pub fn line(&self, prefix: &str, msg: &str) {
        let mut s = String::with_capacity(prefix.len() + msg.len() + 24);
        if self.color {
            s.push_str(
                PALETTE
                    .get(hash(prefix) % PALETTE.len())
                    .copied()
                    .unwrap_or(RESET),
            );
        }
        s.push_str(&format!("{prefix:<w$}", w = self.width));
        if self.color {
            s.push_str(RESET);
        }
        s.push(' ');
        s.push_str(msg);
        s.push('\n');
        if self.tx.try_send(s).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn dim(&self, prefix: &str, msg: &str) {
        if self.color {
            self.line(prefix, &format!("{DIM}{msg}{RESET}"));
        } else {
            self.line(prefix, msg);
        }
    }

    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }
}

fn hash(s: &str) -> usize {
    let h = s.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    usize::try_from(h % 1024).unwrap_or(0)
}
