use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::preset;

pub const FILE_NAME: &str = "zubi.toml";
pub const PROXY_NAME: &str = "proxy";
pub const DEFAULT_PORT_ENV: &str = "PORT";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub proxy: ProxySection,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceSection>,
    #[serde(default)]
    pub upstreams: BTreeMap<String, UpstreamSection>,
    #[serde(default)]
    pub routes: Vec<RouteSection>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxySection {
    #[serde(default = "default_proxy_port")]
    pub port: u16,
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "yes")]
    pub autostart: bool,
    #[serde(default)]
    pub route_by_referer: Vec<String>,
}

impl Default for ProxySection {
    fn default() -> Self {
        ProxySection {
            port: default_proxy_port(),
            bind: default_bind(),
            autostart: true,
            route_by_referer: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSection {
    #[serde(default = "default_dir")]
    pub dir: String,
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default = "default_env_files")]
    pub env_file: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "yes")]
    pub autostart: bool,
    #[serde(default)]
    pub ready: Option<ReadySpec>,
    #[serde(default)]
    pub warm: Vec<String>,
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub port_env: Option<String>,
}

impl Default for ServiceSection {
    fn default() -> Self {
        ServiceSection {
            dir: default_dir(),
            cmd: None,
            preset: None,
            port: None,
            env_file: default_env_files(),
            env: BTreeMap::new(),
            autostart: true,
            ready: None,
            warm: Vec::new(),
            before: None,
            port_env: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ReadySpec {
    Named(String),
    Log { log: String },
    Http { http: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamSection {
    pub url: String,
    #[serde(default)]
    pub rewrite_cookies: bool,
    #[serde(default)]
    pub rewrite_redirects: bool,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSection {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub exact: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub regex: Option<String>,
    #[serde(default)]
    pub strip: bool,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub status: Option<u16>,
}

fn yes() -> bool {
    true
}

fn default_proxy_port() -> u16 {
    8000
}

fn default_bind() -> String {
    "127.0.0.1".to_owned()
}

fn default_dir() -> String {
    ".".to_owned()
}

fn default_env_files() -> Vec<String> {
    vec![".env.local".to_owned(), ".env".to_owned()]
}

fn default_timeout() -> u64 {
    120
}

#[derive(Debug, Clone)]
pub struct Loaded {
    pub root: PathBuf,
    pub file: Option<PathBuf>,
    pub config: Config,
}

impl Loaded {
    pub fn discover(explicit: Option<&Path>) -> Result<Self> {
        let file = match explicit {
            Some(path) => path
                .canonicalize()
                .with_context(|| format!("resolving {}", path.display()))?,
            None => {
                let cwd = std::env::current_dir().context("reading the current directory")?;
                find_upwards(&cwd).ok_or_else(|| {
                    anyhow::anyhow!(
                        "no {FILE_NAME} found in {} or any parent; run `zubi init` to write one",
                        cwd.display()
                    )
                })?
            }
        };
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let config = parse(&text).with_context(|| format!("parsing {}", file.display()))?;
        let root = file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(Loaded {
            root,
            file: Some(file),
            config,
        })
    }

    pub fn in_memory(root: PathBuf, config: Config) -> Result<Self> {
        validate(&config)?;
        Ok(Loaded {
            root,
            file: None,
            config,
        })
    }
}

fn find_upwards(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

pub fn parse(text: &str) -> Result<Config> {
    let config: Config = toml::from_str(text)?;
    validate(&config)?;
    Ok(config)
}

pub fn validate(config: &Config) -> Result<()> {
    if config.services.contains_key(PROXY_NAME) || config.upstreams.contains_key(PROXY_NAME) {
        bail!("{PROXY_NAME:?} is reserved for the proxy itself");
    }
    for name in config.services.keys() {
        if config.upstreams.contains_key(name) {
            bail!("{name:?} is both a service and an upstream");
        }
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            bail!("service name {name:?} must be [A-Za-z0-9_.-]");
        }
    }
    let mut ports: BTreeMap<u16, &str> = BTreeMap::new();
    if config.proxy.autostart {
        ports.insert(config.proxy.port, PROXY_NAME);
    }
    for (name, service) in &config.services {
        match (&service.cmd, &service.preset) {
            (None, None) => bail!("service {name:?} needs a cmd or a preset"),
            (_, Some(p)) if preset::find(p).is_none() => bail!(
                "service {name:?}: unknown preset {p:?}; presets are {}",
                preset::names().join(", ")
            ),
            _ => {}
        }
        if let Some(port) = service.port {
            if port == 0 {
                bail!("service {name:?}: port 0 is not allowed");
            }
            if let Some(other) = ports.insert(port, name) {
                bail!("services {other:?} and {name:?} both use port {port}");
            }
        }
        if let Some(ReadySpec::Named(word)) = &service.ready
            && word != "port"
        {
            bail!(
                "service {name:?}: ready must be \"port\", {{ log = \"...\" }} or {{ http = \"/path\" }}, not {word:?}"
            );
        }
        if let Some(ReadySpec::Log { log }) = &service.ready {
            regex::Regex::new(log).with_context(|| format!("service {name:?}: ready.log"))?;
        }
        if let Some(ReadySpec::Http { http }) = &service.ready
            && !http.starts_with('/')
        {
            bail!("service {name:?}: ready.http must be a path starting with /");
        }
        for path in &service.warm {
            if !path.starts_with('/') {
                bail!("service {name:?}: warm path {path:?} must start with /");
            }
        }
        if !service.warm.is_empty() && service.port.is_none() {
            bail!("service {name:?}: warm needs a port");
        }
        if matches!(service.ready, Some(ReadySpec::Http { .. })) && service.port.is_none() {
            bail!("service {name:?}: ready.http needs a port");
        }
    }
    for (name, upstream) in &config.upstreams {
        let uri: http::Uri = upstream
            .url
            .parse()
            .with_context(|| format!("upstream {name:?}: url {:?}", upstream.url))?;
        if uri.scheme().is_none() || uri.host().is_none() {
            bail!(
                "upstream {name:?}: url {:?} needs a scheme and a host",
                upstream.url
            );
        }
    }
    for (index, route) in config.routes.iter().enumerate() {
        let matchers = [
            route.exact.is_some(),
            route.prefix.is_some(),
            route.regex.is_some(),
        ]
        .into_iter()
        .filter(|set| *set)
        .count();
        if matchers > 1 {
            bail!("route {index}: use only one of exact, prefix, regex");
        }
        if let Some(pattern) = &route.regex {
            regex::Regex::new(pattern).with_context(|| format!("route {index}: regex"))?;
        }
        for path in [&route.exact, &route.prefix].into_iter().flatten() {
            if !path.starts_with('/') {
                bail!("route {index}: {path:?} must start with /");
            }
        }
        if route.strip && route.exact.is_none() && route.prefix.is_none() {
            bail!("route {index}: strip needs an exact or prefix matcher");
        }
        match (&route.to, route.status) {
            (Some(_), Some(_)) | (None, None) => {
                bail!(
                    "route {index}: set exactly one of to (a service or upstream) and status (a fixed answer)"
                )
            }
            (Some(to), None) => {
                let known = config.upstreams.contains_key(to)
                    || config.services.get(to).is_some_and(|s| s.port.is_some());
                if !known {
                    bail!("route {index}: {to:?} is not an upstream or a service with a port");
                }
            }
            (None, Some(status)) => {
                if !(100..=599).contains(&status) {
                    bail!("route {index}: status {status} is not an HTTP status code");
                }
                if route.strip {
                    bail!("route {index}: strip does nothing on a status route");
                }
            }
        }
    }
    for prefix in &config.proxy.route_by_referer {
        if !prefix.starts_with('/') {
            bail!("proxy.route_by_referer entry {prefix:?} must start with /");
        }
    }
    Ok(())
}

pub const EXAMPLE: &str = r#"# zubi — one front door for your dev servers. Docs: https://github.com/enekos/zubi

[proxy]
port = 8000                 # everything is served from http://localhost:8000
# bind = "0.0.0.0"          # to reach it from a phone on the same network
# route_by_referer = ["/__nextjs"]   # dev-overlay paths shared by several apps of one framework

[services.web]
dir = "."                   # relative to this file
preset = "next"             # or cmd = "next dev"; presets: next, vite, modern, serve, rails
port = 3000
# env_file = [".env.local", ".env"]   # first one that exists is loaded (env-cmd semantics)
# env = { FOO = "bar" }
# ready = { log = "Ready in" }        # default: the port is open; or { http = "/healthz" }
# warm = ["/"]                        # request these as soon as the service is ready
# before = "pnpm build:deps"          # run to completion in dir before starting
# autostart = false                   # start only with --with web

# [upstreams.api]
# url = "https://api.staging.example.com"
# rewrite_cookies = true    # drop Domain=, strip Secure on http
# rewrite_redirects = true  # Location: https://api.staging.example.com/x -> http://localhost:8000/x
# timeout_secs = 0          # 0 = never time out waiting for headers (SSE, long polls)

# Routes are tried in order; the first match wins. A route without a matcher matches everything.
# [[routes]]
# prefix = "/api/"
# to = "api"
# [[routes]]
# prefix = "/docs"
# to = "docs"
# strip = true             # forward /docs/x as /x
# [[routes]]
# regex = "^/locales/.*/en\\.json$"
# status = 404             # answer without forwarding
[[routes]]
to = "web"
"#;

#[cfg(test)]
mod tests {
    #![expect(
        clippy::indexing_slicing,
        reason = "a test may panic on a missing index; that is the failure it reports"
    )]
    use super::*;

    #[test]
    fn example_parses() {
        let config = parse(EXAMPLE).unwrap();
        assert_eq!(config.proxy.port, 8000);
        assert_eq!(config.services["web"].port, Some(3000));
        assert_eq!(config.routes.len(), 1);
    }

    #[test]
    fn rejects_unknown_fields_and_bad_routes() {
        assert!(parse("[proxy]\nprot = 1\n").is_err());
        let err = parse("[services.a]\nport = 1\n").unwrap_err().to_string();
        assert!(err.contains("cmd or a preset"), "{err}");
        let err = parse("[services.a]\ncmd = \"x\"\nport = 1\n[[routes]]\nto = \"b\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not an upstream"), "{err}");
        let err = parse("[[routes]]\nprefix = \"/x\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("exactly one of"), "{err}");
        assert!(parse("[[routes]]\nprefix = \"/x\"\nstatus = 404\n").is_ok());
        assert!(parse("[[routes]]\nprefix = \"/x\"\nstatus = 42\n").is_err());
        let err = parse(
            "[services.a]\ncmd = \"x\"\nport = 1\n[[routes]]\nprefix = \"/a\"\nexact = \"/a\"\nto = \"a\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("only one of"), "{err}");
        let err =
            parse("[services.a]\ncmd = \"x\"\nport = 1\n[services.b]\ncmd = \"y\"\nport = 1\n")
                .unwrap_err()
                .to_string();
        assert!(err.contains("both use port 1"), "{err}");
        let err = parse("[services.proxy]\ncmd = \"x\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("reserved"), "{err}");
    }

    #[test]
    fn ready_forms() {
        let config = parse(
            "[services.a]\ncmd = \"x\"\nport = 1\nready = \"port\"\n[services.b]\ncmd = \"x\"\nport = 2\nready = { log = \"Ready in\" }\n[services.c]\ncmd = \"x\"\nport = 3\nready = { http = \"/healthz\" }\n",
        )
        .unwrap();
        assert_eq!(
            config.services["a"].ready,
            Some(ReadySpec::Named("port".into()))
        );
        assert_eq!(
            config.services["b"].ready,
            Some(ReadySpec::Log {
                log: "Ready in".into()
            })
        );
        assert!(parse("[services.a]\ncmd = \"x\"\nready = \"soon\"\n").is_err());
        assert!(parse("[services.a]\ncmd = \"x\"\nready = { http = \"/x\" }\n").is_err());
    }
}
