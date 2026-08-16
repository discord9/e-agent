#!/usr/bin/env python3
"""culture_render.py — Culture 成品渲染：json → md（对照/纯中文），再 build_full + build_epub。

用法：
  python3 culture_render.py                 # 渲染对照版 md + 全本 + epub
  python3 culture_render.py --zh-only       # 只渲染纯中文版
  python3 culture_render.py --all           # 两者都渲染

说明：
- 输入 culture_zh/json/ch{N}-seg{M}.json（单元素数组 [{id,en,zh,summary}]）
- 输出 culture_zh/md/ch{N}.md（对照版）或 culture_zh/md_zh/ch{N}.md（纯中文版）
- 空 seg（en/zh 为空串）自动跳过
- 章标题取 seg0 的首个非空行（章节标题），无则用 "第 N 章"
"""
import json, glob, os, re, sys, argparse
from collections import defaultdict

ROOT = os.path.dirname(os.path.abspath(__file__))
JSON_DIR = os.path.join(ROOT, 'culture_zh', 'json')
MD_DIR = os.path.join(ROOT, 'culture_zh', 'md')
MD_ZH_DIR = os.path.join(ROOT, 'culture_zh', 'md_zh')

def load_segs():
    chapters = defaultdict(list)
    for f in sorted(glob.glob(os.path.join(JSON_DIR, 'ch*-seg*.json'))):
        base = os.path.basename(f)
        m = re.match(r'ch(\d+)-seg(\d+)\.json', base)
        if not m:
            continue
        ch, seg = int(m.group(1)), int(m.group(2))
        try:
            d = json.load(open(f, encoding='utf-8'))
            if len(d) != 1:
                continue
            chapters[ch].append((seg, d[0]))
        except Exception as e:
            print(f'!! 读取失败 {base}: {e}', file=sys.stderr)
    for ch in chapters:
        chapters[ch].sort(key=lambda x: x[0])
    return chapters

def chapter_title(segs):
    """取章标题：优先 zh 的首个非空行（去掉导航残留）"""
    for _, seg in segs:
        zh = seg.get('zh', '')
        for line in zh.split('\n'):
            line = line.strip()
            if line and not line.startswith('←') and '上一章' not in line and not re.match(r'^[\-=_]{3,}$', line):
                return line
    return None

def render(chapters, zh_only):
    os.makedirs(MD_ZH_DIR if zh_only else MD_DIR, exist_ok=True)
    for ch in sorted(chapters):
        segs = chapters[ch]
        title = chapter_title(segs) or f'第 {ch} 章'
        out = [f'# {title}', '']
        for _, seg in segs:
            en = seg.get('en', '')
            zh = seg.get('zh', '')
            if not en.strip() and not zh.strip():
                continue  # 空 seg
            if zh_only:
                # 纯中文：按空行分段的段落输出
                paras = [p.strip() for p in zh.split('\n\n') if p.strip()]
                # 去掉章标题行（标题已在开头）
                if paras and paras[0] == title:
                    paras = paras[1:]
                for p in paras:
                    out.append(p)
                    out.append('')
            else:
                # 对照版：中文在前，英文在后，按段落对齐
                zparas = [p.strip() for p in zh.split('\n\n') if p.strip()]
                eparas = [p.strip() for p in en.split('\n\n') if p.strip()]
                # 去掉标题行
                if zparas and zparas[0] == title:
                    zparas = zparas[1:]
                if eparas and eparas[0] == title:
                    eparas = eparas[1:]
                # 去掉导航行
                zparas = [p for p in zparas if p != '← 上一章']
                eparas = [p for p in eparas if p != '← Previous Chapter']
                n = max(len(zparas), len(eparas))
                for i in range(n):
                    if i < len(zparas):
                        out.append(zparas[i])
                    if i < len(eparas):
                        out.append(eparas[i])
                    out.append('')
        text = '\n'.join(out).rstrip() + '\n'
        fn = os.path.join(MD_ZH_DIR if zh_only else MD_DIR, f'ch{ch}.md')
        with open(fn, 'w', encoding='utf-8') as f:
            f.write(text)
        print(f'  ch{ch}.md ({len(text)} 字符)')

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--zh-only', action='store_true')
    ap.add_argument('--all', action='store_true')
    args = ap.parse_args()
    chapters = load_segs()
    print(f'加载 {len(chapters)} 章')
    if args.all or not args.zh_only:
        print('渲染对照版 md …')
        render(chapters, zh_only=False)
        print('build_full …')
        os.system(f'python3 build_full.py --md culture_zh/md --out culture_zh/culture_zh_en.md --title "The Culture Explores Warhammer 40k（中文对照版）" --copyright culture_zh/copyright_header.md')
        print('build_epub …')
        os.system(f'python3 build_epub.py --md culture_zh/culture_zh_en.md --out culture_zh/culture_zh_en.epub --title "The Culture Explores Warhammer 40k（中文对照版）" --author "GodOfSaltAndSteel / 社区翻译"')
    if args.all or args.zh_only:
        print('渲染纯中文 md …')
        render(chapters, zh_only=True)
        print('build_full …')
        os.system(f'python3 build_full.py --md culture_zh/md_zh --out culture_zh/culture_zh_only.md --title "The Culture Explores Warhammer 40k（纯中文版）" --copyright culture_zh/copyright_header.md')
        print('build_epub …')
        os.system(f'python3 build_epub.py --md culture_zh/culture_zh_only.md --out culture_zh/culture_zh_only.epub --title "The Culture Explores Warhammer 40k（纯中文版）" --author "GodOfSaltAndSteel / 社区翻译"')
    print('完成')

if __name__ == '__main__':
    main()
