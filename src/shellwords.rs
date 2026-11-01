#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SplitError {
    #[error("unterminated quote in {0:?}")]
    UnterminatedQuote(String),
    #[error("command is empty")]
    Empty,
}

pub fn split(line: &str) -> Result<Vec<String>, SplitError> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => match chars.next() {
                Some(next @ ('"' | '\\' | '$' | '`')) => current.push(next),
                Some(next) => {
                    current.push('\\');
                    current.push(next);
                }
                None => current.push('\\'),
            },
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(ch);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                    in_word = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            (None, c) => {
                current.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return Err(SplitError::UnterminatedQuote(line.to_owned()));
    }
    if in_word {
        words.push(current);
    }
    if words.is_empty() {
        return Err(SplitError::Empty);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_a_shell() {
        assert_eq!(
            split("next dev -p 3000").unwrap(),
            ["next", "dev", "-p", "3000"]
        );
        assert_eq!(
            split(r#"sh -c "echo 'a b'" c\ d"#).unwrap(),
            ["sh", "-c", "echo 'a b'", "c d"]
        );
        assert_eq!(split("  spaced   out  ").unwrap(), ["spaced", "out"]);
        assert_eq!(split(r#"x "" y"#).unwrap(), ["x", "", "y"]);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(split("").unwrap_err(), SplitError::Empty);
        assert!(matches!(
            split("a 'b"),
            Err(SplitError::UnterminatedQuote(_))
        ));
    }
}
