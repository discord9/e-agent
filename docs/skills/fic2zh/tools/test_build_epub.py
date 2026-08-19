#!/usr/bin/env python3
"""Fixture-only regression tests for build_epub.py; run directly with python3."""
import ast
import html
import re
from pathlib import Path


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


if __name__ == "__main__":
    test_quote_marker_fixture()
    test_bare_quote_separates_paragraphs()
    print("PASS: EPUB quote-marker fixtures and parser consistency")
