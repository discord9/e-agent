#!/usr/bin/env python3
"""Focused stdlib tests for translate_tool.py validate; run directly."""
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


HERE = Path(__file__).resolve().parent
TOOL = HERE / 'translate_tool.py'


class ValidateTests(unittest.TestCase):
    def run_validate(self, source_rows, translation_rows, *extra,
                     source_bytes=None, translation_bytes=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'source.jsonl'
            translation = root / 'translation.jsonl'
            if source_bytes is None:
                source.write_text(''.join(json.dumps(row, ensure_ascii=False) + '\n'
                                           for row in source_rows), encoding='utf-8')
            else:
                source.write_bytes(source_bytes)
            if translation_bytes is None:
                translation.write_text(''.join(json.dumps(row, ensure_ascii=False) + '\n'
                                                for row in translation_rows), encoding='utf-8')
            else:
                translation.write_bytes(translation_bytes)
            result = subprocess.run(
                [sys.executable, str(TOOL), 'validate', '--source', str(source),
                 '--translation', str(translation), *extra],
                capture_output=True, text=True)
            return result

    @staticmethod
    def source(*rows):
        return [{'unit_id': unit_id, 'exact_text': text} for unit_id, text in rows]

    @staticmethod
    def translation(*rows):
        return [{'unit_id': unit_id, 'en': text, 'zh': zh, 'summary': summary,
                 'status': 'final', 'uncertainties': []}
                for unit_id, text, zh, summary in rows]

    def test_pass_and_expected_boundaries(self):
        result = self.run_validate(
            self.source(('u1', 'One'), ('u2', 'Two')),
            self.translation(('u1', 'One', '一', 'first'), ('u2', 'Two', '二', 'second')),
            '--expected-count', '2', '--first-id', 'u1', '--last-id', 'u2')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, 'PASS: 2 rows validated\n')

    def test_custom_schema_without_summary(self):
        result = self.run_validate(
            [{'unit_id': 'u1', 'exact_text': 'One'}],
            [{'unit_id': 'u1', 'en': 'One', 'zh': '一', 'summary': ''}],
            '--schema', 'legacy')
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_status_uncertainties_contract(self):
        row = self.translation(('u1', 'One', '一', 'ok'))[0]
        row['status'], row['uncertainties'] = 'needs_review', []
        result = self.run_validate(self.source(('u1', 'One')), [row])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('iff uncertainties', result.stderr)
        row['uncertainties'] = ['ambiguous']
        self.assertEqual(self.run_validate(self.source(('u1', 'One')), [row]).returncode, 0)

    def test_exact_en_mismatch(self):
        result = self.run_validate(self.source(('u1', 'One')),
                                   self.translation(('u1', 'Other', '一', 'ok')))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("row 1 field 'en'", result.stderr)

    def test_missing_field(self):
        result = self.run_validate(self.source(('u1', 'One')),
                                   [{'unit_id': 'u1', 'en': 'One', 'zh': '一', 'summary': 'ok'}])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("translation row 1 field 'status': missing", result.stderr)

    def test_duplicate_id(self):
        result = self.run_validate(self.source(('u1', 'One'), ('u2', 'Two')),
                                   self.translation(('u1', 'One', '一', 'ok'),
                                                    ('u1', 'Two', '二', 'dup')))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("translation row 2 field 'unit_id': duplicate ID", result.stderr)

    def test_source_duplicate_id(self):
        result = self.run_validate(self.source(('u1', 'One'), ('u1', 'Again')),
                                   self.translation(('u1', 'One', '一', 'ok'),
                                                    ('u1', 'Again', '再', 'dup')))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("source row 2 field 'unit_id': duplicate ID", result.stderr)

    def test_non_string_id(self):
        result = self.run_validate([{'unit_id': 1, 'exact_text': 'One'}],
                                   [{'unit_id': '1', 'en': 'One', 'zh': '一', 'summary': 'ok',
                                     'status': 'final', 'uncertainties': []}])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("source row 1 field 'unit_id': expected string", result.stderr)

    def test_order_and_id_mismatch(self):
        result = self.run_validate(self.source(('u1', 'One'), ('u2', 'Two')),
                                   self.translation(('u2', 'Two', '二', 'second'),
                                                    ('u1', 'One', '一', 'first')))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("row 1 field 'unit_id': ID/order mismatch", result.stderr)

    def test_schema_order(self):
        rows = [{'zh': '一', 'unit_id': 'u1', 'en': 'One', 'summary': 'ok',
                 'status': 'final', 'uncertainties': []}]
        result = self.run_validate(self.source(('u1', 'One')), rows)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('translation row 1 schema', result.stderr)

    def test_empty_zh(self):
        result = self.run_validate(self.source(('u1', 'One')),
                                   self.translation(('u1', 'One', '  ', 'ok')))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("translation row 1 field 'zh': must be a nonempty string", result.stderr)

    def test_blank_physical_lines(self):
        source = b'{"unit_id":"u1","exact_text":"One"}\n'
        translation = b'{"unit_id":"u1","en":"One","zh":"\xe4\xb8\x80","summary":"ok","status":"final","uncertainties":[]}\n'
        for blank in (b'\n' + source, b' \t\n' + source):
            result = self.run_validate([], [], source_bytes=blank,
                                       translation_bytes=translation)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('source row', result.stderr)
        result = self.run_validate([], [], source_bytes=source + b'\n\n',
                                   translation_bytes=translation)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('source: final newline', result.stderr)

    def test_line_endings_and_newline_failures(self):
        source = b'{"unit_id":"u1","exact_text":"One"}\n'
        translation = b'{"unit_id":"u1","en":"One","zh":"\xe4\xb8\x80","summary":"ok","status":"final","uncertainties":[]}\n'
        mixed = source + b'{"unit_id":"u2","exact_text":"Two"}\r\n'
        for data, label in ((source[:-1], 'source'), (source.replace(b'\n', b'\r\n'), 'source'),
                            (mixed, 'source'), (source + b'\n', 'source'),
                            (translation.replace(b'\n', b'\r\n'), 'translation')):
            result = self.run_validate([], [],
                                       source_bytes=data if label == 'source' else source,
                                       translation_bytes=data if label == 'translation' else translation)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('%s: final newline must be exactly one raw LF' % label, result.stderr)

    def test_duplicate_object_keys(self):
        source = b'{"unit_id":"u1","unit_id":"u2","exact_text":"One"}\n'
        translation = b'{"unit_id":"u1","en":"One","zh":"\xe4\xb8\x80","summary":"ok","status":"final","uncertainties":[]}\n'
        result = self.run_validate([], [], source_bytes=source, translation_bytes=translation)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('invalid JSON', result.stderr)

    def test_invalid_utf8_malformed_and_non_object(self):
        translation = b'{"unit_id":"u1","en":"One","zh":"\xe4\xb8\x80","summary":"ok","status":"final","uncertainties":[]}\n'
        for source, expected in ((b'\xff\n', 'invalid UTF-8'),
                                 (b'{bad}\n', 'invalid JSON'),
                                 (b'[1]\n', 'expected one JSON object')):
            result = self.run_validate([], [], source_bytes=source,
                                       translation_bytes=translation)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(expected, result.stderr)

    def test_physical_lines_in_mismatch(self):
        source = b'{"unit_id":"u0","exact_text":"Zero"}\n{"unit_id":"u1","exact_text":"One"}\n'
        translation = ('{"unit_id":"u0","en":"Zero","zh":"零","summary":"ok","status":"final","uncertainties":[]}\n'
                       '{"unit_id":"u1","en":"Other","zh":"一","summary":"ok","status":"final","uncertainties":[]}\n').encode()
        result = self.run_validate([], [], source_bytes=source, translation_bytes=translation)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('source line 2, translation line 2', result.stderr)

    def test_expected_count_and_boundary_failures(self):
        source = self.source(('u1', 'One'), ('u2', 'Two'))
        translation = self.translation(('u1', 'One', '一', 'first'),
                                       ('u2', 'Two', '二', 'second'))
        for flags, expected in ((('--expected-count', '3'), 'source row count'),
                                (('--first-id', 'wrong'), 'source first ID'),
                                (('--last-id', 'wrong'), 'source last ID')):
            result = self.run_validate(source, translation, *flags)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(expected, result.stderr)


if __name__ == '__main__':
    unittest.main()
