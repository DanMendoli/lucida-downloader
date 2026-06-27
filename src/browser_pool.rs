use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use reqwest::{Client, header};

fn find_python() -> Option<String> {
    for cmd in ["python3", "python"] {
        if Command::new(cmd).arg("--version").output().is_ok() {
            return Some(cmd.into());
        }
    }
    None
}

fn ensure_venv(python: &str) -> Option<PathBuf> {
    let cache_dir = if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".cache")
    } else {
        PathBuf::from("/tmp")
    };

    let venv_dir = cache_dir.join("lucida-downloader").join("venv");
    let venv_python = venv_dir.join("bin").join("python3");

    if venv_python.exists() {
        return Some(venv_dir);
    }

    std::fs::create_dir_all(&venv_dir).ok()?;

    let status = Command::new(python)
        .args(["-m", "venv", &venv_dir.to_string_lossy()])
        .status()
        .ok()?;

    if !status.success() {
        return None;
    }

    Some(venv_dir)
}

fn ensure_playwright(venv_python: &str) -> Option<()> {
    let output = Command::new(venv_python)
        .args(["-c", "import playwright"])
        .output()
        .ok()?;

    if output.status.success() {
        return Some(());
    }

    let status = Command::new(venv_python)
        .args(["-m", "pip", "install", "--quiet", "playwright"])
        .status()
        .ok()?;

    if !status.success() {
        return None;
    }

    let status = Command::new(venv_python)
        .args(["-m", "playwright", "install", "chromium"])
        .status()
        .ok()?;

    if status.success() {
        Some(())
    } else {
        None
    }
}

/// Harvest cookies from the user's Chromium via Playwright CDP.
fn harvest_cookies() -> Option<(Vec<String>, String)> {
    let python = find_python()?;
    let venv_dir = ensure_venv(&python)?;
    let venv_python = venv_dir.join("bin").join("python3");
    let venv_python_str = venv_python.to_string_lossy().to_string();

    ensure_playwright(&venv_python_str)?;

    let script = r#"
import sys, json
from playwright.sync_api import sync_playwright

with sync_playwright() as p:
    browser = p.chromium.connect_over_cdp("http://127.0.0.1:9222")
    context = browser.contexts[0] if browser.contexts else browser.new_context()
    
    # Use an existing page if available, otherwise create one
    pages = context.pages
    if pages:
        page = pages[0]
    else:
        page = context.new_page()
    
    # Navigate to lucida.to to ensure cookies are fresh
    try:
        page.goto("https://lucida.to/", wait_until="domcontentloaded", timeout=30000)
        page.wait_for_timeout(5000)
    except Exception as e:
        print(f"warning: navigation failed: {e}", file=sys.stderr)

    # Get cookies
    cookies = context.cookies("https://lucida.to/")
    cookie_list = [f"{c['name']}={c['value']}" for c in cookies]

    # Get user agent
    ua = page.evaluate("() => navigator.userAgent")

    browser.close()

    result = {"cookies": cookie_list, "user_agent": ua}
    print(json.dumps(result))
"#;

    let output = Command::new(&venv_python_str)
        .arg("-c")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        eprintln!("[browser] cookie harvest failed: {stderr}");
        return None;
    }

    let json: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let cookies: Vec<String> = json
        .get("cookies")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let user_agent = json.get("user_agent")?.as_str()?.to_string();

    Some((cookies, user_agent))
}

/// Build a reqwest client that carries the Cloudflare cookies harvested
/// from the user's browser.
pub fn build_cookie_client() -> Option<Client> {
    eprintln!("[browser] harvesting cookies from user's Chromium...");

    let (cookies, user_agent) = harvest_cookies()?;

    eprintln!("[browser] harvested {} cookies", cookies.len());

    let mut cookie_header = String::new();
    for cookie in &cookies {
        if !cookie_header.is_empty() {
            cookie_header.push_str("; ");
        }
        cookie_header.push_str(cookie);
    }

    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        header::HeaderValue::from_str(&user_agent).ok()?,
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
        .http1_only()
        .build()
        .ok()
}

/// Fetch a page using Playwright connected to the user's Chromium via CDP.
/// This is needed for pages that require JavaScript rendering (e.g. artist
/// pages whose album list is hydrated by the SvelteKit SPA).
pub fn fetch_page_with_browser(url: &str) -> Option<String> {
    let python = find_python()?;
    let venv_dir = ensure_venv(&python)?;
    let venv_python = venv_dir.join("bin").join("python3");
    let venv_python_str = venv_python.to_string_lossy().to_string();

    ensure_playwright(&venv_python_str)?;

    let out_path = "/tmp/lucida_browser_page.html";

    let script = format!(
        r#"import sys, json
from playwright.sync_api import sync_playwright

with sync_playwright() as p:
    try:
        browser = p.chromium.connect_over_cdp("http://127.0.0.1:9222")
        context = browser.contexts[0] if browser.contexts else browser.new_context()
        page = context.new_page()
        page.goto({url:?}, wait_until="domcontentloaded", timeout=60000)
        page.wait_for_timeout(15000)
        html = page.content()
        with open({out_path:?}, "w") as f:
            f.write(html)
        browser.close()
    except Exception as e:
        print(f"ERROR: {{e}}", file=sys.stderr)
        sys.exit(1)
print("OK")
"#,
        url = url,
        out_path = out_path,
    );

    let output = Command::new(&venv_python_str)
        .arg("-c")
        .arg(&script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() || !stdout.contains("OK") {
        eprintln!("[browser] page fetch failed: {stderr}");
        return None;
    }

    let html = std::fs::read_to_string(out_path).ok()?;
    Some(html)
}

/// A thread-safe `Client` wrapper that can refresh its Cloudflare cookies
/// when they expire.
#[derive(Clone)]
pub struct SharedClient {
    inner: Arc<RwLock<Client>>,
}

impl SharedClient {
    /// Create a new `SharedClient` from an initial `Client`.
    pub fn new(client: Client) -> Self {
        Self {
            inner: Arc::new(RwLock::new(client)),
        }
    }

    /// Get a clone of the current inner `Client`.
    pub fn get(&self) -> Client {
        self.inner.read().unwrap().clone()
    }

    /// Re-harvest cookies from the browser and build a fresh `Client`.
    /// Returns `true` on success.
    pub fn refresh(&self) -> bool {
        eprintln!("[browser] refreshing cookies...");
        match build_cookie_client() {
            Some(client) => {
                *self.inner.write().unwrap() = client;
                eprintln!("[browser] cookies refreshed");
                true
            }
            None => {
                eprintln!("[browser] cookie refresh failed");
                false
            }
        }
    }
}
