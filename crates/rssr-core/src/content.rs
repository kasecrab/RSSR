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
    out.split_whitespace().collect::<Vec<_>>().join(" ")
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
    fn token_estimates_round_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
