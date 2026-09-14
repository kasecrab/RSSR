use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer, XmlVersion};

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

/// Writes a subscription list back out as OPML 2.0. Feeds carrying a folder
/// nest under an outline named for it, feeds without one sit at the top level,
/// and folders appear in the order they are first seen so two exports of an
/// unchanged list are byte for byte the same.
///
/// What comes out of here parses back into the same subscriptions.
pub fn write(subs: &[Subscription]) -> Result<String> {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

    let opml = BytesStart::new("opml").with_attributes([("version", "2.0")]);
    writer.write_event(Event::Start(opml))?;

    writer.write_event(Event::Start(BytesStart::new("head")))?;
    writer.write_event(Event::Start(BytesStart::new("title")))?;
    writer.write_event(Event::Text(BytesText::new("rssr subscriptions")))?;
    writer.write_event(Event::End(BytesEnd::new("title")))?;
    writer.write_event(Event::End(BytesEnd::new("head")))?;

    writer.write_event(Event::Start(BytesStart::new("body")))?;
    for (folder, members) in group(subs) {
        match folder {
            Some(name) => {
                let name = label(name);
                let outline = BytesStart::new("outline")
                    .with_attributes([("text", name.as_str()), ("title", name.as_str())]);
                writer.write_event(Event::Start(outline))?;
                for &sub in &members {
                    writer.write_event(Event::Empty(feed_outline(sub)))?;
                }
                writer.write_event(Event::End(BytesEnd::new("outline")))?;
            }
            None => {
                for &sub in &members {
                    writer.write_event(Event::Empty(feed_outline(sub)))?;
                }
            }
        }
    }
    writer.write_event(Event::End(BytesEnd::new("body")))?;
    writer.write_event(Event::End(BytesEnd::new("opml")))?;

    let mut xml = String::from_utf8(writer.into_inner())
        .map_err(|e| Error::Opml(format!("export is not utf-8: {e}")))?;
    xml.push('\n');
    Ok(xml)
}

fn feed_outline(sub: &Subscription) -> BytesStart<'static> {
    let url = url_value(&sub.url);
    // OPML requires `text` on every outline, and a reader that only looks at
    // `title` should still see something. A feed we never managed to fetch has
    // no title of its own, so its address stands in for one.
    let text = sub
        .title
        .as_deref()
        .map(label)
        .unwrap_or_else(|| url.clone());
    BytesStart::new("outline").with_attributes([
        ("type", "rss"),
        ("text", text.as_str()),
        ("title", text.as_str()),
        ("xmlUrl", url.as_str()),
    ])
}

/// Folders in first-seen order, each with its members in input order. A map
/// would sort them; the point here is that the export mirrors what it was
/// handed instead of rearranging it.
fn group(subs: &[Subscription]) -> Vec<(Option<&str>, Vec<&Subscription>)> {
    let mut groups: Vec<(Option<&str>, Vec<&Subscription>)> = Vec::new();
    let mut index: std::collections::HashMap<Option<&str>, usize> =
        std::collections::HashMap::new();
    for sub in subs {
        let key = sub
            .folder
            .as_deref()
            .map(str::trim)
            .filter(|f| !f.is_empty());
        let at = *index.entry(key).or_insert_with(|| {
            groups.push((key, Vec::new()));
            groups.len() - 1
        });
        groups[at].1.push(sub);
    }
    groups
}

/// Escaping alone does not make a title safe to put in an attribute: a newline
/// survives it and comes back as a space, and a control character makes the
/// document unparseable. Flatten both here so the file round-trips.
fn label(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_whitespace() { ' ' } else { ch })
        .filter(|&ch| ch == ' ' || !ch.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A feed address with whitespace in it is already broken; carrying it through
/// would only produce an OPML file that cannot be read back.
fn url_value(url: &str) -> String {
    url.chars()
        .filter(|ch| !ch.is_whitespace() && !ch.is_control())
        .collect()
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

    fn sub(url: &str, title: Option<&str>, folder: Option<&str>) -> Subscription {
        Subscription {
            url: url.into(),
            title: title.map(Into::into),
            folder: folder.map(Into::into),
        }
    }

    #[test]
    fn what_is_written_parses_back_into_the_same_subscriptions() {
        let subs = parse(SAMPLE.as_bytes()).unwrap();
        let xml = write(&subs).unwrap();
        assert_eq!(parse(xml.as_bytes()).unwrap(), subs);
    }

    #[test]
    fn a_folder_wraps_its_feeds_and_loose_ones_stay_at_the_top() {
        let subs = vec![
            sub("https://a.com/feed", Some("A"), Some("Tech")),
            sub("https://b.com/feed", Some("B"), Some("Tech")),
            sub("https://c.com/feed", Some("C"), None),
        ];
        let xml = write(&subs).unwrap();
        assert_eq!(xml.matches("<outline").count(), 4);
        let tech = xml.find(r#"text="Tech""#).unwrap();
        let close = xml.find("</outline>").unwrap();
        assert!(xml[tech..close].contains("https://a.com/feed"));
        assert!(xml[tech..close].contains("https://b.com/feed"));
        assert!(!xml[tech..close].contains("https://c.com/feed"));
        assert_eq!(parse(xml.as_bytes()).unwrap(), subs);
    }

    #[test]
    fn markup_in_a_title_is_escaped_rather_than_emitted() {
        let subs = vec![sub(
            "https://a.com/feed?x=1&y=2",
            Some(r#"Bells & <whistles> "quoted""#),
            Some("R&D"),
        )];
        let xml = write(&subs).unwrap();
        assert!(!xml.contains("<whistles>"));
        assert!(xml.contains("&amp;"));
        assert_eq!(parse(xml.as_bytes()).unwrap(), subs);
    }

    #[test]
    fn a_title_spanning_lines_comes_back_on_one() {
        let subs = vec![sub("https://a.com/feed", Some("Two\nlines\there"), None)];
        let written = parse(write(&subs).unwrap().as_bytes()).unwrap();
        assert_eq!(written[0].title.as_deref(), Some("Two lines here"));
    }

    #[test]
    fn a_control_character_does_not_make_the_file_unreadable() {
        let subs = vec![sub("https://a.com/feed", Some("bad\u{1}title"), None)];
        let written = parse(write(&subs).unwrap().as_bytes()).unwrap();
        assert_eq!(written[0].title.as_deref(), Some("badtitle"));
    }

    #[test]
    fn an_untitled_feed_is_labelled_with_its_address() {
        let subs = vec![sub("https://a.com/feed", None, None)];
        let xml = write(&subs).unwrap();
        assert!(xml.contains(r#"text="https://a.com/feed""#));
        assert_eq!(
            parse(xml.as_bytes()).unwrap()[0].title.as_deref(),
            Some("https://a.com/feed")
        );
    }

    #[test]
    fn an_empty_list_still_writes_a_readable_file() {
        let xml = write(&[]).unwrap();
        assert!(parse(xml.as_bytes()).unwrap().is_empty());
    }

    #[test]
    fn two_exports_of_one_list_are_identical() {
        let subs = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(write(&subs).unwrap(), write(&subs).unwrap());
    }

    #[test]
    fn a_folder_split_across_the_list_is_written_once() {
        let subs = vec![
            sub("https://a.com/feed", Some("A"), Some("Tech")),
            sub("https://c.com/feed", Some("C"), None),
            sub("https://b.com/feed", Some("B"), Some("Tech")),
        ];
        let xml = write(&subs).unwrap();
        assert_eq!(xml.matches(r#"text="Tech""#).count(), 1);
        let round_tripped = parse(xml.as_bytes()).unwrap();
        assert_eq!(round_tripped.len(), 3);
        assert!(round_tripped.iter().all(|s| match s.url.as_str() {
            "https://c.com/feed" => s.folder.is_none(),
            _ => s.folder.as_deref() == Some("Tech"),
        }));
    }

    #[test]
    fn a_blank_folder_name_is_treated_as_no_folder() {
        let subs = vec![sub("https://a.com/feed", Some("A"), Some("   "))];
        let xml = write(&subs).unwrap();
        assert_eq!(xml.matches("<outline").count(), 1);
        assert_eq!(parse(xml.as_bytes()).unwrap()[0].folder, None);
    }

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
