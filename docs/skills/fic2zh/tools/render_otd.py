#!/usr/bin/env python3
"""渲染 OTD 全书中英对照 Markdown（标题映射 + 逐段双语）。"""
import json, os, re, glob

# OTD 标题映射（英文 → 中文，忠实原作；专名意译/保留英文）
TITLES = {
1:'序章 Prologue', 2:'技术档案：行星围城矢量 Tech File PSV', 3:'第一章 Chapter 1', 4:'第二章 Chapter 2', 5:'第三章 Chapter 3',
6:'苦路 The Bitter Path', 7:'技术档案：盖勒炸弹 Tech File Gellar Bomb', 8:'第四章 Chapter 4',
9:'暗星行动 第一部 Operation DARKSTAR Part I', 10:'暗星行动 第二部 Operation DARKSTAR part II',
11:'暗星行动 第三部 Operation DARKSTAR Part III', 12:'暗星行动 第四部 Operation DARKSTAR Part IV',
13:'灵能科技的起源（附评注）The origin of Psitech (with commentary)', 14:'暗星行动 第五部 Operation DARKSTAR PART V',
15:'暮刃 The Dusk Blade', 16:'黑色图书馆 负一部 The Black Library Part Minus one',
17:'技术档案：禁卫装甲 Tech File Praetorian armour', 18:'黑色图书馆 第一部 The Black Library Part I',
19:'地狱之门 第一部 The Gates of Hell Part I', 20:'灵魂的遗训 The testament of the Soul',
21:'黑色图书馆 第二部 The Black Library Part II', 22:'地狱之门 第二部 The Gates of Hell Part II',
23:'技术档案：谋杀级情报舰 Tech File Murder class Intelligence vessel',
24:'技术档案：太阳军团与末日屠戮者 Tech file: Solar Legion and Doom Slayers',
25:'黑色图书馆 第三部 The Black Library Part Three', 26:'解构者 The Unmakers.',
27:'地狱之门 第三部 The Gates of Hell Part III', 28:'技术档案：魔像坦克 Tech File: Golem Tank',
29:'战争循环 第一部 Cycles of War I', 30:'战争循环 第二部 Cycles of War II', 31:'战争循环 第三部 Cycles of War Part III',
32:'战争循环 第四部 Cycles of War Part IV', 33:'战争循环 第五部 Cycles of War Part V',
34:'群星归位 第一部 The stars align Part I', 35:'群星归位 第二部 The Stars align Part II',
36:'群星归位 第三部 The Stars Align Part III', 37:'群星归位 第四部 The Stars Align Part IV',
38:'漫漫归途 第一部 The Long Road home part I', 39:'漫漫归途 第二部 The Long Road home part II',
40:'漫漫归途 第三部 The Long Road home part III',
41:'技术档案：末日使者泰坦 Tech File: Endbringer Titan',
42:'复仇之物由此而来 第一部 Something vengeful this way comes Part I',
43:'复仇之物由此而来 第二部 Something vengeful this way comes Part II',
44:'悔恨 Regrets', 45:'复仇之物由此而来 第三部 Something vengeful this way comes III',
46:'黑暗中注视 In the darkness watching', 47:'石之生 Born of Stone',
48:'风暴俯冲战役 第一部 Stormdive Campaign Part 1',
49:'杰里科边境战役：金牛走廊 第一部 Jericho Reach campaign Taurian corridor part 1',
50:'风暴俯冲战役 第二部 Stormdive campaign Part II',
51:'风暴俯冲战役 第三部 Stormdive campaign part III',
52:'风暴俯冲战役 第四部 Stormdive campaign Part IV',
53:'风暴俯冲战役 第五部（银河静止之日）Stormdive campaign part V (the day the galaxy stood still)',
54:'仇恨的滋味 The taste of hate.', 55:'制胜之策，就是不落子 The winning move is not to play.',
56:'损伤报告（好吧，一切又搞砸了……）Damage Report (So everything is fucked up ... again)',
57:'前往美杜莎之旅 第一部 The journey to Medusa part 1', 58:'前往美杜莎之旅 第二部 The journey to Medusa part 2',
59:'前往美杜莎之旅 第三部 The journey to Medusa Part 3', 60:'前往美杜莎之旅 第四部 The journey to Medusa Part 4',
61:'帷幕分离 The Veil parts', 62:'逼近 The Approach',
63:'美杜莎突袭 第一部 Assault on medusa part I', 64:'美杜莎突袭 第二部 Assault on Medusa Part II',
65:'美杜莎突袭 第三部 Assault on Medusa Part III', 66:'美杜莎突袭 第四部 Assault on Medusa part IV',
67:'鲜血时刻已至！！！！！！ THE TIME OF BLOOD HAS COME!!!!!!',
68:'深渊开启 第一部 The Pit Opens part 1', 69:'深渊开启 第二部 The Pit Opens Part 2',
70:'与此同时，在塔耳塔罗斯星 Meanwhile on Planet Tartarus',
}

def render_one(ch, seg, out_dir, zh_only=False):
    j = f'otd_zh/json/ch{ch}-seg{seg}.json'
    d = json.load(open(j, encoding='utf-8'))[0]
    title = TITLES.get(ch, f'第{ch}章')
    lines = [f'# {title}', '']
    en_paras = d['en'].split('\n\n')
    zh_paras = d['zh'].split('\n\n')
    if not zh_only:
        for ep, zp in zip(en_paras, zh_paras):
            lines.append(f'**EN** {ep}')
            lines.append('')
            lines.append(f'**ZH** {zp}')
            lines.append('')
    else:
        for zp in zh_paras:
            lines.append(zp)
            lines.append('')
    # 去尾空行
    while lines and lines[-1] == '':
        lines.pop()
    os.makedirs(out_dir, exist_ok=True)
    out = os.path.join(out_dir, f'ch{ch:02d}.md')
    with open(out, 'w', encoding='utf-8') as f:
        f.write('\n'.join(lines) + '\n')
    return out

if __name__ == '__main__':
    # 先清空旧渲染
    for d in ['otd_zh/md', 'otd_zh/md_zh']:
        for f in glob.glob(f'{d}/*.md'):
            os.remove(f)
    for ch in range(1, 71):
        segs = sorted(glob.glob(f'otd_zh/json/ch{ch}-seg*.json'))
        for s in segs:
            seg = int(re.search(r'seg(\d+)', s).group(1))
            render_one(ch, seg, 'otd_zh/md')
            render_one(ch, seg, 'otd_zh/md_zh', zh_only=True)
    print('OTD 渲染完成:', len(glob.glob('otd_zh/md/*.md')), '对照 /', len(glob.glob('otd_zh/md_zh/*.md')), '纯中文')
