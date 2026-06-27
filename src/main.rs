use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{env, process};

use clap::Parser;
use futures::future;
use models::{BASE_URL, Cli, DownloadConfig, SkipConfig};
use reqwest::ClientBuilder;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, UPGRADE_INSECURE_REQUESTS};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::signal;

use crate::models::Availability;

mod bypass;
mod browser;
mod browser_cookies;
mod browser_pool;
mod downloaders;
mod models;
mod requests;
mod text_utils;
mod workers;

const CAPTCHA_PROMPT: &str = concat!(
    "lucida requires you to complete a captcha!\n\n",
    "1. Open a new tab in your browser\n",
    "2. Open DevTools using F12 or Ctrl+Shift+I\n",
    "3. Select the Network tab\n",
    "4. Go to https://lucida.to/\n",
    "5. Complete the captcha\n",
    "6. Select one of the requests to lucida.to\n",
    "7. In the Request Headers section locate the Cookie and User-Agent headers\n",
    "8. Run the command again with two more arguments:\n",
    "  - set --cf-clearance to the value of the cf_clearance cookie from the Cookie header\n",
    "  - set --user-agent argument to the value of the User-Agent header; make sure to quote it!"
);

#[tokio::main(flavor = "current_thread")]
#[allow(clippy::too_many_lines)]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let mut urls = cli.urls;

    for file in cli.file {
        let mut lines = BufReader::new(File::open(file).await.unwrap()).lines();

        while let Some(line) = lines.next_line().await.unwrap() {
            if !line.trim().is_empty() {
                urls.push(line);
            }
        }
    }

    urls.reverse();

    if urls.is_empty() {
        eprintln!("no URLs to download");
        return ExitCode::FAILURE;
    }

    let mut cf_clearance = cli.cf_clearance.clone();
    let mut user_agent = cli.user_agent.clone();

    // When --headless is passed, harvest cookies from the user's Chromium
    // and build a reqwest client that carries them.
    let headless_client = if cli.headless {
        browser_pool::build_cookie_client()
    } else {
        None
    };

    if cli.auto_cookies && headless_client.is_none() {
        if cf_clearance.is_none() || user_agent.is_none() {
            match browser_cookies::auto_detect_cookies() {
                Some((auto_cookie, auto_ua)) => {
                    if cf_clearance.is_none() {
                        eprintln!("auto-detected cf_clearance cookie");
                        cf_clearance = Some(auto_cookie);
                    }
                    if user_agent.is_none() {
                        eprintln!("auto-detected User-Agent: {auto_ua}");
                        user_agent = Some(auto_ua);
                    }
                }
                None => {
                    eprintln!(
                        "failed to auto-detect cookies. Make sure you have visited https://lucida.to/ in Chromium recently and completed any Cloudflare challenge."
                    );

                    if cf_clearance.is_none() && user_agent.is_none() {
                        return ExitCode::FAILURE;
                    }
                }
            }
        }
    }

    let shared_client: browser_pool::SharedClient = if let Some(client) = headless_client {
        eprintln!("[browser] using headless browser cookies for all requests");
        browser_pool::SharedClient::new(client)
    } else {
        let mut client = ClientBuilder::new();
        let user_agent_ref = user_agent.as_deref();

        if let Some(user_agent) = user_agent_ref {
            client = client.user_agent(user_agent);
        }

        let mut headers = HeaderMap::new();

        if let Some(cf_clearance) = &cf_clearance {
            headers.insert(
                COOKIE,
                format!("cf_clearance={cf_clearance}").try_into().unwrap(),
            );

            // Add browser-like headers to help Cloudflare accept the clearance cookie.
            headers.insert(
                ACCEPT,
                HeaderValue::from_static(
                    "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7",
                ),
            );
            headers.insert(
                ACCEPT_LANGUAGE,
                HeaderValue::from_static("en-US,en;q=0.9"),
            );
            headers.insert(
                UPGRADE_INSECURE_REQUESTS,
                HeaderValue::from_static("1"),
            );
            headers.insert("referer", HeaderValue::from_static("https://lucida.to/"));

            if user_agent.as_deref().is_some_and(|ua| {
                ua.contains("Chrome") || ua.contains("Chromium")
            }) {
                headers.insert("sec-ch-ua", HeaderValue::from_static("\"Chromium\";v=\"149\", \"Not)A;Brand\";v=\"24\""));
                headers.insert("sec-ch-ua-mobile", HeaderValue::from_static("?0"));
                headers.insert("sec-ch-ua-platform", HeaderValue::from_static("\"Linux\""));
                headers.insert("sec-fetch-dest", HeaderValue::from_static("document"));
                headers.insert("sec-fetch-mode", HeaderValue::from_static("navigate"));
                headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
                headers.insert("sec-fetch-user", HeaderValue::from_static("?1"));
            }
        }

        if !headers.is_empty() {
            client = client.default_headers(headers);
        }

        let client = client.cookie_store(true).build().unwrap();
        browser_pool::SharedClient::new(client)
    };

    // Expand artist page URLs into individual album URLs.
    let mut expanded_urls = Vec::new();
    let mut had_artist_pages = false;
    for url in &urls {
        if url.contains("interpreter") || url.contains("/artist/") {
            had_artist_pages = true;
            eprintln!("detected artist page, extracting album URLs...");

            // Artist pages are SPAs that need JavaScript to render the album
            // list, so we must fetch them with a real browser.
            let albums = if cli.headless {
                match tokio::task::spawn_blocking({
                    let url = url.clone();
                    move || {
                        eprintln!("[extract] fetching artist page with headless browser...");
                        match browser_pool::fetch_page_with_browser(&url) {
                            Some(html) => {
                                eprintln!("[extract] got HTML ({} bytes)", html.len());
                                let _ = std::fs::write("/tmp/debug_artist_page.html", &html);
                                let albums = extract_albums_from_html(&html);
                                eprintln!("[extract] found {} albums", albums.len());
                                albums
                            }
                            None => {
                                eprintln!("[extract] headless browser returned None");
                                Vec::new()
                            }
                        }
                    }
                }).await {
                    Ok(albums) => albums,
                    Err(err) => {
                        eprintln!("[extract] spawn_blocking error: {err}");
                        Vec::new()
                    }
                }
            } else {
                requests::extract_albums_from_artist_page(&shared_client.get(), url).await
            };

            eprintln!("found {} albums on artist page", albums.len());
            // Preserve LIFO order by prepending albums in reverse.
            for album in albums.iter().rev() {
                expanded_urls.push(album.clone());
            }
        } else {
            expanded_urls.push(url.clone());
        }
    }
    urls = expanded_urls;

    let urls_len = urls.len();

    if had_artist_pages {
        eprintln!("expanded to {urls_len} total albums");
    }

    let availability = requests::check_availability(&shared_client.get()).await;

    match availability {
        Availability::Available => (),
        Availability::Captcha => {
            if cf_clearance.is_some() && user_agent.is_some() {
                eprintln!(
                    "Your cf_clearance cookie and User-Agent header weren't accepted. They might be stale"
                );
            } else {
                eprintln!("{CAPTCHA_PROMPT}");
            }

            return ExitCode::FAILURE;
        }
        Availability::Unavailable => {
            eprintln!("lucida seems to be unavailable right now. Visit the website: {BASE_URL}");
            return ExitCode::FAILURE;
        }
    }

    eprintln!("downloading {urls_len} albums");

    let urls = Arc::new(Mutex::new(urls));
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    let worker_count = cli.album_workers.min(urls_len);

    eprintln!("spawning {worker_count} album workers");

    tokio::spawn(async move {
        signal::ctrl_c().await.unwrap();
        running_clone.store(false, Ordering::Relaxed);
        eprintln!("Stopping gracefully");
        signal::ctrl_c().await.unwrap();
        process::exit(1);
    });

    let output = cli.output.unwrap_or_else(|| env::current_dir().unwrap());
    let format_stats: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

    for result in future::join_all((1..=worker_count).map(|album_worker| {
        tokio::spawn(workers::run_album_worker(
            shared_client.clone(),
            urls.clone(),
            output.clone(),
            cli.force,
            cli.group_singles,
            cli.album_year,
            cli.flatten_directories,
            DownloadConfig {
                country: cli.country.clone(),
                metadata: !cli.no_metadata,
                private: cli.private,
                downscale: cli.downscale.clone(),
                format_stats: format_stats.clone(),
            },
            cli.track_workers,
            SkipConfig {
                tracks: cli.skip_tracks,
                cover: cli.skip_cover,
            },
            running.clone(),
            album_worker,
        ))
    }))
    .await
    {
        result.unwrap();
    }

    let stats = format_stats.lock().unwrap();
    let formats: Vec<_> = stats.iter().map(|(k, v)| (k.clone(), *v)).collect();
    drop(stats);

    if !formats.is_empty() {
        eprintln!("download summary:");
        let mut formats = formats;
        formats.sort_by_key(|(format, _)| format.clone());
        for (format, count) in formats {
            eprintln!("  {format}: {count}");
        }
    }

    eprintln!("finished!");
    ExitCode::SUCCESS
}

/// Extract album URLs from the HTML of a lucida.to artist page.
fn extract_albums_from_html(html: &str) -> Vec<String> {
    let mut albums = Vec::new();

    if !html.contains("/album/") {
        eprintln!("[extract] HTML does not contain '/album/'");
        return albums;
    }

    for line in html.lines() {
        let mut rest: &str = line;
        while let Some(start) = rest.find("href=\"") {
            rest = &rest[start + 6..];
            if let Some(end) = rest.find('"') {
                let href = &rest[..end];
                rest = &rest[end..];

                if href.contains("/album/") {
                    let full = if href.starts_with("/?url=") {
                        format!("https://lucida.to{href}")
                    } else if href.starts_with("https://www.qobuz.com/") {
                        href.to_string()
                    } else {
                        continue;
                    };
                    if !albums.contains(&full) {
                        albums.push(full);
                    }
                }
            } else {
                break;
            }
        }
    }

    albums
}
