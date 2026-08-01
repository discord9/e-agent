#!/usr/bin/env python3
"""尚未合入 05fc4c8 的功能：注册为 TODO 用例，--list 可见、--all 不运行。

这些功能在 main（05fc4c8）的 src/ui/app.js 中尚不存在，写用例必然 FAIL；
待对应分支合入后，把 TODO 置空并补上完整用例即可。对应散落脚本：
  * verify_sidebar_persist.py  -> sidebar_persist / sidebar_squeeze
  * web_pin_check.py           -> pin_button
  * verify_conflict.py         -> conflict_card
"""
CASES = [
    {"name": "sidebar_squeeze",
     "desc": "侧边栏挤压布局：桌面打开时内容右移 280px、遮罩隐藏；点消息区不关",
     "todo": "feat/sidebar-persist 未合入 05fc4c8：当前侧边栏是覆盖式（遮罩+translateX），"
             "无挤压布局。合入后参考 verify_sidebar_persist.py 补用例。"},
    {"name": "sidebar_persist",
     "desc": "侧边栏持续打开：切会话/返回列表保持打开 + localStorage 持久化",
     "todo": "feat/sidebar-persist 未合入 05fc4c8：当前 backBtn 会收起侧边栏、无 localStorage。"
             "合入后参考 verify_sidebar_persist.py 补用例。"},
    {"name": "pin_button",
     "desc": "列表/侧边栏 📌 置顶按钮（PUT /api/sessions/{id}/pin，mock pinned 字段）",
     "todo": "feat/pin-frontend 未合入 05fc4c8（05fc4c8 仅合入 pin 后端）。"
             "合入后参考 web_pin_check.py 补用例。"},
    {"name": "conflict_card",
     "desc": "并发写冲突错误友好卡片（.msg-error.conflict + details 折叠）",
     "todo": "feat/conflict-friendly-web 未合入 05fc4c8：appendError 仍是旧样式。"
             "合入后参考 verify_conflict.py 补用例。"},
]
