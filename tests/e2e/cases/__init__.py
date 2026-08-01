"""用例发现：扫描本包下各模块的 CASES 列表。

每个用例是一个 dict：
    {
        "name": 唯一名称（--case 子串匹配）,
        "desc": 一句话描述,
        "run": async callable(case) —— 用例主体；todo 非空时可为 None,
        "todo": "" 就绪 / 非空 = 未合入功能的 TODO 说明（只列出、不运行）,
        "requires_server": True 表示需要真实 server（仅 --real 运行）,
    }
其余键（mobile / real_api / viewport / token / js_check …）会透传给 common.Case。
"""
import importlib
import pkgutil


def all_cases():
    out = []
    for m in pkgutil.iter_modules(__path__):
        if m.name.startswith("_"):
            continue
        mod = importlib.import_module(__name__ + "." + m.name)
        out.extend(getattr(mod, "CASES", []))
    return out
