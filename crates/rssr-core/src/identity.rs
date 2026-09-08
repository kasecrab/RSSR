use std::fmt::Write;

use sha2::{Digest, Sha256};

/// Which field the item id was derived from. Worth keeping: it is the only
/// signal that a feed gave us nothing durable to key on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdSource {
    Guid,
    Link,
    Fallback,
}

impl IdSource {
    pub fn as_str(self) -> &'static str {
        match self {
            IdSource::Guid => "guid",
            IdSource::Link => "link",
            IdSource::Fallback => "fallback",
        }
    }
}

/// Query parameters that identify a campaign rather than an article. Feeds
/// regenerate these per request, so leaving them in makes every refresh look
/// like a page of brand new items.
const VOLATILE_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "utm_id",
    "fbclid",
    "gclid",
    "mc_cid",
    "mc_eid",
    "ref",
    "ref_src",
    "source",
];

pub fn item_id(
    feed_url: &str,
    guid: Option<&str>,
    link: Option<&str>,
    fallback: &str,
) -> (String, IdSource) {
    let (key, source) = match (usable_guid(guid), link) {
        (Some(guid), _) => (normalize_guid(guid), IdSource::Guid),
        (None, Some(link)) => (normalize_url(link), IdSource::Link),
        (None, None) => (fallback.to_string(), IdSource::Fallback),
    };

    let mut hasher = Sha256::new();
    hasher.update(feed_url.as_bytes());
    hasher.update(b"\n");
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();

    let mut id = String::with_capacity(16);
    for byte in &digest[..8] {
        let _ = write!(id, "{byte:02x}");
    }
    (id, source)
}

/// feed-rs invents a UUID when an entry carries no id and no link. That value
/// changes on every parse, so it is worse than having nothing.
fn usable_guid(guid: Option<&str>) -> Option<&str> {
    let guid = guid.map(str::trim).filter(|g| !g.is_empty())?;
    if looks_like_uuid(guid) {
        return None;
    }
    Some(guid)
}

/// Some publishers reuse one article across feed sections and tell them apart
/// with a counter in the guid fragment. A URL-shaped guid gets the same
/// cleaning as a link so those collapse to one item.
fn normalize_guid(guid: &str) -> String {
    if guid.starts_with("http://") || guid.starts_with("https://") {
        normalize_url(guid)
    } else {
        guid.to_string()
    }
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.as_bytes().iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

pub fn normalize_url(url: &str) -> String {
    let url = url.trim();
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, query),
        None => return strip_fragment(url).to_string(),
    };

    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            let name = pair.split('=').next().unwrap_or(pair);
            !VOLATILE_PARAMS.contains(&name)
        })
        .collect();

    let base = strip_fragment(base);
    if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    }
}

fn strip_fragment(url: &str) -> &str {
    url.split_once('#').map_or(url, |(head, _)| head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_item_hashes_the_same_way_twice() {
        let a = item_id("https://f.com/rss", Some("post-1"), None, "x");
        let b = item_id("https://f.com/rss", Some("post-1"), None, "x");
        assert_eq!(a, b);
        assert_eq!(a.1, IdSource::Guid);
        assert_eq!(a.0.len(), 16);
    }

    #[test]
    fn ids_are_namespaced_by_feed() {
        let a = item_id("https://f.com/rss", Some("post-1"), None, "x");
        let b = item_id("https://mirror.com/rss", Some("post-1"), None, "x");
        assert_ne!(a.0, b.0);
    }

    #[test]
    fn a_generated_uuid_is_not_trusted_as_a_guid() {
        let uuid = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
        let (_, source) = item_id(
            "https://f.com/rss",
            Some(uuid),
            Some("https://f.com/a"),
            "x",
        );
        assert_eq!(source, IdSource::Link);
    }

    #[test]
    fn tracking_parameters_do_not_change_identity() {
        let plain = item_id("https://f.com/rss", None, Some("https://f.com/a"), "x");
        let tagged = item_id(
            "https://f.com/rss",
            None,
            Some("https://f.com/a?utm_source=rss&utm_medium=feed#top"),
            "x",
        );
        assert_eq!(plain.0, tagged.0);
    }

    #[test]
    fn real_query_parameters_are_kept() {
        assert_eq!(
            normalize_url("https://f.com/a?id=7&utm_source=rss"),
            "https://f.com/a?id=7"
        );
    }

    #[test]
    fn one_article_listed_twice_under_fragment_guids_is_one_item() {
        let feed = "https://feeds.bbci.co.uk/news/rss.xml";
        let article = "https://www.bbc.co.uk/sport/football/articles/c17j5lr8kkqo";
        let first = item_id(feed, Some(&format!("{article}#1")), Some(article), "x");
        let seventh = item_id(feed, Some(&format!("{article}#7")), Some(article), "x");
        assert_eq!(first.0, seventh.0);
    }

    #[test]
    fn a_guid_that_is_not_a_url_is_left_alone() {
        let a = item_id(
            "https://f.com/rss",
            Some("tag:f.com,2026:post#1"),
            None,
            "x",
        );
        let b = item_id(
            "https://f.com/rss",
            Some("tag:f.com,2026:post#7"),
            None,
            "x",
        );
        assert_ne!(a.0, b.0);
    }

    #[test]
    fn an_item_with_nothing_durable_falls_back() {
        let (_, source) = item_id("https://f.com/rss", None, None, "Title|2026-01-01");
        assert_eq!(source, IdSource::Fallback);
    }
}
