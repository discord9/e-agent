#!/usr/bin/env python3
"""中文全译本拼接工具。

读取 --md 目录下所有 .md 章节文件（支持 turn*_zh.md 与 ch*.md 两种命名），
按章节顺序拼接为一整篇完整小说，文件最头部声明原文链接、原作者版权、
公共服务翻译性质。

用法:
    python3 build_full.py [--md DIR] [--out PATH] [--title TITLE] [--copyright FILE]
默认: --md 本脚本同目录 ../vox_vitae_zh/md, --out 本脚本同目录
      ../vox_vitae_zh/vox_vitae_zh_en.md
"""
import argparse, os, re, sys

HEADER_VOX = """# {title}

> **原文链接**：https://forums.sufficientvelocity.com/threads/vox-vitae-warhammer-ai-quest.136754/
>
> **原作者**：Neablis（Sufficient Velocity 论坛）
> 原帖：https://forums.sufficientvelocity.com/threads/vox-vitae-warhammer-ai-quest.136754/
>
> **版权声明**：本作品《Vox Vitae》及其全部角色、设定之版权归原作者所有，原作者保留一切权益。
> 本中文译本为**非官方、非商业的公共服务性质翻译**，仅供学习交流之用，
> 译者不主张对本译本的任何商业权利；如原作者要求，本译本将随时移除。
> 翻译内容如与原文有出入，一律以原文为准。

---

{toc}

---

"""

HEADER_COLOSSUS = """# {title}

> **原文链接**：https://www.fanfiction.net/s/13230607/1/Colossus
>
> **原作者**：Red Flag（FanFiction.net）
> 原帖：https://www.fanfiction.net/s/13230607/1/Colossus
>
> **版权声明**：本作品《Colossus》及其全部角色、设定之版权归原作者所有，原作者保留一切权益。
> 本中文译本为**非官方、非商业的公共服务性质翻译**，仅供学习交流之用，
> 译者不主张对本译本的任何商业权利；如原作者要求，本译本将随时移除。
> 翻译内容如与原文有出入，一律以原文为准。

---

{toc}

---

"""

HEADER_OTD = """# {title}

> **原文链接**：https://forums.spacebattles.com/threads/out-of-the-dark.922691/
>
> **原作者**：Derain Von Harken（SpaceBattles 论坛）
> 原帖：https://forums.spacebattles.com/threads/out-of-the-dark.922691/
>
> **版权声明**：本作品《Out of the Dark》及其全部角色、设定之版权归原作者所有，原作者保留一切权益。
> 本中文译本为**非官方、非商业的公共服务性质翻译**，仅供学习交流之用，
> 译者不主张对本译本的任何商业权利；如原作者要求，本译本将随时移除。
> 翻译内容如与原文有出入，一律以原文为准。

---

{toc}

---

"""


def chapter_key(fn: str):
    # 序章类（角色创建/邻近文明）排在最前，其余按 Turn 号排序
    if fn.startswith('turncharacter'):
        return (-2, 0)
    if fn.startswith('turnneighboring'):
        return (-1, 0)
    # turn 前缀：turn(\d+) 主号，可带 _(\d+) 子号（turn28_5 -> 28.5）
    m = re.match(r'turn(\d+)(?:_(\d+))?', fn)
    if m:
        main = int(m.group(1))
        sub = int(m.group(2)) if m.group(2) else 0
        return (main, sub)
    # ch 前缀：ch(\d+) 按数字排序（ch1..ch9），同样支持子号
    m = re.match(r'ch(\d+)(?:_(\d+))?', fn)
    if m:
        main = int(m.group(1))
        sub = int(m.group(2)) if m.group(2) else 0
        return (main, sub)
    # 其他文件名：排在 turn/ch 之后，按文件名排序
    return (10**9, fn)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--md', default=os.path.join(os.path.dirname(os.path.abspath(__file__)), 'vox_vitae_zh', 'md'))
    ap.add_argument('--out', default=os.path.join(os.path.dirname(os.path.abspath(__file__)), 'vox_vitae_zh', 'vox_vitae_zh_en.md'))
    ap.add_argument('--title', default='Vox Vitae（生命之声）中文全译本')
    ap.add_argument('--copyright', default=None,
                    help='版权头部模板文件路径（含 {title}/{toc} 占位符）；'
                         '缺省时按章节命名自动选择：全为 ch 前缀用 Colossus 头部，否则用 Vox 头部')
    args = ap.parse_args()

    files = [f for f in os.listdir(args.md) if f.endswith('.md')]
    if not files:
        print('错误: %s 下没有 .md 文件' % args.md, file=sys.stderr)
        sys.exit(1)
    files.sort(key=chapter_key)

    toc_lines = []
    body = []
    for f in files:
        title = re.sub(r'_zh\.md$|\.md$', '', f)  # turn22_zh.md -> turn22, ch1.md -> ch1
        label = re.sub(r'_', '.', title)         # turn28_5 -> turn28.5, ch1 -> ch1
        toc_lines.append('- %s' % label)
        with open(os.path.join(args.md, f), encoding='utf-8') as fh:
            body.append(fh.read().rstrip())

    if args.copyright:
        with open(args.copyright, encoding='utf-8') as fh:
            template = fh.read()
    elif 'otd' in args.out.lower():
        template = HEADER_OTD
    elif all(f.startswith('ch') for f in files):
        template = HEADER_COLOSSUS
    else:
        template = HEADER_VOX
    header = template.format(title=args.title, toc='\n'.join(toc_lines))

    out_dir = os.path.dirname(os.path.abspath(args.out))
    os.makedirs(out_dir, exist_ok=True)
    with open(args.out, 'w', encoding='utf-8') as fh:
        fh.write(header)
        fh.write('\n\n'.join(body))
        fh.write('\n')
    print('已生成: %s (%d 章, %d 字节)' % (args.out, len(files), os.path.getsize(args.out)))

if __name__ == '__main__':
    main()
