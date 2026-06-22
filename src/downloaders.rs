use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future;
use reqwest::Client;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::{fs, time};

use crate::models::{
    AlbumInfo, AlbumYear, DownloadConfig, PageData, Service, SkipConfig, Track, TrackDownload,
    WorkerIds,
};
use crate::{requests, text_utils, workers};

#[expect(
    clippy::too_many_arguments,
    reason = "this function is called from a single place"
)]
pub async fn download_album(
    client: Client,
    url: &str,
    output_path: &Path,
    force_download: bool,
    group_singles: bool,
    album_year: Option<AlbumYear>,
    flatten_directories: bool,
    config: DownloadConfig,
    track_workers: usize,
    skip: SkipConfig,
    running: Arc<AtomicBool>,
    album_worker: usize,
) {
    let Some(page_data) = resolve_album(&client, url, &config, &running, album_worker).await else {
        return;
    };

    let album = AlbumInfo::new(page_data.info, page_data.token);

    eprintln!(
        "[WORKER {album_worker}] downloading album {} - {} with {} tracks",
        album.artist_name, album.title, album.track_count
    );

    let is_grouped_single = group_singles
        && album.track_count == 1
        && album
            .tracks
            .iter()
            .all(|track| track.1.title == album.title);

    let album_path = {
        let sanitized_artist_name = text_utils::sanitize_file_name(&album.artist_name);

        let album_directory = if is_grouped_single {
            "Singles".into()
        } else {
            let sanitized_album_title = text_utils::sanitize_file_name(&album.title);

            match album_year {
                Some(AlbumYear::Append) => {
                    format!("{} ({})", sanitized_album_title, album.release_year)
                }
                Some(AlbumYear::Prepend) => {
                    format!("({}) {}", album.release_year, sanitized_album_title)
                }
                None => sanitized_album_title,
            }
        };

        let album_directory = if flatten_directories {
            vec![format!("{sanitized_artist_name} - {album_directory}")]
        } else {
            vec![sanitized_artist_name, album_directory]
        };

        let mut album_path = PathBuf::from(output_path);
        album_path.extend(album_directory);

        album_path
    };

    fs::create_dir_all(&album_path).await.unwrap();

    let tracks_len = album.tracks.len();
    let tracks = Arc::new(Mutex::new(album.tracks));
    let album_path = Arc::new(album_path);

    if !skip.tracks {
        let worker_count = track_workers.min(tracks_len);

        eprintln!("[WORKER {album_worker}] spawning {worker_count} track workers");

        for result in future::join_all((1..=worker_count).map(|track_worker| {
            tokio::spawn(workers::run_track_worker(
                client.clone(),
                page_data.original_service,
                tracks.clone(),
                album.track_count,
                is_grouped_single,
                page_data.token_expiry,
                force_download,
                config.clone(),
                album_path.clone(),
                running.clone(),
                WorkerIds {
                    track: track_worker,
                    album: album_worker,
                },
            ))
        }))
        .await
        {
            result.unwrap();
        }
    }

    if skip.cover || is_grouped_single || !running.load(Ordering::Relaxed) {
        return;
    }

    download_album_cover(
        client,
        &album.title,
        page_data.original_service,
        &album.cover_artwork_url,
        force_download,
        &album_path,
        running,
        album_worker,
    )
    .await;
}

async fn resolve_album(
    client: &Client,
    url: &str,
    config: &DownloadConfig,
    running: &Arc<AtomicBool>,
    album_worker: usize,
) -> Option<PageData> {
    eprintln!("[WORKER {album_worker}] resolving album {url}");

    loop {
        let html =
            requests::resolve_album(client, url, &config.country, running, album_worker).await?;

        if let Some(error) = [
            "An error occured trying to process your request.",
            "Message: \"Cannot contact any valid server\"",
            "An error occurred. Had an issue getting that item, try again.",
        ]
        .into_iter()
        .find(|&error| html.contains(error))
        {
            eprintln!("[WORKER {album_worker}] HTML contains error: {error}");

            if !running.load(Ordering::Relaxed) {
                return None;
            }

            time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        let data_json = text_utils::parse_enclosed_value(
            ",{\"type\":\"data\",\"data\":",
            ",\"uses\":{\"url\":1}}];\n",
            &html,
        );

        match json5::from_str::<PageData>(data_json) {
            Ok(page_data) => return Some(page_data),
            Err(err) => {
                eprintln!("[WORKER {album_worker}] failed to parse album data: {err}");
                eprintln!("[WORKER {album_worker}] extracted JSON snippet: {data_json:.200}");

                if !running.load(Ordering::Relaxed) {
                    return None;
                }

                time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "this function is called from a single place"
)]
pub async fn request_and_download_track(
    client: Client,
    service: Service,
    track: &Track,
    track_number: Option<u32>,
    track_count: u32,
    is_grouped_single: bool,
    token_expiry: u64,
    force_download: bool,
    config: &DownloadConfig,
    album_path: Arc<PathBuf>,
    running: Arc<AtomicBool>,
    workers: WorkerIds,
) {
    // HACK(jel): this seems to be the only way to detect tracks that are impossible
    // to download yet
    if matches!(service, Service::Qobuz if track.producers.is_none()) {
        eprintln!("{workers} skipping unavailable track {}", track.title);
        return;
    }

    let file_stem =
        text_utils::format_track_stem(track, track_number, track_count, is_grouped_single);

    if !force_download {
        let mut directory = fs::read_dir(album_path.as_path()).await.unwrap();

        while let Some(entry) = directory.next_entry().await.unwrap() {
            if entry.file_type().await.unwrap().is_file()
                && entry
                    .path()
                    .file_stem()
                    .is_some_and(|stem| stem.to_str().unwrap() == file_stem)
            {
                eprintln!("{workers} track {} is already downloaded", track.title);
                return;
            }
        }
    }

    eprintln!("{workers} downloading track {}", track.title);

    request_track_download(
        client,
        track,
        file_stem,
        token_expiry,
        config,
        album_path,
        running,
        workers,
    )
    .await;
}

#[expect(
    clippy::too_many_arguments,
    reason = "this function is called from a single place"
)]
async fn request_track_download(
    client: Client,
    track: &Track,
    file_stem: String,
    token_expiry: u64,
    config: &DownloadConfig,
    album_path: Arc<PathBuf>,
    running: Arc<AtomicBool>,
    workers: WorkerIds,
) {
    let mut current_downscale = config.downscale.clone();

    'request_track_download: loop {
        let mut current_config = config.clone();
        current_config.downscale = current_downscale.clone();

        let Some(track_download) = requests::request_track_download(
            &client,
            track,
            token_expiry,
            &current_config,
            running.clone(),
            workers,
        )
        .await
        else {
            break;
        };

        let mut last_status: Option<(String, String, Instant)> = None;

        loop {
            let Some(track_download) =
                requests::track_download_status(&client, &track_download, workers).await
            else {
                if !running.load(Ordering::Relaxed) {
                    return;
                }

                continue 'request_track_download;
            };

            if last_status.as_ref().is_none_or(|last_status| {
                (&track_download.status, &track_download.message)
                    != (&last_status.0, &last_status.1)
            }) {
                eprintln!(
                    "{workers} new download status: {}: {}",
                    track_download.status,
                    track_download.message.replace("{item}", &track.title)
                );

                last_status = Some((
                    track_download.status.clone(),
                    track_download.message.clone(),
                    Instant::now(),
                ));
            } else if let Some(last_status) = last_status.as_ref()
                && last_status.2.elapsed() >= Duration::from_secs(30)
            {
                eprint!(
                    "{workers} download status stuck for 30 seconds on {}: {}",
                    last_status.0,
                    last_status.1.replace("{item}", &track.title)
                );

                if !running.load(Ordering::Relaxed) {
                    eprintln!();
                    return;
                }

                eprintln!(", retrying");
                continue 'request_track_download;
            }

            if track_download.status == "completed" {
                break;
            }

            if track_download.status == "error" {
                if current_downscale != "original" {
                    eprintln!(
                        "{workers} conversion failed for {}, falling back to original format",
                        track.title
                    );
                    current_downscale = String::from("original");
                    continue 'request_track_download;
                }

                eprintln!(
                    "{workers} track processing failed, retrying from start: {}",
                    track_download.message.replace("{item}", &track.title)
                );
                continue 'request_track_download;
            }

            time::sleep(Duration::from_secs(1)).await;
        }

        if !download_track(
            client.clone(),
            track_download,
            album_path.clone(),
            file_stem.clone(),
            &current_config,
            running.clone(),
            workers,
        )
        .await
        {
            if !running.load(Ordering::Relaxed) {
                return;
            }

            continue 'request_track_download;
        }

        break;
    }
}

async fn download_track(
    client: Client,
    track_download: TrackDownload,
    album_path: Arc<PathBuf>,
    file_stem: String,
    config: &DownloadConfig,
    running: Arc<AtomicBool>,
    workers: WorkerIds,
) -> bool {
    let Some((mut rx, mime_type)) =
        requests::download_track(&client, &track_download, workers).await
    else {
        if !running.load(Ordering::Relaxed) {
            return false;
        }

        eprintln!("{workers} failed to start track download, retrying from start");
        return false;
    };

    let file_extension = match mime_type
        .split_once(';')
        .map_or(mime_type.as_str(), |(mime_type, _)| mime_type)
    {
        "audio/flac" => "flac",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => "m4a",
        _ => {
            eprintln!("{workers} unknown mime type {mime_type}, inferring extension from downscale");
            match config.downscale.as_str() {
                "m4a-aac-320" => "m4a",
                "mp3-320" | "mp3" => "mp3",
                "original" | "flac" | "lossless" => "flac",
                _ => {
                    eprintln!("{workers} falling back to .bin extension");
                    "bin"
                }
            }
        }
    };

    let file_name = format!("{file_stem}.{file_extension}");
    let part_path = album_path.join(format!("{file_name}.part"));
    let mut file = BufWriter::new(File::create(&part_path).await.unwrap());

    while let Some(result) = rx.recv().await {
        if let Ok(chunk) = result {
            file.write_all(&chunk).await.unwrap();
        } else {
            eprintln!("{workers} error receiving track chunk, retrying from start");
            return false;
        }
    }

    fs::rename(part_path, album_path.join(&file_name))
        .await
        .unwrap();

    config
        .format_stats
        .lock()
        .unwrap()
        .entry(file_extension.to_string())
        .and_modify(|count| *count += 1)
        .or_insert(1);

    true
}

#[expect(
    clippy::too_many_arguments,
    reason = "this function is called from a single place"
)]
pub async fn download_album_cover(
    client: Client,
    title: &str,
    service: Service,
    url: &str,
    force_download: bool,
    album_path: &Path,
    running: Arc<AtomicBool>,
    album_worker: usize,
) {
    let cover_path = album_path.join("cover.jpg");

    if !force_download && cover_path.exists() {
        eprintln!("[WORKER {album_worker}] {title} album cover is already downloaded");
        return;
    }

    eprintln!("[WORKER {album_worker}] downloading {title} album cover");

    let url = match service {
        Service::Qobuz => {
            let stripped_url = url.strip_suffix(".jpg").unwrap();
            let end_index = stripped_url.rfind('_').unwrap() + 1;
            Cow::Owned(format!("{}org.jpg", &url[..end_index]))
        }
        Service::Tidal | Service::Soundcloud => Cow::Borrowed(url),
    };

    let part_path = album_path.join("cover.jpg.part");

    'download_album_cover: loop {
        let Some(mut rx) =
            requests::download_album_cover(&client, &url, running.clone(), album_worker).await
        else {
            return;
        };

        let mut file = BufWriter::new(File::create(&part_path).await.unwrap());

        while let Some(chunk) = rx.recv().await {
            if let Ok(chunk) = chunk {
                file.write_all(&chunk).await.unwrap();
            } else {
                if !running.load(Ordering::Relaxed) {
                    return;
                }

                continue 'download_album_cover;
            }
        }

        file.flush().await.unwrap();
        break;
    }

    fs::rename(part_path, cover_path).await.unwrap();
}
