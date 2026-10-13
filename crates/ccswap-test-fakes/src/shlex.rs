//! Python's `shlex.split`: POSIX quoting, no comments.

pub fn split(s: &str) -> Result<Vec<String>, &'static str> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("No closing quotation"),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("No closing quotation"),
                        },
                        Some(c) => word.push(c),
                        None => return Err("No closing quotation"),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next().ok_or("No escaped character")?);
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn splits_like_python_shlex() {
        assert_eq!(
            split(r#"add -a "me" -s 'Claude Code' x\ y a"b c"d '' "q\"\\\n""#).unwrap(),
            [
                "add",
                "-a",
                "me",
                "-s",
                "Claude Code",
                "x y",
                "ab cd",
                "",
                "q\"\\\\n"
            ]
        );
        assert_eq!(split("  \n").unwrap(), Vec::<String>::new());
        assert!(split("'open").is_err());
        assert!(split("tail\\").is_err());
    }
}
