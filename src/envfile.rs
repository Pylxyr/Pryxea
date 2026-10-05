//! A small `.env` reader (KEY=VALUE, `#` comments, optional quotes). It
//! replaces a dotenv dependency; real environment variables still take
//! precedence over the file, as they did in the Python bot.

pub fn parse(text: &str) -> Vec<(String, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() || key.contains(char::is_whitespace) {
            continue;
        }
        out.push((key.to_string(), unquote(value.trim())));
    }
    out
}

fn unquote(value: &str) -> String {
    for quote in ['"', '\''] {
        if let Some(rest) = value.strip_prefix(quote) {
            return match rest.find(quote) {
                Some(end) => rest[..end].to_string(),
                None => rest.to_string(),
            };
        }
    }
    // Unquoted: a ` #` starts a trailing comment.
    let cut = value
        .char_indices()
        .find(|&(i, c)| c == '#' && value[..i].ends_with(char::is_whitespace))
        .map(|(i, _)| i)
        .unwrap_or(value.len());
    value[..cut].trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(v: &'a [(String, String)], k: &str) -> Option<&'a str> {
        v.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
    }

    #[test]
    fn parses_plain_quoted_and_commented_values() {
        let v = parse(
            "\u{feff}# header\nA=1\n  B = two words  \nC=\"quoted # not a comment\"\nD='single'\nE=val # trailing\nexport F=exp\nG=\nnot a line\n=nokey\n",
        );
        assert_eq!(get(&v, "A"), Some("1"));
        assert_eq!(get(&v, "B"), Some("two words"));
        assert_eq!(get(&v, "C"), Some("quoted # not a comment"));
        assert_eq!(get(&v, "D"), Some("single"));
        assert_eq!(get(&v, "E"), Some("val"));
        assert_eq!(get(&v, "F"), Some("exp"));
        assert_eq!(get(&v, "G"), Some(""));
        assert_eq!(v.len(), 7);
    }

    #[test]
    fn hash_without_leading_space_is_kept() {
        let v = parse("TOKEN=abc#def\n");
        assert_eq!(get(&v, "TOKEN"), Some("abc#def"));
    }
}
