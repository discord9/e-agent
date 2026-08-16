---
name: fic2zh
description: Translate SpaceBattles/SufficientVelocity/FanFiction.net Warhammer 40K fanfiction into Simplified Chinese end-to-end — fetch, segment, parallel-translate, reconcile, unify terminology, proofread, render bilingual/Chinese-only md+epub. Use when asked to translate a fanfic book, catch up a missed chapter, or rebuild a translation artifact.
---

# 中文同人小说翻译流水线（fic2zh）

把英文战锤 40K 同人小说（SpaceBattles / Sufficient Velocity / FanFiction.net）端到端翻译为简体中文，产出**中英对照**与**纯中文**两套 Markdown + EPUB。四本已跑通：Vox Vitae（74 章）、Culture Explores WH40K（157 章）、Colossus（17 段）、Out of the Dark（70 章/23 万字）。

工具链在 `vox_vitae_toolkit/`（本仓库子目录），脚本用法见其 `README.md`。本 skill 管**流程与约定**，术语表按书另建（`<book>_zh/glossary_zh-CN.csv`），不随 skill 分发。

## When to use

- 用户要求翻译一本新的同人小说（长文、多章）。
- 追更：作者更新新章节，需要补译并重建成品。
- 排查漏翻/漏段、术语不一致、成品重建。
- 给已有译文做内容校对。

Do NOT use when: 只是零星翻译一段文本（无章/无书结构）；翻译非小说类材料；需要发布/上传译文到任何平台（本流程只产出本地文件）。

## 铁律（不可违反）

1. **人名一律保留英文原文，不音译**（Perturabo、Gottfried、Ramirez…）。舰名/地名/组织名按术语表或意译。
2. **en 字段逐字节等于源文件**（含换行与末尾换行状态）。译文 json 的 `en` 是从 seg 文件复制出来的，不是重写的。
3. **先建基准，再翻译**：抓取后立刻把原文固化进 `<book>_zh/segs/`，翻译前先对账，翻译后全量复检。
4. 对话用「」，引号嵌套用『』；原文拼写错误 en 照实保留、zh 按正确含义译。
5. 术语表按书维护（`glossary_zh-CN.csv`，格式 `source,target,zh-CN`），新书从旧书继承 + 补充专名。
6. 成品命名：`<book>_zh_en.*`（中英对照）/ `<book>_zh_only.*`（纯中文）。
7. 校对发现的术语裁定要**回写术语表**，下一本/下一轮不再犯。

## 工作流

### 0. 准备（一次性）

- 确认工作区在 `vox_vitae_toolkit/`（脚本都在这里）。
- 确认目标书的 URL 与章节结构（SpaceBattles threadmarks 清单，或 SV 帖子列表）。

### 1. 抓取

- SB/SV 直连常被 Cloudflare 拦（403），用 wayback machine reader 存档（`web.archive.org/web/2026/...` 年度重定向可拿到较新快照；2022 旧快照可能缺页）。
- 抓取脚本产出 `<book>_raw/chNN.txt`（每章一个纯文本：标题首行 + 正文 `\n\n` 分段）+ `manifest.tsv`（章号、post id、标题、词数）。
- 抽取规则：threadmark 主类、`bbWrapper` 平衡 div、`<br/>` 转段落分隔；剔除引用块/媒体/iframe/spoiler/图片；章末 "Authors note." 起的内容截断删除（正文内嵌的档案体文字保留）。
- 验收：章数 ≥ 期望值；总词数 ≥ 期望（OTD 70 章约 12.5 万词）；抽查无 HTML 残留、无 "View content/Click to expand" 等 UI 文本、章尾自然收束。

### 2. 切段 + 建基准（先建基准！）

- 每章默认 1 段；>3000 词的章按段落边界拆 2 段（中位章 1600 词时约 10% 的章会拆）。
- 输出 `<book>_zh/segs/chN_segM.txt`，并**立即验证**：切段拼接 == 整章（rstrip 比较），防止拆段丢内容。
- 复制一份原始整章文件也放进 segs/（命名 chNN.txt），作为对账基准。
- 产出 `<book>_zh/json/`（译文输出目录）与 `<book>_zh/glossary_zh-CN.csv`（从旧书术语表继承 + 高频专名扫描补充）。

### 3. 术语表

- 扫描全书高频大写词（`collections.Counter` + 正则 `\b[A-Z][a-zA-Z]{3,}\b`）定位专名，先裁定一批：势力、种族、舰级、地名、军衔、科技名。
- 40K 通行译名优先（铁人/Men of Iron、亚空间/Warp、机械神教/Adeptus Mechanicus、星际战士/Astartes…），同人特有词按语境新译并在翻译任务里声明。
- **人名条目不建**（铁律 1），但可以建"人名→保留英文"的提醒条目（source 人名、target 同英文）。

### 4. 并行翻译

- 按 5 章/批切片，每批一个 fixer 子代理；4–9 批并行（注意：子代理不继承 skill，任务文本必须**自包含**：输入 seg 路径、输出 json 路径、格式、术语表路径、铁律、验证命令）。
- 任务文本固定模板（见文末），术语提示**不得逐批漂移**——同一批内一致即可，跨批统一交给第 6 步。
- 输出格式：单元素数组 json `{"id":"ch1-seg0","en":"<逐字节>","zh":"<镜像换行>","summary":"≤40字"}`；`zh` 必须镜像 `en` 的段落数、空行位置、末尾换行。
- 每批交付必须自带验证命令（en 与源文件逐字节断言），全部 OK 才算完成。
- **沙箱边界**：子代理 bash 读不到工作区外的宿主目录（如 `~/.svtmp/`）。输入文件必须已经复制进工作区；否则子代理会"自行重抓"导致分段边界漂移（踩过：Vox 补翻时 9/10 段不一致）。
- **断点恢复**：API 余额不足会整批失败（HTTP 402）。重派前先检查该批哪些 json 已交付、哪些缺失，只补缺失段。

### 5. 全量对账（翻译完成后第一件事）

- 脚本：`check_alignment.py`（或自写 20 行：遍历 segs ↔ json，断言 en 逐字节一致、zh 非空、无多余 json）。
- 验收：`segs == jsons`，全部 en 一致，反向无多余。**任何缺失/不一致都不允许进入下一阶段**。
- 踩过的坑：拆段章只译了 seg0 漏 seg1（ch14_seg1 事后补翻）；Vox 曾漏 3 整章。对账就是为此设计的。

### 6. 术语统一（跨批冲突裁定）

- 并行批次必然产生同一英文词的多译法。全库扫描冲突分布（人名音译 vs 英文、同词多译）。
- 裁定规则优先级：铁律（人名保留英文）> 术语表 > 出现最多的主流译法 > 40K 通行译名。
- 派一个 fixer 做全局替换：只改 `zh` 字段；人名用边界正则 `(?<![A-Za-z])X(?![A-Za-z])`；**逐处核验 en 上下文**再替换（"黑石"可能对应 blackstone/banestone/其他，别一刀切）；替换后自检残留为 0 + en 未动。
- 已见冲突实例：Perturabo 佩图拉博/英文、黑色图书馆/黑图书馆、Triarch 三圣/三圣议会/三执政团、banestone 禁石/封魔石/祸石、Guts 古茨/英文、Star General 星级将军/星级上将/星将、Night Sentinels 夜哨军团/夜哨卫/夜幕哨兵。

### 7. 内容校对（质量核心）

- 按章范围分 4 批并行（每批 ~18 章），oracle 角色逐段对照 en/zh 精读。
- 重点：漏译（en 有 zh 无，尤其拆段边界附近）、错译（含义偏差）、人名/术语一致性、数字与专名、引号规范。
- 校对直接改 json 的 zh（en/id/summary 不动），报告列出每处：文件、原译文、新译文、原因。
- 实测修正率：4 批 70 处/78 段（漏译整句 5+ 处、ch37 两段整体错位、the Second 误译"第二原体"实为"第二军团"）。
- 校对发现的跨批遗留问题（如舰名双译）汇总，再做一轮收尾统一。

### 8. 渲染成品

- 章节标题映射：忠实原作（多线并行的"第 1 周/第 2 周"不加工），专名意译、人名保留英文；两段章合并为一个 `# 标题`。
- `render_otd.py`（或按书写渲染脚本）输出 `md/`（`**EN**`/`**ZH**` 对照）与 `md_zh/`（纯中文）。**章节标题必须用 `# ` 一级标题**——`build_epub.py` 的 `split_sections` 按 `^# ` 分章，用 `## ` 会把全书并成一章（踩过）。
- `build_full.py --md md --out <book>_zh_en.md --title ...` + `build_epub.py`（**注意用 `/usr/bin/python3`**，它有 ebooklib；uv python3 没有）。build_full 头部模板按书内置（Vox/Colossus/OTD 各自版权头部）。
- epub 验证：xhtml 数 = 章数 + 2（preface + 结尾），抽查首尾章节标题。

### 9. 提交

- 全部产物（json/segs/glossary/md/epub/raw/manifest）一并 `git add -A && git commit`。
- 提交信息含：书名、章数/段数/字数、关键裁定、校验结论。

## 任务文本模板（翻译批）

```text
翻译《<书名>》第 <A>–<B> 章，英译中。
【输入】otd_zh/segs/chN_segM.txt（先 ls 确认；注意 chK 拆两段）
【输出】otd_zh/json/chN-segM.json（每段一个）
【格式】单元素数组 json：{"id","en","zh","summary"}；en 逐字节复制源文件（含换行）；zh 逐段镜像换行/空行结构
【要求】1) 术语表 <book>_zh/glossary_zh-CN.csv 先读全文，表外新词按 40K 通行译名+本作惯例新译并报告；
2) 人名一律保留英文不音译；3) 对话用「」，嵌套用『』；4) en 拼写错误照实保留、zh 按正确含义译；5) 简体中文
【验证】python3 -c "…断言 en==源文件、结构正确…" 全部 OK 才算完成
【返回】summary / changes / verification / 每章 zh 字数 / 新造译名清单 / 不确定处
```

## 已验证的坑（踩过，别重踩）

| 坑 | 对策 |
|---|---|
| 子代理读不到工作区外目录，自行重抓输入 → 分段边界漂移 | 输入 seg 文件先复制进工作区；对账阶段抓 |
| 拆段章只译一段（漏 seg1） | 翻译批次任务里显式点名拆段章；第 5 步全量对账兜底 |
| 术语提示逐批漂移 → 跨批不一致 | 任务模板固定；第 6 步统一轮兜底 |
| API 余额不足整批失败 | 断点恢复：查已交付/缺失，重派缺失段 |
| build_epub 用 `## ` 章节 → 全书并成 1 章 | 渲染一律 `# ` 一级标题 |
| uv python3 无 ebooklib | epub 用 /usr/bin/python3 生成 |
| 全书头部硬编码版权信息错误 | build_full 内置每书模板，按输出路径选择 |
| 校对轮改动与并行批次冲突 | 校对按章范围分片，写范围互斥；遗留问题汇总收尾轮 |

## 验收标准（整本交付）

- 对账：segs == json 数，en 全部逐字节一致，零漏段。
- 术语：全库扫描无残留异译（保留的合理例外逐条说明）。
- 校对：每批有修正清单，漏译/错译清零。
- 成品：`<book>_zh_en.md/.epub`、`<book>_zh_only.md/.epub` 齐全，epub 章数正确，头部版权信息与原著一致。
- git：一次提交包含全部产物。
