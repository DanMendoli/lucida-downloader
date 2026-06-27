use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, StatusCode, Url};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::time;

use crate::models::{
    Account, Availability, DownloadConfig, ResolveAlbumResult, Token, Track, TrackDownload,
    TrackDownloadRequest, TrackDownloadResult, TrackDownloadStatus, Upload, WorkerIds,
};

const IRRECOVERABLE_STATUS_CODES: [StatusCode; 2] =
    [StatusCode::NOT_FOUND, StatusCode::INTERNAL_SERVER_ERROR];

pub async fn check_availability(client: &Client) -> Availability {
    let response = client.get("https://lucida.to/").send().await.unwrap();
    let status = response.status();

    eprintln!("[check] status: {status}");

    match status {
        StatusCode::OK => {
            let html = response.text().await.unwrap();
            eprintln!("[check] html length: {}", html.len());

            if html.contains("challenge-platform")
                || html.contains("cf-chl-widget-")
                || html.contains("Checking your browser before accessing")
                || html.contains("__cf_chl_jschl_tk__")
            {
                eprintln!("[check] detected Cloudflare challenge in HTML");
                Availability::Captcha
            } else if html.contains("Welcome to the world of Lucida")
                || html.contains("download-form")
            {
                Availability::Available
            } else {
                eprintln!("[check] unknown HTML content: {}", &html[..html.len().min(200)]);
                Availability::Unavailable
            }
        }
        StatusCode::FORBIDDEN => Availability::Captcha,
        _ => {
            eprintln!("[check] non-OK status: {status}");
            Availability::Unavailable
        }
    }
}

pub async fn resolve_album(
    client: &Client,
    url: &str,
    country: &str,
    running: &Arc<AtomicBool>,
    album_worker: usize,
) -> ResolveAlbumResult {
    const MAX_RETRIES: usize = 5;

    for attempt in 1..=MAX_RETRIES {
        let response = client
            .get(
                Url::parse_with_params("https://lucida.to/", &[("url", url), ("country", country)])
                    .unwrap(),
            )
            .send()
            .await
            .unwrap();

        let status = response.status();

        match status {
            StatusCode::OK => {
                let html = response.text().await.unwrap();
                if html.contains("challenge-platform")
                    || html.contains("cf-chl-widget-")
                    || html.contains("Checking your browser before accessing")
                    || html.contains("__cf_chl_jschl_tk__")
                {
                    eprintln!(
                        "[WORKER {album_worker}] Cloudflare challenge in response HTML"
                    );
                    return ResolveAlbumResult::Cloudflare;
                }
                return ResolveAlbumResult::Success(html);
            }
            StatusCode::FORBIDDEN => {
                eprintln!(
                    "[WORKER {album_worker}] blocked by Cloudflare (403) when resolving album"
                );
                return ResolveAlbumResult::Cloudflare;
            }
            StatusCode::TOO_MANY_REQUESTS => {
                eprintln!(
                    "[WORKER {album_worker}] rate limited (429) when resolving album"
                );
                return ResolveAlbumResult::Error;
            }
            _ => {
                eprintln!(
                    "[WORKER {album_worker}] received code {} when resolving album (attempt {attempt}/{MAX_RETRIES})",
                    status.as_u16()
                );

                if !running.load(Ordering::Relaxed) {
                    return ResolveAlbumResult::Error;
                }

                time::sleep(Duration::from_secs(5)).await;
            }
        }
    }

    eprintln!("[WORKER {album_worker}] giving up on resolving album after {MAX_RETRIES} attempts");
    ResolveAlbumResult::Error
}

pub async fn request_track_download(
    client: &Client,
    track: &Track,
    token_expiry: u64,
    config: &DownloadConfig,
    running: Arc<AtomicBool>,
    workers: WorkerIds,
) -> Option<TrackDownload> {
    loop {
        let response = client
            .post("https://lucida.to/api/load?url=%2Fapi%2Ffetch%2Fstream%2Fv2")
            .json(&TrackDownloadRequest {
                account: Account {
                    id: &config.country,
                    r#type: "country",
                },
                compat: false,
                downscale: &config.downscale,
                handoff: true,
                metadata: config.metadata,
                private: config.private,
                token: Token {
                    expiry: token_expiry,
                    primary: &track.csrf,
                    secondary: track.csrf_fallback.as_deref(),
                },
                upload: Upload { enabled: false },
                url: &track.url,
            })
            .send()
            .await
            .unwrap();

        let status = response.status();

        if status == StatusCode::OK {
            if let Ok(track_download) = response.json().await {
                match track_download {
                    TrackDownloadResult::Ok(track_download) => break Some(track_download),
                    TrackDownloadResult::Error { error, .. } => {
                        eprintln!("{workers} error when requesting track download: {error}");

                        if !running.load(Ordering::Relaxed) {
                            break None;
                        }

                        time::sleep(Duration::from_secs(5)).await;
                    }
                }
            } else {
                eprintln!("{workers} invalid JSON when requesting track download");

                if !running.load(Ordering::Relaxed) {
                    break None;
                }

                time::sleep(Duration::from_secs(5)).await;
            }
        } else {
            eprintln!(
                "{workers} received code {} when requesting track download",
                status.as_u16()
            );

            if !running.load(Ordering::Relaxed) {
                break None;
            }

            time::sleep(Duration::from_secs(5)).await;
        }
    }
}

pub async fn track_download_status(
    client: &Client,
    stream: &TrackDownload,
    workers: WorkerIds,
) -> Option<TrackDownloadStatus> {
    loop {
        let response = client
            .get(format!(
                "https://{}.lucida.to/api/fetch/request/{}",
                stream.server, stream.handoff
            ))
            .send()
            .await
            .unwrap();

        let status = response.status();

        if status == StatusCode::OK {
            break Some(response.json().await.unwrap());
        }

        eprintln!(
            "{workers} received code {} when checking track processing status",
            status.as_u16()
        );

        if IRRECOVERABLE_STATUS_CODES.contains(&status) {
            break None;
        }

        time::sleep(Duration::from_secs(5)).await;
    }
}

pub async fn download_track(
    client: &Client,
    stream: &TrackDownload,
    workers: WorkerIds,
) -> Option<(UnboundedReceiver<Result<Vec<u8>, ()>>, String)> {
    loop {
        let mut response = client
            .get(format!(
                "https://{}.lucida.to/api/fetch/request/{}/download",
                stream.server, stream.handoff
            ))
            .send()
            .await
            .unwrap();

        let status = response.status();

        if status == StatusCode::OK {
            let mime_type = response.headers()[CONTENT_TYPE]
                .to_str()
                .unwrap()
                .to_owned();

            let (tx, rx) = mpsc::unbounded_channel();

            tokio::spawn(async move {
                loop {
                    let result = response.chunk().await;

                    match result {
                        Ok(chunk) => match chunk {
                            Some(chunk) => tx.send(Ok(chunk.to_vec())).unwrap(),
                            None => break,
                        },
                        Err(err) => {
                            eprintln!("{workers} error when downloading track audio: {err}");
                            tx.send(Err(())).unwrap();
                            break;
                        }
                    }
                }
            });

            break Some((rx, mime_type));
        }

        eprintln!(
            "{workers} received code {} when downloading track audio",
            status.as_u16()
        );

        if IRRECOVERABLE_STATUS_CODES.contains(&status) {
            break None;
        }

        time::sleep(Duration::from_secs(5)).await;
    }
}

/// Given an artist page URL, fetch it and extract all album URLs.
pub async fn extract_albums_from_artist_page(client: &Client, url: &str) -> Vec<String> {
    let response = match client.get(url).send().await {
        Ok(r) => r,
        Err(err) => {
            eprintln!("failed to fetch artist page: {err}");
            return Vec::new();
        }
    };

    let status = response.status();
    if status != StatusCode::OK {
        eprintln!("artist page returned status {status}");
        return Vec::new();
    }

    let html = match response.text().await {
        Ok(h) => h,
        Err(err) => {
            eprintln!("failed to read artist page HTML: {err}");
            return Vec::new();
        }
    };

    let mut albums = Vec::new();

    // Look for album links in the HTML. On lucida.to artist pages, album
    // links typically look like:
    //   href="/?url=https%3A%2F%2Fwww.qobuz.com%2F...%2Falbum%2F..."
    for line in html.lines() {
        let mut rest: &str = line;
        while let Some(start) = rest.find("href=\"") {
            rest = &rest[start + 6..];
            if let Some(end) = rest.find('"') {
                let href = &rest[..end];
                rest = &rest[end..];

                // Only keep links that point to an album on lucida.to
                if href.starts_with("/?url=") && href.contains("/album/") {
                    let full = format!("https://lucida.to{href}");
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

pub async fn download_album_cover(
    client: &Client,
    url: &str,
    running: Arc<AtomicBool>,
    album_worker: usize,
) -> Option<UnboundedReceiver<Result<Vec<u8>, ()>>> {
    loop {
        let mut response = client.get(url).send().await.unwrap();
        let status = response.status();

        if status == StatusCode::OK {
            let (tx, rx) = mpsc::unbounded_channel();

            tokio::spawn(async move {
                loop {
                    let result = response.chunk().await;

                    match result {
                        Ok(chunk) => match chunk {
                            Some(chunk) => tx.send(Ok(chunk.to_vec())).unwrap(),
                            None => break,
                        },
                        Err(err) => {
                            eprintln!(
                                "[WORKER {album_worker}] error when downloading album cover: {err}"
                            );

                            tx.send(Err(())).unwrap();
                            break;
                        }
                    }
                }
            });

            break Some(rx);
        } else if status == StatusCode::NOT_FOUND {
            eprintln!("[WORKER {album_worker}] album doesn't have a cover");
            break None;
        }

        eprintln!(
            "[WORKER {album_worker}] received code {} when downloading album cover from {url}",
            status.as_u16()
        );

        if !running.load(Ordering::Relaxed) {
            return None;
        }
    }
}
