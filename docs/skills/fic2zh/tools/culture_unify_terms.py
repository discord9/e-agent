#!/usr/bin/env python3
"""culture_unify_terms.py — 按 glossary_additions.csv 裁定值统一 culture_zh/json/*.json 中的多译名。

- 只改 zh 字段（en 永不改动）
- 替换规则从 glossary_additions.csv 的 note 列"曾译××"提取（若未提取到则跳过该词）
- 精确子串替换；按 target 长度降序执行，避免子串嵌套误替换
- 替换前打印每个词的命中统计，替换后再次统计确认清零
"""
import csv, glob, json, re, sys, os

ROOT = os.path.dirname(os.path.abspath(__file__))
ADD = os.path.join(ROOT, 'culture_zh', 'glossary_additions.csv')

def load_rules():
    rules = []  # (bad, good)
    with open(ADD, encoding='utf-8') as f:
        for row in csv.reader(f):
            if not row or row[0].startswith('#'):
                continue
            if len(row) < 4:
                continue
            note = row[3] or ''
            # note 形如：曾译"煎饼机"（ch48）/"薄饼机"（ch53），统一为压饼器
            m = re.search(r'曾译(.+?)(?:统一|$)', note)
            if not m:
                continue
            seg = m.group(1)
            # 拆出所有译名（去括号注、分隔符）
            for bad in re.split(r'[「」“”",，、；;（）()/]+', seg):
                bad = bad.strip()
                if not bad or bad == row[1]:
                    continue
                # 去章号注（ch48）已在拆分中去除；防误留
                bad = re.sub(r'ch\d+', '', bad).strip()
                if bad and not bad.isdigit():
                    rules.append((bad, row[1]))
    # 去重、按长度降序
    seen = set()
    out = []
    for bad, good in rules:
        if (bad, good) in seen:
            continue
        seen.add((bad, good))
        # 危险规则：bad 是 good 的子串（如 白翼→白翼号）会把已正确的文本二次替换成 白翼号号，跳过
        if bad in good:
            continue
        out.append((bad, good))
    out.sort(key=lambda x: -len(x[0]))
    return out

def scan(files):
    stats = {}
    for f in files:
        try:
            d = json.load(open(f, encoding='utf-8'))
        except Exception:
            continue
        zh = d[0]['zh']
        for bad, good in rules:
            if bad in zh:
                stats.setdefault((bad, good), []).append(os.path.basename(f))
    return stats

if __name__ == '__main__':
    rules = load_rules()
    print(f'规则数: {len(rules)}')
    for bad, good in rules:
        print(f'  {bad!r} -> {good!r}')
    files = sorted(glob.glob(os.path.join(ROOT, 'culture_zh', 'json', '*.json')))
    print(f'\nJSON 文件数: {len(files)}')
    before = scan(files)
    if not before:
        print('无待统一译名，退出')
        sys.exit(0)
    print(f'\n=== 替换前命中 ===')
    for (bad, good), fl in sorted(before.items(), key=lambda x: -len(x[1])):
        print(f'  {bad!r} -> {good!r}: {len(fl)} 处  {fl[:5]}...')
    n = 0
    for f in files:
        d = json.load(open(f, encoding='utf-8'))
        zh = d[0]['zh']
        new = zh
        for bad, good in rules:
            new = new.replace(bad, good)
        if new != zh:
            d[0]['zh'] = new
            with open(f, 'w', encoding='utf-8') as fh:
                json.dump(d, fh, ensure_ascii=False)
            n += 1
    print(f'\n已修改文件数: {n}')
    after = scan(files)
    print(f'=== 替换后残留 ===')
    for (bad, good), fl in sorted(after.items(), key=lambda x: -len(x[1])):
        print(f'  {bad!r} -> {good!r}: {len(fl)} 处  {fl[:5]}...')
    if after:
        print('\n!! 仍有残留（可能位于 note 或特殊上下文），需人工复核')
        sys.exit(1)
    print('全部统一完成 ✓')
