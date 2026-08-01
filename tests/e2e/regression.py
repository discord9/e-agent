#!/usr/bin/env python3
"""e-agent Web 端回归测试套件 —— 主入口。

用法（在 tests/e2e/ 目录下，需 uv + playwright + chromium headless shell）：

    uv run --with playwright python regression.py --list          # 列出用例
    uv run --with playwright python regression.py --all           # 跑全部就绪用例
    uv run --with playwright python regression.py --case sidebar  # 按名称子串跑
    uv run --with playwright python regression.py --all --real    # 含真实 server 冒烟

核心用例全部用 page.route 拦截 mock，不依赖真实 server；每个用例独立浏览器，
单用例 30s 超时（--timeout 可调），失败打印关键 DOM 快照。退出码 0=全 PASS。
"""
import argparse
import asyncio
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import common
from cases import all_cases


def list_cases(cases):
    print("可用用例（%d 个）：" % len(cases))
    for c in cases:
        tag = ""
        if c.get("todo"):
            tag = "  [TODO-跳过] "
        elif c.get("requires_server"):
            tag = "  [可选-真实server] "
        else:
            tag = "  [就绪] "
        print("  %-24s%s%s" % (c["name"], tag, c["desc"]))
        if c.get("todo"):
            print("        TODO: %s" % c["todo"])
    print("  （TODO-跳过 = 功能未合入当前 checkout，仅登记；--all 不运行）")


async def run_one(case, spec, timeout):
    """执行单个用例（独立浏览器），返回 (status, detail, snapshot_dump)。"""
    for k, v in spec.items():
        if k in ("name", "desc", "run", "todo", "requires_server"):
            continue
        setattr(case, k, v)
    try:
        return await common.run_case(case, spec["run"], timeout)
    except common.SkipCase as e:
        return "SKIPPED", e.reason, None


def main():
    ap = argparse.ArgumentParser(
        description="e-agent Web 端回归测试套件（核心用例全 mock，不依赖真实 server）")
    ap.add_argument("--list", action="store_true", help="列出所有用例")
    ap.add_argument("--all", action="store_true", help="运行全部就绪用例")
    ap.add_argument("--case", metavar="NAME", action="append",
                    help="按名称子串运行用例（可多次指定）")
    ap.add_argument("--real", action="store_true", help="包含真实 server 冒烟用例")
    ap.add_argument("--timeout", type=float, default=common.CASE_TIMEOUT,
                    help="单用例超时秒数（默认 %.0f）" % common.CASE_TIMEOUT)
    args = ap.parse_args()

    cases = all_cases()
    if args.list or not (args.all or args.case):
        list_cases(cases)
        if not (args.all or args.case):
            print("\n用法：python regression.py --all | --case NAME [--case NAME2 ...] [--real] [--timeout N]")
        return 0

    if args.all:
        selected = [c for c in cases
                    if not c.get("todo") and (not c.get("requires_server") or args.real)]
        skipped_real = [c["name"] for c in cases
                        if c.get("requires_server") and not args.real]
    else:
        qs = [q.lower() for q in args.case]
        selected = [c for c in cases if any(q in c["name"].lower() for q in qs)]
        skipped_real = []
        todo_hits = [c for c in selected if c.get("todo")]
        for c in todo_hits:
            print("SKIPPED  %s（未合入：%s）" % (c["name"], c["todo"]))
            print()
        selected = [c for c in selected if not c.get("todo")]
        if not selected and not todo_hits:
            print("没有匹配的用例：%s" % args.case)
            list_cases(cases)
            return 1
        if not selected:
            return 0

    print("浏览器 : %s" % common.EXE)
    print("Base   : %s（核心用例全 mock，冒烟走真实 API）" % common.BASE)
    print("超时   : %ss/用例\n" % args.timeout)

    passed, failed, skipped = [], [], []
    for spec in selected:
        print("==== case: %s（%s）====" % (spec["name"], spec["desc"]))
        case = common.Case(spec["name"], spec["desc"])
        status, detail, snap = asyncio.run(run_one(case, spec, args.timeout))
        n_ok = sum(1 for _, ok, _ in case.checks if ok)
        if status == "PASS":
            passed.append(spec["name"])
        elif status == "SKIPPED":
            skipped.append((spec["name"], detail))
        else:
            failed.append(spec["name"])
        print("---- %s: %s  %d/%d PASS  (%.1fs) ----" %
              (spec["name"], status, n_ok, len(case.checks), case.elapsed))
        if status == "FAIL":
            for name, ok, det in case.checks:
                if not ok:
                    print("    FAIL: %s%s" % (name, "   | " + str(det) if det else ""))
            if snap is not None:
                print("    [DOM 快照] %s" % json.dumps(snap, ensure_ascii=False)[:2500])
        print()

    for name in skipped_real:
        skipped.append((name, "需要 --real（未启用）"))

    print("=" * 64)
    print("回归结果: %d PASS / %d FAIL / %d SKIPPED" %
          (len(passed), len(failed), len(skipped)))
    if failed:
        print("失败用例: %s" % ", ".join(failed))
    if skipped:
        print("跳过    : %s" % "; ".join("%s（%s）" % s for s in skipped))
    print("=" * 64)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
