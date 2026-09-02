#!/usr/bin/env python3
"""Fixture-only regression tests for build_epub.py; run directly with python3."""
import ast
import hashlib
import html
from lxml import etree
import io
import json
import re
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
import zlib


HERE = Path(__file__).resolve().parent
SOURCE = HERE / "build_epub.py"


def load_markdown_functions():
    """Load only the pure Markdown helpers, without importing ebooklib."""
    tree = ast.parse(SOURCE.read_text(encoding="utf-8"), filename=str(SOURCE))
    wanted = {"_iter_inline_blocks", "inline", "markdown_to_html"}
    nodes = [node for node in tree.body
             if isinstance(node, ast.FunctionDef) and node.name in wanted]
    namespace = {"BOLD_RE": re.compile(r"\*\*(.+?)\*\*", re.S), "html": html}
    exec(compile(ast.Module(body=nodes, type_ignores=[]), str(SOURCE), "exec"), namespace)
    return namespace["_iter_inline_blocks"], namespace["markdown_to_html"]


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def test_quote_marker_fixture():
    iter_inline_blocks, markdown_to_html = load_markdown_functions()
    cases = (
        (">>>", "<p>&gt;&gt;&gt;</p>", ["&gt;&gt;&gt;"]),
        (">>text", "<p>&gt;&gt;text</p>", ["&gt;&gt;text"]),
        (">not-space", "<p>&gt;not-space</p>", ["&gt;not-space"]),
        ("> quote", "<blockquote><p>quote</p></blockquote>", ["quote"]),
        (">", "<blockquote></blockquote>", []),
    )
    for source, expected_html, expected_blocks in cases:
        actual_html = markdown_to_html(source)
        actual_blocks = list(iter_inline_blocks(source))
        check(actual_html == expected_html,
              f"unexpected HTML for {source!r}: {actual_html!r}")
        check(actual_blocks == expected_blocks,
              f"inline block mismatch for {source!r}: {actual_blocks!r}")


def test_bare_quote_separates_paragraphs():
    iter_inline_blocks, markdown_to_html = load_markdown_functions()
    source = "> quote\n>\n> second"
    check(markdown_to_html(source) ==
          "<blockquote><p>quote</p><p>second</p></blockquote>",
          "bare quote did not preserve the blockquote paragraph break")
    check(list(iter_inline_blocks(source)) == ["quote", "second"],
          "inline first pass disagrees on a bare quote paragraph break")


class BuildEpubTests(unittest.TestCase):
    def _run_json(self, document, *extra):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "book.json"
            output = root / "book.epub"
            source.write_text(json.dumps(document, ensure_ascii=False), encoding="utf-8")
            result = subprocess.run(
                [sys.executable, str(SOURCE), "--json", str(source),
                 "--out", str(output), *extra],
                capture_output=True, text=True,
            )
            output_bytes = output.read_bytes() if output.exists() else b""
            return result, output_bytes

    @staticmethod
    def _document(with_discussion=True):
        sections = [{
            "id": "chapter-1", "kind": "chapter", "parent_id": None,
            "title": "第一章", "blocks": [
                {"type": "paragraph", "text": "plain **markers**"},
                {"type": "blockquote", "blocks": [
                    {"type": "paragraph", "text": "quoted"}
                ]},
                {"type": "list", "items": ["one", "two"]},
            ],
        }]
        if with_discussion:
            sections += [{
                "id": "discussion-1", "kind": "discussion", "parent_id": "chapter-1",
                "title": "讨论", "blocks": [{"type": "paragraph", "text": "after"}],
            }, {
                "id": "chapter-2", "kind": "chapter", "parent_id": None,
                "title": "第二章", "blocks": [],
            }]
        return {
            "title": "结构化书", "author": "JSON 作者",
            "preface": {"id": "preface", "kind": "preface", "parent_id": None,
                        "title": "前言 **标题**", "blocks": [{"type": "paragraph", "text": "前言 **正文**"}]},
            "sections": sections,
        }

    def test_json_epub_nav_spine_and_zip_invariants(self):
        result, output = self._run_json(self._document())
        self.assertEqual(result.returncode, 0, result.stderr)
        with zipfile.ZipFile(io.BytesIO(output)) as archive:
            self.assertEqual(archive.namelist()[0], "mimetype")
            info = archive.getinfo("mimetype")
            self.assertEqual(info.compress_type, zipfile.ZIP_STORED)
            self.assertEqual(info.CRC, zlib.crc32(archive.read("mimetype")) & 0xffffffff)
            nav = archive.read("EPUB/nav.xhtml").decode()
            nav_root = etree.fromstring(nav.encode())
            ns = {"x": "http://www.w3.org/1999/xhtml"}
            toc = nav_root.find(".//x:nav", ns)
            top_links = toc.find("x:ol", ns).findall("x:li", ns)
            self.assertEqual(top_links[1].find("x:a", ns).get("href"), "chap_001.xhtml")
            self.assertEqual(top_links[1].find("x:ol/x:li/x:a", ns).get("href"), "chap_002.xhtml")
            self.assertEqual(top_links[2].find("x:a", ns).get("href"), "chap_003.xhtml")
            self.assertIsNotNone(top_links[1].find("x:ol", ns))
            self.assertIsNone(top_links[2].find("x:ol", ns))
            self.assertIn("chap_001.xhtml", nav)
            self.assertLess(nav.index("chap_001.xhtml"), nav.index("chap_002.xhtml"))
            chapter_link = nav.index('href="chap_001.xhtml"')
            child_link = nav.index('href="chap_002.xhtml"')
            self.assertLess(nav.index("<ol>", chapter_link), child_link)
            self.assertIn("chap_003.xhtml", nav)
            preface = etree.fromstring(archive.read("EPUB/preface.xhtml"))
            self.assertEqual(preface.find(".//x:h1", namespaces=ns).text, "前言 **标题**")
            self.assertEqual(preface.find(".//x:p", namespaces=ns).text, "前言 **正文**")
            body = archive.read("EPUB/chap_001.xhtml").decode()
            self.assertIn("plain **markers**", body)
            self.assertNotIn("<b>markers</b>", body)
            self.assertIn("<blockquote>", body)
            self.assertIn("<ul>", body)
            opf = archive.read("EPUB/content.opf").decode()
            self.assertIn("<dc:title>结构化书</dc:title>", opf)
            self.assertRegex(opf, r'<item[^>]+id="nav"[^>]+properties="nav"')
            spine = re.findall(r'<itemref idref="([^"]+)"', opf)
            self.assertEqual(spine, ["chapter_0", "chapter_1", "chapter_2", "chapter_3"])
            self.assertNotIn('idref="nav"', opf)

    def test_canonical_translation_records_exact_join(self):
        document = self._document(with_discussion=False)
        document["preface"]["blocks"][0]["unit_id"] = "p0"
        document["sections"][0]["blocks"][0]["unit_id"] = "u1"
        document["sections"][0]["blocks"][1]["blocks"][0]["unit_id"] = "uq"
        records = [
            {"unit_id": "p0", "en": "前言 **正文**", "zh": "介绍", "summary": "", "status": "final", "uncertainties": []},
            {"unit_id": "u1", "en": "plain **markers**", "zh": "普通标记", "summary": "", "status": "final", "uncertainties": []},
            {"unit_id": "uq", "en": "quoted", "zh": "引用", "summary": "", "status": "final", "uncertainties": []},
        ]
        document["sections"][0]["blocks"].pop()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, translations, output = root / "book.json", root / "translations.jsonl", root / "book.epub"
            source.write_text(json.dumps(document, ensure_ascii=False), encoding="utf-8")
            translations.write_text("".join(json.dumps(row, ensure_ascii=False, separators=(",", ":")) + "\n" for row in records), encoding="utf-8")
            result = subprocess.run([sys.executable, str(SOURCE), "--json", str(source), "--translations", str(translations), "--out", str(output)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            with zipfile.ZipFile(output) as archive:
                body = archive.read("EPUB/chap_001.xhtml").decode()
            self.assertIn("普通标记", body)
            self.assertNotIn("plain **markers**", body)

    def test_json_without_discussion_and_cli_overrides(self):
        document = self._document(with_discussion=False)
        result, output = self._run_json(document, "--title", "覆盖标题", "--author", "覆盖作者")
        self.assertEqual(result.returncode, 0, result.stderr)
        with zipfile.ZipFile(io.BytesIO(output)) as archive:
            opf = archive.read("EPUB/content.opf").decode()
            self.assertIn("<dc:title>覆盖标题</dc:title>", opf)
            self.assertIn("<dc:creator id=\"creator\">覆盖作者</dc:creator>", opf)
            self.assertRegex(opf, r'<item[^>]+id="nav"[^>]+properties="nav"')
            spine = re.findall(r'<itemref idref="([^"]+)"', opf)
            self.assertEqual(spine, ["chapter_0", "chapter_1"])
            self.assertNotIn('idref="nav"', opf)
            nav = archive.read("EPUB/nav.xhtml").decode()
            self.assertNotIn("chap_002.xhtml", nav)
            nav_root = etree.fromstring(nav.encode())
            ns = {"x": "http://www.w3.org/1999/xhtml"}
            toc = nav_root.find(".//x:nav", ns)
            top_links = toc.find("x:ol", ns).findall("x:li", ns)
            self.assertEqual(len(top_links), 2)
            self.assertTrue(all(link.find("x:ol", ns) is None for link in top_links[1:]))

    def test_json_discussion_nesting_only_for_explicit_children(self):
        result, output = self._run_json(self._document())
        self.assertEqual(result.returncode, 0, result.stderr)
        with zipfile.ZipFile(io.BytesIO(output)) as archive:
            nav = archive.read("EPUB/nav.xhtml").decode()
            nav_root = etree.fromstring(nav.encode())
            ns = {"x": "http://www.w3.org/1999/xhtml"}
            toc = nav_root.find(".//x:nav", ns)
            top_links = toc.find("x:ol", ns).findall("x:li", ns)
            self.assertIsNotNone(top_links[1].find("x:ol", ns))
            self.assertIsNone(top_links[2].find("x:ol", ns))

    def test_json_newlines_escape_and_markdown_inert(self):
        document = self._document(with_discussion=False)
        document["sections"][0]["blocks"] = [
            {"type": "paragraph", "text": "paragraph one\nparagraph two <>& # > - **bold**"},
            {"type": "blockquote", "blocks": [
                {"type": "paragraph", "text": "quote one\nquote two"},
                {"type": "paragraph", "text": "quote <>& # > - **text**"},
            ]},
            {"type": "list", "items": ["item one\nitem two <>& # > - **text**"]},
        ]
        result, output = self._run_json(document)
        self.assertEqual(result.returncode, 0, result.stderr)
        with zipfile.ZipFile(io.BytesIO(output)) as archive:
            body = archive.read("EPUB/chap_001.xhtml").decode()
            self.assertIn("paragraph one<br/>paragraph two &lt;&gt;&amp; # &gt; - **bold**", body)
            self.assertIn("<blockquote><p>quote one<br/>quote two</p>", body)
            self.assertIn("quote &lt;&gt;&amp; # &gt; - **text**", body)
            self.assertIn("<li>item one<br/>item two &lt;&gt;&amp; # &gt; - **text**</li>", body)
            self.assertNotIn("<b>", body)
            for name in archive.namelist():
                if name.endswith(".xhtml"):
                    etree.fromstring(archive.read(name))

    def test_json_rejects_duplicate_and_invalid_parent(self):
        duplicate = self._document(with_discussion=False)
        duplicate["sections"].append(dict(duplicate["sections"][0]))
        result, _ = self._run_json(duplicate)
        self.assertIn("invalid JSON input: sections[1].id is missing or duplicated", result.stderr)
        invalid = self._document()
        invalid["sections"][1]["parent_id"] = "missing"
        result, _ = self._run_json(invalid)
        self.assertIn("invalid JSON input: sections[1].parent_id must reference a previous top-level chapter", result.stderr)

    def test_json_rejects_strict_preface_and_section_kinds(self):
        cases = [
            ("list", ["preface", [{"type": "paragraph", "text": "x"}]],
             "preface must be an object"),
            ("missing id", {"remove": "id"}, "preface missing field id"),
            ("empty id", {"id": ""}, "preface.id must be a non-empty string"),
            ("duplicate preface id", {"id": "chapter-1"}, "sections[0].id is missing or duplicated"),
            ("wrong kind", {"kind": "chapter"}, "preface.kind must be 'preface'"),
            ("parent", {"parent_id": "x"}, "preface.parent_id must be null"),
            ("section preface", {"section_kind": "preface"}, "sections[0].kind must be 'chapter' or 'discussion'"),
            ("forward parent", {"forward": True}, "sections[0].parent_id must reference a previous top-level chapter"),
        ]
        for label, change, expected in cases:
            with self.subTest(label=label):
                document = self._document(with_discussion=False)
                if label == "list":
                    document["preface"] = change[1]
                elif label == "section preface":
                    document["sections"][0]["kind"] = change["section_kind"]
                elif label == "forward parent":
                    document["sections"].insert(0, {
                        "id": "discussion", "kind": "discussion", "parent_id": "chapter-1",
                        "title": "D", "blocks": [],
                    })
                else:
                    if "remove" in change:
                        document["preface"].pop(change["remove"])
                    else:
                        document["preface"].update(change)
                result, _ = self._run_json(document)
                self.assertIn("invalid JSON input: " + expected, result.stderr)

    def test_old_markdown_cli_builds_preface_and_markdown_block(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "book.md"
            output = root / "book.epub"
            source.write_text(
                "# Markdown Book\n\n**bold** in preface\n\n> quoted preface\n\n"
                "# Chapter\n\n" + "\n".join(
                    hashlib.sha256(str(i).encode()).hexdigest()
                    for i in range(10000)
                ) + "\n",
                encoding="utf-8",
            )
            result = subprocess.run(
                [sys.executable, str(SOURCE), "--md", str(source),
                 "--out", str(output), "--title", "Markdown Book",
                 "--author", "Author"],
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            with zipfile.ZipFile(io.BytesIO(output.read_bytes())) as archive:
                preface = archive.read("EPUB/preface.xhtml").decode()
                self.assertIn("<b>bold</b>", preface)
                self.assertIn("<blockquote><p>quoted preface</p></blockquote>", preface)
                opf = archive.read("EPUB/content.opf").decode()
                self.assertRegex(opf, r'<item[^>]+id="nav"[^>]+properties="nav"')
                spine = re.findall(r'<itemref idref="([^"]+)"', opf)
                self.assertEqual(spine, ["chapter_0", "chapter_1"])
                self.assertNotIn('idref="nav"', opf)

    def test_old_markdown_parser_fixture_still_passes(self):
        test_quote_marker_fixture()
        test_bare_quote_separates_paragraphs()

class CanonicalTranslatorNotesTests(unittest.TestCase):
    @staticmethod
    def _canonical_fixture(root, chapter_specs, note_specs, preface_text="Intro"):
        """Create a structured book and matching, approved canonical fixtures."""
        document = {
            "title": "Canonical notes", "preface": {
                "id": "preface", "kind": "preface", "parent_id": None,
                "title": "Preface", "blocks": [
                    {"type": "paragraph", "unit_id": "p0", "text": preface_text}
                ],
            },
            "sections": [],
        }
        for index, (title, blocks) in enumerate(chapter_specs, 1):
            document["sections"].append({
                "id": f"chapter-{index}", "kind": "chapter", "parent_id": None,
                "title": title, "blocks": blocks,
            })
        records = []
        def collect(blocks):
            for block in blocks:
                if block["type"] == "paragraph":
                    records.append({"unit_id": block["unit_id"], "en": block["text"],
                                    "zh": block["text"], "summary": "",
                                    "status": "final", "uncertainties": []})
                elif block["type"] == "blockquote":
                    collect(block["blocks"])
        collect(document["preface"]["blocks"])
        for section in document["sections"]:
            collect(section["blocks"])
        (root / "translations.jsonl").write_text(
            "".join(json.dumps(row, ensure_ascii=False, separators=(",", ":")) + "\n"
                    for row in records), encoding="utf-8")
        rows = []
        for order, spec in enumerate(note_specs, 1):
            row = {
                "schema_version": 1, "kind": "translator_note",
                "note_id": spec["note_id"], "unit_id": spec["unit_id"],
                "order": spec.get("order", order),
                "anchor": {"text": spec["text"], "occurrence": spec.get("occurrence", 1),
                           "unit_zh_sha256": hashlib.sha256(
                               spec["unit_text"].encode()).hexdigest()},
                "note": spec.get("note", "事实说明。"),
                "factual_scope": "事实。", "render": spec.get("render", True),
                "provenance": {
                    "adjudication": {"artifact": "adjudication.jsonl", "sha256": "0" * 64,
                                      "key": "decision-" + spec["note_id"]},
                    "review": {"artifact": "review.jsonl", "sha256": "0" * 64,
                               "key": "review-" + spec["note_id"]},
                },
            }
            rows.append(row)
        subject_keys = {"schema_version", "kind", "note_id", "unit_id", "order",
                        "anchor", "note", "factual_scope", "render"}
        for filename, prefix, decision in (("adjudication.jsonl", "decision-", "CONFIRMED"),
                                           ("review.jsonl", "review-", "APPROVE")):
            entries = []
            for row in rows:
                subject = {key: row[key] for key in subject_keys}
                subject_sha = hashlib.sha256(json.dumps(
                    subject, ensure_ascii=False, sort_keys=True,
                    separators=(",", ":")).encode()).hexdigest()
                entries.append({"key": prefix + row["note_id"],
                                "note_id": row["note_id"], "unit_id": row["unit_id"],
                                "decision": decision, "subject_sha256": subject_sha})
            (root / filename).write_text("".join(json.dumps(entry, ensure_ascii=False,
                                                              separators=(",", ":")) + "\n"
                                          for entry in entries), encoding="utf-8")
        for row in rows:
            for filename, field in (("adjudication.jsonl", "adjudication"),
                                    ("review.jsonl", "review")):
                row["provenance"][field]["sha256"] = hashlib.sha256(
                    (root / filename).read_bytes()).hexdigest()
        note_path = root / "translator_notes.jsonl"
        note_path.write_text("".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows),
                             encoding="utf-8")
        source = root / "book.json"
        source.write_text(json.dumps(document, ensure_ascii=False), encoding="utf-8")
        return source, note_path, document

    @staticmethod
    def _run_canonical(source, output, notes, root):
        document = json.loads(source.read_text(encoding="utf-8"))
        rows = []
        def visit(blocks):
            for block in blocks:
                if block["type"] == "paragraph":
                    rows.append({"unit_id": block["unit_id"], "en": block["text"],
                                 "zh": block["text"], "summary": "",
                                 "status": "final", "uncertainties": []})
                elif block["type"] == "blockquote":
                    visit(block["blocks"])
        visit(document["preface"]["blocks"])
        for section in document["sections"]:
            visit(section["blocks"])
        (root / "translations.jsonl").write_text(
            "".join(json.dumps(row, ensure_ascii=False, separators=(",", ":")) + "\n"
                    for row in rows), encoding="utf-8")
        return subprocess.run([
            sys.executable, str(SOURCE), "--json", str(source), "--out", str(output),
            "--translator-notes", str(notes), "--approval-root", str(root),
            "--translations", str(root / "translations.jsonl"),
        ], capture_output=True, text=True)

    def test_canonical_mode_rejects_unidentified_and_unsupported_text(self):
        for mutate, expected in (
            (lambda d: d["sections"][0]["blocks"][0].pop("unit_id"), "requires unit_id"),
            (lambda d: d["sections"][0]["blocks"].append({"type": "list", "items": ["orphan"]}), "does not support textual"),
        ):
            with self.subTest(expected=expected), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                document = {"title": "Book", "preface": {"id": "p", "kind": "preface", "parent_id": None, "title": "P", "blocks": []}, "sections": [{"id": "c", "kind": "chapter", "parent_id": None, "title": "C", "blocks": [{"type": "paragraph", "unit_id": "u1", "text": "text"}]}]}
                mutate(document)
                source = root / "book.json"
                source.write_text(json.dumps(document, ensure_ascii=False), encoding="utf-8")
                translations = root / "translations.jsonl"
                translations.write_text("{\"unit_id\":\"u1\",\"en\":\"text\",\"zh\":\"译文\",\"summary\":\"\",\"status\":\"final\",\"uncertainties\":[]}\n", encoding="utf-8")
                result = subprocess.run([sys.executable, str(SOURCE), "--json", str(source), "--translations", str(translations), "--out", str(root / "x.epub")], capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, result.stderr)

    def test_translator_notes_require_canonical_translations(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, notes, _ = self._canonical_fixture(root, [("Chapter", [{"type": "paragraph", "unit_id": "u1", "text": "anchor"}])], [{"note_id": "n1", "unit_id": "u1", "unit_text": "anchor", "text": "anchor"}])
            result = subprocess.run([sys.executable, str(SOURCE), "--json", str(source), "--translator-notes", str(notes), "--approval-root", str(root), "--out", str(root / "x.epub")], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("requires --translations", result.stderr)

    def test_canonical_notes_render_with_local_numbering_and_links(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            document = {
                "title": "Notes", "preface": {
                    "id": "preface", "kind": "preface", "parent_id": None,
                    "title": "Preface", "blocks": [{"type": "paragraph", "unit_id": "p0", "text": "Intro"}],
                },
                "sections": [{
                    "id": "chapter-1", "kind": "chapter", "parent_id": None,
                    "title": "Chapter", "blocks": [{"type": "paragraph", "unit_id": "u1", "text": "Alpha anchor."}],
                }],
            }
            adjudication = root / "adjudication.jsonl"
            review = root / "review.jsonl"
            digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
            note_path = root / "translator_notes.jsonl"
            rows = []
            for order, note_id, render in ((1, "first", True), (2, "hidden", False)):
                rows.append({
                    "schema_version": 1, "kind": "translator_note", "note_id": note_id,
                    "unit_id": "u1", "order": order,
                    "anchor": {"text": "anchor", "occurrence": 1,
                               "unit_zh_sha256": hashlib.sha256(b"Alpha anchor.").hexdigest()},
                    "note": "事实说明。", "factual_scope": "事实。", "render": render,
                    "provenance": {
                        "adjudication": {"artifact": "adjudication.jsonl", "sha256": "0" * 64, "key": "decision-" + note_id},
                        "review": {"artifact": "review.jsonl", "sha256": "0" * 64, "key": "review-" + note_id},
                    },
                })
            subject_keys = {"schema_version", "kind", "note_id", "unit_id", "order",
                            "anchor", "note", "factual_scope", "render"}
            for path, prefix, decision in ((adjudication, "decision-", "CONFIRMED"),
                                           (review, "review-", "APPROVE")):
                entries = []
                for row in rows:
                    subject = {k: row[k] for k in subject_keys}
                    subject_sha = hashlib.sha256(json.dumps(
                        subject, ensure_ascii=False, sort_keys=True,
                        separators=(",", ":")).encode()).hexdigest()
                    entries.append({"key": prefix + row["note_id"],
                                    "note_id": row["note_id"], "unit_id": row["unit_id"],
                                    "decision": decision, "subject_sha256": subject_sha})
                path.write_text("".join(json.dumps(entry, ensure_ascii=False,
                                                     separators=(",", ":")) + "\n"
                                   for entry in entries), encoding="utf-8")
            for row in rows:
                row["provenance"]["adjudication"]["sha256"] = digest(adjudication)
                row["provenance"]["review"]["sha256"] = digest(review)
            note_path.write_text("".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows), encoding="utf-8")
            source = root / "book.json"
            output = root / "book.epub"
            source.write_text(json.dumps(document, ensure_ascii=False), encoding="utf-8")
            records = [{"unit_id": "p0", "en": "Intro", "zh": "Intro", "summary": "", "status": "final", "uncertainties": []},
                       {"unit_id": "u1", "en": "Alpha anchor.", "zh": "Alpha anchor.", "summary": "", "status": "final", "uncertainties": []}]
            (root / "translations.jsonl").write_text(
                "".join(json.dumps(row, ensure_ascii=False, separators=(",", ":")) + "\n" for row in records),
                encoding="utf-8")
            result = subprocess.run([
                sys.executable, str(SOURCE), "--json", str(source), "--out", str(output),
                "--translator-notes", str(note_path), "--approval-root", str(root),
                "--translations", str(root / "translations.jsonl"),
            ], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            with zipfile.ZipFile(output) as archive:
                body = archive.read("EPUB/chap_001.xhtml").decode()
            self.assertEqual(body.count('id="tnref-first"'), 1)
            self.assertEqual(body.count('id="tn-first"'), 1)
            self.assertEqual(body.count('translator-note-backlink'), 1)
            self.assertIn('href="#tn-first">[1]</a>', body)
            self.assertNotIn("hidden", body)
            style = etree.fromstring(body.encode()).find(".//{http://www.w3.org/1999/xhtml}style")
            self.assertIn("a.noteref, a.translator-noteref", style.text)
            self.assertIn("a.translator-noteref:hover", style.text)

    def test_canonical_offset_zero_occurrence_two_and_structured_rendering(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            blocks = [
                {"type": "paragraph", "unit_id": "u0",
                 "text": "anchor\nanchor <>& **bold**"},
                {"type": "blockquote", "blocks": [{"type": "paragraph", "unit_id": "uq",
                                                       "text": "quote\nline <>&"}]},
            ]
            source, notes, _ = self._canonical_fixture(root, [("Chapter", blocks)], [
                {"note_id": "offset-zero", "unit_id": "u0", "unit_text": blocks[0]["text"],
                 "text": "anchor", "occurrence": 1},
                {"note_id": "offset-two", "unit_id": "u0", "unit_text": blocks[0]["text"],
                 "text": "anchor", "occurrence": 2},
            ])
            output = root / "book.epub"
            result = self._run_canonical(source, output, notes, root)
            self.assertEqual(result.returncode, 0, result.stderr)
            with zipfile.ZipFile(output) as archive:
                body = archive.read("EPUB/chap_001.xhtml").decode()
                etree.fromstring(body.encode())
            self.assertIn("anchor<a epub:type=\"noteref\" class=\"translator-noteref\" id=\"tnref-offset-zero\"", body)
            self.assertIn("<br/>anchor<a epub:type=\"noteref\" class=\"translator-noteref\" id=\"tnref-offset-two\"", body)
            self.assertIn("&lt;&gt;&amp; **bold**", body)
            self.assertIn("<blockquote><p>quote<br/>line &lt;&gt;&amp;</p></blockquote>", body)
            self.assertNotIn("\\n", body)

    def test_canonical_equal_and_distinct_overlap_rejection(self):
        for second_text, second_occurrence in (("aa", 1), ("aa", 2)):
            with self.subTest(second=(second_text, second_occurrence)), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                blocks = [{"type": "paragraph", "unit_id": "u1", "text": "aaaa"}]
                source, notes, _ = self._canonical_fixture(
                    root, [("Chapter", blocks)], [
                        {"note_id": "one", "unit_id": "u1", "unit_text": "aaaa", "text": "aaa"},
                        {"note_id": "two", "unit_id": "u1", "unit_text": "aaaa",
                         "text": second_text, "occurrence": second_occurrence},
                    ])
                result = self._run_canonical(source, root / "book.epub", notes, root)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("selected anchor ranges overlap", result.stderr)

    def test_canonical_two_chapters_reset_numbers_and_order_is_deterministic(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            blocks1 = [{"type": "paragraph", "unit_id": "u1", "text": "first second"}]
            blocks2 = [{"type": "paragraph", "unit_id": "u2", "text": "third fourth"}]
            specs = [
                {"note_id": "second", "unit_id": "u1", "unit_text": "first second", "text": "second"},
                {"note_id": "first", "unit_id": "u1", "unit_text": "first second", "text": "first"},
                {"note_id": "third", "unit_id": "u2", "unit_text": "third fourth", "text": "third"},
            ]
            source, notes, _ = self._canonical_fixture(
                root, [("One", blocks1), ("Two", blocks2)], specs)
            output = root / "book.epub"
            result = self._run_canonical(source, output, notes, root)
            self.assertEqual(result.returncode, 0, result.stderr)
            with zipfile.ZipFile(output) as archive:
                chapter1 = archive.read("EPUB/chap_001.xhtml").decode()
                chapter2 = archive.read("EPUB/chap_002.xhtml").decode()
                first_snapshot = (chapter1, chapter2)
            self.assertLess(chapter1.index('id="tnref-first"'), chapter1.index('id="tnref-second"'))
            self.assertIn('href="#tn-first">[2]</a>', chapter1)
            self.assertIn('href="#tn-second">[1]</a>', chapter1)
            self.assertIn('href="#tn-third">[1]</a>', chapter2)
            self.assertLess(chapter1.index('id="tn-second"'), chapter1.index('id="tn-first"'))
            result = self._run_canonical(source, output, notes, root)
            self.assertEqual(result.returncode, 0, result.stderr)
            with zipfile.ZipFile(output) as archive:
                self.assertEqual(first_snapshot, (
                    archive.read("EPUB/chap_001.xhtml").decode(),
                    archive.read("EPUB/chap_002.xhtml").decode()))

    def test_canonical_source_markup_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            blocks = [{"type": "paragraph", "unit_id": "u1", "text": "bad <a id=\"x\">anchor</a>"}]
            source, notes, _ = self._canonical_fixture(root, [("Chapter", blocks)], [])
            result = self._run_canonical(source, root / "book.epub", notes, root)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("cannot render source anchor markup", result.stderr)

    def test_canonical_failure_preserves_existing_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            blocks = [{"type": "paragraph", "unit_id": "u1", "text": "anchor"}]
            source, notes, _ = self._canonical_fixture(root, [("Chapter", blocks)], [{
                "note_id": "n1", "unit_id": "u1", "unit_text": "anchor", "text": "anchor",
            }])
            output = root / "book.epub"
            self.assertEqual(self._run_canonical(source, output, notes, root).returncode, 0)
            original = output.read_bytes()
            valid = json.loads(notes.read_text(encoding="utf-8"))
            for bad in (dict(valid, note="changed"), {"broken": True}):
                notes.write_text(json.dumps(bad) + "\n", encoding="utf-8")
                result = self._run_canonical(source, output, notes, root)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(output.read_bytes(), original)
                notes.write_text(json.dumps(valid, ensure_ascii=False) + "\n", encoding="utf-8")

    def test_canonical_default_legacy_notes_conflict(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            blocks = [{"type": "paragraph", "unit_id": "u1", "text": "anchor"}]
            source, notes, _ = self._canonical_fixture(root, [("Chapter", blocks)], [{
                "note_id": "n1", "unit_id": "u1", "unit_text": "anchor", "text": "anchor",
            }])
            result = subprocess.run([
                sys.executable, str(SOURCE), "--json", str(source), "--out", str(root / "book.epub"),
                "--translator-notes", str(notes), "--approval-root", str(root),
                "--glossary-notes", str(root / "glossary_notes.csv"),
            ], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("cannot be combined", result.stderr)

    def test_legacy_no_note_output_has_no_note_markup(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "book.md"
            output = root / "book.epub"
            source.write_text(
                "# Vox Vitae（生命之声）中文全译本\n\n# Chapter\n\n" + " ".join(
                    hashlib.sha256(str(i).encode()).hexdigest() for i in range(10000)
                ) + "\n", encoding="utf-8")
            result = subprocess.run([sys.executable, str(SOURCE), "--md", str(source),
                                     "--out", str(output)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            with zipfile.ZipFile(output) as archive:
                names = archive.namelist()
                xhtml = {name: archive.read(name).decode() for name in names if name.endswith(".xhtml")}
            self.assertTrue(xhtml)
            self.assertTrue(all('id="tnref-' not in text and "footnote" not in text
                                for text in xhtml.values()))

    def test_canonical_notes_reject_markdown_and_legacy_combination(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            md = root / "book.md"
            md.write_text("# Book\n\n# Chapter\n\ntext\n", encoding="utf-8")
            notes = root / "notes.jsonl"
            notes.write_text("{}\n", encoding="utf-8")
            result = subprocess.run([
                sys.executable, str(SOURCE), "--md", str(md), "--translator-notes", str(notes),
                "--approval-root", str(root), "--glossary-notes", str(root / "old.csv"),
            ], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("cannot be combined", result.stderr)
            result = subprocess.run([
                sys.executable, str(SOURCE), "--md", str(md), "--translator-notes", str(notes),
                "--approval-root", str(root),
            ], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("requires --json", result.stderr)


if __name__ == "__main__":
    unittest.main()
