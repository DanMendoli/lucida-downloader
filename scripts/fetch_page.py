import sys
import time
import tempfile
import os

def fetch_page(url: str) -> str:
    from playwright.sync_api import sync_playwright

    with sync_playwright() as p:
        # Connect to existing Chromium via CDP
        browser = p.chromium.connect_over_cdp("http://127.0.0.1:9222")
        context = browser.contexts[0] if browser.contexts else browser.new_context()
        page = context.new_page()

        try:
            page.goto(url, wait_until="domcontentloaded", timeout=60000)

            # Wait for the SPA to hydrate.
            # The Svelte app can take a while to fully render.
            time.sleep(25)

            # Get fully rendered DOM
            html = page.content()
        finally:
            page.close()
            browser.close()

        # Write to temp file to avoid stdout truncation
        fd, path = tempfile.mkstemp(suffix=".html", prefix="lucida-playwright-")
        try:
            with os.fdopen(fd, 'w') as f:
                f.write(html)
        except:
            os.close(fd)
            raise
        print(path)

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("ERROR: URL required", file=sys.stderr)
        sys.exit(1)
    fetch_page(sys.argv[1])
