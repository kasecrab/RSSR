use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use rssr_core::opml::Subscription;
use rssr_core::refresh::{self, Options, Status};
use rssr_core::store::{Flag, Item, Query, Stats, Upsert};
use rssr_core::{Fetcher, Result, Store, content, duration, extract, fetch, opml};
use serde_json::{Value, json};

/// Exit codes, used the same way by every subcommand:
///   0 done · 1 nothing by that name · 2 bad usage · 3 partly failed · 4 all failed
const NOT_FOUND: u8 = 1;
const USAGE: u8 = 2;
const PARTIAL: u8 = 3;
const FAILED: u8 = 4;

#[derive(Parser)]
#[command(
    name = "rssr",
    version,
    about = "Read RSS and Atom feeds from the shell",
    after_help = "Exit codes: 0 done, 1 nothing by that name, 2 bad usage, 3 partly failed, 4 all failed."
)]
struct Cli {
    /// Database to use instead of the default location.
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,

    /// Print JSON instead of a table. This is the interface to script against.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Subscribe to a single feed and fetch it.
    Add {
        url: String,
        #[arg(long, value_name = "NAME")]
        folder: Option<String>,
        #[arg(long, value_name = "TEXT")]
        title: Option<String>,
        /// Subscribe even if the first fetch fails.
        #[arg(long)]
        force: bool,
    },
    /// Subscribe to every feed in an OPML file.
    Import {
        file: PathBuf,
        /// Let the file's titles and folders replace the ones already stored.
        #[arg(long)]
        update: bool,
    },
    /// Write every subscription out as OPML, for a backup or another reader.
    Export {
        /// Write here instead of standard output.
        file: Option<PathBuf>,
    },
    /// Fetch new items.
    Refresh {
        #[arg(long, value_name = "ID")]
        feed: Option<i64>,
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
    Feeds {
        /// Only the feeds whose last fetch failed, with the reason.
        #[arg(long)]
        failing: bool,
        #[arg(long, value_name = "NAME")]
        folder: Option<String>,
    },
    /// Change or remove one feed.
    Feed {
        id: i64,
        /// Scrape each item's page because this feed only publishes a teaser.
        #[arg(long, value_name = "on|off")]
        full_content: Option<Toggle>,
        #[arg(long, value_name = "TEXT")]
        rename: Option<String>,
        #[arg(long, value_name = "NAME")]
        folder: Option<String>,
        /// Unsubscribe and delete every item stored for it.
        #[arg(long)]
        remove: bool,
    },
    /// List items, newest first.
    List {
        /// Include items already read.
        #[arg(long)]
        all: bool,
        /// Only unread items. The default, except with --starred.
        #[arg(long)]
        unread: bool,
        /// Only starred items. Reading does not remove them from this queue.
        #[arg(long)]
        starred: bool,
        #[arg(long, value_name = "ID")]
        feed: Option<i64>,
        #[arg(long, value_name = "NAME")]
        folder: Option<String>,
        /// Only items this recent: a duration like 24h, or a date like 2026-09-08.
        #[arg(long, value_name = "WHEN")]
        since: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// At most this many items from any one feed.
        #[arg(long, value_name = "N")]
        per_feed: Option<usize>,
        /// Include this many characters of each item's text.
        #[arg(long, value_name = "CHARS")]
        snippet: Option<usize>,
        /// Collapse the same story arriving from several feeds into one row.
        #[arg(long)]
        dedupe: bool,
    },
    /// Print items as text. Reading never marks anything read.
    Read {
        #[arg(required = true)]
        ids: Vec<String>,
        /// Print whole bodies instead of an opening.
        #[arg(long)]
        full: bool,
        /// Rough ceiling on the tokens returned, shared across the items asked for.
        #[arg(long, value_name = "N")]
        max_tokens: Option<usize>,
    },
    /// Full-text search. Terms are AND-ed; "quoted phrases" and prefix* work.
    Search {
        text: String,
        #[arg(long)]
        all: bool,
        #[arg(long, value_name = "ID")]
        feed: Option<i64>,
        #[arg(long, value_name = "NAME")]
        folder: Option<String>,
        #[arg(long, value_name = "WHEN")]
        since: Option<String>,
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
    /// Delete items past a retention window. Off until one is set.
    Prune {
        /// Remember this window and apply it on every refresh, or "off" to stop.
        #[arg(long, value_name = "DURATION|off", conflicts_with = "older_than")]
        set: Option<String>,
        /// Prune to this window once, without changing the stored one.
        #[arg(long, value_name = "DURATION")]
        older_than: Option<String>,
        /// Count what would go without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Delete starred items too. They are kept by default.
        #[arg(long)]
        starred: bool,
    },
    /// Scrape the full article for items whose feed only sent a teaser.
    Extract {
        ids: Vec<String>,
        /// Instead of ids, catch up feeds with full content switched on.
        #[arg(long)]
        pending: bool,
        #[arg(long, value_name = "ID")]
        feed: Option<i64>,
        #[arg(long, default_value_t = 25)]
        limit: usize,
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
            if cli.json {
                print(&json!({ "error": e.to_string(), "code": e.code() }));
            }
            eprintln!("rssr: {e}");
            match e {
                rssr_core::Error::Usage(_) => ExitCode::from(USAGE),
                _ => ExitCode::from(1),
            }
        }
    }
}

fn run(cli: &Cli) -> Result<ExitCode> {
    let path = match &cli.db {
        Some(path) => path.clone(),
        None => Store::default_path()?,
    };
    let mut store = Store::open(&path)?;
    let json = cli.json;

    let Some(command) = &cli.command else {
        return status(&store, &path, json);
    };

    match command {
        Command::Add {
            url,
            folder,
            title,
            force,
        } => add(&mut store, url, folder, title, *force, json),
        Command::Import { file, update } => import(&store, file, *update, json),
        Command::Export { file } => export(&store, file.as_deref(), json),
        Command::Refresh {
            feed,
            workers,
            max_age,
            timeout,
            extract,
        } => {
            if let Some(id) = feed
                && !store.feed_exists(*id)?
            {
                return missing_feed(&store, *id, json);
            }
            let options = Options {
                workers: *workers,
                max_age: max_age.as_deref().map(duration::parse).transpose()?,
                only_feed: *feed,
            };
            let timeout = timeout
                .as_deref()
                .map(duration::parse)
                .transpose()?
                .unwrap_or(fetch::DEFAULT_TIMEOUT);
            do_refresh(&mut store, options, timeout, *extract, json)
        }
        Command::Feeds { failing, folder } => feeds(&store, *failing, folder.as_deref(), json),
        Command::Feed {
            id,
            full_content,
            rename,
            folder,
            remove,
        } => feed(&store, *id, *full_content, rename, folder, *remove, json),
        Command::List {
            all,
            unread,
            starred,
            feed,
            folder,
            since,
            limit,
            per_feed,
            snippet,
            dedupe,
        } => {
            let query = Query {
                // Starring is how an item is kept for later, so reading it
                // must not take it out of that queue.
                unread_only: if *starred { *unread } else { !all },
                starred_only: *starred,
                feed_id: *feed,
                folder: folder.clone(),
                since: since.as_deref().map(parse_since).transpose()?,
                limit: *limit,
                per_feed: *per_feed,
                snippet: *snippet,
                dedupe: *dedupe,
            };
            list(&store, query, json)
        }
        Command::Read {
            ids,
            full,
            max_tokens,
        } => read(&store, ids, *full, *max_tokens, json),
        Command::Search {
            text,
            all,
            feed,
            folder,
            since,
            limit,
        } => {
            let query = Query {
                unread_only: !all,
                feed_id: *feed,
                folder: folder.clone(),
                since: since.as_deref().map(parse_since).transpose()?,
                limit: *limit,
                ..Query::default()
            };
            search(&store, text, query, json)
        }
        Command::Mark { flag, ids } => mark(&store, ids, (*flag).into(), json),
        Command::Prune {
            set,
            older_than,
            dry_run,
            starred,
        } => prune(
            &store,
            set.as_deref(),
            older_than.as_deref(),
            *dry_run,
            *starred,
            json,
        ),
        Command::Extract {
            ids,
            pending,
            feed,
            limit,
        } => {
            if *pending {
                backfill(&mut store, *limit, *feed, json)
            } else if ids.is_empty() {
                eprintln!("rssr: give item ids, or --pending to catch up marked feeds");
                Ok(ExitCode::from(USAGE))
            } else {
                extract_items(&store, ids, json)
            }
        }
    }
}

/// What a bare `rssr` prints: where the database is, what is in it, and the
/// commands worth running next. An agent should not need a manual to start.
fn status(store: &Store, db: &Path, as_json: bool) -> Result<ExitCode> {
    let stats = store.stats()?;
    let retention = store.retention()?.map(duration::render);
    let recent = store.items(&Query {
        limit: 8,
        per_feed: Some(1),
        ..Query::default()
    })?;
    let next = next_steps(&stats);

    if as_json {
        print(&json!({
            "db": db.display().to_string(),
            "feeds": stats.feeds,
            "folders": stats.folders,
            "items": stats.items,
            "unread": stats.unread,
            "starred": stats.starred,
            "full_text": stats.full_text,
            "failing_feeds": stats.failing,
            "last_refresh": stats.last_refresh,
            "retention": retention,
            "recent": rows(&recent),
            "next": next,
        }));
    } else {
        println!("db: {}", db.display());
        match stats.feeds {
            0 => println!("feeds: none"),
            n => println!("feeds: {n} in {} folders", stats.folders),
        }
        println!("unread: {} of {} items", stats.unread, stats.items);
        if stats.starred > 0 || stats.full_text > 0 {
            println!(
                "starred: {}   full text: {}",
                stats.starred, stats.full_text
            );
        }
        if stats.failing > 0 {
            println!(
                "failing feeds: {} (`rssr feeds --failing` for why)",
                stats.failing
            );
        }
        match &stats.last_refresh {
            Some(at) => println!("last refresh: {at}"),
            None => println!("last refresh: never"),
        }
        if let Some(window) = &retention {
            println!("pruning: items older than {window}, starred kept");
        }
        if !recent.is_empty() {
            println!();
            show(&recent);
        }
        println!("\nnext:");
        for step in &next {
            println!("  {step}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn next_steps(stats: &Stats) -> Vec<&'static str> {
    if stats.feeds == 0 {
        vec![
            "rssr add <url> - subscribe to one feed",
            "rssr import <file.opml> - subscribe to every feed in an OPML export",
        ]
    } else if stats.items == 0 {
        vec!["rssr refresh - fetch items for the feeds already subscribed"]
    } else if stats.unread == 0 {
        vec![
            "rssr refresh - look for new items",
            "rssr list --all - items already read",
        ]
    } else {
        vec![
            "rssr list --since 24h --per-feed 2 --snippet 200 - today, one feed cannot flood it",
            "rssr read <id> - one item as text",
            "rssr search <text> - full-text search",
            "rssr mark read <id> - reading never marks anything read on its own",
        ]
    }
}

fn add(
    store: &mut Store,
    url: &str,
    folder: &Option<String>,
    title: &Option<String>,
    force: bool,
    as_json: bool,
) -> Result<ExitCode> {
    let subscription = Subscription {
        url: url.to_string(),
        title: title.clone(),
        folder: folder.clone(),
    };
    let (id, outcome) = store.upsert_feed(&subscription, false)?;

    let summary = refresh::refresh(
        store,
        &Fetcher::new(),
        Options {
            only_feed: Some(id),
            ..Options::default()
        },
    )?;
    let new_items = summary.new_items;
    let failure = summary
        .outcomes
        .iter()
        .find_map(|outcome| match &outcome.status {
            Status::Failed { code, message } => Some((*code, message.clone())),
            _ => None,
        });

    // A feed that cannot be fetched even once is how a dead subscription list
    // accumulates. Keep it only if the caller insists, or if it was already
    // subscribed and this was just a bad day.
    let rolled_back = failure.is_some() && outcome == Upsert::Added && !force;
    if rolled_back {
        store.remove_feed(id)?;
    }

    let stored = store.feeds()?.into_iter().find(|feed| feed.id == id);
    let feed_title = stored.and_then(|feed| feed.title);

    if as_json {
        print(&json!({
            "feed": (!rolled_back).then_some(id),
            "url": url,
            "title": feed_title,
            "added": outcome == Upsert::Added && !rolled_back,
            "kept": !rolled_back,
            "new_items": new_items,
            "error": failure.as_ref().map(|(_, message)| message),
            "code": failure.as_ref().map(|(code, _)| *code),
        }));
    } else {
        match (&outcome, &failure) {
            (Upsert::Added, None) => println!(
                "feed {id}: {} - {new_items} items",
                feed_title.as_deref().unwrap_or(url)
            ),
            (_, None) => println!("feed {id}: already subscribed - {new_items} new items"),
            (_, Some((code, message))) => {
                eprintln!("{url}: {code} {message}");
                if rolled_back {
                    eprintln!(
                        "not subscribed; pass --force to keep a feed that fails its first fetch"
                    );
                }
            }
        }
    }
    Ok(match failure {
        Some(_) => ExitCode::from(FAILED),
        None => ExitCode::SUCCESS,
    })
}

fn import(store: &Store, file: &PathBuf, update: bool, as_json: bool) -> Result<ExitCode> {
    let subscriptions = opml::parse(&std::fs::read(file)?)?;
    let mut added = Vec::new();
    let mut updated = Vec::new();
    let mut unchanged = 0;

    for subscription in &subscriptions {
        let (id, outcome) = store.upsert_feed(subscription, update)?;
        let row = json!({ "id": id, "url": subscription.url, "title": subscription.title });
        match outcome {
            Upsert::Added => added.push(row),
            Upsert::Updated => updated.push(row),
            Upsert::Unchanged => unchanged += 1,
        }
    }

    if as_json {
        print(&json!({
            "found": subscriptions.len(),
            "added": added.len(),
            "updated": updated.len(),
            "unchanged": unchanged,
            "added_feeds": added,
            "updated_feeds": updated,
        }));
    } else {
        println!(
            "{} feeds in file: {} new, {} updated, {unchanged} unchanged",
            subscriptions.len(),
            added.len(),
            updated.len()
        );
        if !update && unchanged > 0 {
            println!("(titles and folders already stored were kept; --update replaces them)");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The other half of `import`: everything subscribed, in a file another
/// reader can take. Without a path the OPML goes to standard output, so it can
/// be piped somewhere without landing on disk first.
fn export(store: &Store, file: Option<&Path>, as_json: bool) -> Result<ExitCode> {
    let feeds = store.feeds()?;
    let subscriptions: Vec<Subscription> = feeds
        .iter()
        .map(|feed| Subscription {
            url: feed.url.clone(),
            title: feed.title.clone(),
            folder: feed.folder.clone(),
        })
        .collect();
    let xml = opml::write(&subscriptions)?;
    let folders = store.folders()?.len();

    match file {
        Some(path) => {
            std::fs::write(path, &xml)?;
            if as_json {
                print(&json!({
                    "feeds": subscriptions.len(),
                    "folders": folders,
                    "path": path.display().to_string(),
                }));
            } else {
                println!(
                    "exported {} feeds to {}",
                    subscriptions.len(),
                    path.display()
                );
            }
        }
        None if as_json => print(&json!({
            "feeds": subscriptions.len(),
            "folders": folders,
            "path": Value::Null,
            "opml": xml,
        })),
        None => print!("{xml}"),
    }
    Ok(ExitCode::SUCCESS)
}

fn do_refresh(
    store: &mut Store,
    options: Options,
    timeout: Duration,
    with_extract: bool,
    as_json: bool,
) -> Result<ExitCode> {
    let fetcher = Fetcher::with_timeout(timeout);
    let summary = refresh::refresh(store, &fetcher, options)?;
    let extracted = if with_extract {
        refresh::extract_pending(store, &fetcher, 25, options.workers, options.only_feed)?
    } else {
        Default::default()
    };

    // A stored window is a standing instruction: the point of setting one is
    // not having to remember to prune. A refresh aimed at a single feed leaves
    // it alone, since deleting items from every other feed is not what was
    // asked for.
    let pruned = match (options.only_feed, store.retention()?) {
        (None, Some(window)) => store.prune(duration::ago(window)?, true)?,
        _ => 0,
    };

    let by_url: std::collections::HashMap<String, i64> = store
        .feeds()?
        .into_iter()
        .map(|feed| (feed.url, feed.id))
        .collect();

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
                    "feed": by_url.get(&outcome.url),
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
            "pruned": pruned,
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
        if pruned > 0 {
            println!("{pruned} items pruned");
        }
    }

    Ok(match summary.failed {
        0 => ExitCode::SUCCESS,
        n if n == summary.outcomes.len() => ExitCode::from(FAILED),
        _ => ExitCode::from(PARTIAL),
    })
}

fn feeds(
    store: &Store,
    failing_only: bool,
    folder: Option<&str>,
    as_json: bool,
) -> Result<ExitCode> {
    let mut feeds = store.feeds()?;
    if failing_only {
        feeds.retain(|feed| feed.status.as_deref() == Some("error"));
    }
    if let Some(folder) = folder {
        if !store.folders()?.iter().any(|known| known == folder) {
            return unknown_folder(store, folder, as_json);
        }
        feeds.retain(|feed| feed.folder.as_deref() == Some(folder));
    }

    if as_json {
        let rows: Vec<_> = feeds
            .iter()
            .map(|feed| {
                json!({
                    "id": feed.id,
                    "url": feed.url,
                    "title": feed.title,
                    "folder": feed.folder,
                    "unread": feed.unread,
                    "status": feed.status,
                    "error": feed.error,
                    "full_content": feed.full_content,
                })
            })
            .collect();
        print(&json!({ "count": rows.len(), "feeds": rows }));
    } else if feeds.is_empty() && failing_only {
        println!("no failing feeds");
    } else if feeds.is_empty() {
        println!("no feeds subscribed - run `rssr add <url>` or `rssr import <file.opml>`");
    } else {
        let mut current: Option<&str> = None;
        for feed in &feeds {
            let group = feed.folder.as_deref().unwrap_or("(no folder)");
            if current != Some(group) {
                if current.is_some() {
                    println!();
                }
                println!("{group}");
                current = Some(group);
            }
            println!(
                "  {:>3}  {:<28}  {:>5} unread  {:<12}  {}",
                feed.id,
                truncate(feed.title.as_deref().unwrap_or("(untitled)"), 28),
                feed.unread,
                feed.status.as_deref().unwrap_or("never fetched"),
                feed.url
            );
            if let Some(error) = &feed.error {
                println!("       {error}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn feed(
    store: &Store,
    id: i64,
    full_content: Option<Toggle>,
    rename: &Option<String>,
    folder: &Option<String>,
    remove: bool,
    as_json: bool,
) -> Result<ExitCode> {
    if !store.feed_exists(id)? {
        return missing_feed(store, id, as_json);
    }

    if remove {
        store.remove_feed(id)?;
        if as_json {
            print(&json!({ "feed": id, "removed": true }));
        } else {
            println!("feed {id} removed with all of its items");
        }
        return Ok(ExitCode::SUCCESS);
    }

    if full_content.is_none() && rename.is_none() && folder.is_none() {
        eprintln!("rssr: nothing to change; pass --full-content, --rename, --folder or --remove");
        return Ok(ExitCode::from(USAGE));
    }

    if let Some(toggle) = full_content {
        store.set_full_content(id, matches!(toggle, Toggle::On))?;
    }
    if rename.is_some() || folder.is_some() {
        let url = store
            .feeds()?
            .into_iter()
            .find(|feed| feed.id == id)
            .map(|feed| feed.url)
            .unwrap_or_default();
        store.upsert_feed(
            &Subscription {
                url,
                title: rename.clone(),
                folder: folder.clone(),
            },
            true,
        )?;
    }

    let updated = store.feeds()?.into_iter().find(|feed| feed.id == id);
    if as_json {
        print(&json!({
            "feed": id,
            "title": updated.as_ref().and_then(|feed| feed.title.clone()),
            "folder": updated.as_ref().and_then(|feed| feed.folder.clone()),
            "full_content": updated.as_ref().is_some_and(|feed| feed.full_content),
        }));
    } else if let Some(feed) = updated {
        println!(
            "feed {id}: {} in {} - full content {}",
            feed.title.as_deref().unwrap_or("(untitled)"),
            feed.folder.as_deref().unwrap_or("(no folder)"),
            if feed.full_content { "on" } else { "off" }
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn list(store: &Store, query: Query, as_json: bool) -> Result<ExitCode> {
    if let Some(id) = query.feed_id
        && !store.feed_exists(id)?
    {
        return missing_feed(store, id, as_json);
    }
    if let Some(folder) = &query.folder
        && !store.folders()?.iter().any(|known| known == folder)
    {
        return unknown_folder(store, folder, as_json);
    }

    let items = store.items(&query)?;
    let total = store.count(&query)?;

    if as_json {
        print(&json!({ "count": items.len(), "total": total, "items": rows(&items) }));
    } else if items.is_empty() {
        println!("no items match. {}", why_empty(store, &query)?);
    } else {
        show(&items);
        println!("{} of {total}", items.len());
    }
    Ok(ExitCode::SUCCESS)
}

/// An empty list has several causes and an agent cannot guess which.
fn why_empty(store: &Store, query: &Query) -> Result<String> {
    let stats = store.stats()?;
    Ok(if stats.feeds == 0 {
        "no feeds subscribed - run `rssr add <url>`".into()
    } else if stats.items == 0 && stats.last_refresh.is_some() {
        "nothing stored - the feeds carried nothing, or a prune took it".into()
    } else if stats.items == 0 {
        "feeds are subscribed but never fetched - run `rssr refresh`".into()
    } else if query.starred_only {
        "nothing starred - `rssr mark star <id>` adds to that queue".into()
    } else if query.since.is_some() {
        "nothing that recent - widen --since".into()
    } else if query.unread_only && stats.unread == 0 {
        format!(
            "all {} items are read - `rssr list --all` shows them",
            stats.items
        )
    } else if query.feed_id.is_some() || query.folder.is_some() {
        format!("the filter matched none of {} items", stats.items)
    } else {
        format!("{} items stored, none unread", stats.items)
    })
}

fn read(
    store: &Store,
    ids: &[String],
    full: bool,
    max_tokens: Option<usize>,
    as_json: bool,
) -> Result<ExitCode> {
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for id in ids {
        match store.body(id)? {
            Some(body) => found.push(body),
            None => missing.push(id.clone()),
        }
    }

    let bodies: Vec<String> = found
        .iter()
        .map(|body| content::to_markdown(&body.content))
        .collect();
    let lengths: Vec<usize> = bodies.iter().map(|text| text.chars().count()).collect();
    let limits = match (full, max_tokens) {
        (true, None) => vec![usize::MAX; bodies.len()],
        (_, Some(budget)) => allocate(&lengths, budget * 4),
        (false, None) => vec![PREVIEW_CHARS; bodies.len()],
    };

    if as_json {
        let items: Vec<_> = found
            .iter()
            .zip(bodies.iter().zip(&limits))
            .map(|(body, (markdown, &limit))| {
                let (text, truncated) = clip(markdown, limit);
                json!({
                    "id": body.item_id,
                    "feed": body.feed_title,
                    "feed_id": body.feed_id,
                    "title": body.title,
                    "url": body.url,
                    "author": body.author,
                    "published": body.published,
                    "source": body.source,
                    "read": body.read,
                    "starred": body.starred,
                    "tokens_estimate": content::estimate_tokens(&text),
                    "truncated": truncated,
                    "content": text,
                })
            })
            .collect();
        print(&json!({ "count": items.len(), "items": items, "missing": missing }));
    } else {
        for (body, (markdown, &limit)) in found.iter().zip(bodies.iter().zip(&limits)) {
            println!("{}", body.title.as_deref().unwrap_or("(untitled)"));
            if let Some(url) = &body.url {
                println!("{url}");
            }
            let (text, truncated) = clip(markdown, limit);
            println!("\n{text}");
            if truncated {
                println!("\n[truncated; --full or --max-tokens N for more]");
            }
            println!();
        }
        for id in &missing {
            eprintln!("no such item: {id}");
        }
    }

    Ok(if missing.is_empty() {
        ExitCode::SUCCESS
    } else if found.is_empty() {
        ExitCode::from(NOT_FOUND)
    } else {
        ExitCode::from(PARTIAL)
    })
}

/// Shares a character budget across items so short ones are served whole and
/// what they leave unspent goes to the long ones, instead of every item being
/// cut to the same flat share.
fn allocate(lengths: &[usize], budget: usize) -> Vec<usize> {
    let mut limits = vec![0usize; lengths.len()];
    let mut pending: Vec<usize> = (0..lengths.len()).collect();
    let mut left = budget;

    while !pending.is_empty() {
        let share = left / pending.len();
        let fits: Vec<usize> = pending
            .iter()
            .copied()
            .filter(|&index| lengths[index] <= share)
            .collect();
        if fits.is_empty() {
            for &index in &pending {
                limits[index] = share;
            }
            break;
        }
        for &index in &fits {
            limits[index] = lengths[index];
            left -= lengths[index];
        }
        pending.retain(|index| !fits.contains(index));
    }
    limits
}

fn search(store: &Store, text: &str, query: Query, as_json: bool) -> Result<ExitCode> {
    if let Some(id) = query.feed_id
        && !store.feed_exists(id)?
    {
        return missing_feed(store, id, as_json);
    }
    if let Some(folder) = &query.folder
        && !store.folders()?.iter().any(|known| known == folder)
    {
        return unknown_folder(store, folder, as_json);
    }

    let items = store.search(text, &query)?;
    if as_json {
        print(&json!({ "count": items.len(), "query": text, "items": rows(&items) }));
    } else if items.is_empty() {
        let stats = store.stats()?;
        println!("no matches for {text:?} in {} items", stats.items);
    } else {
        show(&items);
        println!("{} matches", items.len());
    }
    Ok(ExitCode::SUCCESS)
}

fn mark(store: &Store, ids: &[String], flag: Flag, as_json: bool) -> Result<ExitCode> {
    let mut missing = Vec::new();
    for id in ids {
        if store.item_url(id)?.is_none() && store.body(id)?.is_none() {
            missing.push(id.clone());
        }
    }
    let changed = store.set_flag(ids, flag)?;

    if as_json {
        print(&json!({
            "matched": changed,
            "requested": ids.len(),
            "missing": missing,
        }));
    } else {
        println!("{changed} of {} items updated", ids.len());
        for id in &missing {
            eprintln!("no such item: {id}");
        }
    }
    Ok(if changed == ids.len() {
        ExitCode::SUCCESS
    } else if changed == 0 {
        ExitCode::from(NOT_FOUND)
    } else {
        ExitCode::from(PARTIAL)
    })
}

fn extract_items(store: &Store, ids: &[String], as_json: bool) -> Result<ExitCode> {
    let fetcher = Fetcher::new();
    let mut results = Vec::new();
    let mut failed = 0;
    let mut missing = Vec::new();

    for id in ids {
        let Some(url) = store.item_url(id)? else {
            missing.push(id.clone());
            results
                .push(json!({ "id": id, "ok": false, "error": "no such item, or it has no link" }));
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
        print(&json!({
            "count": results.len(),
            "failed": failed,
            "missing": missing,
            "items": results,
        }));
    } else {
        for result in &results {
            match result["ok"].as_bool() {
                Some(true) => println!("{} {} chars", result["id"], result["chars"]),
                _ => eprintln!("{}: {}", result["id"], result["error"]),
            }
        }
    }

    // Nothing was fetched for an id that does not exist, so that is a lookup
    // miss rather than an extraction failure.
    let scraped = ids.len() - failed - missing.len();
    Ok(if scraped == ids.len() {
        ExitCode::SUCCESS
    } else if scraped > 0 {
        ExitCode::from(PARTIAL)
    } else if failed == 0 {
        ExitCode::from(NOT_FOUND)
    } else {
        ExitCode::from(FAILED)
    })
}

fn backfill(store: &mut Store, limit: usize, feed: Option<i64>, as_json: bool) -> Result<ExitCode> {
    if let Some(id) = feed
        && !store.feed_exists(id)?
    {
        return missing_feed(store, id, as_json);
    }
    let fetcher = Fetcher::new();
    let summary = refresh::extract_pending(store, &fetcher, limit, 16, feed)?;
    if as_json {
        print(&json!({ "extracted": summary.extracted, "failed": summary.failed }));
    } else if summary.extracted == 0 && summary.failed == 0 {
        println!("nothing to scrape - switch a feed on with `rssr feed <id> --full-content on`");
    } else {
        println!("{} extracted, {} failed", summary.extracted, summary.failed);
    }
    Ok(if summary.failed > 0 && summary.extracted == 0 {
        ExitCode::from(FAILED)
    } else {
        ExitCode::SUCCESS
    })
}

/// Deleting is the one thing a reader does that cannot be undone, so it stays
/// off until a window is asked for, starred items are spared, and `--dry-run`
/// says what would go before anything does.
fn prune(
    store: &Store,
    set: Option<&str>,
    older_than: Option<&str>,
    dry_run: bool,
    starred: bool,
    as_json: bool,
) -> Result<ExitCode> {
    if let Some(value) = set {
        let window = match value.trim().to_ascii_lowercase().as_str() {
            "off" | "never" | "none" => None,
            _ => Some(retention_window(value)?),
        };
        store.set_retention(window)?;
        if as_json {
            print(&json!({
                "retention": window.map(duration::render),
                "retention_secs": window.map(|window| window.as_secs()),
            }));
        } else if let Some(window) = window {
            println!(
                "pruning on: items older than {} go on every refresh, starred ones stay",
                duration::render(window)
            );
        } else {
            println!("pruning off: nothing is deleted");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let window = match older_than {
        Some(value) => Some(retention_window(value)?),
        None => store.retention()?,
    };
    let Some(window) = window else {
        if as_json {
            print(&json!({ "error": "pruning is off", "code": "USAGE_ERROR" }));
        }
        eprintln!(
            "rssr: pruning is off. `rssr prune --set 15d` turns it on for good, \
             `rssr prune --older-than 15d` does it once"
        );
        return Ok(ExitCode::from(USAGE));
    };

    let before = duration::ago(window)?;
    let keep_starred = !starred;
    let matched = store.prunable(before, keep_starred)?;
    let deleted = if dry_run {
        0
    } else {
        store.prune(before, keep_starred)?
    };
    // What is old but starred: the one number that explains the gap between
    // how much is past the window and how much of it actually goes.
    let kept_starred = if keep_starred {
        store.prunable(before, false)? - if dry_run { matched } else { 0 }
    } else {
        0
    };

    let window = duration::render(window);
    if as_json {
        print(&json!({
            "dry_run": dry_run,
            "window": window,
            "matched": matched,
            "deleted": deleted,
            "kept_starred": kept_starred,
        }));
    } else {
        let starred_note = match kept_starred {
            0 => String::new(),
            n => format!("; {n} starred kept"),
        };
        match (matched, dry_run) {
            (0, _) => println!("nothing older than {window}{starred_note}"),
            (n, true) => println!("{n} items older than {window} would go{starred_note}"),
            (n, false) => println!("{n} items older than {window} deleted{starred_note}"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// A window of zero is always a typo, and acting on it would empty the
/// database. Nothing else about a duration is this unforgiving.
fn retention_window(value: &str) -> Result<Duration> {
    let window = duration::parse(value)?;
    if window.is_zero() {
        return Err(rssr_core::Error::Usage(format!(
            "a window of {value:?} would delete everything; give a duration like 15d"
        )));
    }
    Ok(window)
}

fn missing_feed(store: &Store, id: i64, as_json: bool) -> Result<ExitCode> {
    let feeds = store.feeds()?;
    if as_json {
        let known: Vec<_> = feeds
            .iter()
            .map(|feed| json!({ "id": feed.id, "title": feed.title, "url": feed.url }))
            .collect();
        print(&json!({
            "error": format!("no feed with id {id}"),
            "feed_count": known.len(),
            "known_feeds": known,
        }));
    } else {
        eprintln!(
            "no feed with id {id}. `rssr feeds` lists the {} there are",
            feeds.len()
        );
    }
    Ok(ExitCode::from(NOT_FOUND))
}

fn unknown_folder(store: &Store, folder: &str, as_json: bool) -> Result<ExitCode> {
    let known = store.folders()?;
    if as_json {
        print(&json!({ "error": format!("no folder named {folder:?}"), "known_folders": known }));
    } else {
        eprintln!("no folder named {folder:?}. known folders:");
        for name in &known {
            eprintln!("  {name}");
        }
    }
    Ok(ExitCode::from(NOT_FOUND))
}

const PREVIEW_CHARS: usize = 4000;

fn rows(items: &[Item]) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            json!({
                "id": item.id,
                "feed": item.feed_title,
                "feed_id": item.feed_id,
                "title": item.title,
                "url": item.url,
                "author": item.author,
                "published": item.published,
                "read": item.read,
                "starred": item.starred,
                "snippet": item.snippet,
                "score": item.score,
                "duplicates": item.duplicates,
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
        if let Some(snippet) = &item.snippet {
            println!("      {snippet}");
        }
    }
}

fn clip(text: &str, chars: usize) -> (String, bool) {
    match text.char_indices().nth(chars) {
        Some((cut, _)) => (text[..cut].to_string(), true),
        None => (text.to_string(), false),
    }
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

/// `--since` takes either a duration back from now, or a calendar date.
fn parse_since(value: &str) -> Result<DateTime<Utc>> {
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        let midnight = date.and_hms_opt(0, 0, 0).unwrap();
        return Ok(Utc.from_utc_datetime(&midnight));
    }
    if let Ok(at) = DateTime::parse_from_rfc3339(value) {
        return Ok(at.with_timezone(&Utc));
    }
    let ago = duration::parse(value).map_err(|_| {
        rssr_core::Error::Usage(format!(
            "cannot read {value:?} as a time; use 24h, 7d, or 2026-09-08"
        ))
    })?;
    Ok(Utc::now() - chrono::Duration::from_std(ago).unwrap_or_default())
}

fn print(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_items_are_served_whole_and_leave_the_rest_to_the_long_ones() {
        let limits = allocate(&[100, 100, 8000, 8000], 4000);
        assert_eq!(limits[0], 100);
        assert_eq!(limits[1], 100);
        assert_eq!(limits[2], 1900);
        assert_eq!(limits[3], 1900);
        assert_eq!(limits.iter().sum::<usize>(), 4000);
    }

    #[test]
    fn a_budget_that_covers_everything_truncates_nothing() {
        let limits = allocate(&[100, 200, 300], 4000);
        assert_eq!(limits, vec![100, 200, 300]);
    }

    #[test]
    fn items_all_too_long_split_the_budget_evenly() {
        assert_eq!(allocate(&[9000, 9000], 4000), vec![2000, 2000]);
    }
}
