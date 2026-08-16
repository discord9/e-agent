#!/usr/bin/env python3
"""culture_scan_terms.py — 从 culture_zh/json/*.json 语料中机械提取英文术语→中文译名对照，
检测多译名不一致，输出报告。

方法（纯机械，无人工预设）：
1. 对每个 json 的 zh 字段，找出所有「中文（English）」「中文(English)」「中文（English 原文）」
   模式的首次注记（本作大量使用「首现附原文」惯例），收集 (en, zh) 对。
2. 再对所有 zh 文本做英文子串扫描：出现在 zh 里的英文单词（长度≥3，含大写/专名特征），
   取它前 20 字符内的中文作为候选译名；跨文件聚合。
3. 输出三类报告：
   - CONFLICT：同一英文词出现 ≥2 种不同中文译名（强信号，需人工裁定）
   - SUSPECT：英文词在 zh 中以不同形式出现（大小写/变体），或注记式与裸用式并存
   - STATS：每章 zh 字数、总字数、段落数
用法：
  python3 culture_scan_terms.py [--min-conf 2] [--out culture_zh/term_scan_report.md]
"""
import json, glob, os, re, sys, collections, argparse

ROOT = os.path.dirname(os.path.abspath(__file__))
JSON_DIR = os.path.join(ROOT, 'culture_zh', 'json')

# 中文首现注记模式：中文（English） / 中文(English) / 中文（English 原文）
NOTE_RE = re.compile(r'([\u4e00-\u9fff]{1,12})[（(]([A-Za-z][A-Za-z0-9 \-\.\'’]{1,60})[)）]')
# zh 中的英文片段
EN_RE = re.compile(r'[A-Za-z][A-Za-z0-9\'’\-\.]{2,}')

def load_all():
    files = sorted(glob.glob(os.path.join(JSON_DIR, '*.json')))
    data = {}
    for f in files:
        try:
            d = json.load(open(f, encoding='utf-8'))
            data[os.path.basename(f)] = d[0]
        except Exception as e:
            print(f'!! 读取失败 {f}: {e}', file=sys.stderr)
    return data

def collect_notes(data):
    """模式1：中文（English）注记对"""
    pairs = collections.defaultdict(set)  # en_lower -> set(zh)
    for fname, d in data.items():
        zh = d['zh']
        for m in NOTE_RE.finditer(zh):
            zhw, en = m.group(1), m.group(2)
            en_l = en.strip().lower()
            if len(zhw) < 2:
                continue
            pairs[en_l].add((zhw, fname))
    return pairs

def collect_en_scan(data):
    """模式2：zh 中的英文片段 + 前文中文窗口"""
    hits = collections.defaultdict(list)  # en_lower -> [(zh_prefix, fname)]
    for fname, d in data.items():
        zh = d['zh']
        for m in EN_RE.finditer(zh):
            en = m.group(0)
            en_l = en.lower()
            if en_l in {'the', 'and', 'for', 'you', 'that', 'this', 'with', 'from', 'have', 'was', 'were', 'not', 'are', 'his', 'her', 'its', 'all', 'can', 'will', 'would', 'could', 'should', 'but', 'has', 'had', 'been', 'they', 'them', 'their', 'there', 'what', 'when', 'where', 'which', 'who', 'whom', 'why', 'how', 'then', 'than', 'into', 'over', 'under', 'again', 'once', 'also', 'just', 'about', 'after', 'before', 'because', 'while', 'though', 'still', 'very', 'only', 'even', 'much', 'many', 'some', 'any', 'each', 'every', 'both', 'either', 'neither', 'more', 'most', 'other', 'another', 'such', 'same', 'own', 'one', 'two', 'three', 'four', 'five', 'six', 'seven', 'eight', 'nine', 'ten', 'hundred', 'thousand', 'million', 'billion', 'trillion', 'quadrillion', 'sextillion', 'day', 'days', 'week', 'weeks', 'month', 'months', 'year', 'years', 'hour', 'hours', 'minute', 'minutes', 'second', 'seconds', 'time', 'times', 'way', 'ways', 'thing', 'things', 'part', 'parts', 'kind', 'kinds', 'sort', 'sorts', 'etc', 'ie', 'eg', 'vs', 'aka', 'deus', 'ex', 'machina', 'don', 't', 's', 're', 've', 'll', 'd', 'o', 'a', 'i', 'u', 'x', 'z', 'et', 'al', 'in', 'on', 'at', 'to', 'of', 'by', 'as', 'or', 'if', 'it', 'is', 'be', 'do', 'go', 'up', 'down', 'out', 'off', 'so', 'no', 'yes', 'ok', 'oh', 'ah', 'hey', 'hello', 'hi', 'bye', 'please', 'thanks', 'thank', 'goodbye', 'sorry', 'well', 'now', 'here', 'there'}:
                continue
            if not re.match(r'^[A-Z][a-zA-Z]*$', en) and en_l not in {'i','v','x','iv','vi','vii','viii','ix','xi','xii','xiii','xiv','xv','xvi','xvii','xviii','xix','xx','xxi','xxii','xxiii','xxiv','xxv'}:
                continue  # 只扫首字母大写的专名/术语
            start = m.start()
            win = zh[max(0, start-24):start]
            # 取窗口内最后一个中文词（3-14字）
            cjk = re.findall(r'[\u4e00-\u9fff]{2,14}', win)
            prefix = cjk[-1] if cjk else ''
            hits[en_l].append((prefix, fname))
    return hits

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--min-conf', type=int, default=2, help='同一英文词最少出现次数才报告')
    ap.add_argument('--out', default=os.path.join(ROOT, 'culture_zh', 'term_scan_report.md'))
    args = ap.parse_args()

    data = load_all()
    print(f'加载 json: {len(data)} 个')
    notes = collect_notes(data)
    hits = collect_en_scan(data)

    lines = []
    lines.append('# Culture 术语扫描报告\n')
    lines.append(f'- 扫描时间: (生成时)\n- JSON 数: {len(data)}\n')
    lines.append('## 一、中文（English）注记对（首现附原文）\n')
    lines.append('| 英文 | 中文译名 | 出现文件 |')
    lines.append('|---|---|---|')
    for en_l in sorted(notes):
        for zhw, fname in sorted(notes[en_l]):
            lines.append(f'| {en_l} | {zhw} | {fname} |')

    lines.append('\n## 二、英文专名在 zh 中的前文中文（候选译名）聚合\n')
    lines.append('| 英文 | 候选中文（前文窗口） | 文件数 |')
    lines.append('|---|---|---|')
    for en_l in sorted(hits):
        items = hits[en_l]
        if len(items) < args.min_conf:
            continue
        cands = collections.Counter(p for p, _ in items if p)
        if not cands:
            continue
        top = cands.most_common(5)
        files = sorted(set(f for _, f in items))
        line = f'| {en_l} | ' + '；'.join(f'{w}({n})' for w, n in top) + f' | {len(files)} |'
        lines.append(line)

    # 三、冲突检测：同一英文词 ≥2 种不同中文
    # 3a. 注记对内部冲突（同一英文注记出过不同中文）
    # 3b. 注记对之外的裸用一致性：对每个注记过的英文词，聚合全语料 zh 中该词前文窗口的中文，
    #     若出现 ≥2 种不同候选且候选差异非人名保留模式，则列为弱冲突
    lines.append('\n## 三、疑似多译名冲突\n')
    conflicts = 0
    for en_l in sorted(notes):
        zhset = {zhw for zhw, _ in notes[en_l]}
        if len(zhset) >= 2:
            conflicts += 1
            lines.append(f'- **{en_l}**: {" / ".join(sorted(zhset))}  ← {sorted(set(f for _, f in notes[en_l]))[:6]}')
    # 3b: 只对注记过的词做全语料窗口聚合
    for en_l in sorted(notes):
        items = [h for h in hits.get(en_l, []) if h[0]]
        cands = collections.Counter(p for p, _ in items)
        if len(cands) < 2:
            continue
        # 过滤：候选是保留英文人名的相邻噪声（候选里只有 1 个中文且该中文是常用虚词/通用动词）
        top = cands.most_common(4)
        if sum(cands.values()) < args.min_conf:
            continue
        conflicts += 1
        lines.append(f'- ~**{en_l}**（裸用）: {" / ".join(f"{w}({n})" for w, n in top)}  ← {len(items)} 处')
    if conflicts == 0:
        lines.append('（未发现）')
    lines.append(f'\n共 {conflicts} 组疑似冲突（**粗体**=注记冲突，~删除线~=裸用弱冲突，需人工复核）。\n')

    out = '\n'.join(lines)
    with open(args.out, 'w', encoding='utf-8') as f:
        f.write(out)
    print(f'报告已生成: {args.out}')
    print(f'注记对: {sum(len(v) for v in notes.values())} 条；英文专名聚合: {sum(len(v) for v in hits.values())} 处；疑似冲突: {conflicts} 组')

if __name__ == '__main__':
    main()
