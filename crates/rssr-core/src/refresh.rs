use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use chrono::Utc;

use crate::fetch::{Fetched, Fetcher, Validators};
use crate::parse::{self, ParsedFeed};
use crate::store::{Feed, Store};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub workers: usize,
    /// Leave a feed alone if it was fetched more recently than this. Skipped
    /// feeds cost no request at all, not even a conditional one.
    pub max_age: Option<Duration>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            workers: 16,
            max_age: None,
        }
    }
}

#[derive(Debug)]
pub enum Status {
    Updated { new_items: usize },
    NotModified,
    Skipped { age_secs: u64 },
    Failed { code: &'static str, message: String },
}

#[derive(Debug)]
pub struct FeedOutcome {
    pub url: String,
    pub status: Status,
    pub elapsed_ms: u128,
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
    elapsed_ms: u128,
}

/// Fetches every subscribed feed and writes what came back.
///
/// Feeds are grouped by host and each worker takes a whole host at a time, so
/// requests to one server stay sequential without a rate limiter. Workers only
/// fetch and parse; the database is written here, on one thread, in one
/// transaction per feed.
pub fn refresh(store: &mut Store, fetcher: &Fetcher, options: Options) -> Result<Summary> {
    let mut summary = Summary::default();
    let mut due = Vec::new();
    for feed in store.feeds()? {
        match fresh_for(&feed, options.max_age) {
            Some(age_secs) => summary.outcomes.push(FeedOutcome {
                url: feed.url,
                status: Status::Skipped { age_secs },
                elapsed_ms: 0,
            }),
            None => due.push(feed),
        }
    }
    if due.is_empty() {
        summary.outcomes.sort_by(|a, b| a.url.cmp(&b.url));
        return Ok(summary);
    }

    let mut by_host: HashMap<String, Vec<Feed>> = HashMap::new();
    for feed in due {
        by_host.entry(host_of(&feed.url)).or_default().push(feed);
    }

    let queue = Mutex::new(by_host.into_values().collect::<Vec<_>>());
    let workers = options.workers.clamp(1, queue.lock().unwrap().len());
    let (tx, rx) = mpsc::sync_channel::<Done>(workers * 2);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let queue = &queue;
            scope.spawn(move || {
                loop {
                    // The guard must be dropped before fetching: held across
                    // the body it would serialise every worker on the queue.
                    let next = queue.lock().unwrap().pop();
                    let Some(host) = next else { return };
                    for feed in host {
                        let started = std::time::Instant::now();
                        let work = fetch_one(fetcher, &feed);
                        let done = Done {
                            feed,
                            work,
                            elapsed_ms: started.elapsed().as_millis(),
                        };
                        if tx.send(done).is_err() {
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
        Ok(Fetched::Body {
            bytes,
            validators,
            content_type,
        }) => match parse::parse(&feed.url, &bytes) {
            Ok(parsed) => Work::Parsed(Box::new(parsed), validators),
            Err(e) => Work::Failed(explain(e, content_type.as_deref())),
        },
        Err(e) => Work::Failed(e),
    }
}

fn write_one(store: &mut Store, done: Done) -> Result<FeedOutcome> {
    let Done {
        feed,
        work,
        elapsed_ms,
    } = done;
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
        elapsed_ms,
    })
}

/// How long ago the feed was fetched, when that is still inside `max_age`.
fn fresh_for(feed: &Feed, max_age: Option<Duration>) -> Option<u64> {
    let max_age = max_age?;
    let fetched_at = feed.fetched_at?;
    let age = Utc::now().signed_duration_since(fetched_at).to_std().ok()?;
    (age < max_age).then_some(age.as_secs())
}

/// A feed URL that has quietly become a web page is the common case behind a
/// parse failure, and the raw XML error does not say so.
fn explain(error: Error, content_type: Option<&str>) -> Error {
    let looks_like_a_page = content_type
        .map(|value| value.starts_with("text/html"))
        .unwrap_or(false);
    match (error, looks_like_a_page) {
        (Error::Parse { url, message }, true) => Error::Parse {
            url,
            message: format!("{message} (the server sent a web page, not a feed)"),
        },
        (error, _) => error,
    }
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
    use std::time::Duration;

    const RSS: &str = "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>T</title>\
<link>https://example.com</link><item><title>One</title><guid>a</guid>\
<link>https://example.com/a</link></item></channel></rss>";

    fn slow_serve(body: &'static str, delay: Duration) -> String {
        serve_with(body, "200 OK", delay)
    }

    /// Answers every request with the same response until dropped.
    fn serve(body: &'static str, status: &'static str) -> String {
        serve_with(body, status, Duration::ZERO)
    }

    fn serve_with(body: &'static str, status: &'static str, delay: Duration) -> String {
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
                std::thread::sleep(delay);
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

        let summary = refresh(
            &mut store,
            &Fetcher::new(),
            Options {
                workers: 4,
                ..Options::default()
            },
        )
        .unwrap();
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

    /// Four hosts that each take a beat to answer must overlap, not queue up.
    #[test]
    fn feeds_on_different_hosts_are_fetched_at_the_same_time() {
        let mut store = Store::open_in_memory().unwrap();
        for _ in 0..4 {
            subscribe(&mut store, &slow_serve(RSS, Duration::from_millis(300)));
        }

        let started = std::time::Instant::now();
        let summary = refresh(
            &mut store,
            &Fetcher::new(),
            Options {
                workers: 4,
                ..Options::default()
            },
        )
        .unwrap();
        let elapsed = started.elapsed();

        assert_eq!(summary.failed, 0);
        assert!(
            elapsed < Duration::from_millis(900),
            "four 300ms feeds took {elapsed:?}, so they ran one after another"
        );
    }

    #[test]
    fn a_recent_feed_is_skipped_without_a_request() {
        let mut store = Store::open_in_memory().unwrap();
        subscribe(&mut store, &serve(RSS, "200 OK"));
        let fetcher = Fetcher::new();
        let options = Options {
            max_age: Some(Duration::from_secs(3600)),
            ..Options::default()
        };

        let first = refresh(&mut store, &fetcher, options).unwrap();
        assert!(matches!(first.outcomes[0].status, Status::Updated { .. }));

        let second = refresh(&mut store, &fetcher, options).unwrap();
        assert!(matches!(second.outcomes[0].status, Status::Skipped { .. }));
        assert_eq!(second.outcomes[0].elapsed_ms, 0);
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
