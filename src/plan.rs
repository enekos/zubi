use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use regex::Regex;

use crate::config::{self, Config, Loaded, PROXY_NAME, ReadySpec, ServiceSection};
use crate::proxy::{ProxyConfig, Target, TargetKind};
use crate::route::Router;
use crate::{envfile, preset, shellwords};

#[derive(Debug, Clone)]
pub enum ReadyCheck {
    Port,
    Log(Regex),
    Http(String),
}

impl ReadyCheck {
    pub fn label(&self) -> String {
        match self {
            ReadyCheck::Port => "port".to_owned(),
            ReadyCheck::Log(re) => format!("log ~ {}", re.as_str()),
            ReadyCheck::Http(path) => format!("http {path}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Launch {
    pub name: String,
    pub dir: PathBuf,
    pub port: Option<u16>,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub env_file: Option<PathBuf>,
    pub ready: ReadyCheck,
    pub warm: Vec<String>,
    pub before: Option<Vec<String>>,
    pub kind: String,
}

impl Launch {
    pub fn command_line(&self) -> String {
        let mut line = self
            .program
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for arg in &self.args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }
}

#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub only: Vec<String>,
    pub with: Vec<String>,
}

impl Selection {
    fn wants(&self, name: &str, autostart: bool) -> bool {
        if !self.only.is_empty() {
            return self.only.iter().any(|n| n == name);
        }
        autostart || self.with.iter().any(|n| n == name)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub ports: Vec<(String, u16)>,
    pub warm: Vec<(String, String)>,
    pub no_warm: bool,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub root: PathBuf,
    pub proxy: Option<ProxyConfig>,
    pub launches: Vec<Launch>,
}

impl Plan {
    pub fn ports(&self) -> Vec<(String, u16)> {
        let mut ports: Vec<(String, u16)> = self
            .launches
            .iter()
            .filter_map(|l| l.port.map(|p| (l.name.clone(), p)))
            .collect();
        if let Some(proxy) = &self.proxy {
            ports.push((PROXY_NAME.to_owned(), proxy.listen.port()));
        }
        ports
    }
}

pub fn build(loaded: &Loaded, selection: &Selection, overrides: &Overrides) -> Result<Plan> {
    let config = &loaded.config;
    let known: Vec<&str> = config
        .services
        .keys()
        .map(String::as_str)
        .chain(std::iter::once(PROXY_NAME))
        .collect();
    for requested in selection
        .only
        .iter()
        .chain(&selection.with)
        .chain(overrides.ports.iter().map(|(n, _)| n))
        .chain(overrides.warm.iter().map(|(n, _)| n))
    {
        if !known.contains(&requested.as_str()) {
            bail!(
                "unknown service {requested:?}; services are: {}",
                known.join(", ")
            );
        }
    }
    let moved: HashMap<&str, u16> = overrides
        .ports
        .iter()
        .map(|(n, p)| (n.as_str(), *p))
        .collect();
    let effective_port =
        |name: &str, service: &ServiceSection| moved.get(name).copied().or(service.port);

    let wants_proxy = selection.wants(PROXY_NAME, config.proxy.autostart);
    let proxy = if wants_proxy {
        Some(proxy_config(config, &effective_port)?)
    } else {
        None
    };

    let mut launches = Vec::with_capacity(config.services.len());
    for (name, service) in &config.services {
        if !selection.wants(name, service.autostart) {
            continue;
        }
        let port = effective_port(name, service);
        if moved.contains_key(name.as_str()) && service.port.is_none() {
            bail!("{name}: --port needs a service that declares a port");
        }
        launches.push(launch_for(&loaded.root, name, service, port, overrides)?);
    }
    Ok(Plan {
        root: loaded.root.clone(),
        proxy,
        launches,
    })
}

fn proxy_config(
    config: &Config,
    effective_port: &dyn Fn(&str, &ServiceSection) -> Option<u16>,
) -> Result<ProxyConfig> {
    let bind: std::net::IpAddr = config
        .proxy
        .bind
        .parse()
        .with_context(|| format!("proxy.bind {:?} is not an IP address", config.proxy.bind))?;
    let listen = std::net::SocketAddr::new(bind, config.proxy.port);
    let base: http::Uri = match &config.proxy.public_url {
        Some(url) => url.trim_end_matches('/').parse(),
        None => format!("http://localhost:{}", config.proxy.port).parse(),
    }
    .context("proxy base url")?;
    let mut targets = HashMap::with_capacity(config.services.len() + config.upstreams.len());
    for (name, service) in &config.services {
        let Some(port) = effective_port(name, service) else {
            continue;
        };
        let uri: http::Uri = format!("http://127.0.0.1:{port}")
            .parse()
            .with_context(|| format!("target for {name}"))?;
        targets.insert(
            name.clone(),
            Target {
                uri,
                kind: TargetKind::Service,
                header_timeout: Some(Duration::from_secs(120)),
            },
        );
    }
    for (name, upstream) in &config.upstreams {
        let uri: http::Uri = upstream
            .url
            .parse()
            .with_context(|| format!("upstream {name}: url"))?;
        let host = uri
            .host()
            .ok_or_else(|| anyhow::anyhow!("upstream {name}: url has no host"))?
            .to_owned();
        targets.insert(
            name.clone(),
            Target {
                uri,
                kind: TargetKind::Upstream {
                    host,
                    rewrite_cookies: upstream.rewrite_cookies,
                    rewrite_redirects: upstream.rewrite_redirects,
                    headers: upstream_headers(name, &upstream.headers)?,
                },
                header_timeout: (upstream.timeout_secs > 0)
                    .then(|| Duration::from_secs(upstream.timeout_secs)),
            },
        );
    }
    let router = Router::build(&config.routes, &config.proxy.route_by_referer)?;
    Ok(ProxyConfig {
        listen,
        base,
        targets,
        router,
        cors_origins: config.proxy.cors_origins.clone(),
        strip_cookie_domains: config.proxy.strip_cookie_domains,
    })
}

fn upstream_headers(
    name: &str,
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<(http::header::HeaderName, http::header::HeaderValue)>> {
    headers
        .iter()
        .map(|(header, value)| {
            let header = http::header::HeaderName::from_bytes(header.as_bytes())
                .with_context(|| format!("upstream {name}: header name {header:?}"))?;
            let mut value = http::header::HeaderValue::from_str(value)
                .with_context(|| format!("upstream {name}: value of header {header}"))?;
            value.set_sensitive(true);
            Ok((header, value))
        })
        .collect()
}

fn launch_for(
    root: &Path,
    name: &str,
    service: &ServiceSection,
    port: Option<u16>,
    overrides: &Overrides,
) -> Result<Launch> {
    let dir = root.join(&service.dir);
    if !dir.is_dir() {
        bail!("{name}: directory {} does not exist", dir.display());
    }
    let preset = service.preset.as_deref().and_then(preset::find);
    let template = match (&service.cmd, preset) {
        (Some(cmd), _) => cmd.clone(),
        (None, Some(p)) => p.cmd.to_owned(),
        (None, None) => bail!("{name}: needs a cmd or a preset"),
    };
    let words = shellwords::split(&preset::expand(&template, port))
        .with_context(|| format!("{name}: cmd"))?;
    let mut words = words.into_iter();
    let program = resolve_program(root, &dir, &words.next().unwrap_or_default());
    let args: Vec<String> = words.collect();

    let env_file = envfile::first_existing(&dir, &service.env_file);
    let mut env = match &env_file {
        Some(path) => envfile::read(path)?,
        None => Vec::new(),
    };
    if let (Some(declared), Some(actual)) = (service.port, port)
        && declared != actual
    {
        move_port(&mut env, declared, actual);
    }
    for (key, value) in &service.env {
        set_env(&mut env, key, value.clone());
    }
    let port_env = service
        .port_env
        .clone()
        .unwrap_or_else(|| config::DEFAULT_PORT_ENV.to_owned());
    if let Some(port) = port
        && !port_env.is_empty()
    {
        set_env(&mut env, &port_env, port.to_string());
    }

    let ready = match &service.ready {
        Some(ReadySpec::Log { log }) => {
            ReadyCheck::Log(Regex::new(log).with_context(|| format!("{name}: ready.log"))?)
        }
        Some(ReadySpec::Http { http }) => ReadyCheck::Http(http.clone()),
        Some(ReadySpec::Named(_)) => ReadyCheck::Port,
        None => match preset.and_then(|p| p.ready_log) {
            Some(pattern) => ReadyCheck::Log(
                Regex::new(pattern).with_context(|| format!("{name}: preset ready pattern"))?,
            ),
            None => ReadyCheck::Port,
        },
    };

    let mut warm = if overrides.no_warm {
        Vec::new()
    } else {
        service.warm.clone()
    };
    for (svc, path) in &overrides.warm {
        if svc == name && !warm.contains(path) {
            warm.push(path.clone());
        }
    }
    if !warm.is_empty() && port.is_none() {
        bail!("{name}: warm needs a port");
    }

    let before = match &service.before {
        Some(line) => {
            let words = shellwords::split(line).with_context(|| format!("{name}: before"))?;
            let mut words = words.into_iter();
            let program = resolve_program(root, &dir, &words.next().unwrap_or_default());
            let mut resolved = vec![program.to_string_lossy().into_owned()];
            resolved.extend(words);
            Some(resolved)
        }
        None => None,
    };

    Ok(Launch {
        name: name.to_owned(),
        dir,
        port,
        program,
        args,
        env,
        env_file,
        ready,
        warm,
        before,
        kind: service.preset.clone().unwrap_or_else(|| "cmd".to_owned()),
    })
}

pub fn resolve_program(root: &Path, dir: &Path, word: &str) -> PathBuf {
    if word.contains('/') {
        return dir.join(word);
    }
    let mut current = Some(dir);
    while let Some(d) = current {
        let candidate = d.join("node_modules/.bin").join(word);
        if candidate.is_file() {
            return candidate;
        }
        if d == root {
            break;
        }
        current = d.parent();
    }
    PathBuf::from(word)
}

fn set_env(env: &mut Vec<(String, String)>, key: &str, value: String) {
    match env.iter_mut().find(|(k, _)| k == key) {
        Some((_, slot)) => *slot = value,
        None => env.push((key.to_owned(), value)),
    }
}

pub fn move_port(env: &mut [(String, String)], from: u16, to: u16) {
    let needles = [
        format!("localhost:{from}"),
        format!("127.0.0.1:{from}"),
        format!("0.0.0.0:{from}"),
    ];
    for (key, value) in env.iter_mut() {
        if key == config::DEFAULT_PORT_ENV {
            if value.trim() == from.to_string() {
                *value = to.to_string();
            }
            continue;
        }
        for needle in &needles {
            *value = replace_port_bounded(value, needle, from, to);
        }
    }
}

fn replace_port_bounded(value: &str, needle: &str, from: u16, to: u16) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find(needle) {
        let after = at + needle.len();
        let head = rest.get(..at).unwrap_or("");
        let tail = rest.get(after..).unwrap_or("");
        out.push_str(head);
        if tail.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
            out.push_str(needle);
        } else {
            out.push_str(&needle.replace(&format!(":{from}"), &format!(":{to}")));
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

pub fn parse_port_override(arg: &str) -> std::result::Result<(String, u16), String> {
    let (name, port) = arg
        .split_once('=')
        .ok_or_else(|| format!("expected service=port, got {arg:?}"))?;
    let port = port
        .trim()
        .parse::<u16>()
        .map_err(|_| format!("{port:?} is not a port number"))?;
    if port == 0 {
        return Err("port 0 is not allowed".into());
    }
    Ok((name.trim().to_owned(), port))
}

pub fn parse_warm(arg: &str) -> std::result::Result<(String, String), String> {
    if arg == "none" {
        return Ok((String::new(), String::new()));
    }
    let (service, path) = arg
        .split_once('=')
        .ok_or_else(|| format!("expected service=/path, got {arg:?}"))?;
    if !path.starts_with('/') {
        return Err(format!("{path:?} must start with /"));
    }
    Ok((service.trim().to_owned(), path.to_owned()))
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::indexing_slicing,
        reason = "a test may panic on a missing index; that is the failure it reports"
    )]
    use super::*;

    fn loaded(toml: &str) -> Loaded {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        std::fs::write(
            dir.path().join("apps/web/.env"),
            "PORT=3000\nASSET_PREFIX=http://localhost:3000\nAPI=http://localhost:8000\n",
        )
        .unwrap();
        let config = config::parse(toml).unwrap();
        let root = dir.keep();
        Loaded {
            root,
            file: None,
            config,
        }
    }

    const BASE: &str = "[services.web]\ndir = \"apps/web\"\npreset = \"next\"\nport = 3000\nwarm = [\"/login\"]\n[services.worker]\ncmd = \"node worker.js\"\nautostart = false\n[upstreams.api]\nurl = \"https://api.example.com\"\nrewrite_cookies = true\n[[routes]]\nprefix = \"/api/\"\nto = \"api\"\n[[routes]]\nto = \"web\"\n";

    #[test]
    fn plan_from_config() {
        let plan = build(&loaded(BASE), &Selection::default(), &Overrides::default()).unwrap();
        assert_eq!(plan.launches.len(), 1);
        let web = &plan.launches[0];
        assert_eq!(web.command_line(), "next dev");
        assert_eq!(web.port, Some(3000));
        assert!(matches!(web.ready, ReadyCheck::Log(_)));
        assert_eq!(web.warm, ["/login"]);
        let get = |k: &str| {
            web.env
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("PORT"), Some("3000"));
        let proxy = plan.proxy.as_ref().unwrap();
        assert_eq!(proxy.listen.port(), 8000);
        assert_eq!(
            proxy.targets["web"].uri.to_string(),
            "http://127.0.0.1:3000/"
        );
        assert_eq!(proxy.router.target_of(None, "/api/x"), Some("api"));
        let mut ports = plan.ports();
        ports.sort();
        assert_eq!(
            ports,
            [("proxy".to_owned(), 8000), ("web".to_owned(), 3000)]
        );
    }

    #[test]
    fn selection_and_port_moves() {
        let selection = Selection {
            only: vec!["web".into(), "worker".into()],
            with: Vec::new(),
        };
        let overrides = Overrides {
            ports: vec![("web".into(), 3300)],
            warm: vec![("web".into(), "/x".into())],
            no_warm: false,
        };
        let plan = build(&loaded(BASE), &selection, &overrides).unwrap();
        assert!(plan.proxy.is_none());
        assert_eq!(plan.launches.len(), 2);
        let web = &plan.launches[0];
        let get = |k: &str| {
            web.env
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("PORT"), Some("3300"));
        assert_eq!(get("ASSET_PREFIX"), Some("http://localhost:3300"));
        assert_eq!(get("API"), Some("http://localhost:8000"));
        assert_eq!(web.warm, ["/login", "/x"]);
        let worker = &plan.launches[1];
        assert_eq!(worker.port, None);
        assert!(matches!(worker.ready, ReadyCheck::Port));
        assert_eq!(worker.command_line(), "node worker.js");

        let with_proxy = build(
            &loaded(BASE),
            &Selection {
                only: vec!["proxy".into()],
                with: Vec::new(),
            },
            &Overrides {
                ports: vec![("web".into(), 3300)],
                ..Overrides::default()
            },
        )
        .unwrap();
        assert!(with_proxy.launches.is_empty());
        assert_eq!(
            with_proxy.proxy.unwrap().targets["web"].uri.port_u16(),
            Some(3300)
        );
    }

    #[test]
    fn unknown_names_are_refused() {
        let err = build(
            &loaded(BASE),
            &Selection {
                only: Vec::new(),
                with: vec!["nope".into()],
            },
            &Overrides::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown service"), "{err}");
    }

    #[test]
    fn moving_a_port_rewrites_every_self_reference() {
        let mut env = vec![
            ("PORT".to_owned(), "3000".to_owned()),
            (
                "ASSET_PREFIX".to_owned(),
                "http://localhost:3000".to_owned(),
            ),
            ("BASE_URL".to_owned(), "http://localhost:8000".to_owned()),
            ("REDIS_PORT".to_owned(), "3000".to_owned()),
            ("OTHER".to_owned(), "http://localhost:30001".to_owned()),
        ];
        move_port(&mut env, 3000, 3300);
        let values: Vec<&str> = env.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(
            values,
            [
                "3300",
                "http://localhost:3300",
                "http://localhost:8000",
                "3000",
                "http://localhost:30001"
            ]
        );
    }

    #[test]
    fn cli_value_parsers() {
        assert_eq!(
            parse_port_override("web=3300").unwrap(),
            ("web".to_owned(), 3300)
        );
        assert!(parse_port_override("web").is_err());
        assert!(parse_port_override("x=0").is_err());
        assert_eq!(
            parse_warm("web=/login").unwrap(),
            ("web".to_owned(), "/login".to_owned())
        );
        assert_eq!(parse_warm("none").unwrap().0, "");
        assert!(parse_warm("web=login").is_err());
    }
}
