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
            spine = re.findall(r'<itemref idref="(chapter_[0-9]+)"', opf)
            self.assertEqual(spine, ["chapter_0", "chapter_1", "chapter_2", "chapter_3"])

    def test_json_without_discussion_and_cli_overrides(self):
        document = self._document(with_discussion=False)
        result, output = self._run_json(document, "--title", "覆盖标题", "--author", "覆盖作者")
        self.assertEqual(result.returncode, 0, result.stderr)
        with zipfile.ZipFile(io.BytesIO(output)) as archive:
            opf = archive.read("EPUB/content.opf").decode()
            self.assertIn("<dc:title>覆盖标题</dc:title>", opf)
            self.assertIn("<dc:creator id=\"creator\">覆盖作者</dc:creator>", opf)
            nav = archive.read("EPUB/nav.xhtml").decode()
            self.assertNotIn("chap_002.xhtml", nav)

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

    def test_old_markdown_parser_fixture_still_passes(self):
        test_quote_marker_fixture()
        test_bare_quote_separates_paragraphs()


if __name__ == "__main__":
    unittest.main()
