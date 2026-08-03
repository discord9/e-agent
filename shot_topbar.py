#!/usr/bin/env python3
"""手机端顶部状态栏截图 + 测量脚本（真实本地 server, 390x844 iPhone 级）。

用法: uv run --with playwright python shot_topbar.py <outdir> <token>
输出: outdir/list_before.png outdir/chat_before.png (整页)
      outdir/topbar_list_before.png outdir/topbar_chat_before.png (顶栏区域)
      outdir/metrics_before.json (顶栏/状态行高度与关键样式)
"""
import json
import sys
from pathlib import Path

EXE = ("/mnt/nvme_rust/cargo-home/playwright-browsers/chromium_headless_shell-1228/"
       "chrome-headless-shell-linux64/chrome-headless-shell")
MOBILE_UA = ("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) "
             "AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 "
             "Mobile/15E148 Safari/604.1")
BASE = "http://127.0.0.1:18777"

def main():
    outdir = Path(sys.argv[1])
    token = sys.argv[2]
    tag = sys.argv[3] if len(sys.argv) > 3 else "x"
    outdir.mkdir(parents=True, exist_ok=True)

    from playwright.sync_api import sync_playwright

    def topbar_metrics(page):
        return page.evaluate("""() => {
          const tb = document.getElementById('topbar');
          const r = tb.getBoundingClientRect();
          const cs = getComputedStyle(tb);
          const ch = document.querySelector('.chat-head');
          const m = {
            topbar: {h: r.height, y: r.y, padding: cs.padding, gap: cs.gap,
                     fontSize: cs.fontSize, lineHeight: cs.lineHeight,
                     flexWrap: cs.flexWrap, overflow: cs.overflow},
          };
          if (ch) {
            const cr = ch.getBoundingClientRect();
            m.chatHead = {h: cr.height, y: cr.y};
          }
          // 顶栏直接子元素高度
          m.children = [...tb.children].map(el => {
            const er = el.getBoundingClientRect();
            const ecs = getComputedStyle(el);
            return {tag: el.tagName, id: el.id, cls: el.className,
                    h: er.height, w: er.width, display: ecs.display,
                    visible: er.width > 0 && er.height > 0};
          });
          return m;
        }""")

    def clip_for(el_id):
        return page.evaluate("""(id) => {
          const el = document.getElementById(id);
          const r = el.getBoundingClientRect();
          return {x: r.x, y: r.y, width: r.width, height: r.height};
        }""", el_id)

    with sync_playwright() as p:
        browser = p.chromium.launch(executable_path=EXE)
        ctx = browser.new_context(
            viewport={"width": 390, "height": 844},
            is_mobile=True, has_touch=True, user_agent=MOBILE_UA,
            device_scale_factor=3,
        )
        page = ctx.new_page()
        page.add_init_script(
            "localStorage.setItem('eagent_token', %s)" % json.dumps(token))
        errors = []
        page.on("pageerror", lambda e: errors.append("pageerror: " + str(e)))
        page.on("console", lambda m: errors.append("console.error: " + m.text)
                if m.type == "error" else None)

        # ---- 列表视图 ----
        page.goto(BASE + "/", wait_until="load")
        page.wait_for_timeout(1500)
        m_list = topbar_metrics(page)
        page.screenshot(path=str(outdir / f"list_{tag}.png"))
        try:
            page.screenshot(path=str(outdir / f"topbar_list_{tag}.png"),
                            clip=clip_for("topbar"))
        except Exception as e:
            print("clip list topbar failed:", e)
        print("LIST topbar h =", round(m_list["topbar"]["h"], 1),
              "children:", [(c["id"] or c["cls"], round(c["h"], 1))
                            for c in m_list["children"] if c["visible"]])

        # ---- 聊天视图：新建会话 ----
        page.click("#newSessionBtn")
        page.wait_for_function("() => state.view === 'chat'", timeout=8000)
        page.wait_for_timeout(1200)
        m_chat = topbar_metrics(page)
        page.screenshot(path=str(outdir / f"chat_{tag}.png"))
        try:
            page.screenshot(path=str(outdir / f"topbar_chat_{tag}.png"),
                            clip=clip_for("topbar"))
        except Exception as e:
            print("clip chat topbar failed:", e)
        print("CHAT topbar h =", round(m_chat["topbar"]["h"], 1),
              "chatHead h =", round((m_chat.get("chatHead") or {}).get("h", 0), 1),
              "children:", [(c["id"] or c["cls"], round(c["h"], 1))
                            for c in m_chat["children"] if c["visible"]])

        with open(outdir / f"metrics_{tag}.json", "w") as f:
            json.dump({"list": m_list, "chat": m_chat, "js_errors": errors},
                      f, indent=1, ensure_ascii=False)
        browser.close()

if __name__ == "__main__":
    main()
