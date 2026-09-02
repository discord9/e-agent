#!/usr/bin/env python3
"""Strict loader for approved translator_notes.jsonl v1.

The two provenance files are JSONL, not prose.  Each line is exactly::

  {"key":"...","note_id":"...","unit_id":"...",\
"decision":"CONFIRMED|APPROVE","subject_sha256":"..."}

An adjudication uses ``CONFIRMED`` and a review uses ``APPROVE``.  The subject
is the note object without ``provenance``, serialized as UTF-8 JSON with sorted
keys, no insignificant whitespace (``sort_keys=True, separators=(',', ':')``),
and ``ensure_ascii=False``.  Its SHA-256 is the ``subject_sha256`` value.  This
keeps provenance from participating in its own digest.
"""
import argparse
import hashlib
import json
import re
import sys
import unicodedata
from pathlib import Path, PurePosixPath

NOTE_KEYS = {"schema_version", "kind", "note_id", "unit_id", "order", "anchor",
             "note", "factual_scope", "render", "provenance"}
ANCHOR_KEYS = {"text", "occurrence", "unit_zh_sha256"}
PROVENANCE_KEYS = {"adjudication", "review"}
RECORD_KEYS = {"artifact", "sha256", "key"}
ARTIFACT_KEYS = {"key", "note_id", "unit_id", "decision", "subject_sha256"}
NOTE_ID_RE = re.compile(r"^[A-Za-z][A-Za-z0-9._-]{0,63}$")
SHA_RE = re.compile(r"^[0-9a-f]{64}$")
MARKUP_RE = re.compile(
    r"<[^>]*>|```?|\*\*|__|!?\[[^]]*\]\([^)]*\)|"
    r"(?<!\w)[*_][^\n]+[*_](?!\w)|"
    r"^\s{0,3}(?:#{1,6}|[-*>])\s",
    flags=re.MULTILINE,
)


class TranslatorNotesError(ValueError):
    """A fail-closed input, provenance, or anchor error."""


def _pairs(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise TranslatorNotesError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def _constant(value):
    raise TranslatorNotesError(f"non-standard JSON constant: {value}")


def _read_jsonl(path, label):
    try:
        raw = Path(path).read_bytes()
    except OSError as exc:
        raise TranslatorNotesError(f"cannot read {label}: {exc}") from exc
    if raw.startswith(b"\xef\xbb\xbf"):
        raise TranslatorNotesError(f"{label} has a UTF-8 BOM")
    if b"\r" in raw:
        raise TranslatorNotesError(f"{label} must use LF newlines")
    if raw and not raw.endswith(b"\n"):
        raise TranslatorNotesError(f"{label} must end with LF")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise TranslatorNotesError(f"{label} is not valid UTF-8") from exc
    rows = []
    lines = text.split("\n")
    for line_no, line in enumerate(lines, 1):
        if not line:
            if line_no == len(lines) and text.endswith("\n"):
                continue
            raise TranslatorNotesError(f"{label} line {line_no} is blank")
        if not line.strip():
            raise TranslatorNotesError(f"{label} line {line_no} is blank")
        try:
            row = json.loads(line, object_pairs_hook=_pairs, parse_constant=_constant)
        except (json.JSONDecodeError, TranslatorNotesError) as exc:
            raise TranslatorNotesError(f"{label} line {line_no} is invalid JSON: {exc}") from exc
        if not isinstance(row, dict):
            raise TranslatorNotesError(f"{label} line {line_no} is not an object")
        rows.append(row)
    return rows


def _plain(value, field):
    if not isinstance(value, str) or not value:
        raise TranslatorNotesError(f"{field} must be non-empty text")
    if any(unicodedata.category(ch).startswith("C") for ch in value):
        raise TranslatorNotesError(f"{field} contains control characters")
    if MARKUP_RE.search(value):
        raise TranslatorNotesError(f"{field} contains markup")
    return value


def _sha(value):
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def subject_digest(row):
    """Digest all canonical fields that affect note eligibility or rendering."""
    subject = {key: row[key] for key in NOTE_KEYS - {"provenance"}}
    encoded = json.dumps(subject, ensure_ascii=False, sort_keys=True,
                         separators=(",", ":")).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def locate_literal_occurrence(text, needle, occurrence):
    """Return the half-open range of a one-based, non-overlapping occurrence."""
    if not isinstance(occurrence, int) or isinstance(occurrence, bool) or occurrence < 1:
        return None
    start = 0
    for _ in range(occurrence):
        start = text.find(needle, start)
        if start < 0:
            return None
        end = start + len(needle)
        start = end
    return (end - len(needle), end)


def _checked_artifact_path(root, artifact, label):
    """Resolve an artifact only through non-symlink path components.

    lstat-before-open is intentionally a small, practical policy: it rejects
    stable symlinks and documents the unavoidable local race between checks and
    reads (callers needing hostile concurrent writers need descriptor-relative
    no-follow I/O).
    """
    if not isinstance(artifact, str) or not artifact or "\\" in artifact:
        raise TranslatorNotesError(f"provenance.{label}.artifact must be relative POSIX")
    pure = PurePosixPath(artifact)
    if (pure.is_absolute() or not pure.parts or ".." in pure.parts
            or "." in pure.parts):
        raise TranslatorNotesError(f"provenance.{label}.artifact must be relative POSIX")
    root = Path(root)
    try:
        absolute_root = root.absolute()
        current = Path(absolute_root.anchor)
        for part in absolute_root.parts[1:]:
            current /= part
            if current.is_symlink():
                raise TranslatorNotesError("approval root contains a symlink")
            current.lstat()
        if not absolute_root.is_dir():
            raise TranslatorNotesError("approval root must be a real directory")
        current = absolute_root
        for part in pure.parts:
            current /= part
            if current.is_symlink():
                raise TranslatorNotesError(
                    f"provenance.{label}.artifact contains a symlink: {artifact}")
            current.lstat()  # reject missing components before opening/hash reading
    except OSError as exc:
        raise TranslatorNotesError(f"provenance.{label}.artifact is missing: {artifact}") from exc
    return current


def _artifact(root, record, label, expected, note_id, unit_id, digest):
    """Require the note-specific machine adjudication/review JSONL contract."""
    if not isinstance(record, dict) or set(record) != RECORD_KEYS:
        raise TranslatorNotesError(f"provenance.{label} has invalid fields")
    artifact, file_digest, key = record["artifact"], record["sha256"], record["key"]
    if not isinstance(file_digest, str) or not SHA_RE.fullmatch(file_digest):
        raise TranslatorNotesError(f"provenance.{label}.sha256 is invalid")
    if not isinstance(key, str) or not key:
        raise TranslatorNotesError(f"provenance.{label}.key must be non-empty")
    path = _checked_artifact_path(root, artifact, label)
    try:
        data = path.read_bytes()
    except OSError as exc:
        raise TranslatorNotesError(f"cannot read provenance.{label}: {exc}") from exc
    if hashlib.sha256(data).hexdigest() != file_digest:
        raise TranslatorNotesError(f"provenance.{label}.artifact SHA-256 mismatch")
    try:
        entries = _read_jsonl(path, f"provenance.{label}.artifact")
    except TranslatorNotesError:
        raise
    seen_keys, matches = set(), []
    for line_no, entry in enumerate(entries, 1):
        if set(entry) != ARTIFACT_KEYS:
            raise TranslatorNotesError(f"provenance.{label} line {line_no} has invalid fields")
        if not all(isinstance(entry[field], str) and entry[field]
                   for field in ("key", "note_id", "unit_id", "decision")):
            raise TranslatorNotesError(f"provenance.{label} line {line_no} has invalid identity")
        if not SHA_RE.fullmatch(entry["subject_sha256"]):
            raise TranslatorNotesError(f"provenance.{label} line {line_no} has invalid subject SHA")
        if entry["key"] in seen_keys:
            raise TranslatorNotesError(f"duplicate provenance key: {entry['key']}")
        seen_keys.add(entry["key"])
        if entry["key"] == key:
            matches.append(entry)
    if len(matches) != 1:
        raise TranslatorNotesError(f"provenance.{label}.key is not an exact artifact entry")
    entry = matches[0]
    if (entry["note_id"], entry["unit_id"], entry["subject_sha256"]) != (note_id, unit_id, digest):
        raise TranslatorNotesError(f"provenance.{label} does not match note subject")
    if entry["decision"] != expected:
        raise TranslatorNotesError(
            f"provenance.{label} decision must be {expected}")


def _validate_protected_spans(protected_spans, units):
    if protected_spans is None:
        return
    if not isinstance(protected_spans, dict):
        raise TranslatorNotesError("protected_spans must map unit IDs to span lists")
    for uid, spans in protected_spans.items():
        if not isinstance(uid, str) or uid not in units or not isinstance(spans, (list, tuple)):
            raise TranslatorNotesError("protected_spans has invalid unit or span list")
        for span in spans:
            if isinstance(span, dict):
                if set(span) != {"start", "end"}:
                    raise TranslatorNotesError("protected span has invalid fields")
                lo, hi = span["start"], span["end"]
            elif isinstance(span, (list, tuple)) and len(span) == 2:
                lo, hi = span
            else:
                raise TranslatorNotesError("protected span has invalid shape")
            if (not isinstance(lo, int) or isinstance(lo, bool)
                    or not isinstance(hi, int) or isinstance(hi, bool)
                    or lo < 0 or hi > len(units[uid]) or lo >= hi):
                raise TranslatorNotesError("protected span has invalid bounds")


def validate_translator_notes(notes_path, translations, workspace_root, protected_spans=None):
    """Load and validate v1 rows, returning ordinary dictionaries.

    ``translations`` is an active translation JSONL path or dictionaries whose
    rows have exactly one non-empty string identifier, ``unit_id`` or
    ``nav_id``.  Only unit rows are targets and must have string ``zh`` fields;
    navigation-row translation content is ignored.  ``protected_spans`` is a
    strict library API for parser-preserving adapters; structured JSON v1 has
    no span field and its renderer rejects detectable source markup instead.
    """
    rows = _read_jsonl(notes_path, "translator notes")
    active = (_read_jsonl(translations, "active translations")
              if isinstance(translations, (str, Path)) else list(translations))
    units = {}
    seen_identifiers = set()
    for i, row in enumerate(active, 1):
        if not isinstance(row, dict):
            raise TranslatorNotesError(f"active translation row {i} is not an object")
        identifier_fields = [field for field in ("unit_id", "nav_id") if field in row]
        if len(identifier_fields) != 1:
            raise TranslatorNotesError(
                f"active translation row {i} must have exactly one of unit_id or nav_id")
        identifier_type = identifier_fields[0]
        identifier = row[identifier_type]
        if not isinstance(identifier, str) or not identifier:
            raise TranslatorNotesError(
                f"active translation row {i} has invalid {identifier_type}")
        key = (identifier_type, identifier)
        if key in seen_identifiers:
            raise TranslatorNotesError(
                f"duplicate active translation {identifier_type}: {identifier}")
        seen_identifiers.add(key)
        if identifier_type == "nav_id":
            continue
        if not isinstance(row.get("zh"), str):
            raise TranslatorNotesError(f"active translation row {i} zh must be a string")
        units[identifier] = row["zh"]
    _validate_protected_spans(protected_spans, units)

    seen_ids, seen_orders, ranges_by_unit = set(), set(), {}
    result = []
    for line_no, row in enumerate(rows, 1):
        if set(row) != NOTE_KEYS:
            unknown = sorted(set(row) - NOTE_KEYS)
            missing = sorted(NOTE_KEYS - set(row))
            detail = f"unknown field {unknown[0]}" if unknown else f"missing field {missing[0]}"
            raise TranslatorNotesError(f"line {line_no}: {detail}")
        if row["schema_version"] != 1 or isinstance(row["schema_version"], bool):
            raise TranslatorNotesError(f"line {line_no}: schema_version must be 1")
        if row["kind"] != "translator_note":
            raise TranslatorNotesError(f"line {line_no}: kind must be translator_note")
        note_id = row["note_id"]
        if not isinstance(note_id, str) or not NOTE_ID_RE.fullmatch(note_id):
            raise TranslatorNotesError(f"line {line_no}: invalid note_id")
        if note_id in seen_ids:
            raise TranslatorNotesError(f"duplicate note_id: {note_id}")
        seen_ids.add(note_id)
        order = row["order"]
        if not isinstance(order, int) or isinstance(order, bool) or order < 1:
            raise TranslatorNotesError(f"line {line_no}: order must be positive integer")
        if order in seen_orders or order != line_no:
            raise TranslatorNotesError(f"line {line_no}: order must be contiguous and match file order")
        seen_orders.add(order)
        uid = row["unit_id"]
        if not isinstance(uid, str) or not uid or uid not in units:
            raise TranslatorNotesError(f"line {line_no}: active unit_id is missing")
        if not isinstance(row["render"], bool):
            raise TranslatorNotesError(f"line {line_no}: render must be boolean")
        anchor = row["anchor"]
        if not isinstance(anchor, dict) or set(anchor) != ANCHOR_KEYS:
            raise TranslatorNotesError(f"line {line_no}: invalid anchor fields")
        text, occ, digest = anchor["text"], anchor["occurrence"], anchor["unit_zh_sha256"]
        if not isinstance(text, str) or not text:
            raise TranslatorNotesError(f"line {line_no}: anchor.text must be non-empty")
        if not isinstance(occ, int) or isinstance(occ, bool) or occ < 1:
            raise TranslatorNotesError(f"line {line_no}: occurrence must be positive integer")
        if not isinstance(digest, str) or not SHA_RE.fullmatch(digest):
            raise TranslatorNotesError(f"line {line_no}: invalid unit_zh_sha256")
        zh = units[uid]
        if _sha(zh) != digest:
            raise TranslatorNotesError(f"line {line_no}: stale unit zh SHA")
        hit = locate_literal_occurrence(zh, text, occ)
        if hit is None:
            raise TranslatorNotesError(f"line {line_no}: anchor occurrence is out of range")
        spans = protected_spans.get(uid, ()) if protected_spans is not None else ()
        if any(hit[0] < hi and lo < hit[1] for lo, hi in (
                (s.get("start"), s.get("end")) if isinstance(s, dict) else tuple(s)
                for s in spans)):
            raise TranslatorNotesError(f"line {line_no}: anchor overlaps protected span")
        if row["render"]:
            prior = ranges_by_unit.setdefault(uid, [])
            if any(hit[0] < hi and lo < hit[1] for lo, hi in prior):
                raise TranslatorNotesError(f"line {line_no}: selected anchor ranges overlap")
            prior.append(hit)
        _plain(row["note"], f"line {line_no} note")
        _plain(row["factual_scope"], f"line {line_no} factual_scope")
        provenance = row["provenance"]
        if not isinstance(provenance, dict) or set(provenance) != PROVENANCE_KEYS:
            raise TranslatorNotesError(f"line {line_no}: invalid provenance fields")
        digest_subject = subject_digest(row)
        _artifact(workspace_root, provenance["adjudication"], "adjudication",
                  "CONFIRMED", note_id, uid, digest_subject)
        _artifact(workspace_root, provenance["review"], "review",
                  "APPROVE", note_id, uid, digest_subject)
        result.append(row)
    return result


load_translator_notes = validate_translator_notes
ValidationError = TranslatorNotesError


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("notes", nargs="?", help="translator_notes.jsonl")
    parser.add_argument("translations_pos", nargs="?", help="active translations JSONL")
    parser.add_argument("--notes", dest="notes_opt")
    parser.add_argument("--translations", dest="translations_opt")
    parser.add_argument("--workspace-root", required=True)
    args = parser.parse_args()
    notes = args.notes_opt or args.notes
    translations = args.translations_opt or args.translations_pos
    if not notes or not translations:
        parser.error("notes and active translations JSONL are required")
    try:
        loaded = validate_translator_notes(notes, translations, args.workspace_root)
    except (TranslatorNotesError, OSError) as exc:
        print(f"invalid translator notes: {exc}", file=sys.stderr)
        return 1
    print(f"PASS: validated {len(loaded)} translator note(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
