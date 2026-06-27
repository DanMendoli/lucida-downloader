use std::process::{Command, Stdio};
use std::time::Duration;

use headless_chrome::Browser;

const CDP_PORT: u16 = 9222;

fn is_chromium_running() -> bool {
    Command::new("pgrep")
        .args(["-c", "chromium"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(0)
        > 0
}

fn get_browser_ws_url() -> Option<String> {
    let body = reqwest::blocking::get(&format!("http://127.0.0.1:{CDP_PORT}/json/version"))
        .ok()?
        .text()
        .ok()?;
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    json.get("webSocketDebuggerUrl")?.as_str().map(String::from)
}

fn launch_chromium_with_cdp() -> Option<std::process::Child> {
    let home = std::env::var("HOME").ok()?;
    let profile = format!("{home}/.config/chromium");

    eprintln!("[browser] launching Chromium with CDP on port {CDP_PORT}...");

    let child = Command::new("chromium")
        .args([
            &format!("--remote-debugging-port={CDP_PORT}"),
            &format!("--user-data-dir={profile}"),
            "--disable-blink-features=AutomationControlled",
            "--disable-web-security",
            "--disable-features=IsolateOrigins,site-per-process",
            "--remote-allow-origins=*",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    std::thread::sleep(Duration::from_secs(3));

    Some(child)
}

/// Fetch a page by connecting to the user's Chromium via CDP.
pub fn fetch_with_cdp(url: &str) -> Option<String> {
    let ws_url = match get_browser_ws_url() {
        Some(url) => url,
        None => {
            if is_chromium_running() {
                eprintln!("\n[bypass] Chromium is already running, but without --remote-debugging-port={CDP_PORT}.");
                eprintln!("[bypass] Please close Chromium and restart it with:");
                eprintln!("[bypass]   chromium --remote-debugging-port={CDP_PORT}");
                eprintln!("[bypass] Then run this command again.\n");
                return None;
            }

            let mut child = launch_chromium_with_cdp()?;

            let ws_url = (0..10).find_map(|i| {
                if i > 0 {
                    std::thread::sleep(Duration::from_secs(1));
                }
                get_browser_ws_url()
            });

            let ws_url = match ws_url {
                Some(url) => url,
                None => {
                    eprintln!("[browser] could not connect to CDP after launch");
                    let _ = child.kill();
                    return None;
                }
            };

            std::mem::forget(child);
            ws_url
        }
    };

    let browser = match Browser::connect(ws_url) {
        Ok(b) => b,
        Err(err) => {
            eprintln!("[browser] failed to connect to CDP: {err}");
            return None;
        }
    };

    let tab = match browser.new_tab() {
        Ok(t) => t,
        Err(err) => {
            eprintln!("[browser] failed to create tab: {err}");
            return None;
        }
    };

    eprintln!("[browser] navigating to: {url}");
    if let Err(err) = tab.navigate_to(url) {
        eprintln!("[browser] failed to navigate: {err}");
        let _ = tab.close(true);
        return None;
    }

    // Wait for the page to load and the SPA to hydrate.
    for i in 0..30 {
        std::thread::sleep(Duration::from_secs(1));

        let html = match tab.get_content() {
            Ok(h) => h,
            Err(_) => continue,
        };

        // Cloudflare challenge pages
        if html.contains("challenge-platform")
            || html.contains("cf-chl-widget-")
            || html.contains("just a sec")
        {
            if i >= 20 {
                eprintln!("[browser] still seeing Cloudflare challenge after {i}s, giving up");
                let _ = tab.close(true);
                return None;
            }
            continue;
        }

        // For album pages, wait until we see the album markup.
        if url.contains("/album/") || url.contains("play.qobuz.com/album/") {
            if html.contains("dl-album") || html.contains("download-form") {
                std::thread::sleep(Duration::from_secs(3));
                let html = match tab.get_content() {
                    Ok(h) => h,
                    Err(_) => html,
                };
                let _ = tab.close(true);
                return Some(html);
            }
        }

        // For artist pages, wait until we see album links.
        if url.contains("interpreter") || url.contains("/artist/") {
            if html.contains("/album/") {
                std::thread::sleep(Duration::from_secs(5));
                let html = match tab.get_content() {
                    Ok(h) => h,
                    Err(_) => html,
                };
                let _ = tab.close(true);
                return Some(html);
            }
        }

        // For the home page / generic pages
        if html.contains("Welcome to the world of Lucida") || html.contains("download-form") {
            if i >= 5 {
                std::thread::sleep(Duration::from_secs(2));
                let html = match tab.get_content() {
                    Ok(h) => h,
                    Err(_) => html,
                };
                let _ = tab.close(true);
                return Some(html);
            }
        }
    }

    let html = tab.get_content().ok();
    let _ = tab.close(true);
    html
}
