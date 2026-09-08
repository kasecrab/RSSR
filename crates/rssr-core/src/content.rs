/// Feed bodies are stored as the publisher sent them and converted on the way
/// out, so a better converter later does not mean refetching anything.
pub fn to_markdown(html: &str) -> String {
    match htmd::convert(html) {
        Ok(markdown) => markdown.trim().to_string(),
        Err(_) => strip_tags(html),
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut depth = 0usize;
    for ch in html.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    unescape(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Feed summaries arrive escaped, often doubly. Leaving `&nbsp;` and `&amp;`
/// in a snippet wastes the reader's characters and reads as a bug.
pub fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        // `find` returns a char boundary; a byte-sliced window would not, and
        // any multi-byte character inside it would panic.
        let Some(end) = rest.find(';').filter(|&at| at <= 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let replacement = match entity {
            "nbsp" => Some(' '),
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "hellip" => Some('…'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "rsquo" => Some('\u{2019}'),
            "lsquo" => Some('\u{2018}'),
            "ldquo" => Some('\u{201c}'),
            "rdquo" => Some('\u{201d}'),
            other => other
                .strip_prefix('#')
                .and_then(|digits| match digits.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => digits.parse().ok(),
                })
                .and_then(char::from_u32),
        };
        match replacement {
            Some(ch) => {
                out.push(ch);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A short plain-text opening, for list output that should not need a second
/// call just to decide whether an item is worth reading.
pub fn preview(html: &str, width: usize) -> String {
    let text = strip_tags(html);
    match text.char_indices().nth(width) {
        Some((cut, _)) => format!("{}…", text[..cut].trim_end()),
        None => text,
    }
}

/// Rough token count, used to let a caller budget before pulling article text.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_markdown() {
        let markdown = to_markdown("<h1>Title</h1><p>Some <b>bold</b> text.</p>");
        assert!(markdown.contains("# Title"));
        assert!(markdown.contains("**bold**"));
    }

    #[test]
    fn links_survive_the_conversion() {
        let markdown = to_markdown(r#"<p>See <a href="https://example.com">this</a>.</p>"#);
        assert!(markdown.contains("[this](https://example.com)"));
    }

    #[test]
    fn the_fallback_drops_markup_without_losing_words() {
        assert_eq!(strip_tags("<p>one <b>two</b>  three</p>"), "one two three");
    }

    #[test]
    fn entities_do_not_survive_into_a_snippet() {
        assert_eq!(
            preview("<p>GPT-6&nbsp;Astra&nbsp;&mdash; Tom &amp; Jerry</p>", 100),
            "GPT-6 Astra — Tom & Jerry"
        );
    }

    #[test]
    fn numeric_entities_decode_and_unknown_ones_are_left_alone() {
        assert_eq!(unescape("caf&#233; &#x41;"), "café A");
        assert_eq!(unescape("Q&A and &notreal; stay"), "Q&A and &notreal; stay");
    }

    #[test]
    fn a_bare_ampersand_next_to_wide_characters_does_not_panic() {
        assert_eq!(unescape("a & │──────│ b"), "a & │──────│ b");
        assert_eq!(unescape("┌───┐ &amp; └───┘"), "┌───┐ & └───┘");
        assert_eq!(unescape("&"), "&");
        assert_eq!(unescape("│&nbsp;│"), "│ │");
    }

    #[test]
    fn token_estimates_round_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
