//! Finding the feed behind a page.
//!
//! People know a site by its home page, not by the address of its feed, so
//! `rssr add https://example.com` has to be able to end up subscribed. A page
//! that advertises its feeds does so in the head, with
//! `<link rel="alternate" type="application/rss+xml" href="…">`, and that is
//! the only signal trusted here: guessing at `/feed` and `/rss` costs requests
//! and subscribes people to whatever happens to answer.

use std::collections::HashSet;

use crate::fetch::Fetcher;
use crate::{Result, charset, content};

/// How much of a page is read looking for its head. A `<link>` past this is
/// past anything a real document does, and the cap is what keeps a hostile or
/// merely enormous page from being scanned end to end.
const HEAD_BYTES: usize = 512 * 1024;

/// What a `<link rel="alternate">` may call a feed. Deliberately short: a
/// `text/xml` alternate is as often a sitemap as a feed, and subscribing to
/// the wrong thing is worse than asking for the real address.
const FEED_TYPES: &[&str] = &[
    "application/atom+xml",
    "application/rss+xml",
    "application/feed+json",
    "application/json",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub url: String,
    pub title: Option<String>,
    /// A feed the page offers alongside its real one — comments, most often.
    /// Kept rather than dropped, because on some sites it is the one wanted.
    pub secondary: bool,
}

/// Fetches a page and returns the feeds it advertises, best first.
pub fn from_page(fetcher: &Fetcher, url: &str) -> Result<Vec<Candidate>> {
    let page = fetcher.get_page(url)?;
    // Only the head is ever read, so only the head is ever decoded. Cutting
    // the bytes first bounds the work on a page that is megabytes long.
    let head = &page.bytes[..boundary_of(&page.bytes, HEAD_BYTES)];
    let html = charset::decode(head, page.content_type.as_deref());
    // Relative addresses resolve against where the page actually came from,
    // which after a redirect is not where it was asked for.
    let base = page.final_url.as_deref().unwrap_or(url);
    Ok(feeds_in(&html, base))
}

/// The feeds an HTML page advertises, in the order a caller should prefer
/// them: the ones that look like the site's own feed first.
pub fn feeds_in(html: &str, page_url: &str) -> Vec<Candidate> {
    let head = head_of(html);
    // Lowercased once and shared. `to_ascii_lowercase` touches only A-Z, so
    // every byte index into it is the same index into `head`.
    let lower = head.to_ascii_lowercase();

    // HTML gives the whole document one base URL, taken from the first
    // `<base href>` wherever it sits, so it is read before any link is.
    let base = tags(head, &lower, "base")
        .into_iter()
        .find_map(|tag| attribute(tag, "href"))
        .and_then(|href| resolve(page_url, &href))
        .unwrap_or_else(|| page_url.to_string());

    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for tag in tags(head, &lower, "link") {
        let attributes = attributes(tag);
        let get = |name: &str| {
            attributes
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };

        if !rel_is_alternate(get("rel").unwrap_or_default()) {
            continue;
        }
        if !is_feed_type(get("type").unwrap_or_default()) {
            continue;
        }
        let Some(url) = get("href").and_then(|href| resolve(&base, href)) else {
            continue;
        };
        // The page linking to itself is not a feed, whatever it claims.
        if url == base || url == page_url {
            continue;
        }
        if !seen.insert(url.clone()) {
            continue;
        }
        let title = get("title")
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string);
        let secondary = looks_secondary(&url, title.as_deref());
        found.push(Candidate {
            url,
            title,
            secondary,
        });
    }

    // Stable, so two feeds of the same standing stay in document order.
    found.sort_by_key(|candidate| candidate.secondary);
    found
}

/// An address typed without a scheme. Subscribing is always over http, and a
/// bare host is what someone types when they mean their browser's address bar.
pub fn absolute(url: &str) -> String {
    let url = url.trim();
    if url.contains("://") {
        url.to_string()
    } else {
        format!("https://{url}")
    }
}

/// `rel` is a space-separated list, and `rel="alternate home"` is ordinary.
fn rel_is_alternate(rel: &str) -> bool {
    rel.split_whitespace()
        .any(|token| token.eq_ignore_ascii_case("alternate"))
}

fn is_feed_type(value: &str) -> bool {
    let value = value.split(';').next().unwrap_or_default().trim();
    FEED_TYPES
        .iter()
        .any(|kind| value.eq_ignore_ascii_case(kind))
}

/// Every WordPress page advertises its comments feed beside its real one, and
/// a reader that subscribed to the comments would look broken. Nothing else is
/// guessed at: an unrecognised second feed stays a first-class choice.
fn looks_secondary(url: &str, title: Option<&str>) -> bool {
    let haystack = format!("{} {}", url, title.unwrap_or_default()).to_lowercase();
    haystack.contains("comment")
}

/// `<link>` belongs in the head. Stopping at the body keeps a long page from
/// being scanned and keeps a stray `<link>` in the content out of the result.
fn head_of(html: &str) -> &str {
    let bounded = &html[..boundary_of(html.as_bytes(), HEAD_BYTES)];
    let lower = bounded.to_ascii_lowercase();
    match lower.find("<body") {
        Some(at) => &bounded[..at],
        None => bounded,
    }
}

/// The largest cut at or below `max` that does not land inside a character.
fn boundary_of(bytes: &[u8], max: usize) -> usize {
    if bytes.len() <= max {
        return bytes.len();
    }
    let mut at = max;
    // A continuation byte is `10xxxxxx`; back up until the cut starts a
    // character, so decoding does not turn the last one into a replacement.
    while at > 0 && bytes[at] & 0b1100_0000 == 0b1000_0000 {
        at -= 1;
    }
    at
}

/// The inside of every `<name …>` in `html`, matched without regard to case.
/// `lower` must be `html` lowercased, which shares its byte indices.
fn tags<'a>(html: &'a str, lower: &str, name: &str) -> Vec<&'a str> {
    let needle = format!("<{name}");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = lower[from..].find(&needle) {
        let start = from + at + needle.len();
        // `<linked>` is not a `<link>`: the name has to end where the tag
        // says it does.
        match lower[start..].chars().next() {
            Some(ch) if ch.is_whitespace() || ch == '/' || ch == '>' => {}
            _ => {
                from = start;
                continue;
            }
        }
        let Some(end) = tag_end(&html[start..]) else {
            break;
        };
        out.push(&html[start..start + end]);
        from = start + end;
    }
    out
}

/// Where a tag closes. A quoted attribute value may hold a `>`, so this
/// tracks quoting instead of looking for the next angle bracket.
fn tag_end(rest: &str) -> Option<usize> {
    let mut quote = None;
    for (at, ch) in rest.char_indices() {
        match (quote, ch) {
            (None, '"' | '\'') => quote = Some(ch),
            (Some(open), ch) if ch == open => quote = None,
            (None, '>') => return Some(at),
            _ => {}
        }
    }
    None
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    attributes(tag)
        .into_iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
}

/// Attributes of one tag: names lowercased, values unescaped and as written.
/// Handles the three quoting styles HTML allows and a bare valueless name.
fn attributes(tag: &str) -> Vec<(String, String)> {
    let bytes: Vec<char> = tag.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;

    while at < bytes.len() {
        while at < bytes.len() && (bytes[at].is_whitespace() || bytes[at] == '/') {
            at += 1;
        }
        let start = at;
        while at < bytes.len()
            && !bytes[at].is_whitespace()
            && !matches!(bytes[at], '=' | '/' | '>')
        {
            at += 1;
        }
        if at == start {
            at += 1;
            continue;
        }
        let name: String = bytes[start..at].iter().collect::<String>().to_lowercase();

        let mut after = at;
        while after < bytes.len() && bytes[after].is_whitespace() {
            after += 1;
        }
        if after >= bytes.len() || bytes[after] != '=' {
            out.push((name, String::new()));
            continue;
        }
        at = after + 1;
        while at < bytes.len() && bytes[at].is_whitespace() {
            at += 1;
        }
        let value: String = match bytes.get(at) {
            Some(&quote @ ('"' | '\'')) => {
                at += 1;
                let start = at;
                while at < bytes.len() && bytes[at] != quote {
                    at += 1;
                }
                let value = bytes[start..at].iter().collect();
                at += 1;
                value
            }
            Some(_) => {
                let start = at;
                while at < bytes.len() && !bytes[at].is_whitespace() && bytes[at] != '>' {
                    at += 1;
                }
                bytes[start..at].iter().collect()
            }
            None => String::new(),
        };
        out.push((name, content::unescape(&value)));
    }
    out
}

/// Resolves `href` against `base`, and only ever to an http or https address.
/// A `javascript:` or `data:` href is not something to subscribe to, so it
/// comes back as nothing rather than as a URL that later fails oddly.
pub fn resolve(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    let href = href.split('#').next().unwrap_or_default();
    if href.is_empty() {
        return None;
    }

    if let Some(scheme) = scheme_of(href) {
        if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            return split(href).map(|_| href.to_string());
        }
        // `feed:` is an old way of saying "this is a feed" and wraps a real
        // address, either as `feed://host/path` or as `feed:https://host/path`.
        if scheme.eq_ignore_ascii_case("feed") {
            return resolve(base, &href[scheme.len() + 1..]);
        }
        return None;
    }

    let base = split(base)?;
    if let Some(rest) = href.strip_prefix("//") {
        return resolve(base.scheme, &format!("{}://{rest}", base.scheme))
            .or_else(|| Some(format!("{}://{rest}", base.scheme)));
    }

    // A query on its own replaces the base's, keeping the whole path.
    if href.starts_with('?') {
        return Some(format!(
            "{}://{}{}{href}",
            base.scheme, base.authority, base.path
        ));
    }

    let (path, query) = match href.find('?') {
        Some(at) => href.split_at(at),
        None => (href, ""),
    };
    let path = match path.strip_prefix('/') {
        Some(_) => normalize(path),
        None => normalize(&format!("{}{path}", directory(base.path))),
    };
    Some(format!("{}://{}{path}{query}", base.scheme, base.authority))
}

struct Split<'a> {
    scheme: &'a str,
    authority: &'a str,
    path: &'a str,
}

/// An http or https URL taken apart far enough to resolve against. Anything
/// else, including an address with no host at all, is not a base.
fn split(url: &str) -> Option<Split<'_>> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let rest = rest.split(['#', '?']).next().unwrap_or_default();
    let (authority, path) = match rest.find('/') {
        Some(at) => rest.split_at(at),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return None;
    }
    Some(Split {
        scheme,
        authority,
        path,
    })
}

/// The scheme of an absolute URL. A scheme is letters, digits and `+-.` after
/// a leading letter, so `//host:8080/x` and `/a:b` correctly have none.
fn scheme_of(url: &str) -> Option<&str> {
    let at = url.find(':')?;
    let scheme = &url[..at];
    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_alphabetic() {
        return None;
    }
    chars
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
        .then_some(scheme)
}

/// The part of a path a relative address is resolved against: everything up
/// to and including its last slash.
fn directory(path: &str) -> &str {
    match path.rfind('/') {
        Some(at) => &path[..=at],
        None => "/",
    }
}

/// Removes the `.` and `..` segments, as resolving a relative address must.
/// A `..` at the root stays at the root rather than escaping the host.
fn normalize(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut out = String::with_capacity(path.len() + 1);
    for segment in &segments {
        out.push('/');
        out.push_str(segment);
    }
    // A path ending in a slash, or in a segment that resolved away, names a
    // directory; dropping the slash would name something else.
    if segments.is_empty() || path.ends_with(['/', '.']) {
        out.push('/');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!DOCTYPE html><html><head>
        <meta charset="utf-8">
        <title>Example</title>
        <link rel="stylesheet" href="/style.css">
        <link rel="alternate" type="application/rss+xml" title="Example Feed" href="/feed.xml">
        <link rel="alternate" type="text/html" href="/print">
    </head><body>
        <link rel="alternate" type="application/rss+xml" href="/not-in-the-head.xml">
    </body></html>"#;

    fn urls(candidates: &[Candidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.url.as_str()).collect()
    }

    /// Answers each connection in turn with the next response, so a redirect
    /// and the page it lands on can both be served. The responses are built
    /// from the address, since a redirect has to name where it is going.
    fn serve(build: impl FnOnce(&str) -> Vec<String>) -> String {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let responses = build(&host);
        std::thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let _ = stream.write_all(response.as_bytes());
            }
        });
        host
    }

    fn page(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    const HEAD: &str = r#"<html><head>
        <link rel="alternate" type="application/rss+xml" title="Posts" href="feed.xml">
    </head><body></body></html>"#;

    #[test]
    fn a_fetched_page_gives_up_the_feed_it_advertises() {
        let host = serve(|_| vec![page(HEAD)]);
        let found = from_page(&Fetcher::new(), &format!("{host}/blog/")).unwrap();
        assert_eq!(urls(&found), [format!("{host}/blog/feed.xml")]);
        assert_eq!(found[0].title.as_deref(), Some("Posts"));
    }

    #[test]
    fn a_relative_address_follows_the_redirect_rather_than_the_request() {
        let host = serve(|host| {
            vec![
                format!(
                    "HTTP/1.1 301 Moved Permanently\r\nLocation: {host}/blog/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                page(HEAD),
            ]
        });
        let found = from_page(&Fetcher::new(), &host).unwrap();
        assert_eq!(urls(&found), [format!("{host}/blog/feed.xml")]);
    }

    #[test]
    fn a_page_that_is_not_html_is_read_without_complaint() {
        let host = serve(|_| vec![page("%PDF-1.4 binary-ish nonsense")]);
        assert!(from_page(&Fetcher::new(), &host).unwrap().is_empty());
    }

    #[test]
    fn a_page_that_cannot_be_fetched_is_an_error_not_an_empty_list() {
        let host = serve(|_| {
            vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()]
        });
        assert!(from_page(&Fetcher::new(), &host).is_err());
    }

    #[test]
    fn a_page_in_another_encoding_keeps_its_title_readable() {
        let body = b"<head><link rel=\"alternate\" type=\"application/rss+xml\" title=\"Caf\xe9\" href=\"/f.xml\"></head><body>";
        let decoded = charset::decode(body, Some("text/html; charset=windows-1252"));
        let found = feeds_in(&decoded, "https://example.com/");
        assert_eq!(found[0].title.as_deref(), Some("Café"));
    }

    #[test]
    fn a_page_advertising_one_feed_gives_up_its_address() {
        let found = feeds_in(PAGE, "https://example.com/blog/");
        assert_eq!(urls(&found), ["https://example.com/feed.xml"]);
        assert_eq!(found[0].title.as_deref(), Some("Example Feed"));
        assert!(!found[0].secondary);
    }

    #[test]
    fn a_stylesheet_or_an_html_alternate_is_not_a_feed() {
        let found = feeds_in(PAGE, "https://example.com/");
        assert_eq!(found.len(), 1);
        assert!(!urls(&found).contains(&"https://example.com/style.css"));
    }

    #[test]
    fn a_link_in_the_body_is_not_read() {
        let found = feeds_in(PAGE, "https://example.com/");
        assert!(!urls(&found).contains(&"https://example.com/not-in-the-head.xml"));
    }

    #[test]
    fn every_way_of_writing_the_tag_is_understood() {
        let html = r#"<head>
            <LINK REL="ALTERNATE" TYPE="APPLICATION/RSS+XML" HREF="/upper.xml">
            <link rel='alternate' type='application/atom+xml' href='/single.xml'>
            <link rel=alternate type=application/rss+xml href=/bare.xml>
            <link
                 rel="alternate home"
                 type="application/rss+xml; charset=utf-8"
                 href="/multi.xml" />
        </head>"#;
        assert_eq!(
            urls(&feeds_in(html, "https://example.com/")),
            [
                "https://example.com/upper.xml",
                "https://example.com/single.xml",
                "https://example.com/bare.xml",
                "https://example.com/multi.xml",
            ]
        );
    }

    #[test]
    fn an_escaped_query_in_an_address_is_decoded() {
        let html = r#"<head><link rel="alternate" type="application/rss+xml"
            href="/feed?format=rss&amp;cat=1"></head>"#;
        assert_eq!(
            urls(&feeds_in(html, "https://example.com/")),
            ["https://example.com/feed?format=rss&cat=1"]
        );
    }

    #[test]
    fn a_greater_than_inside_a_value_does_not_end_the_tag() {
        let html = r#"<head>
            <link rel="alternate" type="application/rss+xml" title="a > b" href="/feed.xml">
        </head>"#;
        let found = feeds_in(html, "https://example.com/");
        assert_eq!(urls(&found), ["https://example.com/feed.xml"]);
        assert_eq!(found[0].title.as_deref(), Some("a > b"));
    }

    #[test]
    fn a_tag_whose_name_merely_starts_the_same_is_skipped() {
        let html = r#"<head>
            <linkedin rel="alternate" type="application/rss+xml" href="/wrong.xml">
            <link rel="alternate" type="application/rss+xml" href="/right.xml">
        </head>"#;
        assert_eq!(
            urls(&feeds_in(html, "https://example.com/")),
            ["https://example.com/right.xml"]
        );
    }

    #[test]
    fn a_base_element_moves_what_relative_addresses_mean() {
        let html = r#"<head>
            <base href="https://cdn.example.net/site/">
            <link rel="alternate" type="application/rss+xml" href="feed.xml">
        </head>"#;
        assert_eq!(
            urls(&feeds_in(html, "https://example.com/blog/post")),
            ["https://cdn.example.net/site/feed.xml"]
        );
    }

    #[test]
    fn a_base_element_after_the_link_still_governs_it() {
        let html = r#"<head>
            <link rel="alternate" type="application/rss+xml" href="feed.xml">
            <base href="https://cdn.example.net/site/">
        </head>"#;
        assert_eq!(
            urls(&feeds_in(html, "https://example.com/blog/post")),
            ["https://cdn.example.net/site/feed.xml"]
        );
    }

    #[test]
    fn a_comments_feed_ranks_below_the_one_the_site_is_for() {
        let html = r#"<head>
            <link rel="alternate" type="application/rss+xml" title="Comments Feed" href="/comments/feed/">
            <link rel="alternate" type="application/rss+xml" title="All posts" href="/feed/">
        </head>"#;
        let found = feeds_in(html, "https://example.com/");
        assert_eq!(
            urls(&found),
            [
                "https://example.com/feed/",
                "https://example.com/comments/feed/"
            ]
        );
        assert!(!found[0].secondary);
        assert!(found[1].secondary);
    }

    #[test]
    fn the_same_feed_advertised_twice_is_offered_once() {
        let html = r#"<head>
            <link rel="alternate" type="application/rss+xml" href="/feed.xml">
            <link rel="alternate" type="application/atom+xml" href="/feed.xml">
        </head>"#;
        assert_eq!(feeds_in(html, "https://example.com/").len(), 1);
    }

    #[test]
    fn a_page_pointing_at_itself_offers_nothing() {
        let html = r#"<head>
            <link rel="alternate" type="application/rss+xml" href="https://example.com/feed">
        </head>"#;
        assert!(feeds_in(html, "https://example.com/feed").is_empty());
    }

    #[test]
    fn a_page_with_no_feed_offers_nothing_rather_than_a_guess() {
        let html = "<head><title>Nothing here</title></head><body><p>Hello.</p></body>";
        assert!(feeds_in(html, "https://example.com/").is_empty());
        assert!(feeds_in("", "https://example.com/").is_empty());
        assert!(feeds_in("<html", "https://example.com/").is_empty());
    }

    #[test]
    fn an_unclosed_tag_at_the_end_does_not_hang_or_panic() {
        let html = r#"<head><link rel="alternate" type="application/rss+xml" href="/a.xml"#;
        assert!(feeds_in(html, "https://example.com/").is_empty());
    }

    #[test]
    fn a_json_feed_counts_and_text_xml_does_not() {
        let html = r#"<head>
            <link rel="alternate" type="application/feed+json" href="/feed.json">
            <link rel="alternate" type="text/xml" href="/sitemap.xml">
        </head>"#;
        assert_eq!(
            urls(&feeds_in(html, "https://example.com/")),
            ["https://example.com/feed.json"]
        );
    }

    #[test]
    fn a_feed_scheme_address_is_unwrapped_into_a_real_one() {
        let html = r#"<head>
            <link rel="alternate" type="application/rss+xml" href="feed://example.com/rss">
            <link rel="alternate" type="application/rss+xml" href="feed:https://other.com/rss">
        </head>"#;
        assert_eq!(
            urls(&feeds_in(html, "https://example.com/")),
            ["https://example.com/rss", "https://other.com/rss"]
        );
    }

    #[test]
    fn nothing_but_an_http_address_is_ever_returned() {
        for href in [
            "javascript:alert(1)",
            "data:text/xml,<rss/>",
            "mailto:someone@example.com",
            "file:///etc/passwd",
            "#",
            "",
        ] {
            let html = format!(
                r#"<head><link rel="alternate" type="application/rss+xml" href="{href}"></head>"#
            );
            assert!(
                feeds_in(&html, "https://example.com/").is_empty(),
                "{href} was accepted"
            );
        }
    }

    #[test]
    fn an_absolute_address_is_kept_and_a_relative_one_is_resolved() {
        let base = "https://example.com/blog/post.html";
        assert_eq!(
            resolve(base, "https://other.com/feed").as_deref(),
            Some("https://other.com/feed")
        );
        assert_eq!(
            resolve(base, "/feed.xml").as_deref(),
            Some("https://example.com/feed.xml")
        );
        assert_eq!(
            resolve(base, "feed.xml").as_deref(),
            Some("https://example.com/blog/feed.xml")
        );
        assert_eq!(
            resolve(base, "./feed.xml").as_deref(),
            Some("https://example.com/blog/feed.xml")
        );
        assert_eq!(
            resolve(base, "../feed.xml").as_deref(),
            Some("https://example.com/feed.xml")
        );
        assert_eq!(
            resolve(base, "//cdn.example.net/feed").as_deref(),
            Some("https://cdn.example.net/feed")
        );
    }

    #[test]
    fn walking_up_past_the_root_stops_at_the_root() {
        assert_eq!(
            resolve("https://example.com/a/b", "../../../../feed").as_deref(),
            Some("https://example.com/feed")
        );
    }

    #[test]
    fn a_directory_keeps_its_trailing_slash() {
        assert_eq!(
            resolve("https://example.com/blog/", "feed/").as_deref(),
            Some("https://example.com/blog/feed/")
        );
        assert_eq!(
            resolve("https://example.com/blog/post", "..").as_deref(),
            Some("https://example.com/")
        );
    }

    #[test]
    fn a_query_survives_resolution_and_a_fragment_does_not() {
        let base = "https://example.com/blog/post?page=2";
        assert_eq!(
            resolve(base, "feed?format=rss").as_deref(),
            Some("https://example.com/blog/feed?format=rss")
        );
        assert_eq!(
            resolve(base, "?format=rss").as_deref(),
            Some("https://example.com/blog/post?format=rss")
        );
        assert_eq!(
            resolve(base, "/feed#top").as_deref(),
            Some("https://example.com/feed")
        );
    }

    #[test]
    fn a_base_that_is_not_a_web_address_resolves_nothing_relative() {
        assert_eq!(resolve("ftp://example.com/x", "feed.xml"), None);
        assert_eq!(resolve("https:///feed", "feed.xml"), None);
        assert_eq!(resolve("not a url", "feed.xml"), None);
    }

    #[test]
    fn a_base_with_no_path_is_treated_as_the_root() {
        assert_eq!(
            resolve("https://example.com", "feed.xml").as_deref(),
            Some("https://example.com/feed.xml")
        );
        assert_eq!(
            resolve("https://example.com:8080", "/feed").as_deref(),
            Some("https://example.com:8080/feed")
        );
    }

    #[test]
    fn a_missing_scheme_is_assumed_to_be_https() {
        assert_eq!(absolute("example.com/feed"), "https://example.com/feed");
        assert_eq!(absolute("  example.com  "), "https://example.com");
        assert_eq!(
            absolute("localhost:8080/feed"),
            "https://localhost:8080/feed"
        );
        assert_eq!(absolute("http://example.com"), "http://example.com");
        assert_eq!(absolute("https://example.com"), "https://example.com");
    }

    #[test]
    fn only_the_head_of_an_enormous_page_is_read() {
        let mut html = String::from("<head>");
        html.push_str(&"<!-- padding -->".repeat(HEAD_BYTES / 16));
        html.push_str(r#"<link rel="alternate" type="application/rss+xml" href="/late.xml">"#);
        html.push_str("</head>");
        assert!(feeds_in(&html, "https://example.com/").is_empty());
    }

    #[test]
    fn cutting_a_page_short_never_lands_inside_a_character() {
        let text = "héllo wörld".repeat(64);
        for max in 0..text.len() {
            let at = boundary_of(text.as_bytes(), max);
            assert!(text.is_char_boundary(at), "cut at {at} for max {max}");
        }
    }
}
