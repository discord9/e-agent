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
Canonical approved translator notes use --translator-notes with --json and
are validated before the EPUB is written. --glossary-notes is deprecated
legacy CSV compatibility and cannot be combined with canonical notes.
"""

import argparse
import csv
import html
import json
import re
import zipfile
from pathlib import Path

from ebooklib import epub

from validate_translator_notes import (
    TranslatorNotesError, locate_literal_occurrence, validate_translator_notes,
)
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
    "a.noteref, a.translator-noteref { color: #0066cc; text-decoration: underline; vertical-align: super;"
    " font-size: 0.75em; line-height: 1; padding: 0 0.1em; }\n"
    "a.noteref:hover, a.noteref:active, a.translator-noteref:hover,"
    " a.translator-noteref:active { color: #003399; background-color: #eef4ff; }"
)

BOLD_RE = re.compile(r"\*\*(.+?)\*\*", re.S)
SOURCE_MARKUP_RE = re.compile(
    r"<[^>]+>|\b(?:id|href)\s*=|epub:type\s*=\s*['\"](?:noteref|footnote)['\"]",
    re.IGNORECASE,
)

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


def json_inline(text: str, gloss=None) -> str:
    """Render JSON text as escaped plain text with visible line breaks.

    Markdown syntax is inert.  Escape and annotate before converting real
    newlines, so glossary matching still sees the original inline text.
    """
    text = html.escape(text)
    if gloss is not None:
        text = gloss.annotate(text)
    return text.replace("\n", "<br/>")


def json_blocks_to_html(blocks, gloss=None) -> str:
    """Render the deliberately small structured-JSON block vocabulary."""
    out = []
    for block in blocks:
        kind = block["type"]
        if kind == "paragraph":
            out.append(f"<p>{json_inline(block['text'], gloss)}</p>")
        elif kind == "blockquote":
            inner = "".join(
                f"<p>{json_inline(paragraph['text'], gloss)}</p>"
                for paragraph in block["blocks"]
            )
            out.append(f"<blockquote>{inner}</blockquote>")
        else:  # list
            items = "".join(f"<li>{json_inline(item, gloss)}</li>"
                            for item in block["items"])
            out.append(f"<ul>{items}</ul>")
    return "\n".join(out)


def _translator_inline(text, unit_id, notes, chapter_key, numbers):
    """Escape one structured unit and insert notes at validated literal ranges.

    Validation and rendering deliberately use the same locator.  The assertion
    also makes a future document mutation fail closed rather than dropping a
    noteref or emitting an unanchored aside.
    """
    rendered = [note for note in notes if note["render"]]
    hits = []
    for note in rendered:
        anchor = note["anchor"]["text"]
        occurrence = note["anchor"]["occurrence"]
        hit = locate_literal_occurrence(text, anchor, occurrence)
        if hit is None:
            raise SystemExit(f"translator note anchor disappeared in {unit_id}")
        if text[hit[0]:hit[1]] != anchor:
            raise SystemExit(f"translator note anchor changed in {unit_id}")
        hits.append((*hit, note))
    if any(a[0] < b[1] and b[0] < a[1]
           for i, a in enumerate(hits) for b in hits[i + 1:]):
        raise SystemExit(f"translator note anchors overlap in {unit_id}")
    hits.sort(key=lambda item: item[0])
    out, cursor = [], 0
    for start, end, note in hits:
        out.append(html.escape(text[cursor:start]))
        out.append(html.escape(text[start:end]))
        number = numbers[note["note_id"]]
        nid = note["note_id"]
        out.append(f'<a epub:type="noteref" class="translator-noteref" id="tnref-{nid}" href="#tn-{nid}">[{number}]</a>')
        cursor = end
    out.append(html.escape(text[cursor:]))
    return "".join(out).replace("\n", "<br/>")


def json_blocks_to_translator_html(blocks, notes_by_unit, chapter_key, numbers):
    """Render structured JSON with unit-addressed, already-approved notes."""
    out = []
    for block in blocks:
        kind = block["type"]
        if kind == "paragraph":
            uid = block["unit_id"]
            out.append(f"<p>{_translator_inline(block['text'], uid, notes_by_unit.get(uid, []), chapter_key, numbers)}</p>")
        elif kind == "blockquote":
            inner = "".join(
                f"<p>{_translator_inline(p['text'], p['unit_id'], notes_by_unit.get(p['unit_id'], []), chapter_key, numbers)}</p>"
                for p in block["blocks"]
            )
            out.append(f"<blockquote>{inner}</blockquote>")
        else:
            # Keep canonical list rendering identical to json_blocks_to_html;
            # list items have no unit IDs and therefore cannot carry notes.
            out.append("<ul>" + "".join(f"<li>{json_inline(item)}</li>"
                                         for item in block["items"]) + "</ul>")
    return "\n".join(out)


def translator_footnotes(notes, numbers):
    items = []
    for note in notes:
        if not note["render"]:
            continue
        nid = note["note_id"]
        n = numbers[nid]
        items.append(
            f'<aside epub:type="footnote" class="translator-note" id="tn-{nid}">'
            f'<p><a class="translator-note-backlink" href="#tnref-{nid}">[{n}]</a> '
            f'译者注：{html.escape(note["note"])}</p></aside>'
        )
    return ("\n" + "\n".join(items) + "\n") if items else ""


def _reject_structured_source_markup(preface, sections):
    """Reject markup the v1 structured vocabulary cannot preserve safely."""
    def visit(blocks, where):
        for index, block in enumerate(blocks):
            if block["type"] == "list":
                values = block["items"]
            elif block["type"] == "paragraph":
                values = [block["text"]]
            else:
                values = [p["text"] for p in block["blocks"]]
            for value in values:
                if SOURCE_MARKUP_RE.search(value):
                    raise SystemExit(
                        f"translator notes cannot render source anchor markup in {where}[{index}]")
            if block["type"] == "blockquote":
                visit(block["blocks"], f"{where}[{index}].blocks")
    visit(preface["blocks"], "preface.blocks")
    for index, section in enumerate(sections):
        visit(section["blocks"], f"sections[{index}].blocks")


def _structured_units(preface, sections):
    """Return targetable paragraph unit IDs and active Chinese text."""
    entries = []
    def visit(blocks):
        for block in blocks:
            if block["type"] == "paragraph":
                entries.append((block.get("unit_id"), block["text"]))
            elif block["type"] == "blockquote":
                visit(block["blocks"])
    visit(preface["blocks"])
    for section in sections:
        visit(section["blocks"])
    ids = set()
    active = []
    for uid, text in entries:
        if not isinstance(uid, str) or not uid:
            raise SystemExit("translator notes require a unit_id on every paragraph")
        if uid in ids:
            raise SystemExit(f"translator notes require unique unit_id: {uid}")
        ids.add(uid)
        active.append({"unit_id": uid, "zh": text})
    return active


def _json_inline_blocks(blocks):
    """Return escaped inline values in the same order as JSON rendering."""
    result = []
    for block in blocks:
        if block["type"] == "paragraph":
            result.append(html.escape(block["text"]))
        elif block["type"] == "blockquote":
            result.extend(html.escape(p["text"]) for p in block["blocks"])
        else:
            result.extend(html.escape(item) for item in block["items"])
    return result


def _json_error(message):
    raise SystemExit(f"invalid JSON input: {message}")


def _validate_json_block(block, where):
    if not isinstance(block, dict) or block.get("type") not in {
        "paragraph", "blockquote", "list"
    }:
        _json_error(f"{where} has an illegal block type")
    kind = block["type"]
    if kind == "paragraph":
        if not isinstance(block.get("text"), str):
            _json_error(f"{where}.text must be a string")
    elif kind == "list":
        if (not isinstance(block.get("items"), list)
                or any(not isinstance(item, str) for item in block["items"])):
            _json_error(f"{where}.items must be a list of strings")
    else:
        children = block.get("blocks")
        if not isinstance(children, list):
            _json_error(f"{where}.blocks must be a list")
        for i, child in enumerate(children):
            if (not isinstance(child, dict)
                    or child.get("type") != "paragraph"
                    or not isinstance(child.get("text"), str)):
                _json_error(f"{where}.blocks[{i}] must be a paragraph")


CANONICAL_RECORD_KEYS = ("unit_id", "en", "zh", "summary", "status", "uncertainties")


def _join_canonical_records(document, path):
    """Replace English unit text with an exact, ordered canonical translation join."""
    raw = Path(path).read_bytes()
    if b"\r" in raw or (not raw.endswith(b"\n") or raw.endswith(b"\n\n")):
        _json_error("canonical translations must be LF-terminated JSONL")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        _json_error(f"canonical translations are not valid UTF-8: {exc}")
    records = {}
    ordered_ids = []
    for line_no, line in enumerate(text.splitlines(), 1):
        try:
            row = json.loads(line)
        except json.JSONDecodeError as exc:
            _json_error(f"canonical translation line {line_no} is invalid JSON: {exc}")
        if (not isinstance(row, dict) or tuple(row) != CANONICAL_RECORD_KEYS
                or not isinstance(row["unit_id"], str) or not row["unit_id"]):
            _json_error(f"canonical translation line {line_no} has the wrong schema")
        if row["unit_id"] in records:
            _json_error(f"canonical translation has duplicate unit_id: {row['unit_id']}")
        if row["status"] != "final" or row["uncertainties"]:
            _json_error(f"canonical translation {row['unit_id']} is not final")
        if (not isinstance(row["en"], str) or not isinstance(row["zh"], str)
                or not isinstance(row["summary"], str)
                or not row["zh"].strip()
                or not isinstance(row["uncertainties"], list)
                or any(not isinstance(value, str) for value in row["uncertainties"])):
            _json_error(f"canonical translation {row['unit_id']} has invalid fields")
        records[row["unit_id"]] = row
        ordered_ids.append(row["unit_id"])

    used = set()
    encountered_ids = []
    def visit(blocks, where):
        for index, block in enumerate(blocks):
            if block["type"] == "paragraph":
                uid = block.get("unit_id")
                if uid is None:
                    _json_error(f"canonical mode requires unit_id for {where}[{index}]")
                if uid not in records:
                    _json_error(f"canonical translation missing unit_id: {uid}")
                row = records[uid]
                if block["text"] != row["en"]:
                    _json_error(f"canonical translation {uid} en does not exactly match input")
                block["text"] = row["zh"]
                used.add(uid)
                encountered_ids.append(uid)
            elif block["type"] == "blockquote":
                visit(block["blocks"], f"{where}[{index}].blocks")
            else:
                _json_error(f"canonical mode does not support textual {where}[{index}] block")
    visit(document["preface"]["blocks"], "preface.blocks")
    for index, section in enumerate(document["sections"]):
        visit(section["blocks"], f"sections[{index}].blocks")
    if used != set(records):
        _json_error("canonical translations contain an unused unit_id")
    if ordered_ids != encountered_ids:
        _json_error("canonical translations are not in canonical unit order")
    return document


def _validate_json_document(document):
    if not isinstance(document, dict) or not isinstance(document.get("title"), str):
        _json_error("root.title must be a string")
    if "author" in document and not isinstance(document["author"], str):
        _json_error("root.author must be a string")
    preface = document.get("preface")
    if not isinstance(preface, dict):
        _json_error("preface must be an object")
    required_preface = {"id", "kind", "parent_id", "title", "blocks"}
    missing_preface = required_preface - preface.keys()
    if missing_preface:
        _json_error(f"preface missing field {sorted(missing_preface)[0]}")
    if not isinstance(preface["id"], str) or not preface["id"]:
        _json_error("preface.id must be a non-empty string")
    if preface["kind"] != "preface":
        _json_error("preface.kind must be 'preface'")
    if preface["parent_id"] is not None:
        _json_error("preface.parent_id must be null")
    if not isinstance(preface["title"], str):
        _json_error("preface.title must be a string")
    if not isinstance(preface["blocks"], list):
        _json_error("preface.blocks must be a list")
    for i, block in enumerate(preface["blocks"]):
        _validate_json_block(block, f"preface.blocks[{i}]")
    sections = document.get("sections")
    if not isinstance(sections, list):
        _json_error("sections must be a list")
    ids = {preface["id"]}
    seen_chapters = set()
    for i, section in enumerate(sections):
        where = f"sections[{i}]"
        if not isinstance(section, dict):
            _json_error(f"{where} must be an object")
        if "parent_id" not in section:
            _json_error(f"{where}.parent_id is required")
        sid = section.get("id")
        if not isinstance(sid, str) or not sid or sid in ids:
            _json_error(f"{where}.id is missing or duplicated")
        ids.add(sid)
        if section.get("kind") not in {"chapter", "discussion"}:
            _json_error(f"{where}.kind must be 'chapter' or 'discussion'")
        if not isinstance(section.get("title"), str):
            _json_error(f"{where}.title must be a string")
        if not isinstance(section.get("blocks"), list):
            _json_error(f"{where}.blocks must be a list")
        for j, block in enumerate(section["blocks"]):
            _validate_json_block(block, f"{where}.blocks[{j}]")
    for i, section in enumerate(sections):
        parent = section.get("parent_id")
        kind = section["kind"]
        if kind == "discussion":
            if parent not in seen_chapters:
                _json_error(f"sections[{i}].parent_id must reference a previous top-level chapter")
        elif parent is not None:
            _json_error(f"sections[{i}].parent_id must be null for chapter")
        if kind == "chapter":
            seen_chapters.add(section["id"])
    return preface, sections


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
    input_group = ap.add_mutually_exclusive_group()
    input_group.add_argument("--md", help="输入全本 Markdown 文件（默认 Vox 全本）")
    input_group.add_argument("--json", dest="json_path", help="输入结构化 canonical JSON 文件")
    ap.add_argument("--translations", help="canonical unit-record JSONL to exact-join into --json")
    ap.add_argument("--out", default=str(OUT), help="输出 EPUB 文件（默认 Vox epub）")
    ap.add_argument("--title", default=None, help="书名（显式值覆盖输入文件）")
    ap.add_argument("--author", default=None, help="作者署名（显式值覆盖输入文件）")
    ap.add_argument("--glossary", default=None,
                    help="术语表 CSV（source,target）；缺省取 --md 同目录 glossary_zh-CN.csv"
                         "（Vox 默认即 vox_vitae_zh/glossary_zh-CN.csv）")
    ap.add_argument("--glossary-notes", default=None,
                    help="已弃用的 legacy 术语注释 CSV；不得与 --translator-notes 合用")
    ap.add_argument("--translator-notes", default=None,
                    help="批准的 translator_notes.jsonl v1（仅结构化 JSON 输入）")
    ap.add_argument("--approval-root", "--workspace-root", dest="approval_root", default=None,
                    help="translator notes provenance artifact 的 workspace root")
    args = ap.parse_args()

    if args.translator_notes and args.glossary_notes:
        raise SystemExit("--translator-notes cannot be combined with deprecated --glossary-notes")
    if args.translator_notes and not args.json_path:
        raise SystemExit("--translator-notes requires --json structured input")
    if args.translator_notes and not args.approval_root:
        raise SystemExit("--translator-notes requires --approval-root")
    if args.translator_notes and not args.translations:
        raise SystemExit("--translator-notes requires --translations canonical unit records")

    json_mode = args.json_path is not None
    md_path = Path(args.md or SRC)
    input_path = Path(args.json_path) if json_mode else md_path
    out_path = Path(args.out)
    md_dir = input_path.resolve().parent
    # 术语表/注释默认跟随输入全本所在目录：Vox 运行解析到 vox_vitae_zh/…，
    # Colossus 运行解析到 colossus_zh/…（其 glossary_notes.csv 不存在 -> 功能跳过）。
    glossary_csv = Path(args.glossary) if args.glossary else md_dir / "glossary_zh-CN.csv"
    glossary_notes_csv = (
        Path(args.glossary_notes) if args.glossary_notes else md_dir / "glossary_notes.csv"
    )

    if json_mode:
        document = json.loads(input_path.read_text(encoding="utf-8"))
        if args.translations:
            _join_canonical_records(document, args.translations)
        preface_data, json_sections = _validate_json_document(document)
        title = args.title if args.title is not None else document["title"]
        author = args.author if args.author is not None else document.get("author", AUTHOR)
        body0 = json_blocks_to_html(preface_data["blocks"])
        preface_title = preface_data["title"]
        chapters = json_sections
        print(f"total JSON sections: {len(chapters)} (explicit preface + sections)")
    else:
        title = args.title if args.title is not None else BOOK_TITLE
        author = args.author if args.author is not None else AUTHOR
        md_text = md_path.read_text(encoding="utf-8")
        sections = split_sections(md_text)
        # Section 0 is the book-title section -> preface (copyright + TOC).
        if not sections:
            raise SystemExit("no sections found in the markdown")
        title0, body0 = sections[0]
        assert title0 == title, f"unexpected first heading: {title0!r}"
        preface_title = PREFACE_TITLE
        chapters = sections[1:]
        print(f"total '# ' sections: {len(sections)} (1 preface + {len(chapters)} chapters)")
    print(f"preface: {preface_title}")
    for idx, section in enumerate(chapters, 1):
        print(f"  {idx:3d}. {section['title'] if json_mode else section[0]}")

    # Canonical notes are validated against the structured document before any
    # EPUB object or output file is created. Legacy CSV remains opt-in only.
    canonical_notes = []
    notes_by_unit = {}
    canonical_numbers = {}
    preface_unit_ids = set()
    if args.translator_notes:
        _reject_structured_source_markup(preface_data, chapters)
        active_rows = _structured_units(preface_data, chapters)
        try:
            canonical_notes = validate_translator_notes(
                args.translator_notes, active_rows, args.approval_root
            )
        except (TranslatorNotesError, OSError) as exc:
            raise SystemExit(f"invalid translator notes: {exc}") from exc
        for note in canonical_notes:
            notes_by_unit.setdefault(note["unit_id"], []).append(note)
        # Display numbering is local to the owning chapter and follows the
        # canonical order. A non-rendered approved row consumes no number.
        def block_unit_ids(blocks):
            unit_ids = set()
            def collect(block_list):
                for block in block_list:
                    if block["type"] == "paragraph":
                        unit_ids.add(block["unit_id"])
                    elif block["type"] == "blockquote":
                        collect(block["blocks"])
            collect(blocks)
            return unit_ids
        preface_unit_ids = block_unit_ids(preface_data["blocks"])
        chapter_unit_ids = [block_unit_ids(section["blocks"]) for section in chapters]
        if any(note["render"] and note["unit_id"] in preface_unit_ids
               for note in canonical_notes):
            raise SystemExit("rendered translator notes must belong to a chapter")
        for unit_ids in chapter_unit_ids:
            n = 0
            for note in canonical_notes:
                if note["render"] and note["unit_id"] in unit_ids:
                    n += 1
                    canonical_numbers[note["note_id"]] = n
        for note in canonical_notes:
            if note["render"] and note["note_id"] not in canonical_numbers:
                raise SystemExit("rendered translator note must belong to a rendered chapter")
        for unit_ids in chapter_unit_ids:
            n = 0
            for note in canonical_notes:
                if note["render"] and note["unit_id"] in unit_ids:
                    n += 1
                    canonical_numbers[note["note_id"]] = n
    else:
        # Deprecated migration-only behavior; existing Markdown builds retain
        # their historical CSV output, while canonical notes never use it.
        all_notes = load_glossary_notes(glossary_csv, glossary_notes_csv)
        chapter_bodies = (
            [section["blocks"] for section in chapters]
            if json_mode else [body for _, body in chapters]
        )
        eligible_notes = []
        chapter_numbers = []
        if all_notes:
            term_matchers = [
                (src, tgt, note, _source_re(html.escape(src)), html.escape(tgt))
                for src, tgt, note in all_notes
            ]
            chapter_blocks = [
                (_json_inline_blocks(blocks) if json_mode
                 else list(_iter_inline_blocks(blocks)))
                for blocks in chapter_bodies
            ]
            sim_t_placeholder = "\x00GLOSS_NOTE_0_T\x00"
            sim_s_placeholder = "\x00GLOSS_NOTE_0_S\x00"
            target_chapter = {}
            source_chapter = {}
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
            anchor_chapter = {}
            for src, tgt, note, src_re, tgt_esc in term_matchers:
                tc = target_chapter.get(src)
                sc = source_chapter.get(src)
                if tc is None and sc is None:
                    continue
                eligible_notes.append((src, tgt, note))
                anchor_chapter[src] = min(x for x in (tc, sc) if x is not None)
            missing = [src for src, _, _ in all_notes if src not in anchor_chapter]
            msg = f"术语注释: 加载 {len(all_notes)} 条，正文出现 {len(eligible_notes)} 条"
            if missing:
                msg += f"（未出现: {', '.join(missing)}）"
            print(msg)
            chapter_numbers = [{} for _ in chapter_blocks]
            for src, tgt, note, src_re, tgt_esc in term_matchers:
                ci = anchor_chapter.get(src)
                if ci is not None:
                    nums = chapter_numbers[ci - 1]
                    nums[src] = len(nums) + 1
        else:
            print("术语注释: 无（legacy glossary_notes.csv 缺失或为空，跳过）")
        gloss = GlossAnnotator(eligible_notes)

    book = epub.EpubBook()
    book.set_identifier(out_path.stem)
    book.set_title(title)
    book.set_language(LANG)
    book.add_author(author)

    # preface chapter
    preface = _StyledEpubHtml(
        title=preface_title,
        file_name="preface.xhtml",
        lang=LANG,
    )
    preface_body = body0 if json_mode else markdown_to_html(body0)
    preface.content = xhtml_fragment(preface_title, preface_body)
    book.add_item(preface)

    # chapters: annotate first in-book occurrences, then append each chapter's
    # popup-footnote asides (same document, so the noteref opens a popup).
    chapter_items = []
    chapter_by_id = {}
    for idx, section in enumerate(chapters, 1):
        section_title = section["title"] if json_mode else section[0]
        section_body = section["blocks"] if json_mode else section[1]
        chap_key = f"chap_{idx:03d}"
        if not args.translator_notes:
            gloss.begin_chapter(
                chap_key, chapter_numbers[idx - 1] if chapter_numbers else {}
            )
        item = _StyledEpubHtml(
            title=section_title,
            file_name=f"{chap_key}.xhtml",
            lang=LANG,
        )
        if args.translator_notes:
            rendered = json_blocks_to_translator_html(
                section_body, notes_by_unit, chap_key, canonical_numbers
            )
            section_unit_ids = set()
            def collect_section_units(blocks):
                for block in blocks:
                    if block["type"] == "paragraph":
                        section_unit_ids.add(block["unit_id"])
                    elif block["type"] == "blockquote":
                        collect_section_units(block["blocks"])
            collect_section_units(section_body)
            footnotes = translator_footnotes(
                [note for note in canonical_notes
                 if note["unit_id"] in section_unit_ids and note["render"]],
                canonical_numbers,
            )
        else:
            rendered = (json_blocks_to_html(section_body, gloss) if json_mode
                        else markdown_to_html(section_body, gloss))
            footnotes = gloss.render_footnotes()
        item.content = xhtml_fragment(section_title, rendered + footnotes)
        book.add_item(item)
        chapter_items.append(item)
        if json_mode:
            chapter_by_id[section["id"]] = item

    if json_mode:
        toc_sections = []
        for section, item in zip(chapters, chapter_items):
            if section["kind"] == "discussion":
                continue
            children = [
                (child_item, [])
                for child, child_item in zip(chapters, chapter_items)
                if child.get("parent_id") == section["id"]
            ]
            toc_sections.append((item, children) if children else item)
        book.toc = (epub.Section(preface_title), preface, *toc_sections)
    else:
        book.toc = (
            epub.Section(preface_title),
            preface,
            *chapter_items,
        )
    book.add_item(epub.EpubNcx())
    book.add_item(epub.EpubNav())
    book.spine = [preface, *chapter_items]

    epub.write_epub(str(out_path), book)

    # quick sanity check on the produced archive
    with zipfile.ZipFile(out_path) as z:
        names = z.namelist()
    size = out_path.stat().st_size
    src_size = input_path.stat().st_size
    print(f"wrote {out_path} ({size} bytes, {len(names)} zip entries)")
    if not json_mode:
        assert size > 30_000, "output too small"
        assert size > 0.15 * src_size, "output implausibly small vs input"
    assert "mimetype" in names and "META-INF/container.xml" in names


if __name__ == "__main__":
    main()
