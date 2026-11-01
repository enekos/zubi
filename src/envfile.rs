use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub fn parse(text: &str) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        {
            continue;
        }
        vars.push((key.to_owned(), unquote(value.trim()).to_owned()));
    }
    vars
}

fn unquote(value: &str) -> &str {
    let quoted = |q: char| value.len() >= 2 && value.starts_with(q) && value.ends_with(q);
    if quoted('"') || quoted('\'') {
        value.get(1..value.len() - 1).unwrap_or(value)
    } else {
        value
    }
}

pub fn read(path: &Path) -> Result<Vec<(String, String)>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse(&text))
}

pub fn first_existing(dir: &Path, candidates: &[String]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_env_cmd_style_lines() {
        let text = "# comment\nPORT=3000\nexport BASE_URL = \"http://localhost:8000\"\nEMPTY=\nQUOTED='a=b'\nBROKEN LINE\nWITH_HASH=abc # not a comment\n";
        let vars = parse(text);
        assert_eq!(
            vars,
            vec![
                ("PORT".into(), "3000".into()),
                ("BASE_URL".into(), "http://localhost:8000".into()),
                ("EMPTY".into(), String::new()),
                ("QUOTED".into(), "a=b".into()),
                ("WITH_HASH".into(), "abc # not a comment".into()),
            ]
        );
    }

    #[test]
    fn does_not_expand_dollar_signs() {
        let vars = parse("SECRET=ab$cd${X}\n");
        assert_eq!(vars, vec![("SECRET".into(), "ab$cd${X}".into())]);
    }
}
