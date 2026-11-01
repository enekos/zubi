use anyhow::{Context, Result};
use regex::Regex;

use crate::config::RouteSection;

#[derive(Debug, Clone)]
enum Matcher {
    Any,
    Exact(String),
    Prefix(String),
    Regex(Regex),
}

impl Matcher {
    fn hits(&self, path: &str) -> bool {
        match self {
            Matcher::Any => true,
            Matcher::Exact(p) => path == p,
            Matcher::Prefix(p) => path.starts_with(p),
            Matcher::Regex(re) => re.is_match(path),
        }
    }

    fn describe(&self) -> String {
        match self {
            Matcher::Any => "*".to_owned(),
            Matcher::Exact(p) => format!("= {p}"),
            Matcher::Prefix(p) => format!("{p}*"),
            Matcher::Regex(re) => format!("~ {}", re.as_str()),
        }
    }
}

#[derive(Debug, Clone)]
enum Answer {
    Forward { to: String, strip: bool },
    Status(u16),
}

#[derive(Debug, Clone)]
struct Rule {
    host: Option<String>,
    matcher: Matcher,
    answer: Answer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matched<'a> {
    Forward { to: &'a str, strip: usize },
    Status(u16),
}

impl Matched<'_> {
    pub fn strip(&self) -> usize {
        match self {
            Matched::Forward { strip, .. } => *strip,
            Matched::Status(_) => 0,
        }
    }

    pub fn forward_path<'p>(&self, path: &'p str) -> std::borrow::Cow<'p, str> {
        let strip = self.strip();
        if strip == 0 {
            return std::borrow::Cow::Borrowed(path);
        }
        match path.get(strip..) {
            Some(rest) if rest.starts_with('/') => std::borrow::Cow::Borrowed(rest),
            Some(rest) => std::borrow::Cow::Owned(format!("/{rest}")),
            None => std::borrow::Cow::Borrowed("/"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Router {
    rules: Vec<Rule>,
    referer_prefixes: Vec<String>,
}

impl Router {
    pub fn build(routes: &[RouteSection], referer_prefixes: &[String]) -> Result<Self> {
        let mut rules = Vec::with_capacity(routes.len());
        for (index, route) in routes.iter().enumerate() {
            let matcher = if let Some(p) = &route.exact {
                Matcher::Exact(p.clone())
            } else if let Some(p) = &route.prefix {
                Matcher::Prefix(p.clone())
            } else if let Some(p) = &route.regex {
                Matcher::Regex(Regex::new(p).with_context(|| format!("route {index}: regex"))?)
            } else {
                Matcher::Any
            };
            let answer = match (&route.to, route.status) {
                (Some(to), _) => Answer::Forward {
                    to: to.clone(),
                    strip: route.strip,
                },
                (None, Some(status)) => Answer::Status(status),
                (None, None) => anyhow::bail!("route {index}: needs to or status"),
            };
            rules.push(Rule {
                host: route.host.as_ref().map(|h| h.to_ascii_lowercase()),
                matcher,
                answer,
            });
        }
        Ok(Router {
            rules,
            referer_prefixes: referer_prefixes.to_vec(),
        })
    }

    pub fn resolve(&self, host: Option<&str>, path: &str) -> Option<Matched<'_>> {
        let host = host.map(host_only);
        self.rules
            .iter()
            .find(|rule| {
                rule.host
                    .as_deref()
                    .is_none_or(|h| host.is_some_and(|given| given.eq_ignore_ascii_case(h)))
                    && rule.matcher.hits(path)
            })
            .map(|rule| match &rule.answer {
                Answer::Forward { to, strip } => Matched::Forward {
                    to: to.as_str(),
                    strip: match (&rule.matcher, strip) {
                        (Matcher::Exact(p) | Matcher::Prefix(p), true) => p.len(),
                        _ => 0,
                    },
                },
                Answer::Status(status) => Matched::Status(*status),
            })
    }

    pub fn target_of(&self, host: Option<&str>, path: &str) -> Option<&str> {
        match self.resolve(host, path)? {
            Matched::Forward { to, .. } => Some(to),
            Matched::Status(_) => None,
        }
    }

    pub fn routes_by_referer(&self, path: &str) -> bool {
        self.referer_prefixes.iter().any(|p| path.starts_with(p))
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn describe(&self) -> Vec<(String, String)> {
        self.rules
            .iter()
            .map(|rule| {
                let mut m = rule.matcher.describe();
                if let Some(h) = &rule.host {
                    m = format!("{h} {m}");
                }
                let answer = match &rule.answer {
                    Answer::Forward { to, strip: true } => format!("{to} (strip)"),
                    Answer::Forward { to, strip: false } => to.clone(),
                    Answer::Status(status) => format!("answer {status}"),
                };
                (m, answer)
            })
            .collect()
    }
}

fn host_only(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        rest.split_once(']').map_or(host, |(h, _)| h)
    } else {
        host.rsplit_once(':')
            .filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit()))
            .map_or(host, |(h, _)| h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(to: &str) -> RouteSection {
        RouteSection {
            to: Some(to.to_owned()),
            ..RouteSection::default()
        }
    }

    fn router() -> Router {
        Router::build(
            &[
                RouteSection {
                    prefix: Some("/api/".into()),
                    ..route("api")
                },
                RouteSection {
                    regex: Some(r"^/(?:[a-z]{2}/)?auth(?:/|$)".into()),
                    ..route("auth")
                },
                RouteSection {
                    exact: Some("/".into()),
                    ..route("marketing")
                },
                RouteSection {
                    host: Some("Admin.localhost".into()),
                    ..route("admin")
                },
                route("web"),
            ],
            &["/__nextjs".to_owned()],
        )
        .unwrap()
    }

    #[test]
    fn first_match_wins_in_order() {
        let r = router();
        assert_eq!(r.target_of(None, "/api/users"), Some("api"));
        assert_eq!(r.target_of(None, "/auth/login"), Some("auth"));
        assert_eq!(r.target_of(None, "/de/auth/login"), Some("auth"));
        assert_eq!(r.target_of(None, "/author"), Some("web"));
        assert_eq!(r.target_of(None, "/"), Some("marketing"));
        assert_eq!(r.target_of(None, "/dashboard"), Some("web"));
    }

    #[test]
    fn host_rules_match_without_port_and_case() {
        let r = router();
        assert_eq!(
            r.target_of(Some("admin.localhost:8000"), "/x"),
            Some("admin")
        );
        assert_eq!(r.target_of(Some("ADMIN.localhost"), "/x"), Some("admin"));
        assert_eq!(r.target_of(Some("localhost:8000"), "/x"), Some("web"));
        assert_eq!(r.target_of(Some("[::1]:8000"), "/api/"), Some("api"));
    }

    #[test]
    fn strip_drops_the_matched_prefix() {
        let r = Router::build(
            &[
                RouteSection {
                    prefix: Some("/docs".into()),
                    strip: true,
                    ..route("docs")
                },
                RouteSection {
                    exact: Some("/old".into()),
                    strip: true,
                    ..route("web")
                },
            ],
            &[],
        )
        .unwrap();
        let m = r.resolve(None, "/docs/guide").unwrap();
        assert_eq!(
            m,
            Matched::Forward {
                to: "docs",
                strip: 5
            }
        );
        assert_eq!(m.forward_path("/docs/guide"), "/guide");
        assert_eq!(r.resolve(None, "/docs").unwrap().forward_path("/docs"), "/");
        assert_eq!(
            r.resolve(None, "/docsy").unwrap().forward_path("/docsy"),
            "/y"
        );
        assert_eq!(r.resolve(None, "/old").unwrap().forward_path("/old"), "/");
        assert_eq!(
            r.describe().first().map(|d| (d.0.as_str(), d.1.as_str())),
            Some(("/docs*", "docs (strip)"))
        );
    }

    #[test]
    fn status_routes_answer_without_a_target() {
        let r = Router::build(
            &[
                RouteSection {
                    regex: Some(r"/en\.json$".into()),
                    status: Some(404),
                    ..RouteSection::default()
                },
                route("web"),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(
            r.resolve(None, "/locales/app/en.json"),
            Some(Matched::Status(404))
        );
        assert_eq!(r.target_of(None, "/locales/app/en.json"), None);
        assert_eq!(r.target_of(None, "/locales/app/de.json"), Some("web"));
        assert_eq!(
            r.describe().first().map(|d| d.1.as_str()),
            Some("answer 404")
        );
    }

    #[test]
    fn no_rules_means_no_target() {
        let r = Router::build(&[], &[]).unwrap();
        assert!(r.is_empty());
        assert_eq!(r.target_of(None, "/"), None);
        assert!(router().routes_by_referer("/__nextjs/font"));
        assert!(!router().routes_by_referer("/next"));
    }
}
