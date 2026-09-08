use dom_smoothie::{Article, Config, Readability};

use crate::fetch::Fetcher;
use crate::{Error, Result};

/// Readability always returns its best guess, and on a page with no article
/// that guess is the navigation. Anything this short is not worth keeping over
/// the summary the feed already gave us.
const MIN_ARTICLE_CHARS: usize = 200;

#[derive(Debug, Clone)]
pub struct Extracted {
    pub title: Option<String>,
    pub byline: Option<String>,
    pub content: String,
    pub chars: usize,
}

/// Fetches an item's page and keeps the part that reads like an article.
/// This is what a feed reader means by "parse full content": the publisher
/// only sent a teaser, so the page itself has to be scraped.
pub fn from_url(fetcher: &Fetcher, url: &str) -> Result<Extracted> {
    let bytes = fetcher.get_page(url)?;
    let html = String::from_utf8_lossy(&bytes);
    from_html(&html, url)
}

pub fn from_html(html: &str, url: &str) -> Result<Extracted> {
    let config = Config {
        max_elements_to_parse: 30_000,
        ..Config::default()
    };
    let mut readability =
        Readability::new(html, Some(url), Some(config)).map_err(|e| Error::Extract {
            url: url.to_string(),
            message: e.to_string(),
        })?;

    let article: Article = readability.parse().map_err(|e| Error::Extract {
        url: url.to_string(),
        message: e.to_string(),
    })?;

    let text = article.text_content.trim();
    if text.chars().count() < MIN_ARTICLE_CHARS {
        return Err(Error::Extract {
            url: url.to_string(),
            message: format!("page has no article worth keeping ({} chars)", text.len()),
        });
    }
    let content = article.content.to_string();

    Ok(Extracted {
        title: non_empty(article.title),
        byline: article.byline.and_then(non_empty),
        chars: article.length,
        content,
    })
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!DOCTYPE html><html><head><title>The Headline</title></head><body>
        <nav><a href="/">Home</a><a href="/about">About</a></nav>
        <header><h1>Site Name</h1></header>
        <article>
          <h1>The Headline</h1>
          <p>The opening paragraph carries enough words to look like real prose
             rather than boilerplate, which is what the scoring pass is looking for
             when it decides which part of the page is the article itself.</p>
          <p>A second paragraph, also long enough to matter, so the container that
             holds both of them outscores the navigation and the footer around it.</p>
        </article>
        <footer><p>Copyright notice and a pile of links.</p></footer>
    </body></html>"#;

    #[test]
    fn the_article_survives_and_the_furniture_does_not() {
        let article = from_html(PAGE, "https://example.com/post").unwrap();
        assert!(article.content.contains("opening paragraph"));
        assert!(article.content.contains("second paragraph"));
        assert!(!article.content.contains("Copyright notice"));
        assert!(!article.content.contains("href=\"/about\""));
    }

    #[test]
    fn the_title_comes_back_with_it() {
        let article = from_html(PAGE, "https://example.com/post").unwrap();
        assert_eq!(article.title.as_deref(), Some("The Headline"));
        assert!(article.chars > 100);
    }

    #[test]
    fn a_page_with_no_article_is_an_error_not_an_empty_string() {
        let bare = "<html><body><nav><a href='/'>Home</a></nav></body></html>";
        assert!(from_html(bare, "https://example.com/").is_err());
    }
}
