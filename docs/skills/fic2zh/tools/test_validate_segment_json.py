#!/usr/bin/env python3
"""Focused stdlib tests for validate_segment_json.py; run directly."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


HERE = Path(__file__).resolve().parent
TOOL = HERE / "validate_segment_json.py"


class ValidateSegmentJsonTests(unittest.TestCase):
    def setUp(self):
        self.tempdir = tempfile.TemporaryDirectory()
        self.root = Path(self.tempdir.name)
        self.source = self.root / "segment.txt"
        self.translation = self.root / "segment.json"
        self.source_text = "First paragraph.\n\nSecond paragraph.\n"
        self.source.write_bytes(self.source_text.encode("utf-8"))

    def tearDown(self):
        self.tempdir.cleanup()

    def write_translation(self, value, final_newline=True):
        data = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
        if final_newline:
            data += "\n"
        self.translation.write_text(data, encoding="utf-8", newline="")

    def run_validator(self, *extra):
        return subprocess.run(
            [
                sys.executable,
                str(TOOL),
                "--source",
                str(self.source),
                "--translation",
                str(self.translation),
                *extra,
            ],
            capture_output=True,
            text=True,
            encoding="utf-8",
        )

    def valid_translation(self):
        return [{"id": "ch1-seg0", "en": self.source_text, "zh": "第一段。\n\n第二段。\n", "summary": "摘要",
                 "status": "final", "uncertainties": []}]

    def test_pass(self):
        self.write_translation(self.valid_translation())
        result = self.run_validator("--expected-id", "ch1-seg0")
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "PASS: segment JSON validated\n")
        self.assertEqual(result.stderr, "")

    def test_top_level_must_be_array(self):
        self.write_translation(self.valid_translation()[0])
        result = self.run_validator()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("top-level must be an array", result.stderr)

    def test_array_must_contain_exactly_one_object(self):
        self.write_translation([])
        result = self.run_validator()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exactly one object", result.stderr)

        self.write_translation(self.valid_translation() * 2)
        result = self.run_validator()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exactly one object", result.stderr)

    def test_schema_and_field_types(self):
        wrong_order = {"en": self.source_text, "id": "ch1-seg0", "zh": "译文", "summary": "摘要"}
        self.write_translation([wrong_order])
        result = self.run_validator()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("schema keys", result.stderr)

        wrong_type = self.valid_translation()
        wrong_type[0]["summary"] = 7
        self.write_translation(wrong_type)
        result = self.run_validator()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("field 'summary' must be a string", result.stderr)

    def test_en_must_exactly_match_source(self):
        value = self.valid_translation()
        value[0]["en"] = self.source_text.rstrip("\n")
        self.write_translation(value)
        result = self.run_validator()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("en' does not exactly match source", result.stderr)

    def test_empty_zh_fails(self):
        value = self.valid_translation()
        value[0]["zh"] = " \n"
        self.write_translation(value)
        result = self.run_validator()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("field 'zh' must be a nonempty string", result.stderr)

    def test_source_and_translation_need_exactly_one_final_lf(self):
        self.write_translation(self.valid_translation(), final_newline=False)
        result = self.run_validator()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("translation: JSON framing must have exactly one raw LF", result.stderr)

        self.write_translation(self.valid_translation())
        self.source.write_bytes(self.source.read_bytes() + b"\n")
        value = self.valid_translation()[0]
        value["en"] = self.source.read_bytes().decode()
        value["zh"] += "\n"
        self.write_translation([value])
        result = self.run_validator()
        self.assertEqual(result.returncode, 0, result.stderr)

        self.source.write_bytes(self.source_text.replace("\n", "\r\n").encode("utf-8"))
        result = self.run_validator()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("source: raw CR is not allowed", result.stderr)

    def test_source_trailing_newline_state_and_shape(self):
        for source_text, zh in (("One", "一"), ("One\n", "一\n"),
                                ("One\n\nTwo", "一\n\n二")):
            self.source.write_bytes(source_text.encode())
            value = self.valid_translation()[0]
            value["en"], value["zh"] = source_text, zh
            self.write_translation([value])
            self.assertEqual(self.run_validator().returncode, 0)
        value = self.valid_translation()[0]
        value["en"] = "One\n\nTwo"
        value["zh"] = "一\n二"
        self.write_translation([value])
        result = self.run_validator()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("paragraph, blank-line, and trailing-newline shape", result.stderr)

    def test_status_and_uncertainties_contract(self):
        value = self.valid_translation()[0]
        value["status"] = "needs_review"
        self.write_translation([value])
        result = self.run_validator()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("iff uncertainties", result.stderr)
        value["uncertainties"] = ["ambiguous"]
        self.write_translation([value])
        self.assertEqual(self.run_validator().returncode, 0)
        value["uncertainties"] = [1]
        self.write_translation([value])
        self.assertNotEqual(self.run_validator().returncode, 0)

    def test_expected_id_must_match_exactly(self):
        self.write_translation(self.valid_translation())
        result = self.run_validator("--expected-id", "wrong-id")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("id' does not match expected ID", result.stderr)


if __name__ == "__main__":
    unittest.main()
