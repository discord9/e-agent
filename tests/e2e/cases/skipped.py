#!/usr/bin/env python3
"""TODO 用例登记（未合入功能，--list 可见、--all 不运行）。

曾登记 4 个 TODO（sidebar_squeeze / sidebar_persist / pin_button / conflict_card），
现已全部合入 main 并转正为正式用例：
  * sidebar_squeeze / sidebar_persist -> cases/sidebar.py
  * pin_button                        -> cases/listview.py（list_pin）
  * conflict_card                     -> cases/chat.py（按现状：仅 statusLabel
    Failed->失败 合入，无 .msg-error.conflict 友好卡片）

未来发现「功能未合入、写用例必然 FAIL」的新功能时，再在此登记。
"""
CASES = []
