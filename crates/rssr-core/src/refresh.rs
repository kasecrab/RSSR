use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::mpsc;

use crate::fetch::{Fetched, Fetcher, Validators};
use crate::parse::{self, ParsedFeed};
use crate::store::{Feed, Store};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub workers: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { workers: 16 }
    }
}

#[derive(Debug)]
pub enum Status {
    Updated { new_items: usize },
    NotModified,
    Failed { code: &'static str, message: String },
}

#[derive(Debug)]
pub struct FeedOutcome {
    pub url: String,
    pub status: Status,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub new_items: usize,
    pub failed: usize,
    pub outcomes: Vec<FeedOutcome>,
}

enum Work {
    Parsed(Box<ParsedFeed>, Validators),
    NotModified,
    Failed(Error),
}

struct Done {
    feed: Feed,
    work: Work,
}

/// Fetches every subscribed feed and writes what came back.
///
/// Feeds are grouped by host and each worker takes a whole host at a time, so
/// requests to one server stay sequential without a rate limiter. Workers only
/// fetch and parse; the database is written here, on one thread, in one
/// transaction per feed.
pub fn refresh(store: &mut Store, fetcher: &Fetcher, options: Options) -> Result<Summary> {
    let feeds = store.feeds()?;
    if feeds.is_empty() {
        return Ok(Summary::default());
    }

    let mut by_host: HashMap<String, Vec<Feed>> = HashMap::new();
    for feed in feeds {
        by_host.entry(host_of(&feed.url)).or_default().push(feed);
    }

    let queue = Mutex::new(by_host.into_values().collect::<Vec<_>>());
    let workers = options.workers.clamp(1, queue.lock().unwrap().len());
    let (tx, rx) = mpsc::sync_channel::<Done>(workers * 2);

    let mut summary = Summary::default();

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let queue = &queue;
            scope.spawn(move || {
                while let Some(host) = queue.lock().unwrap().pop() {
                    for feed in host {
                        let work = fetch_one(fetcher, &feed);
                        if tx.send(Done { feed, work }).is_err() {
                            return;
                        }
                    }
                }
            });
        }
        drop(tx);

        for done in rx {
            match write_one(store, done) {
                Ok(outcome) => {
                    if let Status::Updated { new_items } = outcome.status {
                        summary.new_items += new_items;
                    }
                    if matches!(outcome.status, Status::Failed { .. }) {
                        summary.failed += 1;
                    }
                    summary.outcomes.push(outcome);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    })?;

    summary.outcomes.sort_by(|a, b| a.url.cmp(&b.url));
    Ok(summary)
}

fn fetch_one(fetcher: &Fetcher, feed: &Feed) -> Work {
    let cached = Validators {
        etag: feed.etag.clone(),
        last_modified: feed.last_modified.clone(),
    };
    match fetcher.get(&feed.url, &cached) {
        Ok(Fetched::NotModified) => Work::NotModified,
        Ok(Fetched::Body { bytes, validators }) => match parse::parse(&feed.url, &bytes) {
            Ok(parsed) => Work::Parsed(Box::new(parsed), validators),
            Err(e) => Work::Failed(e),
        },
        Err(e) => Work::Failed(e),
    }
}

fn write_one(store: &mut Store, done: Done) -> Result<FeedOutcome> {
    let Done { feed, work } = done;
    let status = match work {
        Work::Parsed(parsed, validators) => {
            store.update_feed_meta(feed.id, parsed.title.as_deref(), parsed.site_url.as_deref())?;
            let new_items = store.save_items(feed.id, &parsed.items)?;
            store.record_fetch(feed.id, "ok", Some(&validators), None)?;
            Status::Updated { new_items }
        }
        Work::NotModified => {
            store.record_fetch(feed.id, "not_modified", None, None)?;
            Status::NotModified
        }
        Work::Failed(e) => {
            let message = e.to_string();
            store.record_fetch(feed.id, "error", None, Some(&message))?;
            Status::Failed {
                code: e.code(),
                message,
            }
        }
    };
    Ok(FeedOutcome {
        url: feed.url,
        status,
    })
}

fn host_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    host.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opml::Subscription;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    const RSS: &str = "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>T</title>\
<link>https://example.com</link><item><title>One</title><guid>a</guid>\
<link>https://example.com/a</link></item></channel></rss>";

    /// Answers every request with the same response until dropped.
    fn serve(body: &'static str, status: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}/feed.xml")
    }

    fn subscribe(store: &mut Store, url: &str) {
        store
            .upsert_feed(&Subscription {
                url: url.into(),
                title: None,
                folder: None,
            })
            .unwrap();
    }

    #[test]
    fn every_feed_is_fetched_and_stored() {
        let mut store = Store::open_in_memory().unwrap();
        subscribe(&mut store, &serve(RSS, "200 OK"));
        subscribe(&mut store, &serve(RSS, "200 OK"));

        let summary = refresh(&mut store, &Fetcher::new(), Options { workers: 4 }).unwrap();
        assert_eq!(summary.outcomes.len(), 2);
        assert_eq!(summary.new_items, 2);
        assert_eq!(summary.failed, 0);
    }

    #[test]
    fn a_failing_feed_does_not_stop_the_others() {
        let mut store = Store::open_in_memory().unwrap();
        subscribe(&mut store, &serve(RSS, "200 OK"));
        subscribe(&mut store, &serve("", "500 Internal Server Error"));

        let summary = refresh(&mut store, &Fetcher::new(), Options::default()).unwrap();
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.new_items, 1);
    }

    #[test]
    fn a_second_pass_finds_nothing_new() {
        let mut store = Store::open_in_memory().unwrap();
        subscribe(&mut store, &serve(RSS, "200 OK"));
        let fetcher = Fetcher::new();

        refresh(&mut store, &fetcher, Options::default()).unwrap();
        let again = refresh(&mut store, &fetcher, Options::default()).unwrap();
        assert_eq!(again.new_items, 0);
        assert_eq!(store.unread_count().unwrap(), 1);
    }

    #[test]
    fn hosts_are_compared_without_scheme_or_credentials() {
        assert_eq!(
            host_of("https://user:pw@Example.COM:8443/feed"),
            "example.com:8443"
        );
        assert_eq!(host_of("http://example.com/feed"), "example.com");
    }
}
