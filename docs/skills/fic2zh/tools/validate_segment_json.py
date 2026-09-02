#!/usr/bin/env python3
"""Validate one whole-segment translation JSON document.

The default format is a one-element JSON array containing an object with the
ordered keys ``id``, ``en``, ``zh``, ``summary``, ``status``, and
``uncertainties``.  The JSON document itself must have one final LF; the
canonical source text may have any LF-terminated state (including no final
LF), which is preserved byte-for-byte in ``en`` and mirrored by ``zh``.
"""

import argparse
import json
import sys

EXPECTED_KEYS = ("id", "en", "zh", "summary", "status", "uncertainties")
LEGACY_KEYS = ("id", "en", "zh", "summary")


class DuplicateKeyError(ValueError):
    """Raised when a JSON object repeats a key."""


def object_pairs(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise DuplicateKeyError(key)
        result[key] = value
    return result


def error(message):
    sys.stderr.write("ERROR: %s\n" % message)
    return 1


def read_utf8(path, label):
    try:
        with open(path, "rb") as stream:
            raw = stream.read()
    except OSError as exc:
        return None, "%s: cannot read file: %s" % (label, exc)
    try:
        text = raw.decode("utf-8", errors="strict")
    except UnicodeDecodeError as exc:
        return None, "%s: invalid UTF-8: %s" % (label, exc)
    return (raw, text), None


def has_json_framing(raw):
    """The translation container is JSONL-like: exactly one final raw LF."""
    return raw.endswith(b"\n") and not raw.endswith(b"\n\n") and b"\r" not in raw


def newline_shape(text):
    """Return blank-line runs and final-LF count without changing the text."""
    if "\r" in text:
        return None
    final_lfs = len(text) - len(text.rstrip("\n"))
    body = text[:-final_lfs] if final_lfs else text
    lines = body.split("\n")
    # Empty source is one empty line; otherwise compare each physical line's
    # blank/non-blank status, including internal blank lines.
    return ([not line for line in lines], final_lfs)


def validate_status(item, label):
    status = item.get("status")
    uncertainties = item.get("uncertainties")
    if status not in ("final", "needs_review"):
        return "%s field 'status' must be 'final' or 'needs_review'" % label
    if not isinstance(uncertainties, list) or any(
            not isinstance(value, str) for value in uncertainties):
        return "%s field 'uncertainties' must be a list of strings" % label
    if (status == "needs_review") != bool(uncertainties):
        return "%s status must be needs_review iff uncertainties is nonempty" % label
    return None


def validate(args):
    source, problem = read_utf8(args.source, "source")
    if problem:
        return error(problem)
    translation, problem = read_utf8(args.translation, "translation")
    if problem:
        return error(problem)

    source_raw, source_text = source
    translation_raw, translation_text = translation
    if b"\r" in source_raw:
        return error("source: raw CR is not allowed; use LF line endings")
    if not has_json_framing(translation_raw):
        return error("translation: JSON framing must have exactly one raw LF")

    try:
        document = json.loads(translation_text, object_pairs_hook=object_pairs)
    except DuplicateKeyError as exc:
        return error("translation: invalid JSON: duplicate object key %r" % exc.args[0])
    except json.JSONDecodeError as exc:
        return error("translation: invalid JSON: %s" % exc)

    if not isinstance(document, list):
        return error("translation: top-level must be an array")
    if len(document) != 1:
        return error("translation: array must contain exactly one object (found %d)" % len(document))
    item = document[0]
    if not isinstance(item, dict):
        return error("translation: array element 1 must be an object")
    expected_keys = LEGACY_KEYS if args.schema == "legacy" else EXPECTED_KEYS
    if tuple(item.keys()) != expected_keys:
        return error("translation: schema keys must be exactly %s in that order" %
                     ",".join(expected_keys))

    for key in expected_keys:
        if not isinstance(item[key], str) and key not in ("uncertainties",):
            return error("translation: field '%s' must be a string" % key)
    if args.schema != "legacy":
        problem = validate_status(item, "translation")
        if problem:
            return error(problem)
    if args.expected_id is not None and item["id"] != args.expected_id:
        return error("translation: field 'id' does not match expected ID %r" % args.expected_id)
    if item["en"] != source_text:
        return error("translation: field 'en' does not exactly match source")
    if not item["zh"].strip():
        return error("translation: field 'zh' must be a nonempty string")
    source_shape = newline_shape(source_text)
    translation_shape = newline_shape(item["zh"])
    if source_shape is None or translation_shape is None:
        return error("source and translation text must use LF line endings")
    if source_shape != translation_shape:
        return error("translation: en/zh paragraph, blank-line, and trailing-newline shape differs")

    sys.stdout.write("PASS: segment JSON validated\n")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, help="source TXT path")
    parser.add_argument("--translation", required=True, help="translation JSON path")
    parser.add_argument("--expected-id", help="require this exact segment ID")
    parser.add_argument("--schema", choices=("canonical", "legacy"), default="canonical",
                        help="translation schema (legacy is explicit compatibility mode)")
    args = parser.parse_args(argv)
    return validate(args)


if __name__ == "__main__":
    sys.exit(main())
