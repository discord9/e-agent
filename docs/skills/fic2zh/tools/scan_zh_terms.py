#!/usr/bin/env python3
"""scan_zh_terms.py — 通用术语扫描器：适配 Vox/Colossus 的 {chapter, segments:[{id,en,zh}]} 结构。

用法：
  python3 scan_zh_terms.py <json_dir> <glossary_csv> [--out report.md]

从 segments 的 zh 字段提取「中文（English）」注记对，聚合同一英文词的不同中文译名，
输出疑似冲突报告（不修改任何文件）。
"""
import json, glob, os, re, sys, collections, argparse

NOTE_RE = re.compile(r'([\u4e00-\u9fff]{1,12})[（(]([A-Za-z][A-Za-z0-9 \-\.\'’]{1,60})[)）]')

def load_json_files(d):
    data = {}
    for f in sorted(glob.glob(os.path.join(d, '*.json'))):
        try:
            raw = json.load(open(f, encoding='utf-8'))
            if isinstance(raw, dict) and 'segments' in raw:
                for seg in raw['segments']:
                    data[f"{os.path.basename(f)}::{seg['id']}"] = seg
            elif isinstance(raw, list):
                for i, seg in enumerate(raw):
                    data[f"{os.path.basename(f)}::{i}"] = seg
        except Exception as e:
            print(f'!! {f}: {e}', file=sys.stderr)
    return data

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('json_dir')
    ap.add_argument('glossary')
    ap.add_argument('--out', default=None)
    args = ap.parse_args()

    data = load_json_files(args.json_dir)
    print(f'加载 segments: {len(data)}')

    # 术语表既有译名（作为已知项）
    known = {}
    for row in csv_reader(args.glossary):
        if len(row) >= 2 and row[0] and not row[0].startswith('#'):
            known[row[0].strip().lower()] = row[1].strip()

    # 注记对聚合
    pairs = collections.defaultdict(set)  # en_lower -> {(zh, fname)}
    for fname, seg in data.items():
        zh = seg.get('zh', '')
        for m in NOTE_RE.finditer(zh):
            zhw, en = m.group(1), m.group(2)
            if len(zhw) < 2:
                continue
            pairs[en.strip().lower()].add((zhw, fname))

    # 冲突 = 同一英文词 ≥2 种中文
    conflicts = []
    for en_l in sorted(pairs):
        zhset = {zhw for zhw, _ in pairs[en_l]}
        if len(zhset) >= 2:
            conflicts.append((en_l, sorted(zhset), sorted(set(f for _, f in pairs[en_l]))))

    lines = []
    lines.append(f'# 术语扫描报告（{os.path.basename(args.json_dir)}）\n')
    lines.append(f'- segments: {len(data)}\n')
    lines.append(f'- 注记对: {sum(len(v) for v in pairs.values())} 条\n')
    lines.append(f'- 疑似冲突: {len(conflicts)} 组\n')
    lines.append('## 疑似多译名冲突\n')
    for en_l, zhset, files in conflicts:
        lines.append(f'- **{en_l}**: {" / ".join(zhset)}  ← {files[:6]}')
    if not conflicts:
        lines.append('（未发现）')
    out = '\n'.join(lines)
    print(out)
    if args.out:
        with open(args.out, 'w', encoding='utf-8') as f:
            f.write(out + '\n')
        print(f'报告: {args.out}')

def csv_reader(p):
    import csv
    try:
        with open(p, encoding='utf-8') as f:
            return list(csv.reader(f))
    except FileNotFoundError:
        return []

if __name__ == '__main__':
    main()
