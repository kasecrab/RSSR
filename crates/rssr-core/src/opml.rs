use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub url: String,
    pub title: Option<String>,
    pub folder: Option<String>,
}

/// Reads the `<outline>` tree. An outline carrying `xmlUrl` is a feed;
/// one without is a folder, and its `text` labels everything below it.
pub fn parse(xml: &[u8]) -> Result<Vec<Subscription>> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut stack: Vec<Option<String>> = Vec::new();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) if is_outline(&e) => {
                let (url, title) = fields(&e)?;
                match url {
                    Some(url) => {
                        push(&mut out, &mut seen, url, title, folder(&stack));
                        stack.push(None);
                    }
                    None => stack.push(title),
                }
            }
            Event::Empty(e) if is_outline(&e) => {
                let (url, title) = fields(&e)?;
                if let Some(url) = url {
                    push(&mut out, &mut seen, url, title, folder(&stack));
                }
            }
            Event::End(e) if e.name().as_ref().eq_ignore_ascii_case("outline") => {
                stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(out)
}

fn is_outline(e: &BytesStart<'_>) -> bool {
    e.name().as_ref().eq_ignore_ascii_case("outline")
}

fn folder(stack: &[Option<String>]) -> Option<String> {
    stack.iter().rev().find_map(|f| f.clone())
}

fn push(
    out: &mut Vec<Subscription>,
    seen: &mut std::collections::HashSet<String>,
    url: String,
    title: Option<String>,
    folder: Option<String>,
) {
    if seen.insert(url.clone()) {
        out.push(Subscription { url, title, folder });
    }
}

fn fields(e: &BytesStart<'_>) -> Result<(Option<String>, Option<String>)> {
    let mut url = None;
    let mut text = None;
    let mut title = None;

    for attr in e.attributes() {
        let attr = attr.map_err(|e| Error::Opml(e.to_string()))?;
        let key = attr.key.as_ref().to_ascii_lowercase();
        let value = attr
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|e| Error::Opml(e.to_string()))?
            .trim()
            .to_string();
        if value.is_empty() {
            continue;
        }
        match key.as_str() {
            "xmlurl" => url = Some(value),
            "text" => text = Some(value),
            "title" => title = Some(value),
            _ => {}
        }
    }

    Ok((url, text.or(title)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<opml version="2.0">
  <body>
    <outline text="Tech">
      <outline type="rss" text="LWN" xmlUrl="https://lwn.net/headlines/rss"/>
      <outline type="rss" title="Phoronix" xmlUrl="https://phoronix.com/rss.php"/>
    </outline>
    <outline type="rss" text="Loose" xmlUrl="https://example.com/feed.xml"/>
  </body>
</opml>"#;

    #[test]
    fn folders_label_their_children() {
        let subs = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(subs.len(), 3);
        assert_eq!(subs[0].title.as_deref(), Some("LWN"));
        assert_eq!(subs[0].folder.as_deref(), Some("Tech"));
        assert_eq!(subs[1].title.as_deref(), Some("Phoronix"));
        assert_eq!(subs[2].folder, None);
    }

    #[test]
    fn duplicate_urls_collapse() {
        let xml = r#"<opml><body>
            <outline xmlUrl="https://a.com/feed"/>
            <outline xmlUrl="https://a.com/feed"/>
        </body></opml>"#;
        assert_eq!(parse(xml.as_bytes()).unwrap().len(), 1);
    }

    #[test]
    fn a_feed_outline_that_wraps_children_still_nests() {
        let xml = r#"<opml><body>
            <outline text="Parent" xmlUrl="https://a.com/feed">
                <outline text="Child" xmlUrl="https://b.com/feed"/>
            </outline>
            <outline text="After" xmlUrl="https://c.com/feed"/>
        </body></opml>"#;
        let subs = parse(xml.as_bytes()).unwrap();
        assert_eq!(subs.len(), 3);
        assert_eq!(subs[1].folder, None);
        assert_eq!(subs[2].folder, None);
    }
}
