#!/usr/bin/env python3
"""Render hero.html beside this file to docs/images/hero.png at 2x (2560x1440).

    pip install playwright==1.64.1
    playwright install chromium-headless-shell
    python3 docs/readme-hero/render.py

Pass another path to write somewhere else. The page is served from this repo on
loopback, and any request off loopback is blocked, so everything it needs stays
in the repo.
"""

import functools
import http.server
import sys
import threading
from pathlib import Path

from playwright.sync_api import sync_playwright

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
OUT = Path(sys.argv[1]) if len(sys.argv) > 1 else HERE.parent / "images" / "hero.png"
FONTS = ["600 20px Fraunces", '600 20px "Instrument Sans"', '600 20px "JetBrains Mono"']


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


handler = functools.partial(Quiet, directory=str(REPO))
server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f"http://127.0.0.1:{server.server_port}"

with sync_playwright() as p:
    browser = p.chromium.launch()
    page = browser.new_page(viewport={"width": 1280, "height": 720}, device_scale_factor=2)
    page.route("**/*", lambda r: r.continue_() if r.request.url.startswith(base) else r.abort())
    page.goto(f"{base}/docs/readme-hero/hero.html")
    page.evaluate("fonts => Promise.all(fonts.map(f => document.fonts.load(f))).then(() => null)", FONTS)
    page.evaluate("document.fonts.ready.then(() => null)")
    missing = page.evaluate("fonts => fonts.filter(f => !document.fonts.check(f))", FONTS)
    if missing:
        sys.exit(f"fonts did not load: {missing}")
    page.locator("#hero").screenshot(path=str(OUT), scale="device", omit_background=True)
    browser.close()

server.shutdown()
print(OUT)
