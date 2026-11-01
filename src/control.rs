use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use http::{Method, Request, StatusCode, Uri};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::body::{self, Body, html_escape, strip_ansi};
use crate::state::{LogPage, ServiceView, Store};

pub const PREFIX: &str = "/__zubi";
const DEFAULT_LOG_LINES: usize = 200;
const MAX_LOG_LINES: usize = 1000;

pub struct Control {
    pub store: Arc<Store>,
    pub proxy_base: Uri,
    pub root: String,
}

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusView<'a> {
    tool: &'static str,
    root: &'a str,
    proxy: &'a str,
    uptime_ms: u128,
    all_ready: bool,
    services: Vec<ServiceView>,
}

impl Control {
    pub fn status_json(&self) -> String {
        let view = StatusView {
            tool: "zubi",
            root: &self.root,
            proxy: &self.proxy_base.to_string(),
            uptime_ms: self.store.uptime().as_millis(),
            all_ready: self.store.all_ready(),
            services: self.store.snapshot(),
        };
        serde_json::to_string(&view).unwrap_or_else(|_| "{}".to_owned())
    }

    pub async fn handle(&self, req: Request<Incoming>, path: &str) -> http::Response<Body> {
        let query = req.uri().query().unwrap_or("").to_owned();
        match (req.method(), path) {
            (&Method::GET, "" | "/") => body::html(StatusCode::OK, page(DASHBOARD)),
            (&Method::GET, "/health") => body::json(StatusCode::OK, r#"{"ok":true,"tool":"zubi"}"#),
            (&Method::GET, "/api/status") => body::json(StatusCode::OK, self.status_json()),
            (&Method::GET, p) if p.starts_with("/api/logs/") => {
                let service = p.trim_start_matches("/api/logs/");
                self.logs_response(service, &query)
            }
            (&Method::GET, p) if p.starts_with("/logs/") => {
                let service = p.trim_start_matches("/logs/");
                match self.store.phase_of(service) {
                    Some(_) => body::html(
                        StatusCode::OK,
                        page(LOG_VIEW).replace("__SERVICE__", &html_escape(service)),
                    ),
                    None => body::text(
                        StatusCode::NOT_FOUND,
                        format!("unknown service {service}\n"),
                    ),
                }
            }
            (&Method::POST, p) if p.starts_with("/api/restart/") => {
                let service = p.trim_start_matches("/api/restart/");
                self.restart_response(service)
            }
            _ => body::text(StatusCode::NOT_FOUND, "zubi: no such control route\n"),
        }
    }

    fn logs_response(&self, service: &str, query: &str) -> http::Response<Body> {
        let since = query_param(query, "since")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let lines = query_param(query, "lines")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_LOG_LINES)
            .min(MAX_LOG_LINES);
        let Some(page) = self.store.logs(service, since, lines) else {
            return body::json(
                StatusCode::NOT_FOUND,
                format!(
                    r#"{{"error":"unknown service {}"}}"#,
                    service.replace('"', "")
                ),
            );
        };
        let clean = LogPage {
            since: page.since,
            next: page.next,
            lines: page.lines.iter().map(|l| strip_ansi(l)).collect(),
        };
        body::json(
            StatusCode::OK,
            serde_json::to_string(&clean).unwrap_or_else(|_| "{}".to_owned()),
        )
    }

    fn restart_response(&self, service: &str) -> http::Response<Body> {
        if self.store.request_restart(service) {
            body::json(
                StatusCode::ACCEPTED,
                format!(r#"{{"ok":true,"service":"{}"}}"#, service.replace('"', "")),
            )
        } else {
            body::json(
                StatusCode::NOT_FOUND,
                format!(
                    r#"{{"error":"unknown service {}"}}"#,
                    service.replace('"', "")
                ),
            )
        }
    }
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.replace('+', " "))
}

pub fn client() -> Client<HttpConnector, Body> {
    Client::builder(TokioExecutor::new()).build_http()
}

pub async fn get(
    client: &Client<HttpConnector, Body>,
    url: &str,
    timeout: Duration,
) -> Result<(StatusCode, String, Duration)> {
    let uri: Uri = url.parse().with_context(|| format!("parsing {url}"))?;
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(body::empty())
        .map_err(|e| anyhow!("{e}"))?;
    let started = Instant::now();
    let resp = tokio::time::timeout(timeout, client.request(req))
        .await
        .map_err(|_| anyhow!("{url}: no response within {}s", timeout.as_secs()))?
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let bytes = http_body_util::Limited::new(resp.into_body(), 8 << 20)
        .collect()
        .await
        .map_err(|e| anyhow!("reading {url}: {e}"))?
        .to_bytes();
    Ok((
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        started.elapsed(),
    ))
}

pub async fn post(client: &Client<HttpConnector, Body>, url: &str) -> Result<(StatusCode, String)> {
    let uri: Uri = url.parse().with_context(|| format!("parsing {url}"))?;
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .body(body::empty())
        .map_err(|e| anyhow!("{e}"))?;
    let resp = tokio::time::timeout(Duration::from_secs(10), client.request(req))
        .await
        .map_err(|_| anyhow!("{url}: no response within 10s"))?
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let bytes = http_body_util::Limited::new(resp.into_body(), 1 << 20)
        .collect()
        .await
        .map_err(|e| anyhow!("reading {url}: {e}"))?
        .to_bytes();
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

const STYLE: &str = r#"
:root{--bg:#fff;--fg:#1d1d1f;--muted:#6e6e73;--line:#e5e5ea;--ok:#1a7f37;--warn:#9a6700;--bad:#cf222e;--chip:#f2f2f7;--link:#0a66c2}
@media (prefers-color-scheme:dark){:root{--bg:#121214;--fg:#f2f2f2;--muted:#9a9aa0;--line:#2c2c30;--ok:#3fb950;--warn:#d29922;--bad:#f85149;--chip:#1e1e22;--link:#6cb6ff}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.5 -apple-system,system-ui,"Segoe UI",sans-serif}
main{max-width:64rem;margin:0 auto;padding:1.5rem 1rem}h1{font-size:1.1rem;margin:0 0 .25rem}.sub{color:var(--muted);margin:0 0 1rem}
table{width:100%;border-collapse:collapse}th,td{text-align:left;padding:.5rem .6rem;border-bottom:1px solid var(--line);vertical-align:top}th{color:var(--muted);font-weight:600;font-size:12px;text-transform:uppercase;letter-spacing:.04em}
.chip{display:inline-block;padding:.05rem .5rem;border-radius:999px;background:var(--chip);font-size:12px}.ready{color:var(--ok)}.listening,.starting,.restarting{color:var(--warn)}.exited{color:var(--bad)}
a{color:var(--link);text-decoration:none}a:hover{text-decoration:underline}button{font:inherit;padding:.2rem .6rem;border:1px solid var(--line);background:var(--chip);color:var(--fg);border-radius:6px;cursor:pointer}button:hover{border-color:var(--muted)}
.mono{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px}pre{margin:0;white-space:pre-wrap;word-break:break-word;font:12px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace;background:var(--chip);padding:1rem;border-radius:8px;max-height:75vh;overflow:auto}
.muted{color:var(--muted)}
"#;

const DASHBOARD: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>zubi</title><style>__STYLE__</style></head><body><main>
<h1>zubi</h1><p class="sub" id="sub">loading…</p>
<table><thead><tr><th>service</th><th>state</th><th>listening</th><th>ready</th><th>pid</th><th>restarts</th><th></th></tr></thead><tbody id="services"></tbody></table>
<p class="muted" style="margin-top:1rem">JSON: <a class="mono" href="/__zubi/api/status">/__zubi/api/status</a> · logs: <span class="mono">/__zubi/api/logs/&lt;service&gt;?since=N</span> · restart: <span class="mono">POST /__zubi/api/restart/&lt;service&gt;</span></p>
</main><script>
const s=(ms)=>ms==null?'—':(ms/1000).toFixed(1)+'s';
const esc=(t)=>String(t).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
async function tick(){try{const r=await fetch('/__zubi/api/status',{cache:'no-store'});const d=await r.json();
document.getElementById('sub').textContent=d.root+' · proxy '+d.proxy+' · up '+s(d.uptimeMs)+(d.allReady?' · all ready':'');
document.getElementById('services').innerHTML=d.services.map(a=>`<tr><td>${a.url?`<a href="${esc(a.url)}" target="_blank">${esc(a.name)}</a>`:esc(a.name)}<div class="muted mono">${a.port?':'+a.port+' ':''}${esc(a.command)}</div></td><td><span class="chip ${esc(a.phase)}">${esc(a.phase)}${a.status?' '+esc(a.status):''}</span></td><td>${s(a.listeningAfterMs)}</td><td>${s(a.readyAfterMs)}</td><td class="mono">${a.pid??'—'}</td><td>${a.restarts}</td><td><a href="/__zubi/logs/${encodeURIComponent(a.name)}">logs</a> &nbsp; <button onclick="restart('${esc(a.name)}')">restart</button></td></tr>`).join('');
}catch(e){document.getElementById('sub').textContent='zubi is not answering';}}
async function restart(n){await fetch('/__zubi/api/restart/'+encodeURIComponent(n),{method:'POST'});tick();}
tick();setInterval(tick,1500);
</script></body></html>"#;

const LOG_VIEW: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>__SERVICE__ · zubi</title><style>__STYLE__</style></head><body><main>
<h1>__SERVICE__ <span class="muted" id="state"></span></h1><p class="sub"><a href="/__zubi">← all services</a> · <button onclick="restart()">restart</button> · <label><input type="checkbox" id="follow" checked> follow</label></p>
<pre id="log"></pre></main><script>
let next=0;const log=document.getElementById('log');const name='__SERVICE__';
async function pull(){try{const r=await fetch('/__zubi/api/logs/'+encodeURIComponent(name)+'?since='+next+'&lines=1000',{cache:'no-store'});const d=await r.json();
if(d.lines.length){log.textContent+=d.lines.join('\n')+'\n';next=d.next;if(document.getElementById('follow').checked)log.scrollTop=log.scrollHeight;}
const st=await (await fetch('/__zubi/api/status',{cache:'no-store'})).json();const a=st.services.find(x=>x.name===name);if(a)document.getElementById('state').textContent='· '+a.phase+(a.status?' '+a.status:'')+(a.port?' · :'+a.port:'');}catch(e){}}
async function restart(){await fetch('/__zubi/api/restart/'+encodeURIComponent(name),{method:'POST'});}
pull();setInterval(pull,1000);
</script></body></html>"#;

pub fn page(template: &str) -> String {
    template.replace("__STYLE__", STYLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_params() {
        assert_eq!(
            query_param("svc=web&lines=5", "svc").as_deref(),
            Some("web")
        );
        assert_eq!(
            query_param("svc=web&lines=5", "lines").as_deref(),
            Some("5")
        );
        assert_eq!(query_param("", "svc"), None);
    }
}
