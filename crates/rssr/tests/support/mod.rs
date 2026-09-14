//! Running the real binary against a real database and a real server.
//!
//! Everything a subcommand is asked about here goes through the same door a
//! caller uses: arguments in, exit code and output out. Nothing is stubbed,
//! and nothing reaches the network — the feeds are served from a socket the
//! test owns, which is also what makes it possible to assert on what was
//! actually requested.

// Each test uses a different corner of this module; the whole of it is used
// across the file.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

/// A database in a directory of its own, and the binary pointed at it.
pub struct Reader {
    dir: PathBuf,
    pub db: PathBuf,
}

impl Reader {
    pub fn new() -> Reader {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rssr-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Reader {
            db: dir.join("rssr.db"),
            dir,
        }
    }

    /// A second database beside the first, for the tests that move a
    /// subscription list from one to the other.
    pub fn sibling(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn run(&self, args: &[&str]) -> Run {
        self.run_on(&self.db.clone(), args)
    }

    pub fn run_on(&self, db: &std::path::Path, args: &[&str]) -> Run {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rssr"));
        command.arg("--db").arg(db).args(args);
        self.finish(command, args)
    }

    /// Without `--db`, for the arguments that never open one.
    pub fn bare(&self, args: &[&str]) -> Run {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rssr"));
        command.args(args);
        self.finish(command, args)
    }

    fn finish(&self, mut command: Command, args: &[&str]) -> Run {
        let output = command
            // Belt and braces: even an argument that did open the default
            // database would land in this directory rather than the real one.
            .env("XDG_DATA_HOME", &self.dir)
            // No test opens a browser, whatever this machine has installed.
            .env("BROWSER", "/nonexistent/rssr-test-browser")
            .output()
            .unwrap_or_else(|e| panic!("running rssr {args:?}: {e}"));
        Run {
            args: args.iter().map(|arg| arg.to_string()).collect(),
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub struct Run {
    pub args: Vec<String>,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    #[track_caller]
    pub fn ok(&self) -> &Run {
        self.code(0)
    }

    #[track_caller]
    pub fn code(&self, want: i32) -> &Run {
        assert_eq!(self.code, want, "wrong exit code{self}");
        self
    }

    #[track_caller]
    pub fn out(&self, needle: &str) -> &Run {
        assert!(
            self.stdout.contains(needle),
            "stdout is missing {needle:?}{self}"
        );
        self
    }

    #[track_caller]
    pub fn no_out(&self, needle: &str) -> &Run {
        assert!(
            !self.stdout.contains(needle),
            "stdout should not hold {needle:?}{self}"
        );
        self
    }

    #[track_caller]
    pub fn err(&self, needle: &str) -> &Run {
        assert!(
            self.stderr.contains(needle),
            "stderr is missing {needle:?}{self}"
        );
        self
    }

    #[track_caller]
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not json ({e}){self}"))
    }

    /// Lines of the item table, which is everything before the count.
    pub fn lines(&self) -> Vec<&str> {
        self.stdout
            .lines()
            .filter(|line| !line.is_empty())
            .collect()
    }
}

/// Every assertion prints the whole run, so a failure says what happened
/// rather than only what was expected.
impl std::fmt::Display for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "\n  rssr {}\n  exit {}\n  --- stdout ---\n{}  --- stderr ---\n{}",
            self.args.join(" "),
            self.code,
            indent(&self.stdout),
            indent(&self.stderr)
        )
    }
}

fn indent(text: &str) -> String {
    if text.is_empty() {
        return "  (empty)\n".into();
    }
    text.lines().map(|line| format!("  {line}\n")).collect()
}

/// A stand-in web server: a fixed set of paths answered for as long as the
/// test runs, with a record of what was asked for.
pub struct Site {
    pub base: String,
    log: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone)]
pub struct Route {
    status: u16,
    content_type: String,
    body: Vec<u8>,
    etag: Option<String>,
}

impl Route {
    pub fn feed(body: impl Into<String>) -> Route {
        Route::new(200, "application/rss+xml; charset=utf-8", body.into())
    }

    pub fn page(body: impl Into<String>) -> Route {
        Route::new(200, "text/html; charset=utf-8", body.into())
    }

    pub fn missing() -> Route {
        Route::new(404, "text/plain", String::new())
    }

    pub fn broken() -> Route {
        Route::new(503, "text/plain", String::new())
    }

    fn new(status: u16, content_type: &str, body: String) -> Route {
        Route {
            status,
            content_type: content_type.into(),
            body: body.into_bytes(),
            etag: None,
        }
    }

    /// Answers a matching `If-None-Match` with 304, so a second refresh can
    /// be seen to cost nothing.
    pub fn with_etag(mut self, etag: &str) -> Route {
        self.etag = Some(etag.into());
        self
    }
}

impl Site {
    /// The routes are built from the address, since a feed has to name where
    /// its own items live.
    pub fn new(build: impl FnOnce(&str) -> Vec<(String, Route)>) -> Site {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<HashMap<String, Route>> = Arc::new(build(&base).into_iter().collect());
        let log = Arc::new(Mutex::new(Vec::new()));

        let served = Arc::clone(&log);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let routes = Arc::clone(&routes);
                let log = Arc::clone(&served);
                std::thread::spawn(move || answer(stream, &routes, &log));
            }
        });

        Site { base, log }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Every path asked for so far, in order.
    pub fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    pub fn hits(&self, path: &str) -> usize {
        self.requests().iter().filter(|got| *got == path).count()
    }
}

fn answer(
    mut stream: std::net::TcpStream,
    routes: &HashMap<String, Route>,
    log: &Mutex<Vec<String>>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => return,
    });

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let target = request_line.split_whitespace().nth(1).unwrap_or("/");
    let path = target.split('?').next().unwrap_or("/").to_string();

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    log.lock().unwrap().push(path.clone());

    let response = match routes.get(&path) {
        None => respond(404, "text/plain", b"", None),
        Some(route) => {
            let asked_for = headers
                .iter()
                .find(|(name, _)| name == "if-none-match")
                .map(|(_, value)| value.as_str());
            match (&route.etag, asked_for) {
                (Some(held), Some(asked)) if held == asked => respond(304, "text/plain", b"", None),
                _ => respond(
                    route.status,
                    &route.content_type,
                    &route.body,
                    route.etag.as_deref(),
                ),
            }
        }
    };
    let _ = stream.write_all(&response);
}

fn respond(status: u16, content_type: &str, body: &[u8], etag: Option<&str>) -> Vec<u8> {
    let mut head = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(etag) = etag {
        head.push_str(&format!("ETag: {etag}\r\n"));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

/// One entry of a served feed.
pub struct Post {
    pub id: &'static str,
    pub title: &'static str,
    pub at: DateTime<Utc>,
    pub body: &'static str,
}

/// Three items spread far enough apart that `--since` and `prune` have
/// something to cut on, dated from now so the suite does not age.
pub fn sample_posts() -> Vec<Post> {
    vec![
        Post {
            id: "newest",
            title: "Newest headline",
            at: Utc::now() - chrono::Duration::hours(1),
            body: "The newest item, with enough words in it to read like prose.",
        },
        Post {
            id: "middle",
            title: "Middle headline",
            at: Utc::now() - chrono::Duration::days(3),
            body: "Something from the middle of the week, about turnips.",
        },
        Post {
            id: "oldest",
            title: "Oldest headline",
            at: Utc::now() - chrono::Duration::days(40),
            body: "The oldest item, well past any sensible retention window.",
        },
    ]
}

pub fn rss(base: &str, title: &str, posts: &[Post]) -> String {
    let items: String = posts
        .iter()
        .map(|post| {
            format!(
                "  <item>
    <title>{}</title>
    <link>{base}/posts/{}</link>
    <guid isPermaLink=\"false\">{}</guid>
    <pubDate>{}</pubDate>
    <description>{}</description>
  </item>\n",
                post.title,
                post.id,
                post.id,
                post.at.to_rfc2822(),
                post.body,
            )
        })
        .collect();

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<rss version=\"2.0\"><channel>
  <title>{title}</title>
  <link>{base}/</link>
{items}</channel></rss>"
    )
}

/// A page that advertises the feeds given, as a real site's head does.
pub fn page_advertising(feeds: &[(&str, &str)]) -> String {
    let links: String = feeds
        .iter()
        .map(|(href, title)| {
            format!(
                "  <link rel=\"alternate\" type=\"application/rss+xml\" title=\"{title}\" href=\"{href}\">\n"
            )
        })
        .collect();
    format!(
        "<!DOCTYPE html><html><head>\n<title>A Site</title>\n{links}</head><body><p>Hello.</p></body></html>"
    )
}

/// A page with an article on it, long enough for the scraper to keep.
pub fn article_page() -> String {
    "<!DOCTYPE html><html><head><title>Newest headline</title></head><body>
        <nav><a href=\"/\">Home</a></nav>
        <article>
          <p>The opening paragraph carries enough words to look like real prose
             rather than boilerplate, which is what the scoring pass looks for
             when it decides which part of a page is the article itself.</p>
          <p>A second paragraph, also long enough to matter, so that the element
             holding both of them outscores the navigation and the footer.</p>
        </article>
        <footer><p>Copyright notice and a pile of links.</p></footer>
    </body></html>"
        .into()
}

pub fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}
