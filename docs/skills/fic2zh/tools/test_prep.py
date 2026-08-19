#!/usr/bin/env python3
"""Fixture-only tests for prep.py; run directly with python3."""
import importlib.util
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location('prep', HERE / 'prep.py')
prep = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(prep)


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def fixture():
    return '''<html><body>
<article class="message" id="js-post-42"><div class="bbWrapper">
<div class="poll"><p>Opening vote list</p><p>[] option one</p><p>[] option two</p><p>[] option three</p></div>
<p>START: Yeah, we're getting the hell out of dodge," you say to your crew.</p>
<div class="bbCodeSpoiler"><p>Inside spoiler, the crew hears the order.</p><div><p>The ship turns toward the jump point.</p></div></div>
<p>It's a hell of a way to run a war. You're already a fan.</p>
<p>END: Winning Vote: option one</p><div class="status"><p>Remaining Rolls: 2</p><p>Current Capabilities: ships</p></div>
<script>not visible</script><style>.hidden { display:none }</style>
</div></article></body></html>'''


def test_extraction_and_boundaries():
    html = fixture()
    fragment = prep.post_fragment(html, '42')
    body = prep.bbwrapper_html(fragment)
    text = prep.clean(prep.extract_text(body))
    check('Inside spoiler' in text, 'spoiler narrative was dropped')
    check('not visible' not in text and 'hidden' not in text, 'script/style leaked')
    narr = prep.trim_narrative(text, 'START:', 'END:')
    check(narr.startswith('START:'), 'start marker paragraph was not retained')
    check('Opening vote list' not in narr, 'vote block was not excluded')
    check('Winning Vote:' not in narr, 'end marker line was retained')
    check('Remaining Rolls:' not in narr, 'status block was retained')
    check('Inside spoiler' in narr and narr.endswith("You're already a fan."),
          'bounded narrative content is wrong')


def test_old_api_and_segments():
    text = 'First paragraph.\n\nSecond paragraph.'
    check(prep.trim_narrative(text) == text, 'unmarked old trim changed ordinary text')
    parts = prep.segment(text, lo=1, hi=10, target=3)
    check('\n\n'.join(parts) == text, 'segments do not join to source')
    check(all(part for part in parts), 'empty segment emitted')


def test_marker_regressions():
    text = ('Winning Vote: old preamble; START narrative begins here.\n\n'
            'Spoiler: a visible line\n\nStory after start\n\n'
            'END boundary after')
    start_only = prep.trim_narrative(text, 'START narrative')
    check(start_only.startswith('Winning Vote: old preamble; START narrative'),
          'same-paragraph start-only lost the start narrative')
    check('START narrative begins here.' in start_only and start_only.strip(),
          'same-paragraph start-only was empty')
    bounded = prep.trim_narrative(text, 'START narrative', 'END boundary')
    check('Story after start' in bounded, 'end marker search used pre-start occurrence')
    check('Spoiler: a visible line' in bounded, 'generic Spoiler text was treated as a cut')
    check('END boundary after' not in bounded, 'post-start end marker was retained')
    check_raises(lambda: prep.trim_narrative('END boundary before\n\nSTART only',
                                              'START', 'END boundary'),
                 'end marker not found after start marker: END boundary')
    check_raises(lambda: prep.trim_narrative('ordinary', 'MISSING'),
                 'start marker not found')


def check_raises(call, expected):
    try:
        call()
    except ValueError as exc:
        check(expected in str(exc), 'wrong marker error: %s' % exc)
    else:
        raise AssertionError('expected ValueError containing %r' % expected)


def test_cli_validation_and_legacy():
    original = prep.process
    calls = []

    def fake_process(*args, **kwargs):
        calls.append((args, kwargs))
        return 0, []

    prep.process = fake_process
    try:
        check(prep.main([]) == 0, 'empty argv did not run legacy jobs')
        check(len(calls) == 3, 'legacy jobs count changed')
        calls.clear()
        try:
            prep.main(['--start-marker', 'x'])
        except SystemExit as exc:
            check(exc.code != 0, 'incomplete CLI unexpectedly succeeded')
        else:
            raise AssertionError('incomplete CLI did not fail')
        check(not calls, 'incomplete CLI invoked legacy process')
        for target, expected in ((2, 'target words must be >= minimum'),
                                 (6, 'target words must be <= maximum')):
            try:
                prep.main(['--page-file', 'unused', '--post-id', '42', '--turn', '1',
                           '--out-prefix', 'unused', '--segment-min-words', '3',
                           '--segment-max-words', '5', '--segment-target-words', str(target)])
            except SystemExit as exc:
                check(exc.code != 0, 'invalid CLI target unexpectedly succeeded')
            else:
                raise AssertionError('invalid CLI target did not fail')
            check(expected in calls[-1][0] if calls else True,
                  'invalid CLI target unexpectedly called process')
        check(len(calls) == 0, 'invalid CLI target invoked process')
    finally:
        prep.process = original

    check_raises(lambda: prep.segment('x', lo=0), 'minimum words must be > 0')
    check_raises(lambda: prep.segment('x', lo=3, hi=2), 'maximum words must be >=')
    check_raises(lambda: prep.segment('x', target=0), 'target words must be > 0')
    check_raises(lambda: prep.segment('x', lo=3, hi=5, target=2),
                 'target words must be >= minimum')
    check_raises(lambda: prep.segment('x', lo=3, hi=5, target=6),
                 'target words must be <= maximum')


def test_cli():
    directory = HERE / '.test_prep_output'
    if directory.exists():
        shutil.rmtree(directory)
    directory.mkdir()
    try:
        page = directory / 'page.html'
        prefix = directory / 'turn50.3'
        page.write_text(fixture(), encoding='utf-8')
        command = [
            sys.executable, str(HERE / 'prep.py'), '--page-file', str(page),
            '--post-id', '42', '--turn', '50.3', '--out-prefix', str(prefix),
            '--start-marker', 'START:', '--end-marker', 'END:',
            '--segment-min-words', '1', '--segment-max-words', '15',
            '--segment-target-words', '8',
        ]
        result = subprocess.run(command, capture_output=True, text=True)
        check(result.returncode == 0, result.stderr)
        en = (directory / 'turn50.3_en.txt').read_text(encoding='utf-8')
        segments = sorted(directory.glob('turn50.3_seg*.txt'))
        joined = '\n\n'.join(path.read_text(encoding='utf-8').rstrip('\n')
                                 for path in segments)
        check(en.endswith('\n') and en[:-1] == joined,
              'CLI EN and segment files are not join-closed')
        check('Inside spoiler' in en and 'Current Capabilities' not in en,
              'CLI output boundaries are wrong')
    finally:
        shutil.rmtree(directory)


if __name__ == '__main__':
    test_extraction_and_boundaries()
    test_old_api_and_segments()
    test_marker_regressions()
    test_cli_validation_and_legacy()
    test_cli()
    print('PASS: prep fixtures, compatibility, segmentation, and CLI')
