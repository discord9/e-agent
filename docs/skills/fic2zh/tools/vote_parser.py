#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Vox Vitae 章节投票/讨论概览解析器。

扫描 .svtmp/pages/page-*.html（论坛已下载页面），提取：
  * threadmark 列表（帖子 id、标题、作者、时间）
  * 投票 threadmark（"Turn XX vote closed"）中的官方投票统计：
    每个方案 {方案名, 票数, 投票人列表, 方案内容}
输出：
  * vox_vitae_zh/votes.json        全部投票结构化数据
  * vox_vitae_zh/votes_overview.md 人类可读的逐章投票概览

用法: python3 vote_parser.py [--pages DIR] [--out-json PATH] [--out-md PATH]
"""
import argparse
import glob
import html as html_mod
import json
import os
import re

TM_RE = re.compile(
    r'<span id="threadmark-\d+" class="threadmarkLabel[^"]*"[^>]*>(.*?)</span>',
    re.S,
)
POST_RE = re.compile(r'id="js-post-(\d+)"')
VOTE_COUNT_RE = re.compile(
    r'(\d+)\s+people?\s+have voted\s*\n\s*\[X\]\s*(?:Plan:\s*)?(.+?)(?=\n\s*\d+\s+people?\s+have voted|\Z)',
    re.S,
)
VOTE_HEADER_RE = re.compile(
    r'Scheduled vote count started by (\w+) on (.+?),(?:\s*at)?\s*(.+?), finished with (\d+) posts? and (\d+) votes?\.?',
    re.S,
)


def unescape(s: str) -> str:
    return html_mod.unescape(re.sub(r'<[^>]+>', '', s)).strip()


def parse_page(path: str):
    """返回 [(post_id, title, author, text)] threadmark 列表。"""
    raw = open(path, encoding='utf-8', errors='replace').read()
    posts = [(m.start(), m.group(1)) for m in POST_RE.finditer(raw)]
    tms = [(m.start(), m.group(1)) for m in TM_RE.finditer(raw)]
    out = []
    for i, (pos, pid) in enumerate(posts):
        end = posts[i + 1][0] if i + 1 < len(posts) else len(raw)
        seg = raw[pos:end]
        author = re.search(r'data-author="([^"]*)"', seg)
        title = ''
        for tp, tt in tms:
            if pos <= tp < end:
                title = unescape(tt)
                break
        if not title:
            continue
        # 提取正文
        body = re.search(r'class="bbWrapper">(.*?)(?:<div class="message-footer|$)', seg, re.S)
        text = unescape(body.group(1)) if body else ''
        out.append((pid, title, author.group(1) if author else '?', text))
    return out


def parse_vote(text: str):
    """从投票 threadmark 正文提取统计。返回 dict 或 None。"""
    hdr = VOTE_HEADER_RE.search(text)
    meta = {}
    if hdr:
        meta = {
            'started_by': hdr.group(1),
            'date': hdr.group(2).strip(),
            'time': hdr.group(3).strip(),
            'posts': int(hdr.group(4)),
            'votes': int(hdr.group(5)),
        }
    plans = []
    for m in VOTE_COUNT_RE.finditer(text):
        n = int(m.group(1))
        rest = m.group(2)
        lines = [l.strip() for l in rest.split('\n') if l.strip()]
        if not lines:
            continue
        name = lines[0]
        content = [l for l in lines[1:] if re.match(r'^[-–]+\[', l) or re.match(r'^\[', l)]
        voters = [l for l in lines[1:] if not re.match(r'^[-–]*\[', l)]
        # 清洗：去掉垃圾行（下一方案残留、系统文本、纯数字、HTML 痕迹）
        junk = re.compile(
            r'people have voted|\[X\]|Click to expand|Last edited|Reader mode|'
            r'^\d+$|^<article|^data-|^\[X\]|Scheduled vote count|Writing is underway|'
            r'^Voting$|^\d+ people|^\d+$|Nov \d+, \d{4}|\d+:\d+ [AP]M'
        )
        voters = [v for v in voters if not junk.search(v)]
        voters = voters[:n]  # 至多 n 个（插件名单可能混入下一方案残留）
        plans.append({'plan': name, 'votes': n, 'voters': voters, 'content': content})
    if not plans:
        return None
    return {'meta': meta, 'plans': plans}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--pages', default=os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '.svtmp', 'pages'))
    ap.add_argument('--out-json', default=os.path.join(os.path.dirname(os.path.abspath(__file__)), 'vox_vitae_zh', 'votes.json'))
    ap.add_argument('--out-md', default=os.path.join(os.path.dirname(os.path.abspath(__file__)), 'vox_vitae_zh', 'votes_overview.md'))
    args = ap.parse_args()

    files = sorted(glob.glob(os.path.join(args.pages, 'page-*.html')),
                   key=lambda p: int(re.search(r'page-(\d+)', os.path.basename(p)).group(1)))
    all_tm = []
    for f in files:
        for tm in parse_page(f):
            all_tm.append(tm)
    # 按帖子 id 排序（时间序）
    all_tm.sort(key=lambda t: int(t[0]))

    votes = []
    for pid, title, author, text in all_tm:
        tl = title.lower()
        if 'vote' in tl and ('closed' in tl or 'scheduled' in tl or 'vote count' in tl):
            parsed = parse_vote(text)
            if parsed:
                votes.append({
                    'post_id': pid,
                    'threadmark': title,
                    'author': author,
                    'plans': parsed['plans'],
                    'meta': parsed['meta'],
                })

    # 讨论热度：按帖子 id 顺序，threadmark 帖与其后回复归组（下一 threadmark 前）
    all_posts = []
    for f in files:
        raw = open(f, encoding='utf-8', errors='replace').read()
        for m in POST_RE.finditer(raw):
            pid = m.group(1)
            seg = raw[m.start():]
            author = re.search(r'data-author="([^"]*)"', seg)
            all_posts.append((int(pid), author.group(1) if author else '?'))
    all_posts.sort()
    tm_ids = {int(t[0]) for t in all_tm}
    discussion = []
    cur = None
    for pid, author in all_posts:
        if pid in tm_ids:
            cur = {'post_id': str(pid), 'replies': 0, 'authors': set()}
            discussion.append(cur)
        elif cur is not None:
            cur['replies'] += 1
            cur['authors'].add(author)
    for d in discussion:
        d['authors'] = sorted(d['authors'])
        d['reply_count'] = d.pop('replies')
    # 关联 threadmark 标题
    title_by_id = {int(t[0]): t[1] for t in all_tm}
    for d in discussion:
        d['title'] = title_by_id.get(int(d['post_id']), '?')

    os.makedirs(os.path.dirname(args.out_json), exist_ok=True)
    with open(args.out_json, 'w', encoding='utf-8') as f:
        json.dump({'total_threadmarks': len(all_tm), 'votes': votes,
                   'discussion': discussion},
                  f, ensure_ascii=False, indent=2)

    # Markdown 概览
    lines = ['# Vox Vitae 章节投票概览', '',
             '> 数据来源：作者 Neablis 发布的 "Vote closed" threadmark（论坛官方投票统计）。',
             f'> 共 {len(votes)} 场投票。', '']
    for v in votes:
        lines.append(f'## {v["threadmark"]}')
        m = v['meta']
        if m:
            lines.append(f'> 发起：{m["started_by"]} · {m["date"]} {m["time"]} · {m["posts"]} 帖 · {m["votes"]} 票')
        plans = sorted(v['plans'], key=lambda x: -x['votes'])
        for i, p in enumerate(plans):
            crown = ' 🏆' if i == 0 and len(plans) > 1 and p['votes'] > plans[1]['votes'] else ''
            voters = '、'.join(p['voters']) if p['voters'] else '（无记录）'
            lines.append(f'- **{p["plan"]}**：{p["votes"]} 票{crown}')
            if len(p['voters']) != p['votes']:
                lines.append(f'  - 投票人（名单解析不完全，{len(p["voters"])}/{p["votes"]}）：{voters}')
            else:
                lines.append(f'  - 投票人：{voters}')
        lines.append('')
    with open(args.out_md, 'w', encoding='utf-8') as f:
        f.write('\n'.join(lines))

    print(f'threadmark 总数: {len(all_tm)}')
    print(f'投票 threadmark: {len(votes)}')
    print(f'已输出: {args.out_json}')
    print(f'已输出: {args.out_md}')


if __name__ == '__main__':
    main()
