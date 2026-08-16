#!/usr/bin/env python3
"""Vox Vitae 中译工作流 CLI 辅助工具（纯 Python 标准库，无第三方依赖）。

子命令: detect(--text/--file/stdin [--glossary] [--json]) 检测文本中的术语表词语;
extract(--html --post [--out]) 从 XenForo 帖子 HTML 提取正文纯文本(去 spoiler);
append(--id --en --zh [--memory] [--glossary] [--archive] [--summary 摘要] [--json-out 章级JSON]) 追加翻译记录到记忆;
summarize([--memory] [--out]) 从 memory.jsonl 聚合生成分层记忆 chapters.json(L0 全篇脉络/L1 章节回顾);
context(--segment [--memory] [--glossary] [--history N] [--chapters] [--chapter]) 生成翻译 subagent 任务文本;
render(--json [--title] [--out]) 将章级对照 JSON 渲染为中英对照 Markdown。
示例: python3 translate_tool.py detect --text "The Tech-Priest studied scrapcode"
"""
import argparse, csv, json, os, re, sys
from datetime import datetime
from html.parser import HTMLParser

SD = os.path.dirname(os.path.abspath(__file__))
# 术语表默认解析：脚本同目录 glossary.csv -> 同目录实际文件名 -> 上一级实际文件名
GLOSS = [os.path.join(SD, 'glossary.csv'),
         os.path.join(SD, 'vox_vitae_40k_glossary_zh-CN-1.csv'),
         os.path.join(SD, os.pardir, 'vox_vitae_40k_glossary_zh-CN-1.csv')]
MEM = os.path.join(SD, 'memory.jsonl')
CHAPS = os.path.join(SD, 'chapters.json')
SEG_ID = re.compile(r'^(turn\d+(?:_\d+)?)(?:-seg(\d+))?$')
DO = re.compile(r'<div(?=[\s>])', re.I)
DC = re.compile(r'</div\s*>', re.I)
BW = re.compile(r'<div\b[^>]*\bclass="[^"]*bbWrapper[^"]*"[^>]*>', re.I)
SP = re.compile(r'<div\b[^>]*\bclass="[^"]*bbCodeSpoiler[^"]*"[^>]*>', re.I)
REQ = ('[要求] 忠实翻译；术语必须使用术语表译名；专有名词（人名地名船名）首次出现可加音译注；'
       '保留引号/引用块（>）与段落结构；游戏数值/掷骰/技术名保持原文数字与缩略语（如 RP、BP、HP）不译；'
       '只输出译文，不要任何解释或前缀。')

def err(msg):
    sys.stderr.write('错误: %s\n' % msg)
    return 1

def read_text(path, errors=None):
    with open(path, encoding='utf-8', errors=errors) as f:
        return f.read()

def get_glossary(explicit):
    """返回 [(source, target)]（按长度降序、source 去重）或 None（未找到术语表）。"""
    if explicit and not os.path.isfile(explicit):
        return None
    path = explicit or next((c for c in GLOSS if os.path.isfile(c)), None)
    if path is None:
        return None
    terms, seen = [], set()
    with open(path, encoding='utf-8-sig', newline='') as f:
        for row in list(csv.reader(f))[1:]:
            if len(row) >= 2 and row[0].strip() and row[0].strip() not in seen:
                seen.add(row[0].strip())
                terms.append((row[0].strip(), row[1].strip()))
    terms.sort(key=lambda t: len(t[0]), reverse=True)
    return terms

def detect_terms(text, glossary):
    """返回 [(term, target)]，按文本出现顺序；重叠区间跳过、同一术语只报一次。"""
    intervals, found = [], []
    for src, tgt in glossary:
        pat = re.compile(r'(?<![A-Za-z0-9])' + re.escape(src) + r'(?![A-Za-z0-9])', re.I)
        for m in pat.finditer(text):
            if any(m.start() < b and a < m.end() for a, b in intervals):
                continue
            intervals.append(m.span())
            found.append((m.start(), src, tgt))
            break
    return [(t, g) for _, t, g in sorted(found)]

def cmd_detect(args):
    if args.text is not None and args.file:
        return err('--text 与 --file 只能二选一')
    try:
        text = args.text if args.text is not None \
            else read_text(args.file) if args.file else sys.stdin.read()
    except OSError as e:
        return err('无法读取文件: %s' % e)
    glossary = get_glossary(args.glossary)
    if glossary is None:
        return err('未找到术语表（可用 --glossary 指定路径）')
    matched = detect_terms(text, glossary)
    if args.json:
        sys.stdout.write(json.dumps({'matched': [{'term': t, 'target': g} for t, g in matched],
                                     'count': len(matched)}, ensure_ascii=False) + '\n')
    elif not matched:
        sys.stdout.write('# 无匹配术语\n')
    else:
        sys.stdout.write(''.join('%s → %s\n' % (t, g) for t, g in matched)
                         + '# 共 %d 个术语\n' % len(matched))
    return 0

def div_span(frag, start):
    """从 <div 开标签做平衡深度扫描，返回 (start,end) 覆盖整个 div（含标签）；不平衡返回 None。"""
    depth, pos = 1, start + 1
    while True:
        m1, m2 = DO.search(frag, pos), DC.search(frag, pos)
        if m2 is None:
            return None
        if m1 is not None and m1.start() < m2.start():
            depth, pos = depth + 1, m1.end()
        else:
            depth -= 1
            if depth == 0:
                return (start, m2.end())
            pos = m2.end()

def merge_intervals(ivs):
    ivs = sorted(ivs)
    out = [list(ivs[0])] if ivs else []
    for s, e in ivs[1:]:
        if s <= out[-1][1]:
            out[-1][1] = max(out[-1][1], e)
        else:
            out.append([s, e])
    return [(a, b) for a, b in out]

class TextExtractor(HTMLParser):
    """提取纯文本：跳过 script/style；块级标签产生换行。"""
    BLOCK = {'div', 'p', 'br', 'blockquote', 'li', 'ul', 'ol', 'h1', 'h2', 'h3',
             'h4', 'h5', 'h6', 'hr', 'table', 'tr', 'td', 'th', 'pre', 'section'}

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.parts, self.skip = [], 0

    def _tag(self, tag, closing=False):
        tag = tag.lower()
        if tag in ('script', 'style'):
            self.skip = max(0, self.skip + (-1 if closing else 1))
        elif self.skip == 0 and tag in self.BLOCK:
            self.parts.append('\n')

    def handle_starttag(self, tag, attrs):
        self._tag(tag)

    def handle_startendtag(self, tag, attrs):
        if self.skip == 0 and tag.lower() in self.BLOCK:
            self.parts.append('\n')

    def handle_endtag(self, tag):
        self._tag(tag, closing=True)

    def handle_data(self, data):
        if self.skip == 0:
            self.parts.append(data)

    def text(self):
        return ''.join(self.parts)

def clean_text(text):
    """行内空白折叠为单空格；连续空行压缩为最多一个；strip 首尾。"""
    out, blank = [], False
    for ln in (re.sub(r'\s+', ' ', l).strip() for l in text.split('\n')):
        if not ln:
            if blank:
                continue
            blank = True
        else:
            blank = False
        out.append(ln)
    return '\n'.join(out).strip()

def cmd_extract(args):
    try:
        html_text = read_text(args.html, errors='replace')
    except OSError as e:
        return err('无法读取 %s: %s' % (args.html, e))
    marker = 'id="js-post-%s"' % args.post
    pos = html_text.find(marker)
    if pos < 0:
        return err('未找到帖子 %s 的标记 %s' % (args.post, marker))
    a0 = html_text.rfind('<article', 0, pos)
    a1 = html_text.find('</article>', pos)
    if a0 < 0 or a1 < 0:
        return err('帖子 %s 前后未找到 <article/</article>' % args.post)
    frag = html_text[a0:a1 + len('</article>')]
    m = BW.search(frag)
    if not m:
        return err('帖子 %s 中未找到 bbWrapper 容器' % args.post)
    body = div_span(frag, m.start())
    if body is None:
        return err('bbWrapper div 标签不平衡')
    skips = merge_intervals([sp for sm in SP.finditer(frag, body[0], body[1])
                             if (sp := div_span(frag, sm.start())) is not None])
    parser, cur = TextExtractor(), body[0]
    for s, e in skips:
        if s > cur:
            parser.feed(frag[cur:s])
        cur = max(cur, e)
    if cur < body[1]:
        parser.feed(frag[cur:body[1]])
    parser.close()
    out = clean_text(parser.text()) + '\n'
    try:
        if args.out:
            with open(args.out, 'w', encoding='utf-8') as f:
                f.write(out)
        else:
            sys.stdout.write(out)
    except OSError as e:
        return err('无法写入: %s' % e)
    return 0

def split_paras(text):
    return [p.strip() for p in text.split('\n\n') if p.strip()]


def load_pairs(pairs_dir, sid):
    """读取语义配对 JSON（若有）。返回 pairs 列表或 None。"""
    if not pairs_dir:
        return None
    path = os.path.join(pairs_dir, sid.replace('/', '_') + '.json')
    if not os.path.exists(path):
        return None
    try:
        with open(path, encoding='utf-8') as f:
            data = json.load(f)
    except (OSError, ValueError):
        return None
    return list(data.values())[0] if isinstance(data, dict) else None

def cmd_render(args):
    try:
        with open(args.json, encoding='utf-8') as f:
            data = json.load(f)
    except (OSError, ValueError) as e:
        return err('无法读取 %s: %s' % (args.json, e))
    title = args.title if args.title is not None else data.get('chapter', '')
    segs = data.get('segments', [])
    pairs_dir = getattr(args, 'pairs', None)
    zh_only = getattr(args, 'zh_only', False)
    if len(segs) == 1 and re.match(r'^turn\d+$', segs[0].get('id', '')):
        # 整章单段（如 turn22）：zh 为整章译文，直接输出全文，不做 en/zh 交错
        zh = (segs[0].get('zh') or '').strip()
        zh = re.sub(r'^#\s*' + re.escape(title) + r'\s*\n+', '', zh, flags=re.I)
        out = ['# %s' % title, '', zh]
    else:
        out = ['# %s' % title]
        for seg in segs:
            en = split_paras(seg.get('en', ''))
            zh = split_paras(seg.get('zh', ''))
            if zh_only:
                # 纯中文版：只输出中文段，段间空行分隔
                for z in zh:
                    out.append(z)
                    out.append('')
                continue
            pairs = seg.get('pairs') or load_pairs(pairs_dir, seg.get('id', ''))
            if pairs:
                # 语义配对渲染：按配对组输出，中文在前
                for p in pairs:
                    for zj in p['zh']:
                        out.append(zh[zj])
                    for ei in p['en']:
                        out.append(en[ei])
                    out.append('')
            else:
                n = min(len(en), len(zh))
                for i in range(n):
                    out += [zh[i], en[i], '']  # 中文在前、英文对照在后，对与对之间空一行
                for i in range(n, len(en)):
                    out += ['<!-- 未配对段落 -->', en[i], '']
                for i in range(n, len(zh)):
                    out += ['<!-- 未配对段落 -->', zh[i], '']
    text = '\n'.join(out).rstrip() + '\n'
    try:
        if args.out:
            with open(args.out, 'w', encoding='utf-8') as f:
                f.write(text)
        else:
            sys.stdout.write(text)
    except OSError as e:
        return err('无法写入: %s' % e)
    return 0

def cmd_summarize(args):
    """从 memory.jsonl 聚合分层记忆：按 id 分组，输出 L0 脉络 + L1 段落回顾。"""
    groups = {}
    for e in load_memory(args.memory):
        sid, summary = e.get('id') or '', e.get('summary')
        m = SEG_ID.match(sid)
        if not m or not summary:
            continue
        g = groups.setdefault(m.group(1), {'segs': [], 'plain': None})
        if m.group(2) is None:
            g['plain'] = summary
        else:
            g['segs'].append((int(m.group(2)), {'id': sid, 'summary': summary}))
    chapters = []
    for chapter in sorted(groups):
        g = groups[chapter]
        segs = [e for _, e in sorted(g['segs'])]
        if segs:
            chapter_summary = '\n'.join('seg%d: %s' % (n, e['summary'])
                                        for n, e in sorted(g['segs']))
        else:
            chapter_summary = g['plain'] or ''
        one_line = re.sub(r'\s+', ' ', chapter_summary).strip()
        if len(one_line) > 120:
            one_line = one_line[:120] + '…'
        chapters.append({'chapter': chapter, 'segments': segs,
                         'chapter_summary': chapter_summary, 'one_line': one_line})
    try:
        with open(args.out, 'w', encoding='utf-8') as f:
            json.dump(chapters, f, ensure_ascii=False, indent=2)
    except OSError as e:
        return err('无法写入: %s' % e)
    sys.stdout.write('wrote %s (%d chapters)\n' % (args.out, len(chapters)))
    return 0

def load_chapters(path):
    """读 chapters.json（顶层数组）；文件缺失或非法时返回 None。"""
    if not os.path.isfile(path):
        return None
    try:
        with open(path, encoding='utf-8') as f:
            data = json.load(f)
    except (OSError, ValueError):
        return None
    return data if isinstance(data, list) else None

def build_chapter_blocks(chapters, chapter):
    """构造 L0 全篇背景 / L1 本章回顾文本块（无内容时为空字符串）。"""
    l0 = l1 = ''
    prev = sorted((c for c in chapters if c.get('chapter') and c['chapter'] < chapter),
                  key=lambda c: c['chapter'], reverse=True)[:3]
    if prev:
        l0 = '[全篇背景] 全篇脉络（最近 3 章）：\n' + ''.join(
            '- %s: %s\n' % (c['chapter'], c.get('one_line', '')) for c in prev)
    cur = next((c for c in chapters if c.get('chapter') == chapter), None)
    segs = cur.get('segments') if cur else None
    if not segs:
        l1 = '[本章回顾] 本章尚无已译段落。\n'
    else:
        l1 = '[本章回顾] 本章已译各段概要（%s）：\n' % chapter + ''.join(
            '- %s: %s\n' % (s.get('id', '?').split('-', 1)[-1],
                            (s.get('summary') or '')[:80]) for s in segs)
    return l0, l1

def chapter_of(seg_id):
    return seg_id.partition('-')[0]

def upsert_segment(path, seg_id, en, zh):
    """章级对照 JSON 聚合：按 id upsert 段，segments 按 id 排序后写回。"""
    if os.path.isfile(path):
        with open(path, encoding='utf-8') as f:
            data = json.load(f)
    else:
        data = {}
    data.setdefault('chapter', chapter_of(seg_id))
    segs = data.setdefault('segments', [])
    for s in segs:
        if s.get('id') == seg_id:
            s['en'], s['zh'] = en, zh
            break
    else:
        segs.append({'id': seg_id, 'en': en, 'zh': zh})
    data['segments'] = sorted(segs, key=lambda s: str(s.get('id', '')))
    with open(path, 'w', encoding='utf-8') as f:
        json.dump(data, f, ensure_ascii=False, indent=2)

def cmd_append(args):
    try:
        en, zh = read_text(args.en), read_text(args.zh)
    except OSError as e:
        return err('无法读取文件: %s' % e)
    glossary = get_glossary(args.glossary)
    if glossary is None:
        return err('未找到术语表（可用 --glossary 指定路径）')
    terms = [tgt for _, tgt in detect_terms(en, glossary)]
    entry = {'id': args.id, 'en_path': args.en, 'zh_path': args.zh,
             'en_chars': len(en), 'zh_chars': len(zh), 'terms': terms,
             'ts': datetime.now().astimezone().isoformat()}
    if args.summary:
        entry['summary'] = args.summary
    try:
        with open(args.memory, 'a', encoding='utf-8') as f:
            f.write(json.dumps(entry, ensure_ascii=False) + '\n')
        if args.archive:
            with open(args.archive, 'a', encoding='utf-8') as f:
                f.write('## %s\n\n%s\n\n<!-- en: %s | terms: %s -->\n'
                        % (args.id, zh, args.en, ', '.join(terms)))
        if args.json_out:
            upsert_segment(args.json_out, args.id, en, zh)
    except (OSError, ValueError) as e:
        return err('无法写入: %s' % e)
    line = 'appended %s (en %d chars, zh %d chars, %d terms' \
           % (args.id, len(en), len(zh), len(terms))
    if args.summary:
        line += ', summary %d chars' % len(args.summary)
    if args.json_out:
        line += ', json %s' % args.json_out
    sys.stdout.write(line + ')\n')
    return 0
def load_memory(path):
    entries = []
    if os.path.isfile(path):
        with open(path, encoding='utf-8') as f:
            for line in f:
                line = line.strip()
                if line:
                    try:
                        entries.append(json.loads(line))
                    except ValueError:
                        pass
    return entries

def excerpt(path, limit):
    try:
        return re.sub(r'\s+', ' ', read_text(path)).strip()[:limit]
    except OSError:
        return '(无法读取: %s)' % path

def cmd_context(args):
    try:
        segment = read_text(args.segment)
    except OSError as e:
        return err('无法读取 %s: %s' % (args.segment, e))
    glossary = get_glossary(args.glossary)
    matched = detect_terms(segment, glossary) if glossary else []
    terms = '\n'.join('- %s → %s' % (t, g) for t, g in matched) if matched else '未检测到术语。'
    memory = load_memory(args.memory)
    if memory and args.history > 0:
        blocks = []
        for e in memory[-args.history:]:
            try:
                zh = read_text(e['zh_path'])
            except (OSError, KeyError):
                zh = '(无法读取: %s)' % e.get('zh_path', '?')
            if len(zh) > 1500:
                zh = zh[:1500] + '……(截断)'
            title = e.get('summary')
            if title:
                blocks.append('--- %s %s ---\n%s\n---'
                              % (e.get('id', '?'), title[:80], zh))
            else:
                blocks.append('--- %s (%s) ---\n%s\n---'
                              % (e.get('id', '?'), excerpt(e.get('en_path', ''), 80), zh))
        history = '\n'.join(blocks)
    else:
        history = '无历史记忆。'
    l0 = l1 = ''
    if args.chapter:
        chapters = load_chapters(args.chapters)
        if chapters:
            l0, l1 = build_chapter_blocks(chapters, args.chapter)
    sys.stdout.write('[术语表] 本次待译片段中检测到的术语（必须使用下列译名，不得自行创造）：\n%s\n' % terms)
    if l0:
        sys.stdout.write(l0)
    if l1:
        sys.stdout.write(l1)
    sys.stdout.write('[上文] 之前已翻译的段落（用于保持一致；若与术语表冲突以术语表为准）：\n%s\n'
                     '[当前待译片段] 请将以下英文翻译为简体中文：\n%s\n%s\n' % (history, segment, REQ))
    return 0

def main(argv=None):
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding='utf-8')
        except (AttributeError, OSError):
            pass
    parser = argparse.ArgumentParser(prog='translate_tool', description='Vox Vitae 中译工作流辅助工具。')
    sub = parser.add_subparsers(dest='command', metavar='COMMAND', required=True)
    p = sub.add_parser('detect', help='检测文本中的术语')
    p.add_argument('--text'); p.add_argument('--file')
    p.add_argument('--glossary'); p.add_argument('--json', action='store_true')
    p.set_defaults(func=cmd_detect)
    p = sub.add_parser('extract', help='从帖子 HTML 提取正文')
    p.add_argument('--html', required=True); p.add_argument('--post', required=True)
    p.add_argument('--out')
    p.set_defaults(func=cmd_extract)
    p = sub.add_parser('append', help='追加翻译记录到记忆文件')
    p.add_argument('--id', required=True); p.add_argument('--en', required=True)
    p.add_argument('--zh', required=True); p.add_argument('--memory', default=MEM)
    p.add_argument('--glossary'); p.add_argument('--archive')
    p.add_argument('--summary'); p.add_argument('--json-out')
    p.set_defaults(func=cmd_append)
    p = sub.add_parser('summarize', help='从 memory.jsonl 聚合生成分层记忆 chapters.json（L0/L1）')
    p.add_argument('--memory', default=MEM)
    p.add_argument('--out', default=CHAPS)
    p.set_defaults(func=cmd_summarize)
    p = sub.add_parser('context', help='生成翻译 subagent 任务文本')
    p.add_argument('--segment', required=True); p.add_argument('--memory', default=MEM)
    p.add_argument('--glossary'); p.add_argument('--history', type=int, default=3)
    p.add_argument('--chapters', default=CHAPS); p.add_argument('--chapter')
    p.set_defaults(func=cmd_context)
    p = sub.add_parser('render', help='渲染章级对照 JSON 为中英对照 Markdown')
    p.add_argument('--json', required=True); p.add_argument('--out')
    p.add_argument('--title'); p.add_argument('--pairs')
    p.add_argument('--zh-only', action='store_true', help='仅输出中文（无英文对照）')
    p.set_defaults(func=cmd_render)
    args = parser.parse_args(argv)
    return args.func(args)

if __name__ == '__main__':
    sys.exit(main())
