//! Handing a page to whatever shows web pages on this machine.
//!
//! Everything here is one address at a time and deliberately dull. The address
//! came out of a feed, which is to say from someone else, so it is checked
//! before it is handed to a browser carrying the reader's logged-in session.

use std::process::{Command, Stdio};

use crate::{Error, Result};

/// What opens a page when nothing else is configured.
#[cfg(target_os = "macos")]
const OPENER: (&str, &[&str]) = ("open", &[]);

/// Not `cmd /c start`: `&` and `^` in an address mean something to the command
/// interpreter, and an address out of a feed is not ours to trust with that.
#[cfg(target_os = "windows")]
const OPENER: (&str, &[&str]) = ("rundll32.exe", &["url.dll,FileProtocolHandler"]);

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const OPENER: (&str, &[&str]) = ("xdg-open", &[]);

/// Opens `url` and returns as soon as it is handed over. Waiting would hold
/// the terminal for as long as the browser runs.
pub fn open(url: &str) -> Result<()> {
    if !is_web_address(url) {
        return Err(Error::Usage(format!("not a web address: {url:?}")));
    }
    let (program, args) = opener(url);
    Command::new(&program)
        .args(&args)
        // A browser writing over the reader's own output would be a bug that
        // only shows up on someone else's machine.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::NoOpener(format!("{program}: {e}")))?;
    Ok(())
}

/// Only http and https are ever opened. A `file:` or `javascript:` address
/// out of a feed would otherwise be handed to a browser that trusts it, and
/// whitespace in an argument is how one address becomes two.
pub fn is_web_address(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return false;
    }
    !url.chars().any(|ch| ch.is_whitespace() || ch.is_control())
}

/// `$BROWSER` holds a colon-separated list of commands, each of which may put
/// the address somewhere other than the end with `%s`. The first entry wins,
/// which is what the convention says and what a reader would expect.
fn opener(url: &str) -> (String, Vec<String>) {
    if let Some(preferred) = std::env::var_os("BROWSER") {
        let preferred = preferred.to_string_lossy().into_owned();
        if let Some(command) = preferred.split(':').map(str::trim).find(|c| !c.is_empty()) {
            let mut words = command.split_whitespace().map(str::to_string);
            if let Some(program) = words.next() {
                let mut args: Vec<String> = words.collect();
                if args.iter().any(|arg| arg.contains("%s")) {
                    for arg in &mut args {
                        *arg = arg.replace("%s", url);
                    }
                } else {
                    args.push(url.to_string());
                }
                return (program, args);
            }
        }
    }
    let (program, args) = OPENER;
    let mut args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    args.push(url.to_string());
    (program.to_string(), args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_web_address_is_accepted_however_it_is_cased() {
        assert!(is_web_address("http://example.com/a"));
        assert!(is_web_address("https://example.com/a?b=c#d"));
        assert!(is_web_address("HTTPS://EXAMPLE.COM"));
    }

    #[test]
    fn nothing_else_is() {
        for url in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,<script>",
            "mailto:someone@example.com",
            "ftp://example.com/x",
            "//example.com/x",
            "/local/path",
            "",
            "example.com",
        ] {
            assert!(!is_web_address(url), "{url} was accepted");
        }
    }

    #[test]
    fn an_address_that_could_pass_as_two_arguments_is_refused() {
        assert!(!is_web_address("https://example.com/a b"));
        assert!(!is_web_address(
            "https://example.com/a\nhttps://evil.example"
        ));
        assert!(!is_web_address("https://example.com/a\t-flag"));
        assert!(!is_web_address("https://example.com/\u{0}"));
    }

    /// `$BROWSER` is read from the environment, which every test in the
    /// process shares, so the cases that set it run inside one test.
    #[test]
    fn the_opener_takes_the_address_as_an_argument_of_its_own() {
        let url = "https://example.com/a?b=c";

        // SAFETY: single-threaded within this test, and the variable is put
        // back before anything else can read it.
        unsafe { std::env::remove_var("BROWSER") };
        let (program, args) = opener(url);
        assert!(!program.is_empty());
        assert_eq!(args.last().map(String::as_str), Some(url));

        unsafe { std::env::set_var("BROWSER", "firefox --new-tab") };
        assert_eq!(
            opener(url),
            ("firefox".into(), vec!["--new-tab".into(), url.into()])
        );

        unsafe { std::env::set_var("BROWSER", "myviewer %s --quiet") };
        assert_eq!(
            opener(url),
            ("myviewer".into(), vec![url.into(), "--quiet".into()])
        );

        unsafe { std::env::set_var("BROWSER", "firefox:chromium") };
        assert_eq!(opener(url).0, "firefox");

        unsafe { std::env::set_var("BROWSER", "") };
        assert_ne!(opener(url).0, "");

        unsafe { std::env::remove_var("BROWSER") };
    }

    #[test]
    fn an_address_that_is_not_a_web_page_is_refused_before_anything_is_run() {
        let error = open("file:///etc/passwd").unwrap_err();
        assert_eq!(error.code(), "USAGE_ERROR");
    }
}
