use std::path::PathBuf;
use std::process::Command;

const PYTHON_SCRIPT: &str = r#"
import sys, browser_cookie3

try:
    cj = browser_cookie3.chromium(domain_name='lucida.to')
    for cookie in cj:
        if cookie.name == 'cf_clearance':
            print(cookie.value)
            sys.exit(0)
    print('')
except Exception as e:
    print(f'ERROR: {e}', file=sys.stderr)
    sys.exit(1)
"#;

fn cache_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".cache")
    } else {
        PathBuf::from("/tmp")
    }
}

pub fn auto_detect_cookies() -> Option<(String, String)> {
    let python = find_python()?;

    let venv_dir = cache_dir().join("lucida-downloader").join("venv");
    let venv_python = venv_dir.join("bin").join("python3");

    let python_exec = if venv_python.exists() {
        venv_python.to_string_lossy().into_owned()
    } else {
        ensure_venv(&python, &venv_dir)?;
        venv_python.to_string_lossy().into_owned()
    };

    ensure_browser_cookie3(&python_exec)?;

    let output = Command::new(&python_exec)
        .arg("-c")
        .arg(PYTHON_SCRIPT)
        .output()
        .ok()?;

    let cookie = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if cookie.is_empty() || cookie.starts_with("ERROR") {
        return None;
    }

    let user_agent = detect_chromium_user_agent()?;

    Some((cookie, user_agent))
}

fn find_python() -> Option<String> {
    for cmd in ["python3", "python"] {
        if Command::new(cmd).arg("--version").output().is_ok() {
            return Some(cmd.into());
        }
    }
    None
}

fn ensure_venv(python: &str, venv_dir: &PathBuf) -> Option<()> {
    std::fs::create_dir_all(venv_dir.parent()?).ok()?;

    let status = Command::new(python)
        .args(["-m", "venv", &venv_dir.to_string_lossy()])
        .status()
        .ok()?;

    if !status.success() {
        return None;
    }

    Some(())
}

fn ensure_browser_cookie3(python: &str) -> Option<()> {
    let output = Command::new(python)
        .args(["-c", "import browser_cookie3"])
        .output()
        .ok()?;

    if output.status.success() {
        return Some(());
    }

    let status = Command::new(python)
        .args(["-m", "pip", "install", "--quiet", "browser_cookie3"])
        .status()
        .ok()?;

    if status.success() {
        Some(())
    } else {
        None
    }
}

fn detect_chromium_user_agent() -> Option<String> {
    for browser in ["chromium", "google-chrome", "brave", "brave-browser"] {
        let output = Command::new(browser).arg("--version").output().ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);

        let version = stdout
            .split_whitespace()
            .find(|s| s.contains('.'))
            .or_else(|| {
                stdout
                    .split_whitespace()
                    .nth(1)
            })?;

        let ua = format!(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{version} Safari/537.36"
        );

        return Some(ua);
    }

    None
}
