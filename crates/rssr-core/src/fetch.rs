use std::time::Duration;

use ureq::Agent;

use crate::{Error, Result};

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;
const PAGE_ACCEPT: &str = "text/html, application/xhtml+xml;q=0.9, */*;q=0.5";
const ACCEPT: &str = "application/atom+xml, application/rss+xml, application/feed+json, application/xml;q=0.9, */*;q=0.8";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Debug)]
pub enum Fetched {
    NotModified,
    Body {
        bytes: Vec<u8>,
        validators: Validators,
        content_type: Option<String>,
    },
}

#[derive(Clone)]
pub struct Fetcher {
    agent: Agent,
}

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

impl Default for Fetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetcher {
    pub fn new() -> Self {
        Self::with_timeout(DEFAULT_TIMEOUT)
    }

    /// The stage timeouts matter as much as the ceiling: a server that accepts
    /// the connection and then says nothing is the common way one feed holds
    /// up a whole refresh.
    pub fn with_timeout(timeout: Duration) -> Self {
        let stage = timeout.min(Duration::from_secs(6));
        let config = Agent::config_builder()
            .timeout_global(Some(timeout))
            .timeout_resolve(Some(Duration::from_secs(3)))
            .timeout_connect(Some(stage))
            .timeout_recv_response(Some(stage))
            .timeout_recv_body(Some(stage))
            .http_status_as_error(false)
            .user_agent(concat!(
                "rssr/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/kasecrab/rssr)"
            ))
            .max_idle_connections(128)
            .max_idle_connections_per_host(4)
            .build();
        Fetcher {
            agent: config.new_agent(),
        }
    }

    /// Fetches an article page rather than a feed: no validators, since the
    /// body is extracted once and then kept.
    pub fn get_page(&self, url: &str) -> Result<Vec<u8>> {
        let mut resp = self
            .agent
            .get(url)
            .header("Accept", PAGE_ACCEPT)
            .call()
            .map_err(|e| Error::Http {
                url: url.to_string(),
                message: e.to_string(),
            })?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(Error::Status {
                url: url.to_string(),
                code: status,
            });
        }

        resp.body_mut()
            .with_config()
            .limit(MAX_PAGE_BYTES)
            .read_to_vec()
            .map_err(|e| Error::Http {
                url: url.to_string(),
                message: e.to_string(),
            })
    }

    pub fn get(&self, url: &str, cached: &Validators) -> Result<Fetched> {
        let mut req = self.agent.get(url).header("Accept", ACCEPT);
        if let Some(etag) = &cached.etag {
            req = req.header("If-None-Match", etag);
        }
        if let Some(modified) = &cached.last_modified {
            req = req.header("If-Modified-Since", modified);
        }

        let mut resp = req.call().map_err(|e| Error::Http {
            url: url.to_string(),
            message: e.to_string(),
        })?;

        let status = resp.status().as_u16();
        if status == 304 {
            return Ok(Fetched::NotModified);
        }
        if !(200..300).contains(&status) {
            return Err(Error::Status {
                url: url.to_string(),
                code: status,
            });
        }

        let validators = Validators {
            etag: header(&resp, "etag"),
            last_modified: header(&resp, "last-modified"),
        };
        let content_type = header(&resp, "content-type");
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(MAX_BYTES)
            .read_to_vec()
            .map_err(|e| Error::Http {
                url: url.to_string(),
                message: e.to_string(),
            })?;

        Ok(Fetched::Body {
            bytes,
            validators,
            content_type,
        })
    }
}

fn header(resp: &http::Response<ureq::Body>, name: &str) -> Option<String> {
    resp.headers()
        .get(name)?
        .to_str()
        .ok()
        .map(str::to_string)
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// Serves `response` once and hands back the request line and headers.
    fn serve(response: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            stream.write_all(response.as_bytes()).unwrap();
            let _ = tx.send(request);
        });
        (format!("http://{addr}/feed.xml"), rx)
    }

    #[test]
    fn a_200_returns_the_body_and_its_validators() {
        let (url, _rx) = serve(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: \"abc\"\r\nLast-Modified: Wed, 01 Jan 2025 00:00:00 GMT\r\nConnection: close\r\n\r\nhello",
        );
        match Fetcher::new().get(&url, &Validators::default()).unwrap() {
            Fetched::Body {
                bytes, validators, ..
            } => {
                assert_eq!(bytes, b"hello");
                assert_eq!(validators.etag.as_deref(), Some("\"abc\""));
                assert!(validators.last_modified.is_some());
            }
            other => panic!("expected a body, got {other:?}"),
        }
    }

    #[test]
    fn a_cached_feed_sends_its_validators_and_accepts_304() {
        let (url, rx) = serve("HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n");
        let cached = Validators {
            etag: Some("\"abc\"".into()),
            last_modified: Some("Wed, 01 Jan 2025 00:00:00 GMT".into()),
        };
        let fetched = Fetcher::new().get(&url, &cached).unwrap();
        assert!(matches!(fetched, Fetched::NotModified));

        let request = rx.recv().unwrap();
        assert!(
            request.contains("if-none-match: \"abc\"")
                || request.contains("If-None-Match: \"abc\"")
        );
        assert!(request.to_lowercase().contains("if-modified-since:"));
    }

    #[test]
    fn a_server_error_is_reported_with_its_status() {
        let (url, _rx) = serve(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let err = Fetcher::new()
            .get(&url, &Validators::default())
            .unwrap_err();
        assert!(matches!(err, Error::Status { code: 503, .. }));
    }
}
