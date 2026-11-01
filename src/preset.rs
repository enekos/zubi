#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub cmd: &'static str,
    pub ready_log: Option<&'static str>,
}

const PRESETS: [Preset; 5] = [
    Preset {
        name: "next",
        cmd: "next dev",
        ready_log: Some(r"Ready in"),
    },
    Preset {
        name: "vite",
        cmd: "vite --port {port} --strictPort",
        ready_log: Some(r"ready in"),
    },
    Preset {
        name: "modern",
        cmd: "modern dev",
        ready_log: Some(r"built in"),
    },
    Preset {
        name: "serve",
        cmd: "serve -n -l {port}",
        ready_log: Some(r"Accepting connections"),
    },
    Preset {
        name: "rails",
        cmd: "rails server -p {port}",
        ready_log: Some(r"Listening on"),
    },
];

pub fn find(name: &str) -> Option<Preset> {
    PRESETS.iter().copied().find(|p| p.name == name)
}

pub fn names() -> Vec<&'static str> {
    PRESETS.iter().map(|p| p.name).collect()
}

pub fn expand(template: &str, port: Option<u16>) -> String {
    match port {
        Some(port) => template.replace("{port}", &port.to_string()),
        None => template.replace(" {port}", "").replace("{port}", ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_resolve_and_expand() {
        assert_eq!(find("next").map(|p| p.cmd), Some("next dev"));
        assert!(find("nope").is_none());
        assert_eq!(expand("vite --port {port}", Some(5173)), "vite --port 5173");
        assert_eq!(expand("serve -n -l {port}", None), "serve -n -l");
        assert!(names().contains(&"rails"));
    }
}
