#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Build an EPUB of a Chinese full translation (Vox Vitae, Colossus, ...).

Reads the full Markdown text (chapters each starting with a "# " heading; the
leading "# {title}" section holds the copyright notice and the TOC and becomes
the preface chapter), performs a minimal Markdown -> XHTML conversion (builtin
re/html only, no external markdown library), and writes the .epub with
ebooklib.

Command-line arguments (all optional, defaults = Vox Vitae paths for backward
compatibility): --md --out --title --author --glossary --glossary-notes.
The term-footnote feature needs a glossary_notes.csv next to the glossary;
when either file is missing the feature is skipped gracefully.
"""

import argparse
import csv
import html
import re
import zipfile
from pathlib import Path

from ebooklib import epub
from lxml import etree

SRC = Path(__file__).resolve().parent / "vox_vitae_zh" / "vox_vitae_zh_en.md"
OUT = Path(__file__).resolve().parent / "vox_vitae_zh" / "vox_vitae_zh_en.epub"

# Module-level defaults for --md/--out/--title/--author (Vox Vitae, backward
# compatible); main() reads everything from argparse args at runtime. The
# glossary paths are not constants: they default to <md 所在目录>/glossary_zh-CN.csv
# and <md 所在目录>/glossary_notes.csv, so the default Vox run resolves to
# vox_vitae_zh/glossary_zh-CN.csv + glossary_notes.csv exactly as before, while
# a Colossus run resolves to colossus_zh/… (missing notes file -> feature skipped).
BOOK_TITLE = "Vox Vitae（生命之声）中文全译本"
AUTHOR = "Neablis 原著 / 社区翻译"
LANG = "zh-CN"
PREFACE_TITLE = "前言（版权声明与目录）"

# Visible styling for term-footnote links (blue, underlined, superscript).
NOTEREF_CSS = (
    "a.noteref { color: #0066cc; text-decoration: underline; vertical-align: super;"
    " font-size: 0.75em; line-height: 1; padding: 0 0.1em; }\n"
    "a.noteref:hover, a.noteref:active { color: #003399; background-color: #eef4ff; }"
)

BOLD_RE = re.compile(r"\*\*(.+?)\*\*", re.S)

# Placeholders used while scanning for first occurrences, so that an anchor
# inserted for one term can never become search text for another (substring
# targets like 帝国 inside 帝国之影). \x00 never appears in real text.
# The trailing _T/_S distinguishes the Chinese-target anchor from the
# English-source anchor when both sides of a term match in the same block.
GLOSS_MARKER_RE = re.compile(r"\x00GLOSS_NOTE_(\d+)_([TS])\x00")


def _source_re(escaped_source: str) -> re.Pattern:
    """Word-boundary, case-insensitive regex for an English source term as it
    appears in already-escaped text (html.escape may have rewritten chars, e.g.
    the apostrophe in ``mile'ionahd`` becomes ``&#x27;``)."""
    return re.compile(
        r"(?<![A-Za-z0-9])" + re.escape(escaped_source) + r"(?![A-Za-z0-9])",
        re.IGNORECASE,
    )


def _first_match_replace(text, src_re, src_esc, tgt_esc, placeholder_t,
                         placeholder_s, skip_target=False, skip_source=False):
    """If an (already HTML-escaped) inline block contains the term, return
    ``(text_with_first_occurrences_replaced_by_placeholders,
    [(placeholder, matched_word), ...])``; otherwise ``(None, None)``.

    Bilingual rule shared by the first-pass simulation and ``annotate()``:
    every block — Chinese, English, or mixed — is searched for the Chinese
    *target* as a plain substring AND for the English *source* as a
    word-boundary, case-insensitive token, so a 中文段+英文段 block yields up
    to two anchors (target first, then source) for the same term. Replacing
    the first occurrence exactly like annotate() does keeps the two passes in
    lockstep (nested targets like 网道 inside 网道之门 resolve identically).
    ``skip_target``/``skip_source`` suppress a side that was already anchored
    earlier in the book (per-side first-occurrence dedup).
    """
    markers = []
    if not skip_target and tgt_esc in text:
        text = text.replace(tgt_esc, placeholder_t, 1)
        markers.append((placeholder_t, tgt_esc))
    if not skip_source:
        m = src_re.search(text)
        if m:
            text = text[:m.start()] + placeholder_s + text[m.end():]
            markers.append((placeholder_s, src_esc))
    if not markers:
        return None, None
    return text, markers


def _iter_inline_blocks(md_body: str):
    """Yield the exact HTML-escaped inline strings that ``markdown_to_html()``
    hands to ``inline()``/``GlossAnnotator.annotate()`` (escape + bold), so the
    first-pass occurrence scan runs over byte-for-byte the same block texts as
    the annotation pass. Mirrors markdown_to_html's block splitting: '# '
    headings, '> ' blockquote paragraphs, '- ' list items and blank-line
    separated paragraphs (consecutive lines joined with one space); '---'
    yields nothing.
    """
    lines = md_body.split("\n")
    i, n = 0, len(lines)

    def is_quote(s: str) -> bool:
        return s == ">" or s.startswith("> ")

    def is_list_item(s: str) -> bool:
        return s.startswith("- ")

    def escaped(s: str) -> str:
        return BOLD_RE.sub(r"<b>\1</b>", html.escape(s))

    while i < n:
        s = lines[i].strip()
        if not s:
            i += 1
            continue
        if s.startswith("# "):
            yield escaped(s[2:])
            i += 1
            continue
        if s == "---":
            i += 1
            continue
        if is_quote(s):
            paras, cur = [], []
            while i < n and is_quote(lines[i].strip()):
                body = lines[i].strip()[1:].strip()
                if body:
                    cur.append(body)
                elif cur:
                    paras.append(" ".join(cur))
                    cur = []
                i += 1
            if cur:
                paras.append(" ".join(cur))
            for p in paras:
                yield escaped(p)
            continue
        if is_list_item(s):
            items = []
            while i < n and is_list_item(lines[i].strip()):
                items.append(lines[i].strip()[2:].strip())
                i += 1
            for it in items:
                yield escaped(it)
            continue
        para = []
        while i < n:
            l = lines[i].strip()
            if not l:
                break
            if l.startswith("# ") or l == "---" or is_quote(l) or is_list_item(l):
                break
            para.append(l)
            i += 1
        if para:
            yield escaped(" ".join(para))


class GlossAnnotator:
    """Term-footnote support (EPUB 3 popup footnotes).

    Holds the ordered list of (source, target, note) from glossary_notes.csv
    that *actually appear* in the body (filtered by main()'s first pass, CSV
    row order), plus two book-wide "already annotated" sets (one per language
    side). ``begin_chapter()`` installs the chapter's pre-assigned numbers
    (per-chapter [1][2]... in glossary_notes.csv row order, computed by
    main() for the terms whose first in-book occurrence falls in that
    chapter). Every block is searched for the Chinese *target* (substring)
    AND the English *source* (word-boundary, case-insensitive): the Chinese
    side is anchored at the target's first in-book occurrence, the English
    side at the source's first in-book occurrence (cross-chapter dedup via
    ``used_targets``/``used_sources``, so a term lives in exactly one
    chapter's footnote area and carries at most two anchors — one per side —
    pointing at the same aside). Anchors point at an end-of-chapter
    ``<aside epub:type="footnote" id="fn-CHAP-N">`` in the same document
    (popup footnote, no cross-document jump).
    """

    def __init__(self, notes):
        # notes: ordered list of (source, target, note), CSV row order
        self.notes = notes
        self.chap_key = None          # current chapter key, e.g. "chap_001"
        self.used_targets = set()     # book-wide: Chinese side already anchored
        self.used_sources = set()     # book-wide: English side already anchored
        self._numbers = {}            # per-chapter: {source: footnote number}
        self._chapter_footnotes = []  # (n, source, target, note) this chapter
        self._footnote_sources = set()  # sources already given an aside this chapter
        # Precomputed escaped-side matchers shared by every annotate() call.
        self._matchers = [
            (source, target, note, _source_re(html.escape(source)),
             html.escape(source), html.escape(target))
            for (source, target, note) in notes
        ]

    def begin_chapter(self, chap_key: str, numbers: dict) -> None:
        """Start a new chapter: keep the book-wide per-side dedup sets, install
        the chapter's pre-assigned numbers (source -> per-chapter number, in
        glossary_notes.csv row order) and reset the footnote list."""
        self.chap_key = chap_key
        self._numbers = numbers
        self._chapter_footnotes = []
        self._footnote_sources = set()

    def annotate(self, text: str) -> str:
        """Insert noteref anchors into HTML-escaped inline text.

        Per block: first mark every first occurrence (Chinese target and/or
        English source) with a placeholder, then substitute the actual anchor
        markup, so anchors never pollute the search text (avoids matching a
        short target inside a longer one, and stops an inserted anchor from
        becoming search text for other terms). A mixed 中文段+英文段 block can
        therefore yield two anchors for one term — target side and source
        side — both pointing at the same chapter-end aside.
        """
        if not self.notes or self.chap_key is None:
            return text
        markers = {}  # marker string -> matched word (anchor text)
        for source, target, note, src_re, src_esc, tgt_esc in self._matchers:
            if source not in self._numbers:
                # Assigned chapter lies elsewhere (per the first-pass
                # simulation), so it cannot match in this one — skip without
                # touching the number map.
                continue
            n = self._numbers[source]
            t_marker = f"\x00GLOSS_NOTE_{n}_T\x00"
            s_marker = f"\x00GLOSS_NOTE_{n}_S\x00"
            new_text, hits = _first_match_replace(
                text, src_re, src_esc, tgt_esc, t_marker, s_marker,
                skip_target=(source in self.used_targets),
                skip_source=(source in self.used_sources),
            )
            if new_text is None:
                continue
            text = new_text
            for marker, word in hits:
                if marker == t_marker:
                    self.used_targets.add(source)
                else:
                    self.used_sources.add(source)
                markers[marker] = word
            if source not in self._footnote_sources:
                self._footnote_sources.add(source)
                self._chapter_footnotes.append((n, source, target, note))
        return GLOSS_MARKER_RE.sub(
            lambda m: _anchor_for(
                self.chap_key,
                int(m.group(1)),
                markers[m.group(0)],
                "t" if m.group(2) == "T" else "s",
            ),
            text,
        )

    def render_footnotes(self) -> str:
        """End-of-chapter popup footnote asides ('' when the chapter has none).

        One ``<aside epub:type="footnote" id="fn-CHAP-N">`` per term first
        appearing in this chapter, emitted in footnote-number order.
        """
        if not self._chapter_footnotes:
            return ""
        items = "\n".join(
            f'<aside epub:type="footnote" id="fn-{self.chap_key}-{n}">'
            f"<p>{html.escape(source)}（{html.escape(target)}）：{html.escape(note)}</p>"
            f"</aside>"
            for n, source, target, note in sorted(self._chapter_footnotes)
        )
        return f"\n{items}\n"


def _anchor_for(chap_key: str, n: int, word: str, side: str) -> str:
    """Anchor markup for chapter-local note N: the already-escaped matched
    word plus a noteref link to the same document's end-of-chapter aside
    (popup footnote; ``id`` kept for reverse location). ``side`` ("t" for the
    Chinese-target anchor, "s" for the English-source anchor) keeps the
    ``note-ref`` id unique when a term carries two anchors in one chapter."""
    return (
        f"{word}"
        f'<a epub:type="noteref" class="noteref" id="note-ref-{n}-{side}" '
        f'href="#fn-{chap_key}-{n}">[{n}]</a>'
    )


def load_glossary_notes(glossary_csv: Path, glossary_notes_csv: Path):
    """Merge glossary_zh-CN.csv (source->target) with glossary_notes.csv
    (source->note) into an ordered list of (source, target, note).

    All entries are kept (Chinese targets and English-only targets like "STC"
    alike): annotation matches the Chinese target on the Chinese side and the
    English source on the English side. A missing notes file (or missing
    glossary) yields [] and the feature is skipped.
    """
    if not glossary_notes_csv.exists():
        return []
    source_to_target = {}
    if glossary_csv.exists():
        with open(glossary_csv, encoding="utf-8", newline="") as fh:
            for row in csv.DictReader(fh):
                src = (row.get("source") or "").strip()
                tgt = (row.get("target") or "").strip()
                if src and tgt and src not in source_to_target:
                    source_to_target[src] = tgt
    notes = []
    seen_targets = set()
    with open(glossary_notes_csv, encoding="utf-8", newline="") as fh:
        for row in csv.DictReader(fh):
            src = (row.get("source") or "").strip()
            note = (row.get("note") or "").strip()
            if not src or not note:
                continue
            tgt = source_to_target.get(src, "")
            if not tgt or tgt in seen_targets:
                continue
            seen_targets.add(tgt)
            notes.append((src, tgt, note))
    return notes


def inline(text: str, gloss=None) -> str:
    """Escape HTML, then apply the only inline markdown we support: **bold**,
    then (optionally) tag first occurrences of noted glossary terms."""
    text = html.escape(text)
    text = BOLD_RE.sub(r"<b>\1</b>", text)
    if gloss is not None:
        text = gloss.annotate(text)
    return text


def markdown_to_html(text: str, gloss=None) -> str:
    """Minimal Markdown -> XHTML fragment conversion.

    Supported: '# ' -> <h1>, '> ' -> <blockquote> (consecutive quote lines are
    merged into a single blockquote, a bare '>' separates paragraphs inside it),
    '- ' -> <ul><li>, '---' -> <hr/>, blank-line-separated blocks -> <p>.
    Everything else is emitted verbatim into <p> with HTML escaping.
    """
    lines = text.split("\n")
    out = []
    i, n = 0, len(lines)

    def is_quote(s: str) -> bool:
        return s == ">" or s.startswith("> ")

    def is_list_item(s: str) -> bool:
        return s.startswith("- ")

    while i < n:
        s = lines[i].strip()
        if not s:
            i += 1
            continue
        if s.startswith("# "):
            out.append(f"<h1>{inline(s[2:], gloss)}</h1>")
            i += 1
            continue
        if s == "---":
            out.append("<hr/>")
            i += 1
            continue
        if is_quote(s):
            # merge consecutive '>' lines into one blockquote; bare '>' = paragraph break
            paras, cur = [], []
            while i < n and is_quote(lines[i].strip()):
                body = lines[i].strip()[1:].strip()
                if body:
                    cur.append(body)
                elif cur:
                    paras.append(" ".join(cur))
                    cur = []
                i += 1
            if cur:
                paras.append(" ".join(cur))
            inner = "".join(f"<p>{inline(p, gloss)}</p>" for p in paras)
            out.append(f"<blockquote>{inner}</blockquote>")
            continue
        if is_list_item(s):
            items = []
            while i < n and is_list_item(lines[i].strip()):
                items.append(lines[i].strip()[2:].strip())
                i += 1
            lis = "".join(f"<li>{inline(it, gloss)}</li>" for it in items)
            out.append(f"<ul>{lis}</ul>")
            continue
        # plain paragraph: accumulate until blank line or a block-starting line
        para = []
        while i < n:
            l = lines[i].strip()
            if not l:
                break
            if l.startswith("# ") or l == "---" or is_quote(l) or is_list_item(l):
                break
            para.append(l)
            i += 1
        if para:
            out.append(f"<p>{inline(' '.join(para), gloss)}</p>")

    return "\n".join(out)


def split_sections(md_text: str):
    """Split by '^# ' headings. Returns [(title_or_None, body), ...]."""
    sections = []
    cur_title = None
    cur = []
    for line in md_text.split("\n"):
        m = re.match(r"^# (.*)$", line)
        if m:
            if cur_title is not None or cur:
                sections.append((cur_title, "\n".join(cur)))
            cur_title = m.group(1).strip()
            cur = []
        else:
            cur.append(line)
    if cur_title is not None or cur:
        sections.append((cur_title, "\n".join(cur)))
    return sections


def xhtml_fragment(title: str, body: str) -> str:
    """Fragment for a preface/chapter: noteref <style> + <h1> + body.

    The <style> block is emitted here so preface and every chapter carry the
    same rule set; ``_StyledEpubHtml.get_content`` re-injects it into <head>
    on render (ebooklib's EpubHtml.get_content rebuilds the document from a
    template and drops top-level <style> elements of the content fragment).
    """
    return (
        "<style>\n"
        + NOTEREF_CSS
        + "</style>\n"
        + f"<h1>{html.escape(title)}</h1>\n{body}"
    )


class _StyledEpubHtml(epub.EpubHtml):
    """EpubHtml whose <head> carries the noteref <style> block.

    ebooklib's EpubHtml.get_content rebuilds the document from a template:
    it constructs <head> from title + links only and copies just the <body>
    children of the content fragment (top-level <style> elements are dropped
    by the HTML parser it uses). So after the standard render we inject the
    style into <head> where it belongs.
    """

    def get_content(self, default=None):
        data = super().get_content(default)
        try:
            root = etree.fromstring(data)
        except etree.XMLSyntaxError:
            return data
        xhtml = "http://www.w3.org/1999/xhtml"
        head = root.find("{%s}head" % xhtml)
        if head is not None and head.find("{%s}style" % xhtml) is None:
            style = etree.SubElement(head, "{%s}style" % xhtml)
            style.text = "\n" + NOTEREF_CSS + "\n"
            data = etree.tostring(
                root, pretty_print=True, encoding="utf-8", xml_declaration=True
            )
        return data


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--md", default=str(SRC), help="输入全本 Markdown 文件（默认 Vox 全本）")
    ap.add_argument("--out", default=str(OUT), help="输出 EPUB 文件（默认 Vox epub）")
    ap.add_argument("--title", default=BOOK_TITLE, help="书名（默认 Vox Vitae（生命之声）中文全译本）")
    ap.add_argument("--author", default=AUTHOR, help="作者署名（默认 Neablis 原著 / 社区翻译）")
    ap.add_argument("--glossary", default=None,
                    help="术语表 CSV（source,target）；缺省取 --md 同目录 glossary_zh-CN.csv"
                         "（Vox 默认即 vox_vitae_zh/glossary_zh-CN.csv）")
    ap.add_argument("--glossary-notes", default=None,
                    help="术语注释 CSV（source,note）；缺省取 --md 同目录 glossary_notes.csv；"
                         "文件不存在时跳过注释功能")
    args = ap.parse_args()

    md_path = Path(args.md)
    out_path = Path(args.out)
    title = args.title
    md_dir = md_path.resolve().parent
    # 术语表/注释默认跟随输入全本所在目录：Vox 运行解析到 vox_vitae_zh/…，
    # Colossus 运行解析到 colossus_zh/…（其 glossary_notes.csv 不存在 -> 功能跳过）。
    glossary_csv = Path(args.glossary) if args.glossary else md_dir / "glossary_zh-CN.csv"
    glossary_notes_csv = (
        Path(args.glossary_notes) if args.glossary_notes else md_dir / "glossary_notes.csv"
    )

    md_text = md_path.read_text(encoding="utf-8")
    sections = split_sections(md_text)

    # Section 0 is the book-title section -> preface (copyright + TOC).
    if not sections:
        raise SystemExit("no sections found in the markdown")
    title0, body0 = sections[0]
    assert title0 == title, f"unexpected first heading: {title0!r}"
    chapters = sections[1:]
    print(f"total '# ' sections: {len(sections)} (1 preface + {len(chapters)} chapters)")
    print(f"preface: {PREFACE_TITLE}")
    for idx, (t, _) in enumerate(chapters, 1):
        print(f"  {idx:3d}. {t}")

    book = epub.EpubBook()
    book.set_identifier(out_path.stem)
    book.set_title(title)
    book.set_language(LANG)
    book.add_author(args.author)

    # Term footnotes: merge glossary_zh-CN.csv with glossary_notes.csv
    # (empty when the notes file is missing -> feature skipped, no crash).
    all_notes = load_glossary_notes(glossary_csv, glossary_notes_csv)
    chapter_bodies = [body for _, body in chapters]

    # First pass: simulate the annotation pass exactly (same chapter/block
    # order, same first-occurrence replacement via _first_match_replace) to
    # learn, per term, the chapter of the Chinese-side and English-side first
    # occurrences. The term's aside goes to the earlier of the two chapters
    # (min); within that chapter both sides are anchored (up to two anchors
    # sharing one aside). This both filters out terms that never appear
    # anywhere and handles nested targets (网道 inside 网道之门) identically to
    # annotate(), so every numbered term is anchored at least once and no
    # footnote is anchor-less.
    eligible_notes = []
    chapter_numbers = []  # per chapter: {source: per-chapter footnote number}
    if all_notes:
        term_matchers = [
            (src, tgt, note, _source_re(html.escape(src)), html.escape(tgt))
            for src, tgt, note in all_notes
        ]
        chapter_blocks = [
            [block for block in _iter_inline_blocks(body)] for body in chapter_bodies
        ]
        sim_t_placeholder = "\x00GLOSS_NOTE_0_T\x00"
        sim_s_placeholder = "\x00GLOSS_NOTE_0_S\x00"
        target_chapter = {}  # source -> chapter of first Chinese-side hit
        source_chapter = {}  # source -> chapter of first English-side hit
        for ci, blocks in enumerate(chapter_blocks, 1):
            for block in blocks:
                for src, tgt, note, src_re, tgt_esc in term_matchers:
                    if src in target_chapter and src in source_chapter:
                        continue
                    new_block, hits = _first_match_replace(
                        block, src_re, html.escape(src), tgt_esc,
                        sim_t_placeholder, sim_s_placeholder,
                        skip_target=(src in target_chapter),
                        skip_source=(src in source_chapter),
                    )
                    if new_block is None:
                        continue
                    block = new_block
                    for marker, _word in hits:
                        if marker == sim_t_placeholder:
                            target_chapter.setdefault(src, ci)
                        else:
                            source_chapter.setdefault(src, ci)
        anchor_chapter = {}  # source -> chapter owning the term's aside
        for src, tgt, note, src_re, tgt_esc in term_matchers:
            tc = target_chapter.get(src)
            sc = source_chapter.get(src)
            if tc is None and sc is None:
                continue
            eligible_notes.append((src, tgt, note))
            # The term's aside lives in the earlier of the two first-hit
            # chapters; a side whose own first occurrence falls there is
            # anchored there too (both anchors share the same aside).
            anchor_chapter[src] = min(x for x in (tc, sc) if x is not None)
        missing = [src for src, _, _ in all_notes if src not in anchor_chapter]
        msg = f"术语注释: 加载 {len(all_notes)} 条，正文出现 {len(eligible_notes)} 条"
        if missing:
            msg += f"（未出现: {', '.join(missing)}）"
        print(msg)
        # Per-chapter numbers: 1..K in glossary_notes.csv row order over the
        # terms whose first in-book occurrence falls in that chapter.
        chapter_numbers = [{} for _ in chapter_blocks]
        for src, tgt, note, src_re, tgt_esc in term_matchers:
            ci = anchor_chapter.get(src)
            if ci is not None:
                nums = chapter_numbers[ci - 1]
                nums[src] = len(nums) + 1
    else:
        print("术语注释: 无（glossary_notes.csv 缺失或为空，跳过）")
    gloss = GlossAnnotator(eligible_notes)

    # preface chapter
    preface = _StyledEpubHtml(
        title=PREFACE_TITLE,
        file_name="preface.xhtml",
        lang=LANG,
    )
    preface.content = xhtml_fragment(title, markdown_to_html(body0))
    book.add_item(preface)

    # chapters: annotate first in-book occurrences, then append each chapter's
    # popup-footnote asides (same document, so the noteref opens a popup).
    chapter_items = []
    for idx, (title, body) in enumerate(chapters, 1):
        chap_key = f"chap_{idx:03d}"
        gloss.begin_chapter(
            chap_key, chapter_numbers[idx - 1] if chapter_numbers else {}
        )
        item = _StyledEpubHtml(
            title=title,
            file_name=f"{chap_key}.xhtml",
            lang=LANG,
        )
        item.content = xhtml_fragment(
            title, markdown_to_html(body, gloss) + gloss.render_footnotes()
        )
        book.add_item(item)
        chapter_items.append(item)

    book.toc = (
        epub.Section(PREFACE_TITLE),
        preface,
        *chapter_items,
    )
    book.add_item(epub.EpubNcx())
    book.add_item(epub.EpubNav())
    book.spine = ["nav", preface, *chapter_items]

    epub.write_epub(str(out_path), book)

    # quick sanity check on the produced archive
    with zipfile.ZipFile(out_path) as z:
        names = z.namelist()
    size = out_path.stat().st_size
    src_size = md_path.stat().st_size
    print(f"wrote {out_path} ({size} bytes, {len(names)} zip entries)")
    assert size > 30_000, "output too small"
    assert size > 0.15 * src_size, "output implausibly small vs input"
    assert "mimetype" in names and "META-INF/container.xml" in names


if __name__ == "__main__":
    main()
