---
name: fic2zh
description: Translate English fanfiction / web novels (Warhammer 40K and other fandoms) into Simplified Chinese end-to-end — fetch, segment, parallel-translate, reconcile, unify terminology, proofread, render bilingual/Chinese-only md+epub. Works for SpaceBattles / SufficientVelocity / FanFiction.net and similar forum/serial-fiction sites. Use when asked to translate a fanfic book, catch up a missed chapter, or rebuild a translation artifact.
---

# 中文同人小说翻译流水线（fic2zh）

> **Production status（2026-08）**：本流程已连续完整交付 7 本长篇全流程翻译（同人 4 本 + Xeelee 系列合订本 3/10 本），累计 360+ 章、60 万+ 汉字；最近一本《Flux》29 章 12.8 万词端到端通过——8 批并行翻译、术语统一轮 460+ 处替换、4 轮校对 100+ 处修正、对账与 epub 校验全绿。

把英文战锤 40K 同人小说（SpaceBattles / Sufficient Velocity / FanFiction.net）端到端翻译为简体中文，产出**中英对照**与**纯中文**两套 Markdown + EPUB。四本已跑通：Vox Vitae（74 章）、Culture Explores WH40K（157 章）、Colossus（17 段）、Out of the Dark（70 章/23 万字）。

工具脚本随本 skill 分发于 `docs/skills/fic2zh/tools/`（12 个 .py，随主仓库 git 分发）；译文数据在各书目录 `<book>_zh/`（存放于**数据仓库**，本机为 `vox_vitae_toolkit/`，git 与主仓库分离；换环境时只需定位含 `<book>_zh/` 的数据目录即可）。本 skill 管**流程与约定**，术语表按书另建（`<book>_zh/glossary_zh-CN.csv`），不随 skill 分发。

## When to use

- 用户要求翻译一本新的同人小说（长文、多章）。
- 追更：作者更新新章节，需要补译并重建成品。
- 排查漏翻/漏段、术语不一致、成品重建。
- 给已有译文做内容校对。

Do NOT use when: 只是零星翻译一段文本（无章/无书结构）；翻译非小说类材料；需要发布/上传译文到任何平台（本流程只产出本地文件）。

## 铁律（不可违反）

1. **人名一律保留英文原文，不音译**（Perturabo、Gottfried、Ramirez…。本项目约定；**新书开译前先与用户确认人名/专名策略**，部分圈子通行音译）。舰名/地名/组织名按术语表或意译。
2. **en 字段逐字节等于源文件**（含换行与末尾换行状态）。译文 json 的 `en` 是从 seg 文件复制出来的，不是重写的。
3. **先建基准，再翻译**：抓取后立刻把原文固化进 `<book>_zh/segs/`，翻译前先对账，翻译后全量复检。
4. 对话用「」，引号嵌套用『』；原文拼写错误 en 照实保留、zh 按正确含义译。
5. 术语表按书维护（`glossary_zh-CN.csv`，格式 `source,target,zh-CN`）；新书若同 fandom 可从旧书继承，否则从高频专名扫描冷启动 + 补充。
6. 成品命名：`<book>_zh_en.*`（中英对照）/ `<book>_zh_only.*`（纯中文）。
7. 校对发现的术语裁定要**回写术语表**，下一本/下一轮不再犯。

## 工作流

### 0. 准备（一次性）

- 确认工具脚本位置（`docs/skills/fic2zh/tools/`，随 skill 分发）与译文数据位置（数据仓库下各 `<book>_zh/`，本机为 `vox_vitae_toolkit/`）。
- 确认目标书的 URL 与章节结构（**站点差异**：SB/SV 等论坛的正文与读者讨论混在帖子流里，靠楼主维护的 **threadmark 目录**索引——抓取时只取 threadmark 指向的帖子；FFN/AO3/Webnovel 等小说站有独立的章节目录/章节列表，无 threadmark 概念，直接按目录抓）。

### 0.5 新书 / 新站点适配清单

- ① 与用户确认人名/专名策略（保留英文 vs 音译，见铁律 1），并写入 `book_summary.txt` 或项目约定文件；是否允许首次出现附英文原名也要显式约定。该策略须注入翻译、术语统一和内容校对任务，不得只在开书时口头确认。
- ② 确认术语表种子来源：同 fandom 旧书可继承；否则高频专名扫描冷启动（§3）。
- ③ 新站点：先试抓 1 章，写下该站的抽取规则（内容容器 CSS 类、章末截断标记、UI 残留清单），再全量抓。
- ④ 渲染脚本与版权头部按旧书脚本改写；先渲染 1 章验证 `# ` 分章与 epub 章数（§8）。

### 1. 抓取

- SB/SV 直连常被 Cloudflare 拦（403）。对策按序：① 用 **Playwright**（真实浏览器指纹 + JS 执行，可过大部分 Cloudflare 挑战）抓取；② 仍被拦则**向使用者索要登录 cookie**（`cf_clearance` 等），注入请求头后抓取；③ 兜底：wayback machine reader 存档（`web.archive.org/web/2026/...` 年度重定向可拿到较新快照；2022 旧快照可能缺页）。抓取必须带礼貌间隔（限速），避免触发风控。
- 抓取脚本产出 `<book>_raw/chNN.txt`（每章一个纯文本：标题首行 + 正文 `\n\n` 分段）+ `manifest.tsv`（章号、post id、标题、词数）。
- 抽取规则：**SB/SV 论坛**——threadmark 主类帖子、`bbWrapper` 平衡 div、`<br/>` 转段落分隔；剔除引用块/媒体/iframe/spoiler/图片；章末 "Authors note." 起的内容截断删除（正文内嵌的档案体文字保留）。**FFN/AO3 等小说站**——按站内章节页结构提取正文容器，规则另写（见 §0.5③）。
- 验收：章数 ≥ 期望值；总词数 ≥ 期望（按书的 threadmark/章节目录预估，勿用旧书数字当标准）；抽查无 HTML 残留、无 "View content/Click to expand" 等 UI 文本、章尾自然收束。

### 1.5 输入为 EPUB（保留原书结构，只翻正文）

用户可能直接给 EPUB（FFN/AO3 等站下载、或本地已有）。此时**不重排、不重建**，而是在原 EPUB 结构上只替换英文正文为中文，保留其封面/样式/目录/分章。

**EPUB 结构速览**（本质是 ZIP 容器）：
- `mimetype` 条目：必须**无压缩（STORED）且是 zip 第一个条目**，内容 `application/epub+zip`——重打包时顺序/压缩方式错了阅读器会拒绝。
- `META-INF/container.xml`：指向 OPF（`*.opf`）。
- OPF：`manifest` 列出全部资源文件，`spine` 决定章节阅读顺序（按 spine 顺序遍历即正文顺序）。
- `nav.xhtml` / `toc.ncx`：目录（章节标题），需与正文标题同步翻译。

**处理步骤**：
1. **解包**：`python3 -c "import zipfile; zipfile.ZipFile('book.epub').extractall('book_src/')"`；先完整备份原 EPUB。
2. **提取正文**：按 spine 顺序，用 **lxml 解析**每个 XHTML，把正文段落提取为纯文本（`\n\n` 分段，标题首行），存为 `<book>_raw/chNN.txt`——与网页流程的输入格式一致，**后续 §2–§7 全部复用**（切段基准/en 逐字节断言/翻译批次/对账/统一/校对）。
3. **翻译**：完全走标准流程（§2–§7），json 的 en 仍逐字节等于提取出的 chNN.txt。
4. **回填**：译文按段落逐段映射回原 XHTML——**用 lxml 只替换文本节点**（`element.text` / `.tail`），**绝不动标签、属性、class、href、img src**；段落对应靠 en/zh 段落数镜像（已有机制）保证。目录（nav/ncx）里的章节标题同步换成中文标题。
5. **重打包**：`mimetype` 用 `ZIP_STORED` 且**第一个写入**；其余条目 `ZIP_DEFLATED`；OPF 里 `dc:language` 改 `zh-CN`（可选，其余元数据保留）。
6. **校验**：重新解包确认结构完整；用阅读器或 `ebooklib` 打开，验证章数、样式、封面与原书一致，正文为中文。

**要点与坑**：
- **只翻正文文本**——不要用正则改 HTML，必须 lxml/解析器操作，否则极易破坏结构。
- 长章：单个 XHTML 很长时仍以"一个 XHTML = 一个 seg"为单位（翻译批次按字数切，一个文件可多批续译，但 en/zh 段落始终一一对应）。
- 图片 alt、链接文字可译可不译（建议只译正文段落与标题，保持简单）。
- **DRM 加密的 EPUB 无法直接处理**——需用户先用 Calibre 等去除 DRM 再给。
- **结构版脚本写盘必须写全 `range(BODY_LAST+1, SPINE_LAST+1)` 的全部非正文页**——正文之后、spine 末尾之前若有非正文页（如 Flux 的 part0078 TIMELINE），只写 SPINE_FIRST..BODY_FIRST 与 SPINE_LAST 会跳写，但 content.opf/toc.ncx 仍引用它 → 悬空 spine 引用，阅读器报错；生成后核对 opf/ncx 引用的每个 xhtml 都真实存在于归档（无悬空引用）。
- 产出：保留结构的 `<book>_zh_translated.epub`；如需也可另出纯文本版 md/epub（§8）。

### 2. 切段 + 建基准（先建基准！）

- 每章默认 1 段；>3000 词的章按段落边界拆 2 段（中位章 1600 词时约 10% 的章会拆）。
- 输出 `<book>_zh/segs/chN_segM.txt`，并**立即验证**：切段拼接 == 整章（rstrip 比较），防止拆段丢内容。
- 复制一份原始整章文件也放进 segs/（命名 chNN.txt），作为对账基准。
- 产出 `<book>_zh/json/`（译文输出目录）与 `<book>_zh/glossary_zh-CN.csv`（从旧书术语表继承 + 高频专名扫描补充）。

### 3. 术语表

- 扫描全书高频大写词（`collections.Counter` + 正则 `\b[A-Z][a-zA-Z]{3,}\b`）定位专名，先裁定一批：势力、种族、舰级、地名、军衔、科技名。
- 该 fandom 通行/官方译名优先（如 40K 的铁人/Men of Iron、亚空间/Warp、机械神教/Adeptus Mechanicus、星际战士/Astartes…），同人特有词按语境新译并在翻译任务里声明。
- **人名条目不建**（铁律 1），但可以建"人名→保留英文"的提醒条目（source 人名、target 同英文）。

### 3.5 术语表生命周期（逐步完善）

术语表不是一次建成的，而是**随每一本书的流程演进**，分五个阶段逐步完善。任何一轮新译/校对产生的裁定，都必须回写术语表——否则下一轮还会犯。

| 阶段 | 动作 | 产出 |
|---|---|---|
| **① 种子**（§3，书开始前） | 高频专名扫描 + fandom 通行译名初裁；同 fandom 旧书直接继承其 glossary | `glossary_zh-CN.csv` 初版 |
| **② 翻译中收集**（§4，每批） | 每批 fixer 交付必须报告"新造译名清单"（表外新词 + 译文）；批次内术语提示固定 | 逐批新词报告 |
| **③ 统一轮回写**（§6，翻译完成后） | 冲突裁定表（old→new）执行后，**把裁定结果写回 glossary**：新增/修正 source→target 条目 | glossary 更新（统一后状态） |
| **④ 校对轮补录**（§7） | 校对发现的术语不一致（人名音译回退、同词多译）修正后，同样回写 glossary（如 Dusk Raiders→黄昏掠夺者、the Anathema→咒缚者） | glossary 补录 |
| **⑤ 跨书继承**（下一本开始时） | 新书同 fandom（如都是 40K）→ 继承上一本 glossary 作为种子，再按 ① 补充本书特有词 | 下一本 glossary 种子 |

**系列合订本**（多本同系列书在一个合集里，如 Xeelee 10 本合订）：建系列级 `shared_glossary_zh-CN.csv`（与各书目录同级的合集目录下，如 `xeelee_zh/shared_glossary_zh-CN.csv`），收录跨书共享专名（种族/组织/造物/物理概念/天体/科学人物）；**每本种子术语表 = shared_glossary + 本书高频专名扫描补充**；人名各书独立（不同故事不同角色），跨书同角色（如 Michael Poole 贯穿系列）以 shared 为准；两本书译名冲突时以 shared 为准统一，裁定结果回写 shared。

**回写格式**：`source,target,zh-CN` 追加/覆盖行；source 用英文原词，target 用最终裁定译名。人名策略类裁定（保留英文）可写为注释行或"人名→保留英文"提醒条目。

**术语注释（脚注）**：需解释的译名（缩写/异族语/设定词/易误解词）另建 `glossary_notes.csv`（两列 `source,note`，与术语表同目录）；`build_epub.py` 会自动在术语首次出现处生成 EPUB3 弹出式脚注，缺文件则跳过。

**一致性保障**：glossary 是唯一真源——翻译、统一、校对都对照它；每次回写后，第 5 步对账/第 8 步渲染都以 glossary 为基准校验术语残留。

### 4. 并行翻译

- **开书时写全书梗概**：`<book>_zh/book_summary.txt`，100–200 字（主角、核心设定、主线、当前进度），从简介/首章提炼；**追更时更新进度**。每个批次任务都带上它——子代理是全新上下文，只有梗概能让它知道"这本书讲什么"，减少跨批不一致（专名处理、指代、伏笔）。
- 按**目标字数/批**切片（经验值：每批英文 1.5 万–2 万词，约 8–12 段；用 manifest 词数累计到目标即切一批），每批一个 fixer 子代理；4–9 批并行（注意：子代理不继承 skill，任务文本必须**自包含**：输入 seg 路径、输出 json 路径、格式、术语表路径、铁律、验证命令）。短章（Tech File 等 <500 词）合并进相邻批，长章（>3000 词已拆 2 段）按段计。
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
- 裁定规则优先级：铁律（人名策略）> 术语表 > 出现最多的主流译法 > 该 fandom 通行/官方译名。
- 派一个 fixer 做全局替换：只改 `zh` 字段；人名用边界正则 `(?<![A-Za-z])X(?![A-Za-z])`；**逐处核验 en 上下文**再替换（"黑石"可能对应 blackstone/banestone/其他，别一刀切）；替换后自检残留为 0 + en 未动。
- 已见冲突实例：Perturabo 佩图拉博/英文、黑色图书馆/黑图书馆、Triarch 三圣/三圣议会/三执政团、banestone 禁石/封魔石/祸石、Guts 古茨/英文、Star General 星级将军/星级上将/星将、Night Sentinels 夜哨军团/夜哨卫/夜幕哨兵。

### 7. 内容校对（质量核心）

- 按章范围分 4 批并行（每批 ~18 章），oracle 角色逐段对照 en/zh 精读。**负责范围内每个英文段落都必须检查，不得只抽查开头、结尾或可疑短段**；报告已审/未审段数，但数量只证明覆盖范围，不证明翻译质量。
- 重点：漏译（en 有 zh 无，尤其拆段边界附近）、错译（含义偏差）、无依据增译、人名/术语一致性、数字与专名、引号规范；还必须检查笑话的铺垫—包袱、双关两端、官僚委婉语与真实含义的反差、跨段指代与施受关系、角色语气，以及文化/宗教/谍报/技术典故。不得把源文有意的模糊表达擅自具体化。
- **结构 PASS 不等于内容 PASS**：`en` 逐字节一致、ID/段数齐全、JSON 可解析、构建/EPUB 校验成功，都不能单独作为翻译质量证据。
- 建议先由 oracle 只读审查，再由 fixer 按清单修改。每条 finding 固定包含：严重度、源文件/段落、英文原文、当前译文、问题原因、建议修正、是否需回写术语表；另列明确 PASS 的章节、需编辑裁定的双关、整段漏译数和无依据增译数。
- fixer 只改 zh（en/id/summary 不动），报告实际修改文件。**修复后必须逐条回归 finding 清单**：Main 或独立 oracle 读取实际持久译文，确认旧错误消失、新措辞存在；不得仅采信 fixer 的完成声明。
- 每本书须明确一个译文内容真源（通常为 `json/chN-segM.json`，或项目明确指定的持久 `.zh.txt`）；章级 JSON、memory 引用文件、渲染输入等均由真源再生或做全量一致性比较，禁止手工修改一份而漏同步其他副本。
- 实测修正率：4 批 70 处/78 段（漏译整句 5+ 处、ch37 两段整体错位、the Second 误译"第二原体"实为"第二军团"）。
- 校对发现的跨批遗留问题（如舰名双译）汇总，再做一轮收尾统一；形成的术语裁定按 §3.5 回写 glossary。

### 8. 渲染成品

- 章节标题映射：忠实原作（多线并行的"第 1 周/第 2 周"不加工），专名意译、人名保留英文；两段章合并为一个 `# 标题`。
- `render_otd.py`（或按书写渲染脚本）输出 `md/`（`**EN**`/`**ZH**` 对照）与 `md_zh/`（纯中文）。**章节标题必须用 `# ` 一级标题**——`build_epub.py` 的 `split_sections` 按 `^# ` 分章，用 `## ` 会把全书并成一章（踩过）。
- `build_full.py --md md --out <book>_zh_en.md --title ...` + `build_epub.py`（**推荐用 `uv run --with ebooklib python3 build_epub.py ...` 或项目内 `uv add ebooklib` 后直接跑**——脚本依赖 `ebooklib`/`lxml`，用 uv 管理本地依赖，不绑系统 python）。build_full 头部模板按书内置（Vox/Colossus/OTD 各自版权头部）。**出版书/无原文链接作品（如 Baxter《Raft》）用 `--copyright published`**（内置模板：原著 + 版权归原作者及原出版方 + 非商业声明，可加 `--author` 填原著行）；默认 ch 前缀章会误选 Colossus 头部（硬编码 FFN 链接/Red Flag 作者名），**出版书必须显式指定**。
- `build_epub.py --glossary-notes <file>`：指定术语注释 CSV（两列 `source,note`）；缺省取 md 同目录 `glossary_notes.csv`，无该文件则跳过，epub 无脚注。
- **json 反向生成**：渲染管线（render_md.py / build_full.py / build_structure_epub.py）以 json/chN-segM.json 为唯一输入；若 json/ 为空（翻译批直接产出 md/md_zh），渲染前先从定稿 md/md_zh + segs 反向生成 json（格式：list of {id,en,zh,summary}，含标题段，seg 边界按 segs 切分），生成后做双向逐字节校验——每章 chN-segM.json = [{"id": "chN-segM", "en": "<该 seg 的 EN 段（含 Chapter N 标题段）以 \n\n 连接>", "zh": "<对应 ZH 段（含「# 第N章」标题段）>", "summary": ""}]；seg 边界按 segs/chN_seg0.txt 的非空段数切分；生成后校验 en[1:] == md 的 EN 块、zh[1:] == md_zh 段、seg0+seg1 == segs/chN.txt（字节级）。
- epub 验证：xhtml 数 = 章数 + 2（preface + 结尾），抽查首尾章节标题。

### 9. 提交

- 全部产物（json/segs/glossary/md/epub/raw/manifest）一并 `git add -A && git commit`。
- 提交信息含：书名、章数/段数/字数、关键裁定、校验结论。

## 工具脚本速览（12 个，位于 tools/）

所有脚本用 `python3` 直接跑（无第三方依赖；`build_epub.py` 例外，见 §8）。**核心是 `translate_tool.py`**，其余按阶段调用。

| 脚本 | 用途 | 关键调用 |
|---|---|---|
| `translate_tool.py` | 核心 CLI，8 子命令见下 | 见下 |
| `build_full.py` | 章 md → 全本 md（版权头部按书内置） | `build_full.py --md <book>_zh/md --out <book>_zh_en.md --title ...`；出版书加 `--copyright published`（可配 `--author`） |
| `build_epub.py` | 全本 md → epub（自动生成术语脚注：读 md 同目录 glossary_notes.csv，无则跳过） | `uv run --with ebooklib python3 build_epub.py --md ... --out ... --title ... --author ...` |
| `check_alignment.py` | 对账：检测 en/zh 段落错位 | `check_alignment.py --json-dir <book>_zh/json` |
| `culture_scan_terms.py` | 扫多译名冲突报告 | `culture_scan_terms.py --min-conf 2` |
| `scan_zh_terms.py` | 通用术语冲突扫描 | `scan_zh_terms.py <json_dir> <glossary> --out ...` |
| `culture_unify_terms.py` | 按裁定表批量统一 zh | `culture_unify_terms.py` |
| `culture_render.py` / `render_otd.py` | 按书渲染 md（对照+纯中文） | `python3 render_otd.py` |
| `prep.py` / `colossus_prep.py` | 论坛 HTML / 纯文本 → 分段 | `python3 prep.py` |
| `vote_parser.py` | 投票统计页 → votes.json/md | `vote_parser.py --pages ...` |

### translate_tool.py 子命令与渐进式上下文

**子命令**：`detect`（术语检测）、`extract`（帖子 HTML→正文）、`append`（译文录入记忆）、`summarize`（聚合分层记忆）、`context`（生成接力任务）、`render`（JSON→md）。

**渐进式上下文机制**（Vox 用；OTD 简化版只用术语表+自包含任务）——三份数据配合，解决"子代理是全新上下文、没有全局观"的问题：

```
memory.jsonl  ──append 逐段写入──▶  每段 {id, en_path, zh_path, summary}
     │
     ▼ summarize（按章聚合）
chapters.json ──▶ L0 每章一行 one_line（全篇脉络） + L1 各段摘要
     │
     ▼ context（现场组装）
[全书梗概] book_summary.txt（开书写，追更更新）  ← 书级，L0 补强
[术语表] 当前片段命中的术语（硬约束）
[全篇背景] 最近 3 章 one_line（recent-level）
[本章回顾] 本章各段摘要
[上文] 最近 N 段译文（默认 3，截断 1500 字）
[当前待译片段] 英文原文 + 翻译要求
```

**用法**：
```sh
# 译完一段后录入记忆（--summary 必填，累积成 L0/L1）
translate_tool.py append --id ch12-seg1 --en segs/ch12_seg1.txt --zh json/ch12-seg1.zh.txt --summary "战斗场景" --memory <book>_zh/memory.jsonl
# 一批译完，聚合分层记忆
translate_tool.py summarize --memory <book>_zh/memory.jsonl --out <book>_zh/chapters.json
# 给接力译者生成任务（自带全书梗概+术语+脉络+上文）
translate_tool.py context --segment segs/ch13_seg0.txt --memory <book>_zh/memory.jsonl --chapters <book>_zh/chapters.json --chapter ch13 --glossary <book>_zh/glossary_zh-CN.csv
```

**要点**：
- `append --summary` 是 L0/L1 的原料，**每段必填且 ≤40 字**（质量决定全局观质量）。
- `context` 输出**直接粘贴进翻译批次任务**（或由编排方读取后转发给 fixer）。
- book_summary.txt 与 memory.jsonl 同目录；`context` 自动读取输出 `[全书梗概]`，无文件则跳过（向后兼容）。
- 术语表是硬约束，memory/chapters 是软参考——冲突以术语表为准。

## 任务文本模板（翻译批）

```text
翻译《<书名>》第 <A>–<B> 章，英译中。
【全书梗概】<book>_zh/book_summary.txt 的内容（主角/设定/主线/当前进度）——先读，理解全书再动笔
【输入】otd_zh/segs/chN_segM.txt（先 ls 确认；注意 chK 拆两段）
【输出】otd_zh/json/chN-segM.json（每段一个）
【格式】单元素数组 json：{"id","en","zh","summary"}；en 逐字节复制源文件（含换行）；zh 逐段镜像换行/空行结构
【标题段】segs 首段 "Chapter N" 只作为 md/md_zh 首行「# 第N章」标题，不进入正文段落对（md_zh 非空段数 == segs 非空段数、md 的 EN 块数 == segs 段数 − 1）
【要求】1) 术语表 <book>_zh/glossary_zh-CN.csv 先读全文，表外新词按该 fandom 通行译名+本作惯例新译并报告；
2) 人名一律保留英文不音译；3) 对话用「」，嵌套用『』；4) en 拼写错误照实保留、zh 按正确含义译；5) 简体中文；
6) 遇到缩写/异族语/高概念词/易误解译名，顺手登记进 <book>_zh/glossary_notes.csv（source,note 两列，与术语表同目录）
【验证】python3 -c "…断言 en==源文件、结构正确…" 全部 OK 才算完成
【返回】summary / changes / verification / 每章 zh 字数 / 新造译名清单 / 新增 notes 条目清单 / 不确定处
```

## 已验证的坑（踩过，别重踩）

| 坑 | 对策 |
|---|---|
| 子代理读不到工作区外目录，自行重抓输入 → 分段边界漂移 | 输入 seg 文件先复制进工作区；对账阶段抓 |
| 拆段章只译一段（漏 seg1） | 翻译批次任务里显式点名拆段章；第 5 步全量对账兜底 |
| 术语提示逐批漂移 → 跨批不一致 | 任务模板固定；第 6 步统一轮兜底 |
| API 余额不足整批失败 | 断点恢复：查已交付/缺失，重派缺失段 |
| build_epub 用 `## ` 章节 → 全书并成 1 章 | 渲染一律 `# ` 一级标题 |
| 系统缺 ebooklib/lxml | 用 `uv run --with ebooklib python3 build_epub.py ...`，本地依赖不污染环境 |
| 全书头部硬编码版权信息错误 | build_full 内置每书模板，按输出路径选择 |
| 校对轮改动与并行批次冲突 | 校对按章范围分片，写范围互斥；遗留问题汇总收尾轮 |
| segs 首段 "Chapter N" 被多译成一个正文段 → md_zh 比 segs 多 1 段、md 多一对 EN/ZH，ch1–4 全部偏移（Flux 批1） | 标题段只作 md/md_zh 首行「# 第N章」，不进入正文段落对；任务模板【标题段】显式声明 |
| 结构版 epub 写盘只写 SPINE_FIRST..BODY_FIRST 与 SPINE_LAST，跳过正文后、spine 末尾前的非正文页（Flux part0078 TIMELINE）→ content.opf/toc.ncx 悬空引用，阅读器报错 | 写盘写全 range(BODY_LAST+1, SPINE_LAST+1) 的全部非正文页；生成后核对 opf/ncx 引用的每个 xhtml 真实存在 |

## 验收标准（整本交付）

- 对账：segs == json 数，en 全部逐字节一致，零漏段。
- 术语：全库扫描无残留异译（保留的合理例外逐条说明）。
- 校对：每批有修正清单，漏译/错译清零。
- 成品：`<book>_zh_en.md/.epub`、`<book>_zh_only.md/.epub` 齐全，epub 章数正确，头部版权信息与原著一致。
- git：一次提交包含全部产物。
