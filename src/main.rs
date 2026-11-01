use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use zubi::config::{self, Loaded};
use zubi::plan::{self, Overrides, Plan, Selection};
use zubi::run::{self, UpOptions};

#[derive(Parser, Debug)]
#[command(
    name = "zubi",
    version,
    about = "Front door for local dev servers: starts them, knows when they are ready, proxies them on one port"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Debug, Clone)]
struct Target {
    #[arg(
        short,
        long,
        global = true,
        help = "Path to zubi.toml; defaults to the nearest one above the current directory"
    )]
    config: Option<PathBuf>,
    #[arg(
        long,
        value_delimiter = ',',
        help = "Start exactly these services (comma-separated); 'proxy' is a name too"
    )]
    only: Vec<String>,
    #[arg(
        long = "with",
        value_delimiter = ',',
        help = "Also start these services on top of the autostart set"
    )]
    with: Vec<String>,
    #[arg(
        long = "port",
        value_parser = plan::parse_port_override,
        help = "Run a service on another port, e.g. --port web=3300 (rewrites PORT and localhost:<old> in its env, moves its route)"
    )]
    ports: Vec<(String, u16)>,
    #[arg(
        long = "warm",
        value_parser = plan::parse_warm,
        help = "Request a page as soon as its service is ready, e.g. --warm web=/login; --warm none disables the configured ones"
    )]
    warm: Vec<(String, String)>,
}

impl Target {
    fn resolve(&self) -> Result<Plan> {
        let loaded = Loaded::discover(self.config.as_deref())?;
        let selection = Selection {
            only: self.only.clone(),
            with: self.with.clone(),
        };
        let overrides = Overrides {
            ports: self.ports.clone(),
            no_warm: self.warm.iter().any(|(svc, _)| svc.is_empty()),
            warm: self
                .warm
                .iter()
                .filter(|(svc, _)| !svc.is_empty())
                .cloned()
                .collect(),
        };
        plan::build(&loaded, &selection, &overrides)
    }
}

#[derive(Args, Debug, Clone)]
struct Locate {
    #[arg(
        short,
        long,
        global = true,
        help = "Path to zubi.toml, to find the proxy port"
    )]
    config: Option<PathBuf>,
    #[arg(
        long,
        help = "Base URL of the running zubi proxy, e.g. http://localhost:8000"
    )]
    url: Option<String>,
}

impl Locate {
    fn base(&self) -> Result<String> {
        if let Some(url) = &self.url {
            return Ok(url.trim_end_matches('/').to_owned());
        }
        let loaded = Loaded::discover(self.config.as_deref())?;
        Ok(format!("http://localhost:{}", loaded.config.proxy.port))
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    #[command(
        about = "Start the proxy and the services, stream their logs, stop everything on Ctrl-C"
    )]
    Up {
        #[command(flatten)]
        target: Target,
        #[arg(long, help = "Skip each service's `before` command")]
        no_before: bool,
        #[arg(
            long,
            help = "Start even if a port is already taken by another process"
        )]
        ignore_busy_ports: bool,
        #[arg(
            long,
            help = "Open the proxy URL in the browser once every service is ready"
        )]
        open: bool,
    },
    #[command(about = "Run only the proxy, for services started some other way")]
    Proxy {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    #[command(about = "Show what `up` would start and how requests would route")]
    Plan {
        #[command(flatten)]
        target: Target,
    },
    #[command(about = "Write a starter zubi.toml in the current directory")]
    Init {
        #[arg(long, help = "Overwrite an existing zubi.toml")]
        force: bool,
    },
    #[command(about = "Show the state of the running instance")]
    Status {
        #[command(flatten)]
        locate: Locate,
    },
    #[command(about = "Print a service's recent log lines; -f follows")]
    Logs {
        service: String,
        #[arg(short = 'n', long, default_value_t = 200)]
        lines: usize,
        #[arg(short, long)]
        follow: bool,
        #[command(flatten)]
        locate: Locate,
    },
    #[command(about = "Restart one service of the running instance")]
    Restart {
        service: String,
        #[command(flatten)]
        locate: Locate,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the tokio runtime")?;
    runtime.block_on(async move {
        match cli.command {
            Command::Up {
                target,
                no_before,
                ignore_busy_ports,
                open,
            } => {
                let plan = target.resolve()?;
                run::up(
                    plan,
                    UpOptions {
                        ignore_busy_ports,
                        open,
                        skip_before: no_before,
                    },
                )
                .await
            }
            Command::Proxy { config } => {
                let target = Target {
                    config,
                    only: vec![config::PROXY_NAME.to_owned()],
                    with: Vec::new(),
                    ports: Vec::new(),
                    warm: Vec::new(),
                };
                run::proxy_only(target.resolve()?).await
            }
            Command::Plan { target } => show_plan(&target.resolve()?),
            Command::Init { force } => init(force),
            Command::Status { locate } => run::status(&locate.base()?).await,
            Command::Logs {
                service,
                lines,
                follow,
                locate,
            } => run::logs(&locate.base()?, &service, lines, follow).await,
            Command::Restart { service, locate } => run::restart(&locate.base()?, &service).await,
        }
    })
}

fn init(force: bool) -> Result<()> {
    let path = PathBuf::from(config::FILE_NAME);
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        );
    }
    std::fs::write(&path, config::EXAMPLE)
        .with_context(|| format!("writing {}", path.display()))?;
    println!(
        "wrote {}; edit the services and run `zubi up`",
        path.display()
    );
    Ok(())
}

fn show_plan(plan: &Plan) -> Result<()> {
    println!("root      {}", plan.root.display());
    match &plan.proxy {
        Some(proxy) => println!("proxy     {} (listening on {})", proxy.base, proxy.listen),
        None => println!("proxy     no"),
    }
    println!();
    println!(
        "{:<16} {:>5}  {:<8} {:<30} {:<18} env file",
        "service", "port", "kind", "command", "ready"
    );
    for launch in &plan.launches {
        println!(
            "{:<16} {:>5}  {:<8} {:<30} {:<18} {}",
            launch.name,
            launch.port.map_or("—".to_owned(), |p| p.to_string()),
            launch.kind,
            launch.command_line(),
            launch.ready.label(),
            launch.env_file.as_ref().map_or_else(
                || "-".to_owned(),
                |p| p
                    .strip_prefix(&plan.root)
                    .unwrap_or(p)
                    .display()
                    .to_string()
            )
        );
        if let Some(before) = &launch.before {
            println!("{:<16} before: {}", "", before.join(" "));
        }
        if !launch.warm.is_empty() {
            println!("{:<16} warm: {}", "", launch.warm.join(" "));
        }
    }
    if let Some(proxy) = &plan.proxy {
        println!();
        let mut targets: Vec<_> = proxy.targets.iter().collect();
        targets.sort_by(|a, b| a.0.cmp(b.0));
        for (name, target) in targets {
            println!("{name:<16} -> {}", target.uri);
        }
        println!();
        if proxy.router.is_empty() {
            println!("no routes: every request will answer 404");
        }
        for (matcher, to) in proxy.router.describe() {
            println!("{matcher:<40} -> {to}");
        }
    }
    Ok(())
}
