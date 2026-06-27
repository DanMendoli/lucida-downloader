import sys
import shutil
import tempfile
import time
import json

def fetch(url: str) -> str:
    from playwright.sync_api import sync_playwright

    home = __import__('os').environ['HOME']
    original_profile = f"{home}/.config/chromium"

    tmp_dir = tempfile.mkdtemp(prefix="lucida-chromium-")
    profile_dir = f"{tmp_dir}/profile"

    try:
        shutil.copytree(original_profile, profile_dir, ignore=shutil.ignore_patterns("Singleton*"))

        with sync_playwright() as p:
            context = p.chromium.launch_persistent_context(
                profile_dir,
                headless=True,
                args=[
                    "--disable-blink-features=AutomationControlled",
                    "--disable-web-security",
                    "--disable-features=IsolateOrigins,site-per-process",
                    "--remote-allow-origins=*",
                ],
            )
            page = context.new_page()
            page.goto(url, wait_until="networkidle", timeout=30000)
            html = page.content()
            context.close()

            if "Welcome to the world" in html:
                print(html)
            elif "challenge-platform" in html or "cf-chl-widget-" in html or "just a sec" in html:
                print("ERROR: Cloudflare blocked", file=sys.stderr)
                sys.exit(1)
            else:
                print(html)
    finally:
        shutil.rmtree(tmp_dir, ignore_errors=True)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("ERROR: URL required", file=sys.stderr)
        sys.exit(1)
    fetch(sys.argv[1])
