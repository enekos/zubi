# zubi

*Basque: bridge.*

One static binary that starts your dev servers, knows when each one is actually
ready, and fronts all of them on a single port. It replaces the usual pile of
`concurrently` / Procfile runner / `env-cmd` / an Express or Caddy proxy with a
~5 MB executable that listens in 60 ms and adds 0.03 ms per request.

It does not touch your bundler. It removes everything around it.

```bash
cargo install --path .        # or: make install  (→ ~/.local/bin/zubi)
zubi init                     # writes zubi.toml
zubi up                       # proxy + every autostart service, logs interleaved, Ctrl-C stops all
```

## What it does that a Procfile runner or a plain proxy does not

- **Ready, not just listening.** A service is *listening* when its port opens and
  *ready* when its bundler says so (`ready = { log = "Ready in" }`), when a health
  path answers (`ready = { http = "/healthz" }`), or when the port opens
  (the default). Until then the proxy answers a 502 page that names the service,
  links to its log, and reloads every 2 s.
- **One origin.** Ordered routes (`prefix`, `exact`, `regex`, optional `host`)
  send each path to a local service or a remote upstream. Cookies stay on one
  origin, so an auth flow against a staging API works from `localhost:8000`.
- **Upstreams behave.** `rewrite_cookies` drops `Domain=` and strips `Secure` on
  http; `rewrite_redirects` points a `Location:` that targets the upstream back
  at the proxy. `timeout_secs = 0` for SSE and long polls.
- **Move a port without editing anything.** `--port web=3300` rewrites `PORT`
  and every `localhost:3000` self-reference in the service's env, and moves its
  route. Two checkouts can run side by side.
- **Warm-up.** `warm = ["/login"]` requests a page the moment the service is
  ready, so the first real visit skips the on-demand compile.
- **Dashboard and API.** `http://localhost:8000/__zubi` shows state, timings and a
  following log per service with a restart button. The same data is JSON under
  `/__zubi/api/status`, `/__zubi/api/logs/<service>?since=N`,
  `POST /__zubi/api/restart/<service>`; `zubi status | logs <svc> -f | restart <svc>`
  use it from another terminal.
- **Process hygiene.** Each service runs in its own process group with
  `kill_on_drop`; Ctrl-C sends SIGTERM to every group, waits 8 s, then SIGKILL.
  A crashed service restarts up to 5 times. It refuses to start over a busy
  port and names the owner. Logs go through one bounded queue; under load lines
  are dropped and counted, never blocking a dev server.
- **WebSockets** are forwarded to the routed service, so HMR works through the
  proxy.

## zubi.toml

```toml
[proxy]
port = 8000
# bind = "0.0.0.0"                    # default 127.0.0.1
# route_by_referer = ["/__nextjs"]    # dev-overlay paths shared by several apps of one framework

[services.web]
dir = "apps/web"                      # relative to this file
preset = "next"                       # next · vite · modern · serve · rails — or cmd = "..."
port = 3000
# cmd = "pnpm dev -p {port}"          # {port} is substituted; the first word is resolved in
#                                     # node_modules/.bin (walking up to the root) then PATH
# env_file = [".env.local", ".env"]   # first one that exists is loaded (env-cmd semantics)
# env = { FOO = "bar" }               # overrides the file
# port_env = "PORT"                   # which variable gets the port; "" for none
# ready = { log = "Ready in" }        # or { http = "/healthz" } or "port" (default)
# warm = ["/"]
# before = "pnpm build:deps"          # runs to completion in dir before the service starts
# autostart = false                   # start only with --with web or --only web

[upstreams.api]
url = "https://api.staging.example.com"
rewrite_cookies = true
rewrite_redirects = true
# timeout_secs = 120                  # 0 = never time out waiting for headers

# first match wins; a route with no matcher matches everything
[[routes]]
prefix = "/api/"
to = "api"
[[routes]]
regex = "^/(?:[a-z]{2}/)?auth(?:/|$)"
to = "auth"
[[routes]]
prefix = "/docs"
to = "docs"
strip = true                          # forward /docs/x as /x
[[routes]]
regex = "^/locales/.*/en\\.json$"
status = 404                          # answer without forwarding
[[routes]]
to = "web"
```

Presets fill in the command and the ready pattern: `next dev` / `Ready in`,
`vite --port {port} --strictPort` / `ready in`, `modern dev` / `built in`,
`serve -n -l {port}` / `Accepting connections`, `rails server -p {port}` /
`Listening on`. Anything you set wins over the preset.

## Commands

```bash
zubi up                         # proxy + autostart services
zubi up --with worker           # add a service to the set
zubi up --only proxy,web        # exactly these ('proxy' is a name too)
zubi up --port web=3300         # move a service and its route
zubi up --warm web=/login       # extra warm-up page; --warm none disables them all
zubi up --open                  # open the proxy once everything is ready
zubi up --no-before             # skip the before commands
zubi up --ignore-busy-ports
zubi proxy                      # only the proxy, for services started elsewhere
zubi plan                       # what up would start, and the route table
zubi status                     # state, ports, listening/ready times, pids, restarts
zubi logs web -f                # one service's log, following
zubi restart web
```

Run it from anywhere below the `zubi.toml`, or pass `-c path/to/zubi.toml`.

## Things to know

- A service that block-buffers stdout when it is not a terminal (Python does)
  never delivers its ready line; set `env = { PYTHONUNBUFFERED = "1" }` or use
  `ready = { http = "/" }`.
- A route without a matcher matches everything; put it last.
- `--port` only applies to services that declare a `port`.
- The proxy forwards `Host:` as the target's authority. Upstreams that check the
  `Origin` header still see `localhost`.

## Using it as a library

The binary is thin. `zubi::Loaded::in_memory(root, Config)` takes a config
built in code, `zubi::plan::build` turns it into a plan, and
`zubi::run::up(plan, options)` runs it. A repo-specific front door can discover
its own layout, emit a `Config`, and reuse everything else.

## Development

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all --check
```
