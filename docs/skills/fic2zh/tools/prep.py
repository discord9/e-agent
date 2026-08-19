#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Chapter preprocessing for the Vox Vitae translation workflow.

For each (page, postid, turn) triple this module locates the post's
``bbWrapper``, extracts visible text, removes mechanics at explicit content
markers, and splits the result at paragraph boundaries.  A post can also be
processed directly with the command-line interface below; no network access
is performed by this script.
"""
import argparse
import re
import sys
from html.parser import HTMLParser

# ---------------------------------------------------------------- extraction

BLOCK_TAGS = {'p', 'div', 'br', 'li', 'h1', 'h2', 'h3', 'h4', 'tr'}


class TextExtractor(HTMLParser):
    """Extract visible plain text, retaining text in spoiler containers."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.out = []
        self.skip_depth = 0          # inside script/style

    def handle_starttag(self, tag, attrs):
        if self.skip_depth:
            self.skip_depth += 1
            return
        if tag in ('script', 'style'):
            self.skip_depth = 1
            return
        if tag in BLOCK_TAGS:
            self.out.append('\n')
        elif tag == 'blockquote':
            self.out.append('\n')

    def handle_endtag(self, tag):
        if self.skip_depth:
            self.skip_depth -= 1
            return
        if tag in BLOCK_TAGS:
            self.out.append('\n')
        elif tag == 'blockquote':
            self.out.append('\n')

    def handle_data(self, data):
        if not self.skip_depth:
            self.out.append(data)


def post_fragment(html, post_id):
    """Return the article containing ``post_id``.

    XenForo normally emits ``id=\"js-post-ID\"``.  The small regex fallback
    also accepts single quotes and arbitrary attribute order, which makes
    saved pages and fixtures behave the same way.
    """
    post_id = str(post_id)
    id_re = re.compile(
        r'<article\b[^>]*\bid\s*=\s*([\'\"])js-post-%s\1[^>]*>'
        % re.escape(post_id),
        re.IGNORECASE,
    )
    match = id_re.search(html)
    if match:
        start = match.start()
        marker_end = match.end()
    else:
        # Preserve the old, deliberately simple fallback for malformed or
        # minimally saved fragments.
        marker = 'id="js-post-%s"' % post_id
        marker_end = html.find(marker)
        if marker_end < 0:
            raise ValueError('post id %s not found' % post_id)
        start = html.rfind('<article', 0, marker_end)
        marker_end += len(marker)
    if start < 0:
        raise ValueError('article boundaries not found for post %s' % post_id)
    end = html.find('</article>', marker_end)
    if end < 0:
        raise ValueError('article boundaries not found for post %s' % post_id)
    return html[start:end + len('</article>')]


def bbwrapper_html(fragment):
    """Return the balanced ``<div class=bbWrapper>`` body."""
    # Attribute order and quote style vary between saved forum pages.
    match = re.search(
        r'<div\b[^>]*\bclass\s*=\s*([\'\"])'
        r'[^\'\"]*\bbbWrapper\b[^\'\"]*\1[^>]*>',
        fragment,
        re.IGNORECASE,
    )
    if not match:
        # Also accept an unquoted class value in a minimal fixture.
        match = re.search(r'<div\b[^>]*\bbbWrapper\b[^>]*>', fragment, re.IGNORECASE)
    if not match:
        raise ValueError('bbWrapper not found')
    open_start = match.start()

    # balanced div scan from the opening wrapper tag
    depth = 0
    pos = open_start
    pattern = re.compile(r'<div\b|</div\s*>', re.IGNORECASE)
    while True:
        item = pattern.search(fragment, pos)
        if not item:
            raise ValueError('unbalanced div in bbWrapper')
        depth += 1 if item.group(0).lower().startswith('<div') else -1
        pos = item.end()
        if depth == 0:
            return fragment[open_start:pos]


def extract_text(html_frag):
    """Extract visible text; CSS classes never determine text visibility."""
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
    return re.sub(r'\n{3,}', '\n\n', text).strip()


# -------------------------------------------------------------- trim markers

# These are content markers, rather than CSS/container rules.  They are kept
# broad enough for the old batch jobs while an individual post may use the
# precise --start-marker/--end-marker options.
SINGLE_MARKERS = [
    'Winning Vote:',
    'Remaining Actions in Turn:',
    'Remaining Rolls:',
    "Author's note:",
    'Author’s note:',
    'Current Capabilities:',
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
    # Do not treat the generic word "Spoiler:" as a boundary: it can be
    # ordinary visible prose.  Callers with a known end should pass it as an
    # explicit end_marker.
    # QM mechanics notes observed outside the status block (e.g. turn 25)
    'You cannot build',
]

VOTE_RE = re.compile(r'^(?:\[\]|\[-X\]|\[- \]|\[X\]|\[x\])')


def _default_cut_line(text, scan_pos=0):
    """Find the first default mechanics boundary at or after ``scan_pos``.

    ``scan_pos`` is a character position, not merely a line number.  This is
    important when a preamble marker and the selected start marker share one
    line: the preamble must not win the default scan.
    """
    lines = text.split('\n')
    line_starts = []
    position = 0
    for line in lines:
        line_starts.append(position)
        position += len(line) + 1
    first_line = text.count('\n', 0, scan_pos)
    cut = None
    for idx, ln in enumerate(lines):
        if idx < first_line:
            continue
        candidate = ln
        if line_starts[idx] < scan_pos:
            candidate = ln[max(0, scan_pos - line_starts[idx]):]
        stripped = candidate.strip()
        if any(stripped.startswith(marker) for marker in SINGLE_MARKERS):
            cut = idx
            break
        if 'moratorium' in stripped.lower():
            cut = idx
            break

    # A vote-list run is a boundary only when it is clearly a list, not when
    # an isolated checkbox-like line appears in the prose.  The first line is
    # considered only from scan_pos onward, so preamble votes cannot qualify.
    run = 0
    for idx, ln in enumerate(lines[first_line:], first_line):
        candidate = ln
        if line_starts[idx] < scan_pos:
            candidate = ln[max(0, scan_pos - line_starts[idx]):]
        if VOTE_RE.match(candidate.strip()):
            run += 1
            if run >= 3:
                run_start = idx - run + 1
                if cut is None or run_start < cut:
                    cut = run_start
                break
        else:
            run = 0
    return None if cut is None else sum(len(line) + 1 for line in lines[:cut])


def _marker_position(text, marker, name, start=0):
    if marker is None:
        return None
    if not marker:
        raise ValueError('%s marker must not be empty' % name)
    position = text.find(marker, start)
    if position < 0:
        raise ValueError('%s marker not found: %s' % (name, marker))
    return position


def trim_narrative(text, start_marker=None, end_marker=None):
    """Return the narrative bounded by content markers.

    ``start_marker`` starts at the beginning of the paragraph containing the
    marker, so the marker paragraph itself is retained.  ``end_marker`` cuts
    at the beginning of the marker's line, excluding the marker and everything
    after it.  With no explicit end marker, the historical content-marker
    trimming rules remain active for old jobs and API callers.
    """
    start_pos = _marker_position(text, start_marker, 'start')
    end_search = 0 if start_pos is None else start_pos + len(start_marker)
    if end_marker is None:
        end_pos = None
    elif not end_marker:
        raise ValueError('end marker must not be empty')
    else:
        end_pos = text.find(end_marker, end_search)
        if end_pos < 0:
            if start_pos is None:
                raise ValueError('end marker not found: %s' % end_marker)
            raise ValueError('end marker not found after start marker: %s' % end_marker)

    if start_pos is not None:
        paragraph_start = text.rfind('\n\n', 0, start_pos)
        start = 0 if paragraph_start < 0 else paragraph_start + 2
    else:
        start = 0

    if end_pos is not None:
        # Exclude the whole marker line.  This avoids leaving a status header
        # in the narrative while retaining any preceding line in its block.
        end = text.rfind('\n', 0, end_pos) + 1
    elif end_marker is None:
        scan_pos = end_search if start_pos is not None else start
        cut = _default_cut_line(text, scan_pos)
        end = len(text) if cut is None else cut
    else:
        # An explicitly requested boundary is strict: silently retaining an
        # unbounded tail would make a reproducible post extraction unsafe.
        raise ValueError('end marker not found after start marker')

    if end < start:
        return ''
    return text[start:end].strip()


# ---------------------------------------------------------------- segmenting

def segment(text, lo=1300, hi=1700, target=1500):
    """Split text into chunks at paragraph boundaries (never split a para)."""
    if lo <= 0:
        raise ValueError('segment minimum words must be > 0')
    if hi < lo:
        raise ValueError('segment maximum words must be >= minimum words')
    if target <= 0:
        raise ValueError('segment target words must be > 0')
    if target < lo:
        raise ValueError('segment target words must be >= minimum words')
    if target > hi:
        raise ValueError('segment target words must be <= maximum words')
    if not text.strip():
        return []
    paras = text.split('\n\n')
    words = [len(p.split()) for p in paras]
    total = sum(words)
    n = max(1, min(len(paras), 6, round(total / target)))
    while n > 1 and total < n * lo:
        n -= 1
    if n == 1:
        return ['\n\n'.join(paras)]
    pref = [0]
    for word_count in words:
        pref.append(pref[-1] + word_count)
    segs = []
    start = 0
    for s in range(n - 1):
        remaining_after = n - s - 2
        tgt = (total - pref[start]) / (n - s)
        best_i, best_diff = None, None
        for i in range(start, len(paras) - 1):
            seg_w = pref[i + 1] - pref[start]
            tail = total - pref[i + 1]
            if tail < remaining_after * lo:
                break
            if seg_w > hi:
                break
            diff = abs(seg_w - tgt)
            if best_diff is None or diff < best_diff:
                best_diff, best_i = diff, i
        if best_i is None:                   # constraints infeasible
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

def process(page_file, post_id, turn, out_prefix, start_marker=None,
            end_marker=None, segment_min_words=1300,
            segment_max_words=1700, segment_target_words=1500):
    """Process one saved page and write its English body and segments."""
    with open(page_file, encoding='utf-8', errors='replace') as page:
        html = page.read()
    frag = post_fragment(html, post_id)
    body = bbwrapper_html(frag)
    text = clean(extract_text(body))
    narr = trim_narrative(text, start_marker, end_marker)
    if not narr:
        raise ValueError('empty narrative after trim')
    segs = segment(narr, segment_min_words, segment_max_words,
                   segment_target_words)
    with open('%s_en.txt' % out_prefix, 'w', encoding='utf-8') as output:
        output.write(narr + '\n')
    seg_words = []
    for i, part in enumerate(segs, 1):
        with open('%s_seg%d.txt' % (out_prefix, i), 'w', encoding='utf-8') as output:
            output.write(part)
        seg_words.append(len(part.split()))
    print('turn%s: OK words=%d segs=%d seg_words=%s' %
          (turn, len(narr.split()), len(segs), seg_words))
    return len(narr.split()), seg_words


def _parser():
    parser = argparse.ArgumentParser(
        description='Extract and segment one saved forum post (no network access).')
    parser.add_argument('--page-file', help='saved HTML page')
    parser.add_argument('--post-id', help='numeric post id')
    parser.add_argument('--turn', help='turn label used in the status message')
    parser.add_argument('--out-prefix', help='prefix for _en.txt and _segN.txt')
    parser.add_argument('--start-marker', help='start at the paragraph containing this text')
    parser.add_argument('--end-marker', help='exclude the line containing this text and later text')
    parser.add_argument('--segment-min-words', type=int, default=1300)
    parser.add_argument('--segment-max-words', type=int, default=1700)
    parser.add_argument('--segment-target-words', type=int, default=1500)
    return parser


def main(argv=None):
    raw_argv = sys.argv[1:] if argv is None else list(argv)
    args = _parser().parse_args(raw_argv)
    values = (args.page_file, args.post_id, args.turn, args.out_prefix)
    if not raw_argv:
        # Keep the historical no-argument batch entry point for existing jobs.
        jobs = [
            ('p388.html', '34056239', '24'),
            ('p417.html', '34124795', '25'),
            ('p453.html', '34210637', '26'),
        ]
        ok = True
        for page, post_id, turn in jobs:
            try:
                process(page, post_id, turn, 'turn' + turn)
            except Exception as exc:  # noqa: BLE001
                ok = False
                print('turn%s: FAILED %s: %s' % (turn, page, exc), file=sys.stderr)
        return 0 if ok else 1

    missing = [name for name, value in zip(
        ('--page-file', '--post-id', '--turn', '--out-prefix'), values) if value is None]
    if missing:
        _parser().error('single-post mode requires ' + ', '.join(missing))
    try:
        segment('', args.segment_min_words, args.segment_max_words,
                args.segment_target_words)
    except ValueError as exc:
        _parser().error(str(exc))
    process(args.page_file, args.post_id, args.turn, args.out_prefix,
            args.start_marker, args.end_marker, args.segment_min_words,
            args.segment_max_words, args.segment_target_words)
    return 0


if __name__ == '__main__':
    sys.exit(main())
