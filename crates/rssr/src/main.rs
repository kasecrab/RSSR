use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use rssr_core::refresh::{self, Options, Status};
use rssr_core::store::{Flag, Item, Query};
use rssr_core::{Fetcher, Result, Store, content, duration, extract, fetch, opml};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "rssr",
    version,
    about = "Read RSS and Atom feeds from the shell"
)]
struct Cli {
    /// Database to use instead of the default location.
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,

    /// Print JSON instead of a table.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Subscribe to every feed in an OPML file.
    Import { file: PathBuf },
    /// Fetch new items for every subscribed feed.
    Refresh {
        #[arg(long, default_value_t = 16)]
        workers: usize,
        /// Skip feeds fetched more recently than this, e.g. 15m or 6h.
        #[arg(long, value_name = "DURATION")]
        max_age: Option<String>,
        /// Give up on a feed after this long.
        #[arg(long, value_name = "DURATION")]
        timeout: Option<String>,
        /// Also scrape article pages for feeds with full content switched on.
        #[arg(long)]
        extract: bool,
    },
    /// List subscribed feeds.
    Feeds,
    /// Change a feed's settings.
    Feed {
        id: i64,
        /// Scrape each item's page because this feed only publishes a teaser.
        #[arg(long, value_name = "on|off")]
        full_content: Toggle,
    },
    /// Scrape the full article for items whose feed only sent a teaser.
    Extract {
        ids: Vec<String>,
        /// Instead of ids, catch up every feed with full content switched on.
        #[arg(long)]
        pending: bool,
        #[arg(long, default_value_t = 25)]
        limit: usize,
    },
    /// List items, newest first.
    List {
        /// Include items already read.
        #[arg(long)]
        all: bool,
        #[arg(long, value_name = "ID")]
        feed: Option<i64>,
        #[arg(long, value_name = "NAME")]
        folder: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Print one or more items as text. Reading never marks anything read.
    Read {
        #[arg(required = true)]
        ids: Vec<String>,
        /// Print the whole body instead of the first few thousand characters.
        #[arg(long)]
        full: bool,
    },
    /// Full-text search across every stored item.
    Search {
        text: String,
        #[arg(long, default_value_t = 25)]
        limit: usize,
    },
    /// Mark items read, unread, starred or unstarred.
    Mark {
        #[arg(value_enum)]
        flag: MarkFlag,
        #[arg(required = true)]
        ids: Vec<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Toggle {
    On,
    Off,
}

#[derive(Clone, Copy, ValueEnum)]
enum MarkFlag {
    Read,
    Unread,
    Star,
    Unstar,
}

impl From<MarkFlag> for Flag {
    fn from(flag: MarkFlag) -> Self {
        match flag {
            MarkFlag::Read => Flag::Read,
            MarkFlag::Unread => Flag::Unread,
            MarkFlag::Star => Flag::Star,
            MarkFlag::Unstar => Flag::Unstar,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("rssr: {e}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: &Cli) -> Result<ExitCode> {
    let path = match &cli.db {
        Some(path) => path.clone(),
        None => Store::default_path()?,
    };
    let mut store = Store::open(&path)?;

    match &cli.command {
        Command::Import { file } => import(&store, file, cli.json),
        Command::Refresh {
            workers,
            max_age,
            timeout,
            extract,
        } => {
            let max_age = max_age.as_deref().map(duration::parse).transpose()?;
            let timeout = timeout
                .as_deref()
                .map(duration::parse)
                .transpose()?
                .unwrap_or(fetch::DEFAULT_TIMEOUT);
            do_refresh(
                &mut store,
                Options {
                    workers: *workers,
                    max_age,
                },
                timeout,
                *extract,
                cli.json,
            )
        }
        Command::Feeds => feeds(&store, cli.json),
        Command::Feed { id, full_content } => {
            set_full_content(&store, *id, matches!(full_content, Toggle::On), cli.json)
        }
        Command::Extract {
            ids,
            pending,
            limit,
        } => {
            if *pending {
                backfill(&mut store, *limit, cli.json)
            } else if ids.is_empty() {
                eprintln!("rssr: give item ids, or --pending to catch up marked feeds");
                Ok(ExitCode::from(2))
            } else {
                extract_items(&store, ids, cli.json)
            }
        }
        Command::List {
            all,
            feed,
            folder,
            limit,
        } => list(
            &store,
            Query {
                unread_only: !all,
                feed_id: *feed,
                folder: folder.clone(),
                limit: *limit,
            },
            cli.json,
        ),
        Command::Read { ids, full } => read(&store, ids, *full, cli.json),
        Command::Search { text, limit } => search(&store, text, *limit, cli.json),
        Command::Mark { flag, ids } => mark(&store, ids, (*flag).into(), cli.json),
    }
}

fn list(store: &Store, query: Query, as_json: bool) -> Result<ExitCode> {
    let items = store.items(&query)?;
    let total = store.count(&query)?;

    if as_json {
        print(&json!({ "count": items.len(), "total": total, "items": rows(&items) }));
    } else if items.is_empty() {
        println!("nothing to read");
    } else {
        show(&items);
        println!("{} of {total}", items.len());
    }
    Ok(ExitCode::SUCCESS)
}

fn rows(items: &[Item]) -> Vec<serde_json::Value> {
    items
        .iter()
        .map(|item| {
            json!({
                "id": item.id,
                "feed": item.feed_title,
                "title": item.title,
                "url": item.url,
                "author": item.author,
                "published": item.published,
                "read": item.read,
                "starred": item.starred,
            })
        })
        .collect()
}

fn show(items: &[Item]) {
    for item in items {
        println!(
            "{}  {}{}  {:<14} {:<10} {}",
            item.id,
            if item.read { " " } else { "*" },
            if item.starred { "s" } else { " " },
            truncate(item.feed_title.as_deref().unwrap_or("-"), 14),
            item.published
                .as_deref()
                .unwrap_or("")
                .get(..10)
                .unwrap_or(""),
            item.title.as_deref().unwrap_or("(untitled)"),
        );
    }
}

fn backfill(store: &mut Store, limit: usize, as_json: bool) -> Result<ExitCode> {
    let fetcher = Fetcher::new();
    let summary = refresh::extract_pending(store, &fetcher, limit, 16)?;
    if as_json {
        print(&json!({ "extracted": summary.extracted, "failed": summary.failed }));
    } else {
        println!("{} extracted, {} failed", summary.extracted, summary.failed);
    }
    Ok(if summary.failed > 0 && summary.extracted == 0 {
        ExitCode::from(4)
    } else {
        ExitCode::SUCCESS
    })
}

fn set_full_content(store: &Store, id: i64, on: bool, as_json: bool) -> Result<ExitCode> {
    if !store.set_full_content(id, on)? {
        eprintln!("no such feed: {id}");
        return Ok(ExitCode::from(1));
    }
    if as_json {
        print(&json!({ "feed": id, "full_content": on }));
    } else {
        println!("feed {id}: full content {}", if on { "on" } else { "off" });
    }
    Ok(ExitCode::SUCCESS)
}

fn extract_items(store: &Store, ids: &[String], as_json: bool) -> Result<ExitCode> {
    let fetcher = Fetcher::new();
    let mut results = Vec::new();
    let mut failed = 0;

    for id in ids {
        let Some(url) = store.item_url(id)? else {
            failed += 1;
            results.push(json!({ "id": id, "ok": false, "error": "no such item or no link" }));
            continue;
        };
        match extract::from_url(&fetcher, &url) {
            Ok(article) => {
                let content = match store.body(id)?.and_then(|body| body.title) {
                    Some(title) => extract::drop_repeated_heading(&article.content, &title),
                    None => article.content,
                };
                store.save_body(id, &content, "extracted")?;
                results.push(json!({ "id": id, "ok": true, "chars": article.chars, "url": url }));
            }
            Err(e) => {
                failed += 1;
                results.push(json!({ "id": id, "ok": false, "error": e.to_string() }));
            }
        }
    }

    if as_json {
        print(&json!({ "count": results.len(), "failed": failed, "items": results }));
    } else {
        for result in &results {
            match result["ok"].as_bool() {
                Some(true) => println!("{} {} chars", result["id"], result["chars"]),
                _ => eprintln!("{}: {}", result["id"], result["error"]),
            }
        }
    }

    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else if failed == ids.len() {
        ExitCode::from(4)
    } else {
        ExitCode::from(3)
    })
}

const PREVIEW_CHARS: usize = 4000;

fn read(store: &Store, ids: &[String], full: bool, as_json: bool) -> Result<ExitCode> {
    let mut found = Vec::new();
    let mut missing = Vec::new();

    for id in ids {
        match store.body(id)? {
            Some(body) => found.push(body),
            None => missing.push(id.clone()),
        }
    }

    if as_json {
        let items: Vec<_> = found
            .iter()
            .map(|body| {
                let text = content::to_markdown(&body.content);
                let (text, truncated) = clip(&text, full);
                json!({
                    "id": body.item_id,
                    "title": body.title,
                    "url": body.url,
                    "source": body.source,
                    "tokens_estimate": content::estimate_tokens(&text),
                    "truncated": truncated,
                    "content": text,
                })
            })
            .collect();
        print(&json!({ "count": items.len(), "items": items, "missing": missing }));
    } else {
        for body in &found {
            println!("{}", body.title.as_deref().unwrap_or("(untitled)"));
            if let Some(url) = &body.url {
                println!("{url}");
            }
            let (text, truncated) = clip(&content::to_markdown(&body.content), full);
            println!("\n{text}");
            if truncated {
                println!("\n[truncated, rerun with --full]");
            }
            println!();
        }
        for id in &missing {
            eprintln!("no such item: {id}");
        }
    }

    Ok(if found.is_empty() && !missing.is_empty() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn clip(text: &str, full: bool) -> (String, bool) {
    if full {
        return (text.to_string(), false);
    }
    match text.char_indices().nth(PREVIEW_CHARS) {
        Some((cut, _)) => (text[..cut].to_string(), true),
        None => (text.to_string(), false),
    }
}

fn search(store: &Store, text: &str, limit: usize, as_json: bool) -> Result<ExitCode> {
    let items = store.search(text, limit)?;
    if as_json {
        print(&json!({ "count": items.len(), "items": rows(&items) }));
    } else if items.is_empty() {
        println!("no matches for {text:?}");
    } else {
        show(&items);
        println!("{} matches", items.len());
    }
    Ok(ExitCode::SUCCESS)
}

fn mark(store: &Store, ids: &[String], flag: Flag, as_json: bool) -> Result<ExitCode> {
    let changed = store.set_flag(ids, flag)?;
    if as_json {
        print(&json!({ "matched": changed, "requested": ids.len() }));
    } else {
        println!("{changed} of {} items updated", ids.len());
    }
    Ok(ExitCode::SUCCESS)
}

/// Clips to `width` printed characters, ellipsis included, so columns line up.
fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    let cut = value
        .char_indices()
        .nth(width.saturating_sub(1))
        .map_or(value.len(), |(index, _)| index);
    format!("{}…", &value[..cut])
}

fn import(store: &Store, file: &PathBuf, as_json: bool) -> Result<ExitCode> {
    let subscriptions = opml::parse(&std::fs::read(file)?)?;
    let mut added = 0;
    for subscription in &subscriptions {
        if store.feed_id(&subscription.url)?.is_none() {
            added += 1;
        }
        store.upsert_feed(subscription)?;
    }

    if as_json {
        print(&json!({ "found": subscriptions.len(), "added": added }));
    } else {
        println!("{} feeds in file, {added} new", subscriptions.len());
    }
    Ok(ExitCode::SUCCESS)
}

fn do_refresh(
    store: &mut Store,
    options: Options,
    timeout: std::time::Duration,
    with_extract: bool,
    as_json: bool,
) -> Result<ExitCode> {
    let fetcher = Fetcher::with_timeout(timeout);
    let summary = refresh::refresh(store, &fetcher, options)?;
    let extracted = if with_extract {
        refresh::extract_pending(store, &fetcher, 25, options.workers)?
    } else {
        Default::default()
    };

    if as_json {
        let feeds: Vec<_> = summary
            .outcomes
            .iter()
            .map(|outcome| {
                let (status, new_items, age_secs, code, message) = match &outcome.status {
                    Status::Updated { new_items } => {
                        ("updated", Some(*new_items), None, None, None)
                    }
                    Status::NotModified => ("not_modified", None, None, None, None),
                    Status::Skipped { age_secs } => ("skipped", None, Some(*age_secs), None, None),
                    Status::Failed { code, message } => {
                        ("failed", None, None, Some(*code), Some(message.as_str()))
                    }
                };
                json!({
                    "url": outcome.url,
                    "status": status,
                    "elapsed_ms": outcome.elapsed_ms,
                    "new_items": new_items,
                    "age_secs": age_secs,
                    "code": code,
                    "message": message,
                })
            })
            .collect();
        print(&json!({
            "feeds": feeds,
            "new_items": summary.new_items,
            "failed": summary.failed,
            "extracted": extracted.extracted,
            "extract_failed": extracted.failed,
        }));
    } else {
        for outcome in &summary.outcomes {
            if let Status::Failed { code, message } = &outcome.status {
                eprintln!("{}: {code} {message}", outcome.url);
            }
        }
        let skipped = summary
            .outcomes
            .iter()
            .filter(|outcome| matches!(outcome.status, Status::Skipped { .. }))
            .count();
        println!(
            "{} feeds, {} new items, {} failed, {skipped} skipped",
            summary.outcomes.len(),
            summary.new_items,
            summary.failed
        );
        if with_extract {
            println!(
                "{} articles scraped, {} failed",
                extracted.extracted, extracted.failed
            );
        }
    }

    Ok(match summary.failed {
        0 => ExitCode::SUCCESS,
        n if n == summary.outcomes.len() => ExitCode::from(4),
        _ => ExitCode::from(3),
    })
}

fn feeds(store: &Store, as_json: bool) -> Result<ExitCode> {
    let feeds = store.feeds()?;
    if as_json {
        let rows: Vec<_> = feeds
            .iter()
            .map(|feed| {
                json!({
                    "id": feed.id,
                    "url": feed.url,
                    "title": feed.title,
                    "folder": feed.folder,
                })
            })
            .collect();
        print(&json!({ "count": rows.len(), "feeds": rows }));
    } else {
        let mut current: Option<&str> = None;
        for feed in &feeds {
            let folder = feed.folder.as_deref().unwrap_or("(no folder)");
            if current != Some(folder) {
                if current.is_some() {
                    println!();
                }
                println!("{folder}");
                current = Some(folder);
            }
            println!(
                "  {:>3}  {:<28}  {}",
                feed.id,
                truncate(feed.title.as_deref().unwrap_or("(untitled)"), 28),
                feed.url
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}
