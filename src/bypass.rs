use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use headless_chrome::{Browser, LaunchOptions};
use reqwest::{Client, header};

fn copy_user_profile() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let original = PathBuf::from(&home).join(".config/chromium");

    if !original.exists() {
        return None;
    }

    let tmp = PathBuf::from("/tmp/lucida-chromium-profile");
    let _ = fs::remove_dir_all(&tmp);

    let status = Command::new("cp")
        .args(["-r", &format!("{}/.", original.display()), &tmp.to_string_lossy()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;

    if !status.success() {
        return None;
    }

    for lock_file in &["SingletonLock", "SingletonCookie", "SingletonSocket"] {
        let _ = fs::remove_file(tmp.join(lock_file));
    }

    Some(tmp)
}

/// Launch a headless Chromium with a copy of the user's profile, navigate to
/// lucida.to, let Cloudflare resolve automatically, then harvest cookies and
/// build a reqwest client.
pub fn create_bypass_client() -> Option<Client> {
    eprintln!("[bypass] launching headless browser with user profile to resolve Cloudflare...");

    let profile = copy_user_profile()?;

    let user_agent = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";
    let ua_arg = format!("--user-agent={user_agent}");

    let options = LaunchOptions::default_builder()
        .headless(true)
        .sandbox(false)
        .user_data_dir(Some(profile.clone()))
        .args(vec![
            std::ffi::OsStr::new("--disable-blink-features=AutomationControlled"),
            std::ffi::OsStr::new(&ua_arg),
            std::ffi::OsStr::new("--no-sandbox"),
            std::ffi::OsStr::new("--disable-dev-shm-usage"),
            std::ffi::OsStr::new("--disable-gpu"),
            std::ffi::OsStr::new("--disable-extensions"),
            std::ffi::OsStr::new("--disable-web-security"),
            std::ffi::OsStr::new("--disable-features=IsolateOrigins,site-per-process"),
            std::ffi::OsStr::new("--remote-allow-origins=*"),
        ])
        .build()
        .ok()?;

    let browser = match Browser::new(options) {
        Ok(b) => b,
        Err(err) => {
            eprintln!("[bypass] failed to launch Chromium: {err}");
            let _ = fs::remove_dir_all(&profile);
            return None;
        }
    };

    let tab = match browser.new_tab() {
        Ok(t) => t,
        Err(err) => {
            eprintln!("[bypass] failed to create tab: {err}");
            let _ = fs::remove_dir_all(&profile);
            return None;
        }
    };

    let target_url = "https://lucida.to";

    eprintln!("[bypass] navigating to {target_url}...");
    if let Err(err) = tab.navigate_to(target_url) {
        eprintln!("[bypass] navigation failed: {err}");
        let _ = fs::remove_dir_all(&profile);
        return None;
    }

    if let Err(err) = tab.wait_for_element("body") {
        eprintln!("[bypass] wait for body failed: {err}");
        let _ = fs::remove_dir_all(&profile);
        return None;
    }

    let mut bypassed = false;
    for i in 0..30 {
        std::thread::sleep(Duration::from_secs(1));

        let title = match tab.get_title() {
            Ok(t) => t,
            Err(_) => continue,
        };

        let title_low = title.to_lowercase();

        if title_low.contains("just a moment")
            || title_low.contains("just a sec")
            || title_low.contains("cloudflare")
        {
            continue;
        }

        if title_low.contains("521")
            || title_low.contains("503")
            || title_low.contains("502")
            || title_low.contains("web server is down")
            || title_low.contains("bad gateway")
            || title_low.contains("service unavailable")
        {
            eprintln!("[bypass] site returned error page (title={title})");
            break;
        }

        if !title_low.is_empty() {
            eprintln!("[bypass] page loaded after {}s (title={title})", i + 1);
            bypassed = true;
            break;
        }
    }

    if !bypassed {
        eprintln!("[bypass] timed out waiting for page to load");
        let _ = fs::remove_dir_all(&profile);
        return None;
    }

    let cookies = match tab.get_cookies() {
        Ok(c) => c,
        Err(err) => {
            eprintln!("[bypass] failed to get cookies: {err}");
            let _ = fs::remove_dir_all(&profile);
            return None;
        }
    };

    eprintln!("[bypass] harvested {} cookies", cookies.len());
    for cookie in &cookies {
        eprintln!("[bypass]   {}={}", cookie.name, cookie.value);
    }

    let real_user_agent = tab
        .evaluate("navigator.userAgent", false)
        .ok()
        .and_then(|v| v.value)
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| user_agent.to_string());

    drop(tab);
    drop(browser);
    let _ = fs::remove_dir_all(&profile);

    eprintln!("[bypass] building reqwest client...");

    let mut cookie_header = String::new();
    for cookie in &cookies {
        if !cookie_header.is_empty() {
            cookie_header.push_str("; ");
        }
        cookie_header.push_str(&format!("{}={}", cookie.name, cookie.value));
    }

    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        header::HeaderValue::from_str(&real_user_agent).ok()?,
    );
    headers.insert(
        header::REFERER,
        header::HeaderValue::from_static("https://lucida.to/"),
    );
    headers.insert(
        header::ACCEPT,
        header::HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        ),
    );
    headers.insert(
        header::ACCEPT_LANGUAGE,
        header::HeaderValue::from_static("en-US,en;q=0.9"),
    );
    headers.insert(
        header::COOKIE,
        header::HeaderValue::from_str(&cookie_header).ok()?,
    );

    Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(30))
        .build()
        .ok()
}
