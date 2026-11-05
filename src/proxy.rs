use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::uri::{Authority, PathAndQuery, Scheme};
use http::{Method, Request, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Empty, Full, Limited};
use hyper::body::Incoming;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::body::{self, Body, BoxError, html_escape};
use crate::control::{self, Control};
use crate::log::Logger;
use crate::route::{Matched, Router};
use crate::state::Phase;

const GRAPHQL_BODY_LIMIT: usize = 8 << 20;

#[derive(Debug, Clone)]
pub enum TargetKind {
    Service,
    Upstream {
        host: String,
        rewrite_cookies: bool,
        rewrite_redirects: bool,
        headers: Vec<(HeaderName, HeaderValue)>,
    },
}

#[derive(Debug, Clone)]
pub struct Target {
    pub uri: Uri,
    pub kind: TargetKind,
    pub header_timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub listen: SocketAddr,
    pub base: Uri,
    pub targets: HashMap<String, Target>,
    pub router: Router,
    pub cors_origins: Vec<String>,
    pub strip_cookie_domains: bool,
}

#[derive(Debug, thiserror::Error)]
enum ProxyError {
    #[error("{target} is not answering on {uri}: {source}")]
    Upstream {
        target: String,
        uri: String,
        source: BoxError,
    },
    #[error("{target} did not send response headers within {}s", .after.as_secs())]
    Timeout { target: String, after: Duration },
    #[error("request body larger than {GRAPHQL_BODY_LIMIT} bytes")]
    BodyTooLarge,
    #[error("no route matches {0}")]
    NoRoute(String),
    #[error("route answers {0} for {1}")]
    Fixed(StatusCode, String),
    #[error("route points at {0}, which is not a service with a port or an upstream")]
    UnknownTarget(String),
    #[error("websocket upgrade to {0} is only supported for http targets")]
    UpgradeScheme(String),
    #[error("{0}")]
    Internal(#[from] anyhow::Error),
}

impl ProxyError {
    fn status(&self) -> StatusCode {
        match self {
            ProxyError::Upstream { .. } | ProxyError::UpgradeScheme(_) => StatusCode::BAD_GATEWAY,
            ProxyError::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            ProxyError::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ProxyError::NoRoute(_) | ProxyError::UnknownTarget(_) => StatusCode::NOT_FOUND,
            ProxyError::Fixed(status, _) => *status,
            ProxyError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

struct Proxy {
    cfg: ProxyConfig,
    client: Client<HttpsConnector<HttpConnector>, Body>,
    log: Logger,
    control: Arc<Control>,
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proxy")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

pub async fn serve(
    cfg: ProxyConfig,
    control: Arc<Control>,
    log: Logger,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .build();
    let client = Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(90))
        .build(https);
    let listener = TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("binding {}", cfg.listen))?;
    let proxy = Arc::new(Proxy {
        cfg,
        client,
        log,
        control,
    });
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _peer) = accepted.context("accepting a connection")?;
                let proxy = Arc::clone(&proxy);
                connections.spawn(async move {
                    let service = hyper::service::service_fn(move |req| {
                        let proxy = Arc::clone(&proxy);
                        async move { Ok::<_, std::convert::Infallible>(proxy.handle(req).await) }
                    });
                    let conn = hyper::server::conn::http1::Builder::new()
                        .preserve_header_case(true)
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades();
                    if let Err(err) = conn.await
                        && !is_benign_connection_error(&err)
                    {
                        eprintln!("zubi proxy: connection error: {err}");
                    }
                });
            }
            Some(finished) = connections.join_next(), if !connections.is_empty() => {
                if let Err(err) = finished
                    && err.is_panic()
                {
                    eprintln!("zubi proxy: a connection task panicked: {err}");
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    connections.shutdown().await;
    Ok(())
}

fn is_benign_connection_error(err: &hyper::Error) -> bool {
    err.is_incomplete_message() || err.is_canceled() || err.is_closed()
}

impl Proxy {
    async fn handle(self: Arc<Self>, req: Request<Incoming>) -> Response<Body> {
        let started = Instant::now();
        let method = req.method().clone();
        let path_and_query = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str().to_owned())
            .unwrap_or_else(|| "/".into());
        let wants_html = req
            .headers()
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.contains("text/html"));
        let cors = (!self.cfg.cors_origins.is_empty()).then(|| self.allowed_origin(req.headers()));
        if let Some(origin) = &cors
            && method == Method::OPTIONS
        {
            let mut resp = body::text(StatusCode::NO_CONTENT, String::new());
            apply_cors(resp.headers_mut(), origin.as_ref());
            self.log
                .line("proxy", &format!("{method} {path_and_query} => [cors] 204"));
            return resp;
        }
        let mut resp = self
            .respond(req, &method, &path_and_query, wants_html, started)
            .await;
        if let Some(origin) = &cors {
            apply_cors(resp.headers_mut(), origin.as_ref());
        }
        resp
    }

    fn allowed_origin(&self, headers: &HeaderMap) -> Option<HeaderValue> {
        let origin = headers.get(header::ORIGIN)?;
        let host = origin
            .to_str()
            .ok()?
            .parse::<Uri>()
            .ok()?
            .authority()?
            .as_str()
            .to_ascii_lowercase();
        self.cfg
            .cors_origins
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&host))
            .then(|| origin.clone())
    }

    async fn respond(
        &self,
        req: Request<Incoming>,
        method: &Method,
        path_and_query: &str,
        wants_html: bool,
        started: Instant,
    ) -> Response<Body> {
        match self.route(req).await {
            Ok((label, resp)) => {
                self.log.line(
                    "proxy",
                    &format!(
                        "{method} {path_and_query} => {label} {} {}ms",
                        resp.status().as_u16(),
                        started.elapsed().as_millis()
                    ),
                );
                resp
            }
            Err(err) => {
                self.log.line(
                    "proxy",
                    &format!(
                        "{method} {path_and_query} => {} {err}",
                        err.status().as_u16()
                    ),
                );
                self.error_response(&err, wants_html && *method == Method::GET)
            }
        }
    }

    fn resolve(&self, req: &Request<Incoming>) -> Result<(Matched<'_>, &Target), ProxyError> {
        let path = req.uri().path();
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok());
        let referer_path = self.cfg.router.routes_by_referer(path).then(|| {
            req.headers()
                .get(header::REFERER)
                .and_then(|v| v.to_str().ok())
                .and_then(|r| r.parse::<Uri>().ok())
                .map(|r| r.path().to_owned())
        });
        let matched = match referer_path.flatten() {
            Some(referer) => self
                .cfg
                .router
                .resolve(host, &referer)
                .map(|m| match m {
                    Matched::Forward { to, .. } => Matched::Forward {
                        to,
                        strip: 0,
                        keep_prefix_on_upgrade: false,
                    },
                    fixed @ Matched::Status(_) => fixed,
                })
                .or_else(|| self.cfg.router.resolve(host, path)),
            None => self.cfg.router.resolve(host, path),
        }
        .ok_or_else(|| ProxyError::NoRoute(path.to_owned()))?;
        let to = match matched {
            Matched::Forward { to, .. } => to,
            Matched::Status(code) => {
                let status = StatusCode::from_u16(code)
                    .map_err(|_| anyhow!("route status {code} is not valid"))?;
                return Err(ProxyError::Fixed(status, path.to_owned()));
            }
        };
        let target = self
            .cfg
            .targets
            .get(to)
            .ok_or_else(|| ProxyError::UnknownTarget(to.to_owned()))?;
        Ok((matched, target))
    }

    async fn route(&self, req: Request<Incoming>) -> Result<(String, Response<Body>), ProxyError> {
        let path = req.uri().path().to_owned();
        if path == control::PREFIX || path.starts_with("/__zubi/") {
            let rest = path.strip_prefix(control::PREFIX).unwrap_or("");
            let resp = self.control.handle(req, rest).await;
            return Ok(("[zubi]".into(), resp));
        }
        let (matched, target) = self.resolve(&req)?;
        let key = match matched {
            Matched::Forward { to, .. } => to.to_owned(),
            Matched::Status(_) => String::new(),
        };
        let target = target.clone();
        if is_upgrade(req.headers()) {
            let forward = forward_path_and_query(req.uri(), matched.for_upgrade())?;
            return self.upgrade(req, &key, &target, forward).await;
        }
        let forward = forward_path_and_query(req.uri(), matched)?;

        let (parts, body) = req.into_parts();
        let (body, operation) = if is_graphql_json_post(&parts.method, &path, &parts.headers) {
            let bytes = Limited::new(body, GRAPHQL_BODY_LIMIT)
                .collect()
                .await
                .map_err(|_| ProxyError::BodyTooLarge)?
                .to_bytes();
            let operation = graphql_operation_name(&bytes);
            (
                Full::new(bytes).map_err(|never| match never {}).boxed(),
                operation,
            )
        } else {
            (body.map_err(BoxError::from).boxed(), None)
        };

        let label = match operation {
            Some(op) => format!("{op} => [{key}] {}", target.uri),
            None => format!("[{key}] {}", target.uri),
        };
        let resp = self.forward(parts, body, &key, &target, forward).await?;
        Ok((label, resp))
    }

    async fn forward(
        &self,
        parts: http::request::Parts,
        body: Body,
        key: &str,
        target: &Target,
        forward: PathAndQuery,
    ) -> Result<Response<Body>, ProxyError> {
        let uri = join_uri(&target.uri, Some(&forward))?;
        let mut upstream = Request::builder()
            .method(parts.method)
            .uri(uri)
            .body(body)
            .map_err(|e| anyhow!("building upstream request: {e}"))?;
        copy_request_headers(&parts.headers, upstream.headers_mut(), &target.uri);
        set_upstream_headers(upstream.headers_mut(), target);

        let fut = self.client.request(upstream);
        let resp = match target.header_timeout {
            Some(after) => {
                tokio::time::timeout(after, fut)
                    .await
                    .map_err(|_| ProxyError::Timeout {
                        target: key.to_owned(),
                        after,
                    })?
            }
            None => fut.await,
        };
        let resp = resp.map_err(|source| ProxyError::Upstream {
            target: key.to_owned(),
            uri: target.uri.to_string(),
            source: Box::new(source),
        })?;

        let (rparts, rbody) = resp.into_parts();
        let mut out = Response::builder()
            .status(rparts.status)
            .body(rbody.map_err(BoxError::from).boxed())
            .map_err(|e| anyhow!("building response: {e}"))?;
        self.copy_response_headers(&rparts.headers, out.headers_mut(), target);
        Ok(out)
    }

    fn copy_response_headers(&self, from: &HeaderMap, to: &mut HeaderMap, target: &Target) {
        let (host, cookies, redirects) = match &target.kind {
            TargetKind::Upstream {
                host,
                rewrite_cookies,
                rewrite_redirects,
                ..
            } => (host.as_str(), *rewrite_cookies, *rewrite_redirects),
            TargetKind::Service => ("", false, false),
        };
        let strip_secure = self.cfg.base.scheme() != Some(&Scheme::HTTPS);
        let cors = !self.cfg.cors_origins.is_empty();
        for (name, value) in from {
            if is_hop_by_hop(name) || (cors && name == header::ACCESS_CONTROL_ALLOW_ORIGIN) {
                continue;
            }
            if self.cfg.strip_cookie_domains
                && name == header::SET_COOKIE
                && let Ok(cookie) = value.to_str()
                && let Ok(rewritten) =
                    HeaderValue::from_str(&strip_set_cookie_domain(cookie, strip_secure))
            {
                to.append(name.clone(), rewritten);
                continue;
            }
            if cookies
                && name == header::SET_COOKIE
                && let Ok(cookie) = value.to_str()
                && let Ok(rewritten) =
                    HeaderValue::from_str(&rewrite_set_cookie(cookie, host, strip_secure))
            {
                to.append(name.clone(), rewritten);
                continue;
            }
            if redirects
                && name == header::LOCATION
                && let Ok(location) = value.to_str()
                && let Some(rewritten) = rewrite_redirect(location, host, &self.cfg.base)
                && let Ok(hv) = HeaderValue::from_str(&rewritten)
            {
                to.append(name.clone(), hv);
                continue;
            }
            to.append(name.clone(), value.clone());
        }
    }

    async fn upgrade(
        &self,
        mut req: Request<Incoming>,
        key: &str,
        target: &Target,
        forward: PathAndQuery,
    ) -> Result<(String, Response<Body>), ProxyError> {
        let uri = &target.uri;
        if uri.scheme() != Some(&Scheme::HTTP) {
            return Err(ProxyError::UpgradeScheme(uri.to_string()));
        }
        let host = uri.host().ok_or_else(|| anyhow!("{uri} has no host"))?;
        let port = uri.port_u16().unwrap_or(80);
        let upstream_error = |source: BoxError| ProxyError::Upstream {
            target: key.to_owned(),
            uri: uri.to_string(),
            source,
        };

        let stream = TcpStream::connect((host, port))
            .await
            .map_err(|e| upstream_error(Box::new(e)))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| upstream_error(Box::new(e)))?;
        tokio::spawn(async move {
            if let Err(err) = conn.with_upgrades().await
                && !is_benign_connection_error(&err)
            {
                eprintln!("zubi proxy: upgrade connection error: {err}");
            }
        });

        let client_upgrade = hyper::upgrade::on(&mut req);
        let (parts, _body) = req.into_parts();
        let mut up_req = Request::builder()
            .method(parts.method)
            .uri(Uri::from(forward))
            .body(Empty::<Bytes>::new())
            .map_err(|e| anyhow!("building upgrade request: {e}"))?;
        for (name, value) in &parts.headers {
            if name == header::HOST {
                continue;
            }
            up_req.headers_mut().append(name.clone(), value.clone());
        }
        up_req.headers_mut().insert(header::HOST, host_header(uri)?);
        set_upstream_headers(up_req.headers_mut(), target);

        let mut resp = sender
            .send_request(up_req)
            .await
            .map_err(|e| upstream_error(Box::new(e)))?;
        if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
            let (rparts, rbody) = resp.into_parts();
            let out = Response::from_parts(rparts, rbody.map_err(BoxError::from).boxed());
            return Ok((format!("[{key}] {uri} (upgrade refused)"), out));
        }
        let upstream_upgrade = hyper::upgrade::on(&mut resp);
        let log = self.log.clone();
        let label = key.to_owned();
        tokio::spawn(async move {
            match tokio::try_join!(client_upgrade, upstream_upgrade) {
                Ok((client, upstream)) => {
                    let mut client = TokioIo::new(client);
                    let mut upstream = TokioIo::new(upstream);
                    if let Err(err) =
                        tokio::io::copy_bidirectional(&mut client, &mut upstream).await
                        && err.kind() != std::io::ErrorKind::NotConnected
                        && err.kind() != std::io::ErrorKind::BrokenPipe
                        && err.kind() != std::io::ErrorKind::ConnectionReset
                    {
                        log.dim("proxy", &format!("[{label}] websocket closed: {err}"));
                    }
                }
                Err(err) => log.line(
                    "proxy",
                    &format!("[{label}] websocket upgrade failed: {err}"),
                ),
            }
        });
        let (rparts, _) = resp.into_parts();
        let out = Response::from_parts(
            rparts,
            Empty::<Bytes>::new()
                .map_err(|never| match never {})
                .boxed(),
        );
        Ok((format!("[{key}] {uri} (websocket)"), out))
    }

    fn error_response(&self, err: &ProxyError, html: bool) -> Response<Body> {
        let status = err.status();
        if !html {
            return body::text(status, format!("zubi: {err}\n"));
        }
        let (service, phase) = match err {
            ProxyError::Upstream { target, .. } => {
                (Some(target.as_str()), self.control.store.phase_of(target))
            }
            _ => (None, None),
        };
        let refresh = if matches!(err, ProxyError::Upstream { .. }) {
            r#"<meta http-equiv="refresh" content="2">"#
        } else {
            ""
        };
        let prefix = control::PREFIX;
        let state_line = match (service, phase) {
            (Some(s), Some(Phase::Exited { status })) => format!(
                "<p><strong>{a}</strong> exited ({s}). <a href=\"{prefix}/logs/{a}\">Read its log</a> or restart it from the <a href=\"{prefix}\">dashboard</a>.</p>",
                a = html_escape(s),
                s = html_escape(&status),
            ),
            (Some(s), Some(phase)) => format!(
                "<p><strong>{a}</strong> is {phase} under zubi. This page retries every 2 seconds. <a href=\"{prefix}/logs/{a}\">Watch its log</a>.</p>",
                a = html_escape(s),
            ),
            (Some(s), None) => format!(
                "<p><strong>{a}</strong> is not managed by this zubi instance; start it yourself, or pass <code>--with {a}</code>. This page retries every 2 seconds.</p>",
                a = html_escape(s),
            ),
            (None, _) => String::new(),
        };
        let page = format!(
            "<!doctype html><html><head><meta charset=\"utf-8\">{refresh}<title>zubi · {status}</title><style>{ERROR_STYLE}</style></head>\
             <body><main><h1>zubi: {status}</h1><p><code>{message}</code></p>{state_line}\
             <p class=\"muted\"><a href=\"{prefix}\">all services</a></p></main></body></html>",
            message = html_escape(&err.to_string()),
        );
        body::html(status, page)
    }
}

const ERROR_STYLE: &str = "body{font:16px/1.5 -apple-system,system-ui,sans-serif;margin:3rem auto;max-width:40rem;padding:0 1rem;color:#1d1d1f;background:#fff}\
@media (prefers-color-scheme:dark){body{color:#f2f2f2;background:#121214}}code{background:rgba(127,127,127,.15);padding:.1em .3em;border-radius:3px}h1{font-size:1.2rem}.muted{opacity:.7}a{color:inherit}";

fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.get(header::UPGRADE).is_some()
        && headers
            .get(header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.to_ascii_lowercase().contains("upgrade"))
}

const CORS_METHODS: &str = "GET,POST,PUT,PATCH,DELETE,OPTIONS";
const CORS_HEADERS: &str = "Content-Type, Authorization, baggage, sentry-trace";

fn apply_cors(headers: &mut HeaderMap, origin: Option<&HeaderValue>) {
    headers.remove(header::ACCESS_CONTROL_ALLOW_ORIGIN);
    let Some(origin) = origin else {
        return;
    };
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(CORS_METHODS),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(CORS_HEADERS),
    );
    let varies_by_origin = headers.get_all(header::VARY).iter().any(|v| {
        v.to_str().is_ok_and(|v| {
            v.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("origin"))
        })
    });
    if !varies_by_origin {
        headers.append(header::VARY, HeaderValue::from_static("Origin"));
    }
}

fn set_upstream_headers(headers: &mut HeaderMap, target: &Target) {
    if let TargetKind::Upstream { headers: extra, .. } = &target.kind {
        for (name, value) in extra {
            headers.insert(name.clone(), value.clone());
        }
    }
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn is_graphql_json_post(method: &Method, path: &str, headers: &HeaderMap) -> bool {
    method == Method::POST
        && path.ends_with("/graphql")
        && headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/json"))
}

pub fn graphql_operation_name(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    match value {
        serde_json::Value::Object(map) => map
            .get("operationName")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        serde_json::Value::Array(items) => {
            let names: Vec<&str> = items
                .iter()
                .filter_map(|item| item.get("operationName"))
                .filter_map(|v| v.as_str())
                .collect();
            (!names.is_empty()).then(|| names.join("+"))
        }
        _ => None,
    }
}

fn forward_path_and_query(uri: &Uri, matched: Matched<'_>) -> Result<PathAndQuery, ProxyError> {
    let original = uri
        .path_and_query()
        .cloned()
        .unwrap_or_else(|| PathAndQuery::from_static("/"));
    if matched.strip() == 0 {
        return Ok(original);
    }
    let path = matched.forward_path(uri.path());
    let rebuilt = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.into_owned(),
    };
    rebuilt.parse::<PathAndQuery>().map_err(|e| {
        anyhow!(
            "rewriting {} after stripping the route prefix: {e}",
            uri.path()
        )
        .into()
    })
}

fn join_uri(target: &Uri, path_and_query: Option<&PathAndQuery>) -> Result<Uri, ProxyError> {
    let mut parts = target.clone().into_parts();
    parts.path_and_query = Some(
        path_and_query
            .cloned()
            .unwrap_or_else(|| PathAndQuery::from_static("/")),
    );
    Uri::from_parts(parts)
        .map_err(|e| anyhow!("joining {target} with the request path: {e}").into())
}

fn host_header(target: &Uri) -> Result<HeaderValue, ProxyError> {
    let authority = target
        .authority()
        .ok_or_else(|| anyhow!("{target} has no authority"))?;
    let default_port = if target.scheme() == Some(&Scheme::HTTPS) {
        443
    } else {
        80
    };
    let value = match authority.port_u16() {
        Some(port) if port == default_port => authority.host().to_owned(),
        _ => authority.as_str().to_owned(),
    };
    HeaderValue::from_str(&value).map_err(|e| anyhow!("host header for {target}: {e}").into())
}

fn copy_request_headers(from: &HeaderMap, to: &mut HeaderMap, target: &Uri) {
    for (name, value) in from {
        if is_hop_by_hop(name) || name == header::HOST {
            continue;
        }
        to.append(name.clone(), value.clone());
    }
    if let Ok(host) = host_header(target) {
        to.insert(header::HOST, host);
    }
}

pub fn rewrite_set_cookie(cookie: &str, upstream_host: &str, strip_secure: bool) -> String {
    let dotted = format!(".{upstream_host}");
    rewrite_cookie_attrs(cookie, strip_secure, |domain| {
        domain.eq_ignore_ascii_case(".localhost")
            || domain.eq_ignore_ascii_case("localhost")
            || domain.eq_ignore_ascii_case(&dotted)
            || domain.eq_ignore_ascii_case(upstream_host)
    })
}

pub fn strip_set_cookie_domain(cookie: &str, strip_secure: bool) -> String {
    rewrite_cookie_attrs(cookie, strip_secure, |_| true)
}

fn rewrite_cookie_attrs(
    cookie: &str,
    strip_secure: bool,
    drop_domain: impl Fn(&str) -> bool,
) -> String {
    let mut attrs = cookie.split(';');
    let mut out = attrs.next().unwrap_or("").trim().to_owned();
    for attr in attrs {
        let attr = attr.trim();
        if attr.is_empty() {
            continue;
        }
        let (name, value) = attr
            .split_once('=')
            .map_or((attr, ""), |(n, v)| (n.trim(), v.trim()));
        if name.eq_ignore_ascii_case("domain") && drop_domain(value) {
            continue;
        }
        if strip_secure && name.eq_ignore_ascii_case("secure") {
            continue;
        }
        out.push_str("; ");
        out.push_str(attr);
    }
    out
}

pub fn rewrite_redirect(location: &str, upstream_host: &str, base: &Uri) -> Option<String> {
    let uri: Uri = location.parse().ok()?;
    if !uri.host()?.eq_ignore_ascii_case(upstream_host) {
        return None;
    }
    let mut parts = uri.into_parts();
    parts.scheme = Some(base.scheme().cloned().unwrap_or(Scheme::HTTP));
    parts.authority = Some(
        base.authority()
            .cloned()
            .unwrap_or_else(|| Authority::from_static("localhost")),
    );
    if parts.path_and_query.is_none() {
        parts.path_and_query = Some(PathAndQuery::from_static("/"));
    }
    Uri::from_parts(parts).ok().map(|u| u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_lose_upstream_domains_and_secure_on_http() {
        let cookie = "auth=abc; Path=/; Domain=.api.example.com; Secure; HttpOnly; SameSite=Lax";
        assert_eq!(
            rewrite_set_cookie(cookie, "api.example.com", true),
            "auth=abc; Path=/; HttpOnly; SameSite=Lax"
        );
        assert_eq!(
            rewrite_set_cookie(cookie, "api.example.com", false),
            "auth=abc; Path=/; Secure; HttpOnly; SameSite=Lax"
        );
        assert_eq!(
            rewrite_set_cookie(
                "theme=dark; Domain=other.com; Secure",
                "api.example.com",
                false
            ),
            "theme=dark; Domain=other.com; Secure"
        );
        assert_eq!(
            rewrite_set_cookie("sid=1; Domain=.localhost; Secure", "api.example.com", true),
            "sid=1"
        );
    }

    #[test]
    fn stripping_cookie_domains_drops_every_domain() {
        assert_eq!(
            strip_set_cookie_domain("sid=1; Domain=web-abc123.localhost; Path=/; Secure", true),
            "sid=1; Path=/"
        );
        assert_eq!(
            strip_set_cookie_domain("sid=1; domain=.join.com; Secure", false),
            "sid=1; Secure"
        );
    }

    #[test]
    fn cors_reflects_the_origin_and_replaces_the_upstream_wildcard() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
        headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        let origin = HeaderValue::from_static("http://web-abc123.localhost:8001");
        apply_cors(&mut headers, Some(&origin));
        assert_eq!(
            headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&origin)
        );
        assert_eq!(
            headers
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .unwrap(),
            "true"
        );
        assert_eq!(headers.get_all(header::VARY).iter().count(), 2);
        apply_cors(&mut headers, Some(&origin));
        assert_eq!(headers.get_all(header::VARY).iter().count(), 2);

        let mut denied = HeaderMap::new();
        denied.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
        apply_cors(&mut denied, None);
        assert!(denied.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    }

    #[test]
    fn upstream_headers_override_the_client() {
        let mut value = HeaderValue::from_static("secret");
        value.set_sensitive(true);
        let target = Target {
            uri: "https://api.example.com".parse().unwrap(),
            kind: TargetKind::Upstream {
                host: "api.example.com".into(),
                rewrite_cookies: false,
                rewrite_redirects: false,
                headers: vec![(HeaderName::from_static("x-token"), value)],
            },
            header_timeout: None,
        };
        let mut headers = HeaderMap::new();
        headers.insert("x-token", HeaderValue::from_static("from-browser"));
        set_upstream_headers(&mut headers, &target);
        assert_eq!(headers.get("x-token").unwrap(), "secret");
        assert!(!format!("{target:?}").contains("secret"));
    }

    #[test]
    fn upstream_redirects_come_back_to_the_proxy() {
        let base: Uri = "http://localhost:8000".parse().unwrap();
        assert_eq!(
            rewrite_redirect(
                "https://api.example.com/dashboard?x=1",
                "api.example.com",
                &base
            )
            .as_deref(),
            Some("http://localhost:8000/dashboard?x=1")
        );
        assert_eq!(
            rewrite_redirect("https://elsewhere.com/", "api.example.com", &base),
            None
        );
        assert_eq!(
            rewrite_redirect("/relative", "api.example.com", &base),
            None
        );
    }

    #[test]
    fn graphql_operation_names() {
        assert_eq!(
            graphql_operation_name(
                br#"{"operationName":"CreateJob","query":"mutation CreateJob {}"}"#
            )
            .as_deref(),
            Some("CreateJob")
        );
        assert_eq!(
            graphql_operation_name(br#"[{"operationName":"A"},{"operationName":"B"}]"#).as_deref(),
            Some("A+B")
        );
        assert_eq!(graphql_operation_name(b"not json"), None);
        assert_eq!(
            graphql_operation_name(br#"{"query":"{ me { id } }"}"#),
            None
        );
    }

    #[test]
    fn host_header_drops_default_ports() {
        assert_eq!(
            host_header(&"http://localhost:3000".parse().unwrap()).unwrap(),
            "localhost:3000"
        );
        assert_eq!(
            host_header(&"https://api.example.com".parse().unwrap()).unwrap(),
            "api.example.com"
        );
        assert_eq!(
            host_header(&"https://cdn.example.com:443".parse().unwrap()).unwrap(),
            "cdn.example.com"
        );
    }

    #[test]
    fn stripped_forward_path_keeps_the_query() {
        let uri: Uri = "http://localhost:8000/docs/guide?x=1".parse().unwrap();
        let pq = forward_path_and_query(
            &uri,
            Matched::Forward {
                to: "docs",
                strip: 5,
                keep_prefix_on_upgrade: false,
            },
        )
        .unwrap();
        assert_eq!(pq.as_str(), "/guide?x=1");
        let pq = forward_path_and_query(
            &uri,
            Matched::Forward {
                to: "docs",
                strip: 0,
                keep_prefix_on_upgrade: false,
            },
        )
        .unwrap();
        assert_eq!(pq.as_str(), "/docs/guide?x=1");
    }

    #[test]
    fn join_uri_keeps_the_query() {
        let pq: PathAndQuery = "/graphql?op=1".parse().unwrap();
        assert_eq!(
            join_uri(&"https://api.example.com".parse().unwrap(), Some(&pq))
                .unwrap()
                .to_string(),
            "https://api.example.com/graphql?op=1"
        );
    }
}
