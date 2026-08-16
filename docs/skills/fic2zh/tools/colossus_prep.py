#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Colossus 轻量预处理：纯文本章节 → 切段（复用 prep.clean/segment）。"""
import re, sys, os
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from prep import clean, segment

SRC = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'colossus_zh', 'src')
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'colossus_zh', 'segs')

def main():
    os.makedirs(OUT, exist_ok=True)
    for i in range(1, 10):
        raw = open(os.path.join(SRC, f'ch{i}.txt'), encoding='utf-8').read()
        c = clean(raw)
        # 去掉开头可能的章节标题行（FFN 的 "Chapter N: Title"）
        c = re.sub(r'^Chapter \d+: .+\n', '', c)
        segs = segment(c)
        # 存整章 + 分段
        with open(os.path.join(OUT, f'ch{i}.txt'), 'w', encoding='utf-8') as f:
            f.write(c)
        for j, s in enumerate(segs):
            with open(os.path.join(OUT, f'ch{i}_seg{j}.txt'), 'w', encoding='utf-8') as f:
                f.write(s)
        print(f"ch{i}: {len(c.split())} 词 -> {len(segs)} 段")
    print("完成")

if __name__ == '__main__':
    main()
