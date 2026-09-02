#!/usr/bin/env python3
"""Focused stdlib tests for the fail-closed translator-note loader."""
import hashlib
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
TOOL = HERE / "validate_translator_notes.py"
spec = importlib.util.spec_from_file_location("validate_translator_notes", TOOL)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)


class TranslatorNotesTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.adjudication = self.root / "decisions.txt"
        self.review = self.root / "reviews.txt"
        self.adjudication.write_text("{}\n", encoding="utf-8")
        self.review.write_text("{}\n", encoding="utf-8")
        self.zh = "这是锚点，锚点。"
        self.active = [{"unit_id": "u1", "zh": self.zh}]

    def tearDown(self):
        self.tmp.cleanup()

    def _row(self, **changes):
        def digest(path):
            return hashlib.sha256(path.read_bytes()).hexdigest()
        row = {
            "schema_version": 1, "kind": "translator_note", "note_id": "note-1",
            "unit_id": "u1", "order": 1,
            "anchor": {"text": "锚点", "occurrence": 1,
                       "unit_zh_sha256": hashlib.sha256(self.zh.encode()).hexdigest()},
            "note": "这是一个事实说明。", "factual_scope": "词源事实。", "render": True,
            "provenance": {
                "adjudication": {"artifact": "decisions.txt", "sha256": digest(self.adjudication), "key": "decision-1"},
                "review": {"artifact": "reviews.txt", "sha256": digest(self.review), "key": "review-1"},
            },
        }
        row.update(changes)
        subject = mod.subject_digest(row)
        for path, key, decision in ((self.adjudication, "decision-1", "CONFIRMED"),
                                    (self.review, "review-1", "APPROVE")):
            path.write_text(json.dumps({"key": key, "note_id": row["note_id"],
                                        "unit_id": row["unit_id"], "decision": decision,
                                        "subject_sha256": subject}, ensure_ascii=False,
                                       separators=(",", ":")) + "\n", encoding="utf-8")
        for record, path in ((row["provenance"]["adjudication"], self.adjudication),
                             (row["provenance"]["review"], self.review)):
            record["sha256"] = digest(path)
        return row

    def _refresh_provenance(self, rows, review_decision="APPROVE"):
        """Write one complete pair of artifacts for rows after row mutations."""
        subject_keys = {"schema_version", "kind", "note_id", "unit_id", "order",
                        "anchor", "note", "factual_scope", "render"}
        for path, field, decision in ((self.adjudication, "adjudication", "CONFIRMED"),
                                      (self.review, "review", review_decision)):
            entries = []
            for row in rows:
                subject = {key: row[key] for key in subject_keys}
                subject_sha = hashlib.sha256(json.dumps(
                    subject, ensure_ascii=False, sort_keys=True,
                    separators=(",", ":")).encode()).hexdigest()
                entries.append({"key": row["provenance"][field]["key"],
                                "note_id": row["note_id"], "unit_id": row["unit_id"],
                                "decision": decision, "subject_sha256": subject_sha})
            path.write_text("".join(json.dumps(entry, ensure_ascii=False,
                                                 separators=(",", ":")) + "\n"
                               for entry in entries), encoding="utf-8")
        digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
        for row in rows:
            row["provenance"]["adjudication"]["sha256"] = digest(self.adjudication)
            row["provenance"]["review"]["sha256"] = digest(self.review)

    def _write(self, rows, raw=None):
        path = self.root / "translator_notes.jsonl"
        if raw is None:
            raw = "".join(json.dumps(row, ensure_ascii=False, separators=(",", ":")) + "\n" for row in rows)
        path.write_bytes(raw.encode("utf-8") if isinstance(raw, str) else raw)
        return path

    def _validate(self, rows=None, path=None, active=None):
        return mod.validate_translator_notes(
            path or self._write(rows or [self._row()]),
            active if active is not None else self.active,
            self.root,
        )

    def test_valid_load_returns_plain_dict(self):
        result = self._validate()
        self.assertIs(type(result[0]), dict)
        self.assertEqual(result[0]["note_id"], "note-1")

    def test_duplicate_keys_unknown_fields_bom_and_blank_lines(self):
        row = json.dumps(self._row(), ensure_ascii=False, separators=(",", ":"))
        cases = [
            row.replace('"kind":"translator_note"', '"kind":"translator_note","kind":"translator_note"'),
            json.dumps(dict(self._row(), extra=True), ensure_ascii=False),
            "\ufeff" + row + "\n",
            row + "\n\n",
        ]
        for raw in cases:
            with self.subTest(raw=raw[:20]):
                with self.assertRaises(mod.TranslatorNotesError):
                    self._validate(path=self._write([], raw))

    def test_duplicate_ids_orders_and_order_sequence(self):
        duplicate_id = self._row()
        duplicate_id["order"] = 2
        with self.assertRaises(mod.TranslatorNotesError):
            self._validate([self._row(), duplicate_id])
        duplicate_order = self._row(note_id="note-2")
        with self.assertRaises(mod.TranslatorNotesError):
            self._validate([self._row(), duplicate_order])
        out_of_order = self._row(order=2)
        with self.assertRaises(mod.TranslatorNotesError):
            self._validate([out_of_order])

    def test_missing_duplicate_units_and_hash(self):
        with self.assertRaises(mod.TranslatorNotesError):
            self._validate(active=[{"unit_id": "u1", "zh": self.zh}, {"unit_id": "u1", "zh": self.zh}])
        with self.assertRaises(mod.TranslatorNotesError):
            self._validate(active=[])
        with self.assertRaises(mod.TranslatorNotesError):
            self._validate([self._row(anchor={"text": "锚点", "occurrence": 1, "unit_zh_sha256": "0" * 64})])

    def test_mixed_unit_and_navigation_aggregate(self):
        active = [{"unit_id": "u1", "zh": self.zh}, {"nav_id": "nav-1", "zh_title": "章节"}]
        self.assertEqual(self._validate(active=active)[0]["unit_id"], "u1")

    def test_active_identifier_shape_and_duplicates_are_strict(self):
        for active in (
            [{"unit_id": "u1", "nav_id": "nav-1", "zh": self.zh}],
            [{"title": "not an identified row"}],
            [{"unit_id": 1, "zh": self.zh}],
            [{"nav_id": ""}],
            [{"unit_id": "u1", "zh": self.zh}, {"unit_id": "u1", "zh": self.zh}],
            [{"nav_id": "nav-1"}, {"nav_id": "nav-1"}],
        ):
            with self.subTest(active=active):
                with self.assertRaises(mod.TranslatorNotesError):
                    self._validate(active=active)

    def test_navigation_rows_are_not_note_targets(self):
        row = self._row(unit_id="nav-1")
        with self.assertRaisesRegex(mod.TranslatorNotesError, "active unit_id is missing"):
            self._validate([row], active=[{"nav_id": "nav-1", "zh_title": "章节"}])

    def test_navigation_content_is_ignored_but_unit_zh_is_required(self):
        active = [{"nav_id": "nav-1"}, {"unit_id": "u1", "zh": self.zh}]
        self.assertEqual(self._validate(active=active)[0]["unit_id"], "u1")
        for unit in ({"unit_id": "u1"}, {"unit_id": "u1", "zh": None}):
            with self.subTest(unit=unit):
                with self.assertRaisesRegex(mod.TranslatorNotesError, "zh must be a string"):
                    self._validate(active=[unit])

    def test_occurrence_is_one_based_literal_and_overlap(self):
        second = self._row(anchor={"text": "锚点", "occurrence": 2,
                                   "unit_zh_sha256": hashlib.sha256(self.zh.encode()).hexdigest()})
        self.assertEqual(self._validate([second])[0]["anchor"]["occurrence"], 2)
        for occurrence in (0, 3):
            with self.assertRaises(mod.TranslatorNotesError):
                self._validate([self._row(anchor={"text": "锚点", "occurrence": occurrence,
                                                  "unit_zh_sha256": hashlib.sha256(self.zh.encode()).hexdigest()})])
        repeated = "aaaa"
        active = [{"unit_id": "u1", "zh": repeated}]
        row = self._row(anchor={"text": "aa", "occurrence": 2,
                                "unit_zh_sha256": hashlib.sha256(repeated.encode()).hexdigest()})
        self.assertEqual(self._validate([row], active=active)[0]["anchor"]["occurrence"], 2)

    def test_provenance_paths_hash_key_and_approval(self):
        for field, value in (("artifact", "/absolute"), ("artifact", "../x"),
                             ("sha256", "A" * 64), ("key", "missing")):
            row = self._row()
            row["provenance"]["adjudication"][field] = value
            with self.assertRaises(mod.TranslatorNotesError):
                self._validate([row])
        row = self._row()
        self.review.write_text(json.dumps({"key": "review-1", "note_id": "note-1",
                                            "unit_id": "u1", "decision": "REJECT",
                                            "subject_sha256": mod.subject_digest(row)}) + "\n",
                                  encoding="utf-8")
        row["provenance"]["review"]["sha256"] = hashlib.sha256(
            self.review.read_bytes()).hexdigest()
        with self.assertRaisesRegex(mod.TranslatorNotesError,
                                    "provenance.review decision must be APPROVE"):
            self._validate([row])

    def test_provenance_key_boundary_subject_reuse_and_artifact_shape(self):
        row = self._row()
        self.review.write_text(json.dumps({"key": "review-10", "note_id": "note-1",
                                            "unit_id": "u1", "decision": "APPROVE",
                                            "subject_sha256": mod.subject_digest(row)}) + "\n",
                                  encoding="utf-8")
        self.review.write_bytes(self.review.read_bytes())
        row["provenance"]["review"]["sha256"] = hashlib.sha256(
            self.review.read_bytes()).hexdigest()
        with self.assertRaisesRegex(mod.TranslatorNotesError,
                                    "provenance.review.key is not an exact artifact entry"):
            self._validate([row])

        changed = self._row()
        changed["note"] = "changed"
        with self.assertRaisesRegex(mod.TranslatorNotesError,
                                    "provenance.adjudication does not match note subject"):
            self._validate([changed])

        row = self._row()
        self.review.write_text("not-json\n", encoding="utf-8")
        row["provenance"]["review"]["sha256"] = hashlib.sha256(
            self.review.read_bytes()).hexdigest()
        with self.assertRaisesRegex(mod.TranslatorNotesError,
                                    "provenance.review.artifact line 1 is invalid JSON"):
            self._validate([row])

        row = self._row()
        self.review.write_text(json.dumps({"key": "review-1", "note_id": "note-1",
                                            "unit_id": "u1", "decision": "APPROVE",
                                            "subject_sha256": mod.subject_digest(row)}) + "\n" +
                                  json.dumps({"key": "review-1", "note_id": "note-1",
                                              "unit_id": "u1", "decision": "APPROVE",
                                              "subject_sha256": mod.subject_digest(row)}) + "\n",
                                  encoding="utf-8")
        row["provenance"]["review"]["sha256"] = hashlib.sha256(
            self.review.read_bytes()).hexdigest()
        with self.assertRaisesRegex(mod.TranslatorNotesError, "duplicate provenance key: review-1"):
            self._validate([row])

    def test_approval_root_and_artifact_component_symlinks(self):
        if not hasattr(Path, "symlink_to"):
            self.skipTest("symlinks unavailable")
        row = self._row()
        link_root = self.root / "root-link"
        link_root.symlink_to(self.root, target_is_directory=True)
        with self.assertRaisesRegex(mod.TranslatorNotesError, "approval root contains a symlink"):
            mod.validate_translator_notes(self._write([row]), self.active, link_root)

        nested = self.root / "nested"
        nested.mkdir()
        real = nested / "real.jsonl"
        real.write_text(self.review.read_text(encoding="utf-8"), encoding="utf-8")
        artifact_link = nested / "link.jsonl"
        artifact_link.symlink_to(real)
        row = self._row()
        row["provenance"]["review"]["artifact"] = "nested/link.jsonl"
        row["provenance"]["review"]["sha256"] = hashlib.sha256(real.read_bytes()).hexdigest()
        with self.assertRaisesRegex(mod.TranslatorNotesError, "artifact contains a symlink"):
            self._validate([row])

        component = self.root / "component"
        component.mkdir()
        (component / "alias").symlink_to(nested, target_is_directory=True)
        row = self._row()
        row["provenance"]["review"]["artifact"] = "component/alias/real.jsonl"
        row["provenance"]["review"]["sha256"] = hashlib.sha256(real.read_bytes()).hexdigest()
        with self.assertRaisesRegex(mod.TranslatorNotesError, "artifact contains a symlink"):
            self._validate([row])

    def test_plain_text_and_protected_span(self):
        for text in ("bad <b>x</b>", "bad **x**", "bad\u0001"):
            with self.assertRaises(mod.TranslatorNotesError):
                self._validate([self._row(note=text)])
        with self.assertRaisesRegex(mod.TranslatorNotesError, "anchor overlaps protected span"):
            mod.validate_translator_notes(self._write([self._row()]), self.active, self.root,
                                         {"u1": [(2, 4)]})

    def test_protected_span_mapping_and_bounds_are_strict(self):
        cases = [
            ({"u1": "not-a-list"}, "protected_spans has invalid unit or span list"),
            ({"unknown": []}, "protected_spans has invalid unit or span list"),
            ({"u1": [{"start": 0, "end": 1, "extra": 2}]}, "protected span has invalid fields"),
            ({"u1": [{"start": True, "end": 1}]}, "protected span has invalid bounds"),
            ({"u1": [{"start": 2, "end": 2}]}, "protected span has invalid bounds"),
            ({"u1": [{"start": 0, "end": len(self.zh) + 1}]}, "protected span has invalid bounds"),
        ]
        for spans, message in cases:
            with self.subTest(spans=spans):
                with self.assertRaisesRegex(mod.TranslatorNotesError, message):
                    mod.validate_translator_notes(self._write([self._row()]), self.active,
                                                  self.root, spans)

    def test_rendered_equal_and_distinct_start_ranges_overlap(self):
        for anchors in (("aaa", 1, "aa", 1), ("aaa", 1, "aa", 2)):
            first = self._row(note_id="first", order=1,
                              anchor={"text": anchors[0], "occurrence": anchors[1],
                                      "unit_zh_sha256": hashlib.sha256(b"aaaa").hexdigest()})
            second = self._row(note_id="second", order=2,
                               anchor={"text": anchors[2], "occurrence": anchors[3],
                                       "unit_zh_sha256": hashlib.sha256(b"aaaa").hexdigest()},
                               provenance={
                                   "adjudication": {"artifact": "decisions.txt", "sha256": "0" * 64,
                                                    "key": "decision-2"},
                                   "review": {"artifact": "reviews.txt", "sha256": "0" * 64,
                                              "key": "review-2"},
                               })
            rows = [first, second]
            self._refresh_provenance(rows)
            with self.subTest(anchors=anchors):
                with self.assertRaisesRegex(mod.TranslatorNotesError,
                                            "selected anchor ranges overlap"):
                    self._validate(rows, active=[{"unit_id": "u1", "zh": "aaaa"}])

    def test_cli_does_not_write_output(self):
        notes = self._write([self._row()])
        active_path = self.root / "active.jsonl"
        active_path.write_text(json.dumps(self.active[0], ensure_ascii=False) + "\n", encoding="utf-8")
        result = subprocess.run([sys.executable, str(TOOL), str(notes), str(active_path),
                                 "--workspace-root", str(self.root)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "output").exists())


if __name__ == "__main__":
    unittest.main()
