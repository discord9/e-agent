#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Chapter preprocessing for Vox Vitae translation workflow.

For each (page, postid, turn) triple:
  1. Locate the post <article>, extract its bbWrapper body (balanced-div scan).
  2. Convert to plain text via html.parser: skip script/style, skip spoiler
     blocks (bbCodeSpoiler), keep blockquotes (newlines around, content only),
     newline at p/div/br/li/h1-h4/tr.
  3. Whitespace cleanup: collapse inline whitespace to single space, collapse
     blank lines to at most one, strip.
  4. Narrative trim: cut at the earliest mechanics marker line.
  5. Segment the narrative into ~1300-1700-word chunks at paragraph
     boundaries (never split a paragraph).
Outputs: turnNN_en.txt and turnNN_segK.txt (UTF-8).
"""
import re
import sys
from html.parser import HTMLParser

# ---------------------------------------------------------------- extraction

BLOCK_TAGS = {'p', 'div', 'br', 'li', 'h1', 'h2', 'h3', 'h4', 'tr'}


class TextExtractor(HTMLParser):
    """Extract plain text, skipping spoiler blocks entirely."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.out = []
        self.skip_depth = 0          # inside script/style
        self.spoiler_depth = 0       # inside spoiler (0 = not in spoiler)
        self._started_spoiler = False

    def _is_spoiler(self, attrs):
        for k, v in attrs:
            if k == 'class' and 'bbCodeSpoiler' in (v or '').split():
                return True
        return False

    def handle_starttag(self, tag, attrs):
        if self.skip_depth:
            self.skip_depth += 1
            return
        if tag in ('script', 'style'):
            self.skip_depth = 1
            return
        if self._is_spoiler(attrs):
            self.spoiler_depth = 1
            return
        if self.spoiler_depth:
            if tag == 'div':
                self.spoiler_depth += 1
            return
        if tag in BLOCK_TAGS:
            self.out.append('\n')
        elif tag == 'blockquote':
            self.out.append('\n')

    def handle_endtag(self, tag):
        if self.skip_depth:
            self.skip_depth -= 1
            return
        if self.spoiler_depth:
            if tag == 'div':
                self.spoiler_depth -= 1
            return
        if tag in BLOCK_TAGS:
            self.out.append('\n')
        elif tag == 'blockquote':
            self.out.append('\n')

    def handle_data(self, data):
        if self.skip_depth or self.spoiler_depth:
            return
        self.out.append(data)


def post_fragment(html, post_id):
    marker = 'id="js-post-%s"' % post_id
    i = html.find(marker)
    if i < 0:
        raise ValueError('post id %s not found' % post_id)
    start = html.rfind('<article', 0, i)
    end = html.find('</article>', i)
    if start < 0 or end < 0:
        raise ValueError('article boundaries not found for post %s' % post_id)
    return html[start:end + len('</article>')]


def bbwrapper_html(fragment):
    j = fragment.find('class="bbWrapper"')
    if j < 0:
        # attribute order may vary; search for bbWrapper token inside a tag
        m = re.search(r'<div[^>]*\bbbWrapper\b', fragment)
        if not m:
            raise ValueError('bbWrapper not found')
        open_start = m.start()
    else:
        open_start = fragment.rfind('<div', 0, j)
    # balanced div scan from the open tag
    depth = 0
    pos = open_start
    pattern = re.compile(r'<div\b|</div\s*>', re.IGNORECASE)
    while True:
        m = pattern.search(fragment, pos)
        if not m:
            raise ValueError('unbalanced div in bbWrapper')
        depth += 1 if m.group(0).lower().startswith('<div') else -1
        pos = m.end()
        if depth == 0:
            return fragment[open_start:pos]


def extract_text(html_frag):
    parser = TextExtractor()
    parser.feed(html_frag)
    return ''.join(parser.out)


# ------------------------------------------------------------------ cleanup

def clean(raw):
    lines = []
    for ln in raw.split('\n'):
        collapsed = re.sub(r'[ \t\r\f\v]+', ' ', ln).rstrip()
        if collapsed.strip() == '':
            collapsed = ''
        lines.append(collapsed)
    # collapse consecutive blank lines
    out = []
    prev_blank = False
    for ln in lines:
        if ln == '':
            if not prev_blank:
                out.append('')
            prev_blank = True
        else:
            out.append(ln)
            prev_blank = False
    text = '\n'.join(out).strip()
    # also collapse stray whitespace-only lines already handled; final pass
    text = re.sub(r'\n{3,}', '\n\n', text)
    return text.strip()


# -------------------------------------------------------------- trim markers

# status-bar style markers: single occurrence is enough
SINGLE_MARKERS = [
    'Current capabilities:',
    'Actions (',
    'Available ships:',
    'Available ground forces:',
    'Command Points',
    'Research Capacity',
    'Void Build Capacity',
    'Ground Build Capacity',
    'Flagship:',
    'In progress',
    'Misc assets:',
    'Avatars:',
    'Current Installations:',
    'Military:',
    'Crew :',
    'Spoiler:',
    'Spoiler :',
    # QM mechanics notes observed outside the status block (e.g. turn 25)
    'You cannot build',
]

VOTE_RE = re.compile(r'^(\[\]|\[-X\]|\[- \])')


def trim_narrative(text):
    """Cut at the earliest mechanics marker line (line not included).

    Rules (earliest line wins):
      * single-line markers: status-block headers, 'You cannot build'
        mechanics notes, and any line containing 'moratorium' (author note);
      * vote-list runs: >=3 consecutive lines starting with [] / -[X] / -[ ]
        (single isolated vote-looking lines inside narrative are ignored).
    """
    lines = text.split('\n')
    cut = None
    # 1) single-line scan (status markers + moratorium note)
    for idx, ln in enumerate(lines):
        stripped = ln.strip()
        if any(stripped.startswith(m) for m in SINGLE_MARKERS):
            cut = idx
            break
        if 'moratorium' in stripped.lower():
            cut = idx
            break
    # 2) vote-list runs of >=3 consecutive lines
    run = 0
    for idx, ln in enumerate(lines):
        stripped = ln.strip()
        if VOTE_RE.match(stripped):
            run += 1
            if run >= 3:
                run_start = idx - run + 1
                if cut is None or run_start < cut:
                    cut = run_start
                break
        else:
            run = 0
    if cut is not None:
        lines = lines[:cut]
    return '\n'.join(lines).strip()


# ---------------------------------------------------------------- segmenting

def segment(text, lo=1300, hi=1700, target=1500):
    """Split text into balanced chunks of ~1300-1700 words at paragraph
    boundaries (paragraphs are never split). Target chunk size adapts to
    total/n so the tail is not starved."""
    paras = text.split('\n\n')
    if not paras:
        return []
    words = [len(p.split()) for p in paras]
    total = sum(words)
    n = max(1, min(6, round(total / target)))
    while n > 1 and total < n * lo:
        n -= 1
    if n == 1:
        return ['\n\n'.join(paras)]
    pref = [0]
    for w in words:
        pref.append(pref[-1] + w)
    segs = []
    start = 0
    for s in range(n - 1):
        remaining_after = n - s - 2          # segments still to come after this one
        tgt = (total - pref[start]) / (n - s)  # fair share for this segment
        best_i, best_diff = None, None
        for i in range(start, len(paras) - 1):
            seg_w = pref[i + 1] - pref[start]
            tail = total - pref[i + 1]
            if tail < remaining_after * lo:
                break                        # cutting later starves the tail
            if seg_w > hi:
                break
            diff = abs(seg_w - tgt)
            if best_diff is None or diff < best_diff:
                best_diff, best_i = diff, i
        if best_i is None:                   # constraints infeasible: closest to target
            best_i, best_diff = start, None
            for i in range(start, len(paras) - 1):
                seg_w = pref[i + 1] - pref[start]
                diff = abs(seg_w - tgt)
                if best_diff is None or diff < best_diff:
                    best_diff, best_i = diff, i
        segs.append('\n\n'.join(paras[start:best_i + 1]))
        start = best_i + 1
    segs.append('\n\n'.join(paras[start:]))
    return segs


# --------------------------------------------------------------------- main

def process(page_file, post_id, turn, out_prefix):
    html = open(page_file, encoding='utf-8', errors='replace').read()
    frag = post_fragment(html, post_id)
    body = bbwrapper_html(frag)
    raw = extract_text(body)
    text = clean(raw)
    narr = trim_narrative(text)
    if not narr:
        raise ValueError('empty narrative after trim')
    words = len(narr.split())
    segs = segment(narr)
    with open('%s_en.txt' % out_prefix, 'w', encoding='utf-8') as f:
        f.write(narr + '\n')
    seg_words = []
    for i, s in enumerate(segs, 1):
        with open('%s_seg%d.txt' % (out_prefix, i), 'w', encoding='utf-8') as f:
            f.write(s + '\n')
        seg_words.append(len(s.split()))
    print('turn%s: OK words=%d segs=%d seg_words=%s' % (turn, words, len(segs), seg_words))
    return words, seg_words


if __name__ == '__main__':
    jobs = [
        ('p388.html', '34056239', '24'),
        ('p417.html', '34124795', '25'),
        ('p453.html', '34210637', '26'),
    ]
    ok = True
    for page, pid, turn in jobs:
        prefix = 'turn' + turn
        try:
            process(page, pid, turn, prefix)
        except Exception as e:  # noqa: BLE001
            ok = False
            print('turn%s: FAILED %s: %s' % (turn, page, e), file=sys.stderr)
    sys.exit(0 if ok else 1)
