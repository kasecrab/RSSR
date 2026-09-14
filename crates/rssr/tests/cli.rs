//! The command surface, exercised the way a caller meets it.
//!
//! These cover what unit tests cannot reach: that an argument arrives where it
//! was aimed, that the documented exit codes are the ones returned, and that
//! the JSON a script is told to rely on keeps its shape. One database and one
//! server per test, both thrown away afterwards.

mod support;

use serde_json::Value;
use support::{Post, Reader, Route, Site, article_page, page_advertising, rss, sample_posts};

/// A reader with one working feed already subscribed, which is where most of
/// these start.
fn subscribed() -> (Reader, Site) {
    let site = Site::new(|base| {
        vec![(
            "/feed.xml".into(),
            Route::feed(rss(base, "Test Feed", &sample_posts())),
        )]
    });
    let reader = Reader::new();
    reader.run(&["add", &site.url("/feed.xml")]).ok();
    (reader, site)
}

fn ids(reader: &Reader, args: &[&str]) -> Vec<String> {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    reader.run(&full).json()["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["id"].as_str().expect("id").to_string())
        .collect()
}

// ---------------------------------------------------------------- status ---

#[test]
fn a_bare_run_on_an_empty_database_says_what_to_do_next() {
    let reader = Reader::new();
    let run = reader.run(&[]);
    run.ok()
        .out("feeds: none")
        .out("unread: 0 of 0 items")
        .out("last refresh: never")
        .out("rssr add <url>")
        .out("rssr import <file.opml>");
    assert!(run.stdout.contains(&reader.db.display().to_string()));
}

#[test]
fn the_status_json_carries_the_whole_picture() {
    let reader = Reader::new();
    let status = reader.run(&["--json"]).json();
    for key in [
        "db",
        "feeds",
        "folders",
        "items",
        "unread",
        "starred",
        "full_text",
        "failing_feeds",
        "last_refresh",
        "retention",
        "recent",
        "next",
    ] {
        assert!(status.get(key).is_some(), "status json has no {key:?}");
    }
    assert_eq!(status["feeds"], 0);
    assert_eq!(status["last_refresh"], Value::Null);
    assert_eq!(status["retention"], Value::Null);
}

#[test]
fn a_stocked_database_reports_what_is_in_it() {
    let (reader, _site) = subscribed();
    reader
        .run(&[])
        .ok()
        .out("feeds: 1 in 0 folders")
        .out("unread: 3 of 3 items")
        .out("rssr read <id>");
}

// ------------------------------------------------------------------- add ---

#[test]
fn adding_a_feed_subscribes_to_it_and_fetches_it() {
    let (reader, site) = subscribed();
    reader.run(&["feeds"]).ok().out("Test Feed");
    let feed = &reader.run(&["--json", "feeds"]).json()["feeds"][0];
    assert_eq!(feed["url"], site.url("/feed.xml"));
    assert_eq!(feed["unread"], 3);
    assert_eq!(feed["status"], "ok");
}

#[test]
fn adding_the_same_feed_twice_changes_nothing() {
    let (reader, site) = subscribed();
    reader
        .run(&["add", &site.url("/feed.xml")])
        .ok()
        .out("already subscribed");
    assert_eq!(reader.run(&["--json", "feeds"]).json()["count"], 1);
}

#[test]
fn a_feed_that_fails_its_first_fetch_is_not_left_behind() {
    let site = Site::new(|_| vec![("/gone.xml".into(), Route::missing())]);
    let reader = Reader::new();
    reader
        .run(&["add", &site.url("/gone.xml")])
        .code(4)
        .err("FEED_NOT_FOUND")
        .err("not subscribed");
    assert_eq!(reader.run(&["--json", "feeds"]).json()["count"], 0);
}

#[test]
fn force_keeps_a_feed_that_fails_its_first_fetch() {
    let site = Site::new(|_| vec![("/gone.xml".into(), Route::missing())]);
    let reader = Reader::new();
    reader
        .run(&["add", &site.url("/gone.xml"), "--force"])
        .code(4);
    let feeds = reader.run(&["--json", "feeds"]).json();
    assert_eq!(feeds["count"], 1);
    assert_eq!(feeds["feeds"][0]["status"], "error");
}

#[test]
fn a_title_and_folder_given_on_the_command_line_are_kept() {
    let site = Site::new(|base| {
        vec![(
            "/feed.xml".into(),
            Route::feed(rss(base, "Published Name", &sample_posts())),
        )]
    });
    let reader = Reader::new();
    reader
        .run(&[
            "add",
            &site.url("/feed.xml"),
            "--title",
            "My Name",
            "--folder",
            "Tech",
        ])
        .ok();
    let feed = &reader.run(&["--json", "feeds"]).json()["feeds"][0];
    assert_eq!(feed["title"], "My Name");
    assert_eq!(feed["folder"], "Tech");
}

#[test]
fn the_add_json_says_what_happened() {
    let (reader, site) = subscribed();
    let again = reader
        .run(&["--json", "add", &site.url("/feed.xml")])
        .json();
    assert_eq!(again["added"], false);
    assert_eq!(again["kept"], true);
    assert_eq!(again["new_items"], 0);
    assert_eq!(again["error"], Value::Null);
    assert_eq!(again["discovered_from"], Value::Null);
}

// ------------------------------------------------------------- discovery ---

#[test]
fn a_page_advertising_one_feed_is_followed_to_it() {
    let site = Site::new(|base| {
        vec![
            (
                "/".into(),
                Route::page(page_advertising(&[
                    ("/feed.xml", "All posts"),
                    ("/comments.xml", "Comments Feed"),
                ])),
            ),
            (
                "/feed.xml".into(),
                Route::feed(rss(base, "Test Feed", &sample_posts())),
            ),
        ]
    });
    let reader = Reader::new();
    let run = reader.run(&["add", &site.url("/")]);
    run.ok().out("advertises").out("/feed.xml");

    let feeds = reader.run(&["--json", "feeds"]).json();
    assert_eq!(feeds["count"], 1);
    assert_eq!(feeds["feeds"][0]["url"], site.url("/feed.xml"));
    assert_eq!(feeds["feeds"][0]["title"], "Test Feed");
}

#[test]
fn a_page_advertising_several_feeds_asks_rather_than_guesses() {
    let site = Site::new(|_| {
        vec![(
            "/".into(),
            Route::page(page_advertising(&[
                ("/news.xml", "News"),
                ("/releases.xml", "Releases"),
            ])),
        )]
    });
    let reader = Reader::new();
    reader
        .run(&["add", &site.url("/")])
        .code(2)
        .err("advertises 2 feeds")
        .err("/news.xml")
        .err("/releases.xml");
    assert_eq!(reader.run(&["--json", "feeds"]).json()["count"], 0);
}

#[test]
fn the_ambiguous_json_lists_what_was_found() {
    let site = Site::new(|_| {
        vec![(
            "/".into(),
            Route::page(page_advertising(&[
                ("/news.xml", "News"),
                ("/comments.xml", "Comments"),
                ("/releases.xml", "Releases"),
            ])),
        )]
    });
    let reader = Reader::new();
    let run = reader.run(&["--json", "add", &site.url("/")]);
    run.code(2);
    let found = run.json();
    assert_eq!(found["code"], "FEED_AMBIGUOUS");
    assert_eq!(found["kept"], false);
    let candidates = found["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 3);
    // The comments feed is offered, but never first.
    assert_eq!(candidates[2]["secondary"], true);
}

#[test]
fn a_page_with_no_feed_on_it_fails_rather_than_guessing_an_address() {
    let site = Site::new(|_| {
        vec![(
            "/".into(),
            Route::page("<html><head><title>Nothing</title></head><body>Hi.</body></html>"),
        )]
    });
    let reader = Reader::new();
    reader
        .run(&["add", &site.url("/")])
        .code(4)
        .err("FEED_PARSE_FAILED");
    assert_eq!(reader.run(&["--json", "feeds"]).json()["count"], 0);
}

// -------------------------------------------------------- import, export ---

#[test]
fn a_subscription_list_survives_a_round_trip_through_opml() {
    let site = Site::new(|base| {
        vec![
            (
                "/a.xml".into(),
                Route::feed(rss(base, "Feed A", &sample_posts())),
            ),
            (
                "/b.xml".into(),
                Route::feed(rss(base, "Feed B", &sample_posts())),
            ),
        ]
    });
    let reader = Reader::new();
    reader
        .run(&["add", &site.url("/a.xml"), "--folder", "Tech"])
        .ok();
    reader.run(&["add", &site.url("/b.xml")]).ok();

    let file = reader.path("out.opml");
    reader
        .run(&["export", file.to_str().unwrap()])
        .ok()
        .out("exported 2 feeds");

    let second = reader.sibling("second.db");
    reader
        .run_on(&second, &["import", file.to_str().unwrap()])
        .ok()
        .out("2 new");

    let before = reader.run(&["--json", "feeds"]).json();
    let after = reader.run_on(&second, &["--json", "feeds"]).json();
    let names = |feeds: &Value| -> Vec<String> {
        feeds["feeds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|feed| format!("{}|{}|{}", feed["url"], feed["title"], feed["folder"]))
            .collect()
    };
    assert_eq!(names(&before), names(&after));
}

#[test]
fn export_without_a_path_writes_opml_to_standard_output() {
    let (reader, site) = subscribed();
    let run = reader.run(&["export"]);
    run.ok()
        .out("<opml version=\"2.0\">")
        .out(&site.url("/feed.xml"));

    let parsed = rssr_core::opml::parse(run.stdout.as_bytes()).expect("valid opml");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].url, site.url("/feed.xml"));
}

#[test]
fn the_export_json_carries_the_document_it_did_not_write() {
    let (reader, _site) = subscribed();
    let exported = reader.run(&["--json", "export"]).json();
    assert_eq!(exported["feeds"], 1);
    assert_eq!(exported["path"], Value::Null);
    assert!(
        exported["opml"]
            .as_str()
            .is_some_and(|xml| xml.contains("<opml")),
        "no opml in {exported}"
    );
}

#[test]
fn an_import_leaves_an_existing_label_alone_unless_told_otherwise() {
    let site = Site::new(|base| {
        vec![(
            "/a.xml".into(),
            Route::feed(rss(base, "Feed A", &sample_posts())),
        )]
    });
    let reader = Reader::new();
    reader
        .run(&[
            "add",
            &site.url("/a.xml"),
            "--title",
            "Mine",
            "--folder",
            "Mine",
        ])
        .ok();

    let file = reader.path("in.opml");
    std::fs::write(
        &file,
        format!(
            "<opml version=\"2.0\"><body><outline text=\"Theirs\" xmlUrl=\"{}\"/></body></opml>",
            site.url("/a.xml")
        ),
    )
    .unwrap();

    reader
        .run(&["import", file.to_str().unwrap()])
        .ok()
        .out("1 unchanged");
    assert_eq!(
        reader.run(&["--json", "feeds"]).json()["feeds"][0]["title"],
        "Mine"
    );

    reader
        .run(&["import", file.to_str().unwrap(), "--update"])
        .ok()
        .out("1 updated");
    assert_eq!(
        reader.run(&["--json", "feeds"]).json()["feeds"][0]["title"],
        "Theirs"
    );
}

#[test]
fn importing_a_file_that_is_not_opml_is_an_error_not_an_empty_list() {
    let reader = Reader::new();
    let junk = reader.path("junk.opml");
    std::fs::write(&junk, "this is not xml at all").unwrap();
    reader
        .run(&["import", junk.to_str().unwrap()])
        .code(2)
        .err("not a subscription list");

    let empty = reader.path("empty.opml");
    std::fs::write(&empty, "<opml version=\"2.0\"><body></body></opml>").unwrap();
    reader
        .run(&["import", empty.to_str().unwrap()])
        .ok()
        .out("0 feeds in file");

    reader
        .run(&["import", "/no/such/file.opml"])
        .code(1)
        .err("rssr:");
}

// --------------------------------------------------------------- refresh ---

#[test]
fn a_second_refresh_costs_nothing_when_the_feed_says_so() {
    let site = Site::new(|base| {
        vec![(
            "/feed.xml".into(),
            Route::feed(rss(base, "Test Feed", &sample_posts())).with_etag("\"v1\""),
        )]
    });
    let reader = Reader::new();
    reader.run(&["add", &site.url("/feed.xml")]).ok();

    let run = reader.run(&["--json", "refresh"]);
    run.ok();
    let refreshed = run.json();
    assert_eq!(refreshed["feeds"][0]["status"], "not_modified");
    assert_eq!(refreshed["new_items"], 0);
    assert_eq!(refreshed["failed"], 0);
    assert_eq!(site.hits("/feed.xml"), 2);
}

#[test]
fn a_recently_fetched_feed_is_skipped_without_a_request() {
    let (reader, site) = subscribed();
    let before = site.hits("/feed.xml");
    let run = reader.run(&["--json", "refresh", "--max-age", "1h"]);
    run.ok();
    assert_eq!(run.json()["feeds"][0]["status"], "skipped");
    assert_eq!(site.hits("/feed.xml"), before);
}

#[test]
fn refreshing_a_feed_that_is_not_there_says_which_ones_are() {
    let (reader, _site) = subscribed();
    reader
        .run(&["refresh", "--feed", "99"])
        .code(1)
        .err("no feed with id 99");
}

#[test]
fn a_duration_that_is_not_one_is_a_usage_error() {
    let (reader, _site) = subscribed();
    for args in [
        vec!["refresh", "--max-age", "soon"],
        vec!["refresh", "--timeout", "10w"],
        vec!["list", "--since", "whenever"],
    ] {
        reader.run(&args).code(2);
    }
}

#[test]
fn a_feed_that_breaks_is_reported_without_stopping_the_others() {
    let site = Site::new(|base| {
        vec![
            (
                "/good.xml".into(),
                Route::feed(rss(base, "Good", &sample_posts())),
            ),
            ("/bad.xml".into(), Route::broken()),
        ]
    });
    let reader = Reader::new();
    reader.run(&["add", &site.url("/good.xml")]).ok();
    reader
        .run(&["add", &site.url("/bad.xml"), "--force"])
        .code(4);

    let run = reader.run(&["refresh"]);
    run.code(3).out("2 feeds").err("FEED_FETCH_FAILED");
    reader
        .run(&["feeds", "--failing"])
        .ok()
        .out("/bad.xml")
        .no_out("/good.xml");
}

// ---------------------------------------------------------- feeds, feed ---

#[test]
fn feeds_are_grouped_by_folder_and_a_folder_that_is_not_there_is_named() {
    let (reader, _site) = subscribed();
    reader.run(&["feed", "1", "--folder", "Tech"]).ok();
    reader.run(&["feeds"]).ok().out("Tech");
    reader
        .run(&["feeds", "--folder", "Nope"])
        .code(1)
        .err("no folder named \"Nope\"")
        .err("Tech");
}

#[test]
fn a_feed_can_be_renamed_moved_and_switched_to_full_content() {
    let (reader, _site) = subscribed();
    reader
        .run(&[
            "feed",
            "1",
            "--rename",
            "New Name",
            "--folder",
            "Tech",
            "--full-content",
            "on",
        ])
        .ok()
        .out("New Name")
        .out("full content on");

    let feed = &reader.run(&["--json", "feeds"]).json()["feeds"][0];
    assert_eq!(feed["title"], "New Name");
    assert_eq!(feed["folder"], "Tech");
    assert_eq!(feed["full_content"], true);
}

#[test]
fn changing_a_feed_without_saying_what_to_change_is_a_usage_error() {
    let (reader, _site) = subscribed();
    reader.run(&["feed", "1"]).code(2).err("nothing to change");
    reader.run(&["feed", "99", "--rename", "x"]).code(1);
}

#[test]
fn removing_a_feed_takes_its_items_with_it() {
    let (reader, _site) = subscribed();
    reader.run(&["feed", "1", "--remove"]).ok().out("removed");
    reader.run(&["feeds"]).ok().out("no feeds subscribed");
    assert_eq!(reader.run(&["--json", "list", "--all"]).json()["count"], 0);
}

// ------------------------------------------------------------------ list ---

#[test]
fn listing_shows_unread_items_and_all_shows_the_rest() {
    let (reader, _site) = subscribed();
    let first = ids(&reader, &["list"])[0].clone();
    reader.run(&["mark", "read", &first]).ok();

    reader.run(&["list"]).ok().out("2 of 2");
    reader.run(&["list", "--all"]).ok().out("3 of 3");
}

#[test]
fn a_limit_narrows_the_page_without_hiding_the_total() {
    let (reader, _site) = subscribed();
    reader.run(&["list", "--limit", "1"]).ok().out("1 of 3");
    assert_eq!(
        reader.run(&["--json", "list", "--limit", "1"]).json()["total"],
        3
    );
}

#[test]
fn since_cuts_by_age() {
    let (reader, _site) = subscribed();
    assert_eq!(ids(&reader, &["list", "--since", "2d"]).len(), 1);
    assert_eq!(ids(&reader, &["list", "--since", "10d"]).len(), 2);
    assert_eq!(ids(&reader, &["list", "--since", "365d"]).len(), 3);
}

#[test]
fn a_starred_item_stays_in_its_queue_after_being_read() {
    let (reader, _site) = subscribed();
    let first = ids(&reader, &["list"])[0].clone();
    reader.run(&["mark", "star", &first]).ok();
    reader.run(&["mark", "read", &first]).ok();

    let starred = ids(&reader, &["list", "--starred"]);
    assert_eq!(starred, vec![first.clone()]);
    reader.run(&["list"]).ok().no_out(&first);
}

#[test]
fn a_snippet_comes_back_as_plain_text_with_the_row() {
    let (reader, _site) = subscribed();
    let listed = reader.run(&["--json", "list", "--snippet", "40"]).json();
    let snippet = listed["items"][0]["snippet"].as_str().expect("snippet");
    assert!(!snippet.is_empty());
    assert!(!snippet.contains('<'), "markup left in {snippet:?}");
}

#[test]
fn a_filter_that_matches_nothing_says_why() {
    let reader = Reader::new();
    reader.run(&["list"]).ok().out("no feeds subscribed");

    let (reader, _site) = subscribed();
    reader
        .run(&["list", "--starred"])
        .ok()
        .out("nothing starred");
    for id in ids(&reader, &["list"]) {
        reader.run(&["mark", "read", &id]).ok();
    }
    reader.run(&["list"]).ok().out("all 3 items are read");
}

#[test]
fn listing_a_feed_or_folder_that_is_not_there_is_a_miss_not_an_empty_page() {
    let (reader, _site) = subscribed();
    reader.run(&["list", "--feed", "99"]).code(1);
    reader.run(&["list", "--folder", "Nope"]).code(1);
    reader.run(&["search", "x", "--feed", "99"]).code(1);
}

#[test]
fn the_list_json_gives_every_row_the_same_shape() {
    let (reader, _site) = subscribed();
    let listed = reader.run(&["--json", "list"]).json();
    assert_eq!(listed["count"], 3);
    for item in listed["items"].as_array().unwrap() {
        for key in [
            "id",
            "feed",
            "feed_id",
            "title",
            "url",
            "published",
            "read",
            "starred",
        ] {
            assert!(item.get(key).is_some(), "row has no {key:?}: {item}");
        }
    }
}

// ------------------------------------------------------------------ read ---

#[test]
fn reading_an_item_prints_it_and_marks_nothing() {
    let (reader, _site) = subscribed();
    let first = ids(&reader, &["list"])[0].clone();
    reader
        .run(&["read", &first])
        .ok()
        .out("Newest headline")
        .out("newest item");
    reader.run(&["list"]).ok().out("3 of 3");
}

#[test]
fn a_token_budget_is_shared_and_says_when_it_ran_out() {
    let (reader, _site) = subscribed();
    let all = ids(&reader, &["list"]);
    let args: Vec<&str> = ["--json", "read", "--max-tokens", "4"]
        .into_iter()
        .chain(all.iter().map(String::as_str))
        .collect();
    let read = reader.run(&args).json();
    assert_eq!(read["count"], 3);
    assert!(
        read["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["truncated"] == true),
        "nothing was truncated by a four token budget"
    );
}

#[test]
fn reading_asks_about_items_that_are_not_there_without_losing_the_ones_that_are() {
    let (reader, _site) = subscribed();
    let first = ids(&reader, &["list"])[0].clone();
    reader.run(&["read", "deadbeefdeadbeef"]).code(1);
    let run = reader.run(&["read", &first, "deadbeefdeadbeef"]);
    run.code(3).out("Newest headline").err("no such item");
}

// ------------------------------------------------------------------ open ---

#[test]
fn print_gives_the_address_instead_of_opening_it() {
    let (reader, site) = subscribed();
    let first = ids(&reader, &["list"])[0].clone();
    reader
        .run(&["open", &first, "--print"])
        .ok()
        .out(&site.url("/posts/newest"));
}

#[test]
fn opening_a_feed_opens_the_site_it_belongs_to() {
    let (reader, site) = subscribed();
    let printed = reader.run(&["open", "--feed", "1", "--print"]);
    printed.ok().out(&site.base);
    reader.run(&["open", "--feed", "99", "--print"]).code(1);
}

#[test]
fn open_refuses_more_tabs_than_anyone_meant_to_ask_for() {
    let posts: Vec<Post> = (0..12)
        .map(|n| Post {
            id: Box::leak(format!("p{n}").into_boxed_str()),
            title: Box::leak(format!("Headline {n}").into_boxed_str()),
            at: chrono::Utc::now() - chrono::Duration::minutes(n),
            body: "A body long enough to be worth a line of its own.",
        })
        .collect();
    let site =
        Site::new(move |base| vec![("/feed.xml".into(), Route::feed(rss(base, "Many", &posts)))]);
    let reader = Reader::new();
    reader.run(&["add", &site.url("/feed.xml")]).ok();

    let all = ids(&reader, &["list"]);
    let mut args = vec!["open"];
    args.extend(all.iter().map(String::as_str));
    reader.run(&args).code(2).err("more than 10 browser tabs");

    args.push("--print");
    assert_eq!(reader.run(&args).ok().lines().len(), 12);
}

#[test]
fn open_with_nothing_to_open_says_so() {
    let (reader, _site) = subscribed();
    reader.run(&["open"]).code(2).err("give item ids");
    reader.run(&["open", "deadbeefdeadbeef", "--print"]).code(1);
}

#[test]
fn a_machine_with_no_browser_says_that_rather_than_failing_silently() {
    let (reader, _site) = subscribed();
    let first = ids(&reader, &["list"])[0].clone();
    let run = reader.run(&["--json", "open", &first]);
    run.code(4);
    assert_eq!(run.json()["items"][0]["code"], "NO_OPENER");
}

// ---------------------------------------------------------------- search ---

#[test]
fn search_finds_items_and_can_be_scoped() {
    let (reader, _site) = subscribed();
    reader
        .run(&["search", "turnips"])
        .ok()
        .out("Middle headline");
    reader
        .run(&["search", "turnips", "--feed", "1"])
        .ok()
        .out("Middle headline");
    reader
        .run(&["search", "aardvark"])
        .ok()
        .out("no matches for \"aardvark\"");
}

#[test]
fn a_search_result_carries_its_score_and_a_plain_text_snippet() {
    let (reader, _site) = subscribed();
    let found = reader.run(&["--json", "search", "headline"]).json();
    assert_eq!(found["query"], "headline");
    let first = &found["items"][0];
    assert!(first["score"].is_number(), "no score on {first}");
    let snippet = first["snippet"].as_str().unwrap_or_default();
    assert!(!snippet.contains('<'), "markup left in {snippet:?}");
}

// ------------------------------------------------------------------ mark ---

#[test]
fn every_flag_can_be_set_and_taken_off_again() {
    let (reader, _site) = subscribed();
    let first = ids(&reader, &["list"])[0].clone();

    reader.run(&["mark", "read", &first]).ok().out("1 of 1");
    assert_eq!(reader.run(&["--json", "list"]).json()["count"], 2);
    reader.run(&["mark", "unread", &first]).ok();
    assert_eq!(reader.run(&["--json", "list"]).json()["count"], 3);

    reader.run(&["mark", "star", &first]).ok();
    assert_eq!(ids(&reader, &["list", "--starred"]).len(), 1);
    reader.run(&["mark", "unstar", &first]).ok();
    assert_eq!(ids(&reader, &["list", "--starred"]).len(), 0);
}

#[test]
fn marking_reports_misses_by_name() {
    let (reader, _site) = subscribed();
    let first = ids(&reader, &["list"])[0].clone();
    reader
        .run(&["mark", "read", "deadbeefdeadbeef"])
        .code(1)
        .err("no such item: deadbeefdeadbeef");
    reader
        .run(&["mark", "read", &first, "deadbeefdeadbeef"])
        .code(3);
    reader.run(&["mark", "sideways", &first]).code(2);
}

// ----------------------------------------------------------------- prune ---

#[test]
fn pruning_is_off_until_it_is_asked_for() {
    let (reader, _site) = subscribed();
    reader.run(&["prune"]).code(2).err("pruning is off");
    assert_eq!(reader.run(&["--json"]).json()["retention"], Value::Null);
}

#[test]
fn a_stored_window_shows_in_the_status_and_runs_on_every_refresh() {
    let (reader, _site) = subscribed();
    reader
        .run(&["prune", "--set", "30d"])
        .ok()
        .out("items older than 30d");
    assert_eq!(reader.run(&["--json"]).json()["retention"], "30d");
    reader.run(&[]).ok().out("pruning: items older than 30d");

    let run = reader.run(&["--json", "refresh"]);
    run.ok();
    assert_eq!(run.json()["pruned"], 1);
    assert_eq!(reader.run(&["--json", "list", "--all"]).json()["count"], 2);

    reader
        .run(&["prune", "--set", "off"])
        .ok()
        .out("pruning off");
    assert_eq!(reader.run(&["--json"]).json()["retention"], Value::Null);
}

#[test]
fn a_dry_run_counts_what_a_real_one_would_take() {
    let (reader, _site) = subscribed();
    let run = reader.run(&["--json", "prune", "--older-than", "30d", "--dry-run"]);
    run.ok();
    let dry = run.json();
    assert_eq!(dry["matched"], 1);
    assert_eq!(dry["deleted"], 0);
    assert_eq!(reader.run(&["--json", "list", "--all"]).json()["count"], 3);

    reader
        .run(&["prune", "--older-than", "30d"])
        .ok()
        .out("1 items older than 30d deleted");
    assert_eq!(reader.run(&["--json", "list", "--all"]).json()["count"], 2);
}

#[test]
fn a_starred_item_outlives_the_window_that_would_have_taken_it() {
    let (reader, _site) = subscribed();
    let oldest = ids(&reader, &["list", "--since", "365d"])
        .last()
        .expect("an oldest item")
        .clone();
    reader.run(&["mark", "star", &oldest]).ok();

    reader
        .run(&["prune", "--older-than", "30d"])
        .ok()
        .out("nothing older than 30d")
        .out("1 starred kept");
    assert_eq!(reader.run(&["--json", "list", "--all"]).json()["count"], 3);

    reader
        .run(&["prune", "--older-than", "30d", "--starred"])
        .ok();
    assert_eq!(reader.run(&["--json", "list", "--all"]).json()["count"], 2);
}

#[test]
fn a_window_that_would_empty_the_database_is_refused() {
    let (reader, _site) = subscribed();
    reader
        .run(&["prune", "--older-than", "0"])
        .code(2)
        .err("would delete everything");
    reader.run(&["prune", "--set", "0s"]).code(2);
    // Configuring and pruning at once is two different requests.
    reader
        .run(&["prune", "--set", "15d", "--older-than", "5d"])
        .code(2);
    assert_eq!(reader.run(&["--json", "list", "--all"]).json()["count"], 3);
}

// --------------------------------------------------------------- extract ---

#[test]
fn an_item_can_be_scraped_from_the_page_behind_it() {
    let site = Site::new(|base| {
        vec![
            (
                "/feed.xml".into(),
                Route::feed(rss(base, "Teaser Feed", &sample_posts())),
            ),
            ("/posts/newest".into(), Route::page(article_page())),
        ]
    });
    let reader = Reader::new();
    reader.run(&["add", &site.url("/feed.xml")]).ok();
    let first = ids(&reader, &["list"])[0].clone();

    reader.run(&["extract", &first]).ok().out("chars");
    reader
        .run(&["read", &first, "--full"])
        .ok()
        .out("opening paragraph")
        .no_out("Copyright notice");

    let read = reader.run(&["--json", "read", &first]).json();
    assert_eq!(read["items"][0]["source"], "extracted");
}

#[test]
fn extract_needs_to_be_told_what_to_scrape() {
    let (reader, _site) = subscribed();
    reader
        .run(&["extract"])
        .code(2)
        .err("give item ids, or --pending");
    reader
        .run(&["extract", "--pending"])
        .ok()
        .out("nothing to scrape");
    reader
        .run(&["extract", "--pending", "--feed", "99"])
        .code(1);
    reader.run(&["extract", "deadbeefdeadbeef"]).code(1);
}

// ---------------------------------------------------------------- global ---

#[test]
fn the_binary_describes_itself_without_touching_a_database() {
    let reader = Reader::new();
    reader.bare(&["--version"]).ok().out("rssr");
    reader
        .bare(&["--help"])
        .ok()
        .out("Read RSS and Atom feeds from the shell")
        .out("Exit codes: 0 done");
    reader.bare(&["nonsense"]).code(2);
    assert!(!reader.db.exists(), "--help opened a database");
}

#[test]
fn every_subcommand_has_help_of_its_own() {
    let reader = Reader::new();
    for command in [
        "add", "import", "export", "refresh", "feeds", "feed", "list", "read", "open", "search",
        "mark", "prune", "extract",
    ] {
        reader.bare(&[command, "--help"]).ok().out("Usage:");
    }
}

#[test]
fn a_database_is_made_where_it_is_asked_for() {
    let reader = Reader::new();
    let nested = reader.sibling("deeper").join("further").join("rssr.db");
    reader.run_on(&nested, &[]).ok().out("feeds: none");
    assert!(nested.exists(), "no database at {}", nested.display());
}

#[test]
fn an_error_in_json_mode_is_still_json() {
    let (reader, _site) = subscribed();
    let run = reader.run(&["--json", "refresh", "--max-age", "soon"]);
    run.code(2);
    let error = run.json();
    assert_eq!(error["code"], "USAGE_ERROR");
    assert!(error["error"].is_string(), "no message in {error}");
}
