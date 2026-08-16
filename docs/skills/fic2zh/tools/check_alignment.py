#!/usr/bin/env python3
"""check_alignment.py -- detect en/zh paragraph misalignment in chapter-level JSON.

Vox Vitae 翻译的 19 个 seg 曾因段落粒度不一致（翻译模型合并/拆分段落）导致
en/zh 位置错位，已用语义配对修复（配对内嵌在章级 JSON 的 segments[i].pairs）。
本工具自动扫描章级 JSON，检测这类错位，防止未来新增章节再犯。

用法:
    python3 check_alignment.py [--json-dir DIR] [--threshold N] [--verbose]

默认 --json-dir 为脚本所在目录的 vox_vitae_zh/json；也支持传 colossus_zh/chapters。

检测规则:
  * seg 已有内嵌 pairs（已修复）: 校验覆盖完整 —— 每个 en/zh 索引恰好出现一次、
    顺序递增、无越界。不合法 => ERROR ALIGN_BROKEN。
  * 无 pairs 且 en/zh 段落数不一致（差 >= 1）: ERROR MISALIGNED（粒度不一致，
    需人工配对）。
  * 无 pairs 且段落数一致: 内容信号检测。若 zh[i] 与 en[i-1] 或 en[i+1] 的共享
    强信号 token 明显多于与 en[i] 的共享数 => WARN SUSPECT（可能错位）。
    "明显多于" = 相邻共享数 >= 自身共享数 * --threshold（默认 2.0），且相邻
    共享数至少 2（绝对下限，避免人名跨段重复造成的误报）。

报告分级:
  ERROR（必须处理）: MISALIGNED、ALIGN_BROKEN
  WARN（可能错位）: SUSPECT
  全部通过时打印: OK: N segs checked, no misalignment

退出码: 有 ERROR 返回 1，否则 0（WARN 不改变退出码）。

纯 Python 标准库，无第三方依赖。
"""

import argparse
import json
import os
import re
import sys

# ---------------------------------------------------------------------------
# 强信号 token 提取 —— 参考语义配对修复的 toks/STOP/TOK（出处：
#   Vox Vitae 修复脚本 .svtmp/fixalign/align.py 的 toks(s)/STOP/TOK，
#   思路：数字、大写缩写、专名是强信号；功能词是噪声，过滤掉）。
# 原参考文件在部署环境不可用，以下按同样语义实现。
# ---------------------------------------------------------------------------

# 停用词：句首大写也会出现的功能词/代词/数词一律过滤，避免噪声。
STOP = frozenset("""
a an and are as at be but by for from have if in is it its of on or that the to
was were will with you your yours our ours we they them their theirs this that
these those there here not no nor yes so as then than when where what which who
whom whose why how all any both each few more most other some such only own same
too very just can could should would shall may might must do does did done has
had having being been am are is was were me my mine us him his her hers its
it's they're we're you're i've you've we've they've don't can't won't isn't
aren't wasn't weren't didn't doesn't hasn't haven't hadn't i'll you'll he'll
she'll we'll they'll i'd you'd he'd she'd we'd they'd into onto upon about
against between through during before after above below under over again
further once how why out up down off in on at to from of and or one two three
four five six seven eight nine ten hundred thousand million
""".split())

# TOK: 强信号 token 正则 —— 大写开头的单词（专名/缩写/句首大写，统一转小写）
# 以及数字（含千分位/小数点）。不做整词边界断言，"Anexa's" 只会取到 "Anexa"。
TOK_RE = re.compile(r"[A-Z][A-Za-z]+|\d+(?:[.,]\d+)*")


def toks(s):
    """提取一段文字的强信号 token 集合（小写化；数字去掉千分位逗号）。

    数字归一化：英文 "1,500" 与中文 "1500" 视为同一 token；小数分隔符原样保留。
    返回值是 set[str]。
    """
    out = set()
    for m in TOK_RE.finditer(s):
        t = m.group(0)
        if t[0].isdigit():
            out.add(t.replace(",", ""))
        else:
            low = t.lower()
            if len(low) >= 2 and low not in STOP:
                out.add(low)
    return out


def split_paras(text):
    """按空行切分段落（章节 JSON 中段落以 \\n\\n 分隔）。"""
    return text.split("\n\n")


# ---------------------------------------------------------------------------
# 检测逻辑
# ---------------------------------------------------------------------------

def check_pairs(seg):
    """校验内嵌 pairs 覆盖完整。

    返回错误描述列表；空列表表示通过。覆盖完整 = 展平后的 en/zh 索引序列恰好
    等于 range(n_en) / range(n_zh)（隐含：每个索引恰好一次、顺序递增、无越界，
    且每个 pair 内索引非空、按序）。
    """
    en = split_paras(seg["en"])
    zh = split_paras(seg["zh"])
    n_en, n_zh = len(en), len(zh)
    pairs = seg["pairs"]

    if not isinstance(pairs, list) or not pairs:
        return ["pairs 为空或不是列表"]

    flat_en = []
    flat_zh = []
    for k, pair in enumerate(pairs):
        if not isinstance(pair, dict) or "en" not in pair or "zh" not in pair:
            return ["pair %d 缺少 'en'/'zh' 键" % k]
        for key, flat in (("en", flat_en), ("zh", flat_zh)):
            idxs = pair[key]
            if not isinstance(idxs, list) or not idxs:
                return ["pair %d 的 %s 为空或不是列表" % (k, key)]
            for x in idxs:
                if isinstance(x, bool) or not isinstance(x, int):
                    return ["pair %d 的 %s 索引 %r 不是整数" % (k, key, x)]
                flat.append(x)

    errors = []
    if flat_en != list(range(n_en)):
        errors.append(
            "en 索引未完整覆盖 [0..%d)（重复/缺失/乱序/越界）" % n_en
        )
    if flat_zh != list(range(n_zh)):
        errors.append(
            "zh 索引未完整覆盖 [0..%d)（重复/缺失/乱序/越界）" % n_zh
        )
    return errors


def check_suspect(seg, threshold):
    """无 pairs 且段落数一致时的内容信号检测。

    对每段 i：若 zh[i] 与 en[i-1] 或 en[i+1] 的共享强信号 token 数 best 明显
    多于与 en[i] 的共享数（best >= max(2, self * threshold)，其中 self 为与
    en[i] 的共享数；self == 0 时 best >= 2 即报），判为 SUSPECT。

    返回发现列表 [(para_index, self_share, prev_share, next_share), ...]。
    """
    en = split_paras(seg["en"])
    zh = split_paras(seg["zh"])
    if len(en) != len(zh):
        return None  # 段落数不一致，由调用方按 MISALIGNED 处理

    en_toks = [toks(p) for p in en]
    zh_toks = [toks(p) for p in zh]
    n = len(en)
    findings = []

    for i in range(n):
        self_share = len(zh_toks[i] & en_toks[i])
        prev_share = len(zh_toks[i] & en_toks[i - 1]) if i > 0 else 0
        next_share = len(zh_toks[i] & en_toks[i + 1]) if i < n - 1 else 0
        best = max(prev_share, next_share)
        # 绝对下限 2：单个专名跨相邻段重复出现（如人名贯穿对话）不算错位证据。
        if best < 2:
            continue
        if self_share == 0 or best >= self_share * threshold:
            findings.append((i, self_share, prev_share, next_share))

    return findings


def resolve_json_dir(arg, script_dir):
    """解析 --json-dir。

    接受：章级 JSON 目录，或单个章级 JSON 文件（测试时指向单个文件更方便）。
    相对路径优先按当前目录解析，失败再按脚本所在目录解析。
    """
    if arg is None:
        return os.path.join(script_dir, "vox_vitae_zh", "json")
    if os.path.isdir(arg):
        return arg
    if os.path.isfile(arg) and arg.endswith(".json"):
        return arg
    alt = os.path.join(script_dir, arg)
    if os.path.isdir(alt):
        return alt
    if os.path.isfile(alt) and alt.endswith(".json"):
        return alt
    raise SystemExit("错误: 找不到 JSON 目录或文件: %s" % arg)


def collect_json_files(json_dir):
    """展开 --json-dir 为 JSON 文件列表（目录则取其中所有 *.json）。"""
    if os.path.isfile(json_dir):
        return [json_dir]
    return [
        os.path.join(json_dir, name)
        for name in sorted(os.listdir(json_dir))
        if name.endswith(".json")
    ]


def load_segments(json_dir):
    """读取章级 JSON 的 segments，返回 [(file, seg), ...]。"""
    out = []
    for path in collect_json_files(json_dir):
        try:
            data = json.load(open(path, encoding="utf-8"))
        except (OSError, ValueError) as e:
            out.append((path, {"_parse_error": str(e)}))
            continue
        if not isinstance(data, dict) or "segments" not in data:
            out.append((path, {"_parse_error": "缺少 segments 键"}))
            continue
        for seg in data["segments"]:
            out.append((path, seg))
    return out


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="扫描章级 JSON，检测 en/zh 段落错位。"
    )
    parser.add_argument(
        "--json-dir",
        default=None,
        help="章级 JSON 目录（默认: 脚本所在目录的 vox_vitae_zh/json；"
             "也支持 colossus_zh/chapters）",
    )
    parser.add_argument(
        "--threshold",
        type=float,
        default=2.0,
        help="SUSPECT 信号强度：相邻段共享 token 数须 >= 自身共享数 * N "
             "（默认 2.0，且相邻共享数至少 2）",
    )
    parser.add_argument(
        "--verbose", action="store_true", help="打印每个 seg 的段落数对比"
    )
    args = parser.parse_args(argv)

    script_dir = os.path.dirname(os.path.abspath(__file__))
    json_dir = resolve_json_dir(args.json_dir, script_dir)

    if args.threshold <= 1:
        parser.error("--threshold 必须 > 1")

    entries = load_segments(json_dir)
    if not entries:
        print("错误: 没有找到任何 JSON 内容: %s" % json_dir)
        return 2

    errors = []
    warnings = []
    n_segs = 0
    n_pairs = 0

    for path, seg in entries:
        if "_parse_error" in seg:
            errors.append((path, "(文件级)", "BAD_JSON", seg["_parse_error"]))
            continue
        if "id" not in seg:
            seg_id = "(no id)"
        else:
            seg_id = seg["id"]
        if "en" not in seg or "zh" not in seg:
            errors.append((path, seg_id, "BAD_SEG", "缺少 en/zh 字段"))
            continue

        n_segs += 1
        en_paras = split_paras(seg["en"])
        zh_paras = split_paras(seg["zh"])
        if args.verbose:
            print(
                "  %s: en=%d zh=%d" % (seg_id, len(en_paras), len(zh_paras))
            )

        if "pairs" in seg:
            n_pairs += 1
            problems = check_pairs(seg)
            if problems:
                for p in problems:
                    errors.append((path, seg_id, "ALIGN_BROKEN", p))
            continue

        # 无 pairs
        if len(en_paras) != len(zh_paras):
            errors.append(
                (
                    path,
                    seg_id,
                    "MISALIGNED",
                    "段落数不一致 en=%d zh=%d，无 pairs，需人工配对"
                    % (len(en_paras), len(zh_paras)),
                )
            )
            continue

        findings = check_suspect(seg, args.threshold)
        if findings:
            for i, self_share, prev_share, next_share in findings:
                warnings.append(
                    (
                        path,
                        seg_id,
                        "SUSPECT",
                        "段落 %d: zh 与 en[%d](%d)/en[%d](%d) 共享 token 多于 "
                        "en[%d](%d)，可能错位"
                        % (
                            i,
                            i - 1 if i > 0 else -1,
                            prev_share,
                            i + 1 if i < len(en_paras) - 1 else -1,
                            next_share,
                            i,
                            self_share,
                        ),
                    )
                )

    # 输出分级报告
    for path, seg_id, kind, detail in errors:
        print("ERROR: %s [%s] %s: %s" % (os.path.basename(path), seg_id, kind, detail))
    for path, seg_id, kind, detail in warnings:
        print("WARN:  %s [%s] %s: %s" % (os.path.basename(path), seg_id, kind, detail))

    if args.verbose:
        print("checked %d segs (%d with embedded pairs)" % (n_segs, n_pairs))

    if not errors and not warnings:
        print("OK: %d segs checked, no misalignment" % n_segs)
        return 0
    if errors:
        print("FAIL: %d seg(s) with ERROR, %d seg(s) with WARN" % (len(errors), len(warnings)))
        return 1
    # 只有 WARN：退出码仍为 0
    print("OK (with warnings): %d segs checked, %d SUSPECT warning(s)" % (n_segs, len(warnings)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
