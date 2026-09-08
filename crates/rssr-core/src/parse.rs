use chrono::{DateTime, Utc};
use feed_rs::model::{Entry, Link, Text};
use feed_rs::parser::Builder;

use crate::identity::{IdSource, item_id};
use crate::{Error, Result};

#[derive(Debug, Clone)]
pub struct ParsedFeed {
    pub title: Option<String>,
    pub site_url: Option<String>,
    pub items: Vec<ParsedItem>,
}

#[derive(Debug, Clone)]
pub struct ParsedItem {
    pub id: String,
    pub id_source: IdSource,
    pub guid: Option<String>,
    pub url: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub summary: Option<String>,
    pub content: Option<String>,
    pub published: Option<DateTime<Utc>>,
    pub updated: Option<DateTime<Utc>>,
}

impl ParsedItem {
    /// RSS carries `pubDate`, Atom carries `updated`, and plenty of feeds set
    /// only one of them. Ordering has to work either way.
    pub fn dated_at(&self) -> Option<DateTime<Utc>> {
        self.published.or(self.updated)
    }
}

pub fn parse(feed_url: &str, bytes: &[u8]) -> Result<ParsedFeed> {
    let parser = Builder::new().base_uri(Some(feed_url)).build();
    let feed = parser.parse(bytes).map_err(|e| Error::Parse {
        url: feed_url.to_string(),
        message: e.to_string(),
    })?;

    let items = feed
        .entries
        .into_iter()
        .map(|entry| item(feed_url, entry))
        .collect();

    Ok(ParsedFeed {
        title: feed.title.and_then(text),
        site_url: primary_link(&feed.links),
        items,
    })
}

fn item(feed_url: &str, entry: Entry) -> ParsedItem {
    let url = primary_link(&entry.links);
    let title = entry.title.and_then(text);
    let published = entry.published;
    let updated = entry.updated;

    let fallback = format!(
        "{}|{}",
        title.as_deref().unwrap_or(""),
        published
            .or(updated)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default()
    );
    let guid = Some(entry.id).filter(|id| !id.is_empty());
    let (id, id_source) = item_id(feed_url, guid.as_deref(), url.as_deref(), &fallback);

    ParsedItem {
        id,
        id_source,
        guid,
        url,
        title,
        author: entry.authors.into_iter().next().map(|person| person.name),
        summary: entry.summary.and_then(text),
        content: entry
            .content
            .and_then(|content| content.body)
            .filter(|b| !b.is_empty()),
        published,
        updated,
    }
}

fn text(value: Text) -> Option<String> {
    let trimmed = value.content.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn primary_link(links: &[Link]) -> Option<String> {
    links
        .iter()
        .find(|link| link.rel.as_deref() == Some("alternate"))
        .or_else(|| links.iter().find(|link| link.rel.is_none()))
        .or_else(|| links.first())
        .map(|link| link.href.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <title>Example</title>
  <link>https://example.com</link>
  <item>
    <title>First</title>
    <link>https://example.com/first</link>
    <guid isPermaLink="false">post-1</guid>
    <pubDate>Tue, 01 Jul 2025 10:00:00 GMT</pubDate>
    <description>A summary.</description>
  </item>
</channel></rss>"#;

    const ATOM: &str = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Atom Example</title>
  <link href="https://example.com"/>
  <entry>
    <title>Second</title>
    <link href="/second"/>
    <id>urn:example:2</id>
    <updated>2025-07-01T10:00:00Z</updated>
  </entry>
</feed>"#;

    #[test]
    fn reads_an_rss_channel() {
        let feed = parse("https://example.com/rss", RSS.as_bytes()).unwrap();
        assert_eq!(feed.title.as_deref(), Some("Example"));
        assert_eq!(feed.site_url.as_deref(), Some("https://example.com/"));

        let item = &feed.items[0];
        assert_eq!(item.title.as_deref(), Some("First"));
        assert_eq!(item.guid.as_deref(), Some("post-1"));
        assert_eq!(item.id_source, IdSource::Guid);
        assert!(item.published.is_some());
    }

    #[test]
    fn reads_an_atom_feed_and_resolves_relative_links() {
        let feed = parse("https://example.com/atom.xml", ATOM.as_bytes()).unwrap();
        let item = &feed.items[0];
        assert_eq!(item.url.as_deref(), Some("https://example.com/second"));
        assert_eq!(item.dated_at(), item.updated);
    }

    #[test]
    fn the_same_feed_parsed_twice_yields_the_same_ids() {
        let a = parse("https://example.com/rss", RSS.as_bytes()).unwrap();
        let b = parse("https://example.com/rss", RSS.as_bytes()).unwrap();
        assert_eq!(a.items[0].id, b.items[0].id);
    }

    #[test]
    fn an_empty_title_is_absent_rather_than_blank() {
        let xml = r#"<rss version="2.0"><channel><title>F</title>
            <item><title></title><guid>a</guid><link>https://f.com/a</link></item>
        </channel></rss>"#;
        let feed = parse("https://f.com/rss", xml.as_bytes()).unwrap();
        assert_eq!(feed.items[0].title, None);
    }

    #[test]
    fn malformed_xml_is_an_error_not_a_panic() {
        assert!(parse("https://example.com/rss", b"<rss><channel>").is_err());
    }
}
