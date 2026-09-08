use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rssr_core::refresh::{self, Options, Status};
use rssr_core::{Fetcher, Result, Store, opml};
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
    },
    /// List subscribed feeds.
    Feeds,
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
        Command::Refresh { workers } => {
            do_refresh(&mut store, Options { workers: *workers }, cli.json)
        }
        Command::Feeds => feeds(&store, cli.json),
    }
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

fn do_refresh(store: &mut Store, options: Options, as_json: bool) -> Result<ExitCode> {
    let summary = refresh::refresh(store, &Fetcher::new(), options)?;

    if as_json {
        let feeds: Vec<_> = summary
            .outcomes
            .iter()
            .map(|outcome| match &outcome.status {
                Status::Updated { new_items } => {
                    json!({ "url": outcome.url, "status": "updated", "new_items": new_items })
                }
                Status::NotModified => {
                    json!({ "url": outcome.url, "status": "not_modified" })
                }
                Status::Failed { code, message } => {
                    json!({ "url": outcome.url, "status": "failed", "code": code, "message": message })
                }
            })
            .collect();
        print(&json!({
            "feeds": feeds,
            "new_items": summary.new_items,
            "failed": summary.failed,
        }));
    } else {
        for outcome in &summary.outcomes {
            if let Status::Failed { code, message } = &outcome.status {
                eprintln!("{}: {code} {message}", outcome.url);
            }
        }
        println!(
            "{} feeds, {} new items, {} failed",
            summary.outcomes.len(),
            summary.new_items,
            summary.failed
        );
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
        for feed in &feeds {
            println!(
                "{:<4} {:<12} {:<32} {}",
                feed.id,
                feed.folder.as_deref().unwrap_or("-"),
                feed.title.as_deref().unwrap_or("-"),
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
