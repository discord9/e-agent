import json, sys
from pathlib import Path
EXE = ("/mnt/nvme_rust/cargo-home/playwright-browsers/chromium_headless_shell-1228/"
       "chrome-headless-shell-linux64/chrome-headless-shell")
MOBILE_UA = ("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) "
             "AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 "
             "Mobile/15E148 Safari/604.1")
BASE = "http://127.0.0.1:18777"
from playwright.sync_api import sync_playwright
token = open("/mnt/nvme_rust/cargo-home/eagent-mt-state/e-agent/server.token").read().strip()
with sync_playwright() as p:
    b = p.chromium.launch(executable_path=EXE)
    ctx = b.new_context(viewport={"width":390,"height":844}, is_mobile=True, has_touch=True, user_agent=MOBILE_UA)
    page = ctx.new_page()
    page.add_init_script("localStorage.setItem('eagent_token', %s)" % json.dumps(token))
    page.goto(BASE + "/", wait_until="load"); page.wait_for_timeout(1500)
    page.click("#newSessionBtn")
    page.wait_for_function("() => state.view === 'chat'", timeout=8000)
    page.wait_for_timeout(1000)
    info = page.evaluate("""() => {
      const q = s => document.querySelector(s);
      const r = el => { const x = el.getBoundingClientRect();
        return {x:Math.round(x.x), y:Math.round(x.y), w:Math.round(x.width), h:Math.round(x.height),
                disp:getComputedStyle(el).display, fw:getComputedStyle(el).flexWrap,
                fs:getComputedStyle(el).fontSize, ws:getComputedStyle(el).whiteSpace}; };
      const ta = q('#topActions');
      const kids = [...ta.children].map(el => ({id:el.id, cls:el.className, ...r(el)}));
      const tb = q('#topbar');
      return {topbar: r(tb), topActions: r(ta),
              kids,
              innerW: window.innerWidth, docScrollW: document.documentElement.scrollWidth,
              bodyScrollW: document.body.scrollWidth};
    }""")
    print(json.dumps(info, indent=1, ensure_ascii=False))
    b.close()
