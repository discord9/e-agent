---
name: fic2zh
description: Translate English fanfiction / web novels (Warhammer 40K and other fandoms) into Simplified Chinese end-to-end — fetch, segment, parallel-translate, reconcile, unify terminology, proofread, render bilingual/Chinese-only md+epub. Works for SpaceBattles / SufficientVelocity / FanFiction.net and similar forum/serial-fiction sites. Use when asked to translate a fanfic book, catch up a missed chapter, or rebuild a translation artifact.
---

# 中文同人小说翻译流水线（fic2zh）

把英文同人小说端到端翻译为简体中文，产出**中英对照**与**纯中文**两套 Markdown + EPUB。工具脚本在 `docs/skills/fic2zh/tools/`；译文数据在数据仓库各 `<book>_zh/`（本机常见为 `vox_vitae_toolkit/`）。本 skill 管流程与约定，术语表按书另建，不随 skill 分发。

## When to use

用于新书翻译、追更补章、排查漏翻/漏段与术语不一致、重建成品或校对已有译文。不用于零星段落、非小说材料，或向平台发布/上传译文。

## 铁律（不可违反）

1. **人名保留英文，不音译**（Perturabo、Gottfried、Ramirez 等）；新书开译前与用户确认人名/专名策略，并写入 `book_summary.txt` 或项目约定文件。舰名、地名、组织名按术语表或意译。
2. **`en` 字段逐字节等于源文件**，含换行与末尾换行状态；从 seg 文件复制，不得重写。
3. 先抓取并把原文固化到 `<book>_zh/segs/`，翻译前对账，翻译后全量复检。
4. 对话用「」，嵌套用『』；原文拼写错误在 `en` 照实保留，`zh` 按正确含义翻译。
5. 原有脚注、尾注、作者注、编者注、弹窗注释、注号、反向链接和注释标题是正文语义与 EPUB 结构的一部分：完整翻译或按项目约定保留，保持 note↔noteref 锚点、编号、顺序和跳转。只有正文无法自然承载且确有文化、语言、双关或典故理解门槛时，才可添加简短克制的译者注；不得用注释代替翻译、解释显而易见内容、剧透或编造资料。
6. 术语表按书维护，CSV 形状为 `source,target,zh-CN`；同 fandom 可继承旧表，否则扫描高频专名冷启动。
7. 成品命名为 `<book>_zh_en.*`（对照）与 `<book>_zh_only.*`（纯中）。
8. 校对或统一产生的实际术语裁定回写术语表；只写回实际决定，不为未采用的候选造条目。
9. **必须亲自翻译，禁止套娃模型或机器翻译。** 每个翻译单元由执行任务的智能模型逐行阅读并亲自完成。严禁 Argos、本地/更小模型、机器翻译引擎、在线翻译服务、翻译 API、浏览器翻译或生成 `zh` 的脚本，也不得后编辑机器译文冒充亲译。程序只能切段、序列化和机械验证。
10. 权威格式由工作区决定。canonical unit-record JSONL 是唯一英文/中文真相源：每行严格按 `unit_id,en,zh,summary,status,uncertainties` 排列；canonical book JSON 只保存章节/块结构，并由 `build_epub.py --translations` 按 unit ID 精确拼接后渲染。TXT、Markdown、aggregate、context、memory、EPUB 都是派生产物，不得反向覆盖真相源。
11. from scratch 或旧译污染时，不得读取、复制、润色或后编辑旧 `zh`；只能依据 canonical 英文、术语表和批准的上下文重译。
12. 每个 unit 显式返回 `status: final|needs_review` 和 typed string-list `uncertainties`；`needs_review` 当且仅当列表非空。未解决不确定性禁止物化。旧四字段 schema 仅可通过显式 `--schema legacy` 验证，不能冒充 canonical。

## 工作流

### 0. 准备与新站点适配

- 确认工具目录、数据仓库、目标 URL 和章节结构。SB/SV 正文与讨论混在帖子流中，按楼主维护的 **threadmark** 只取指向帖子；FFN/AO3/Webnovel 按独立章节目录抓。
- 新书先确认人名/专名策略、是否首次附英文原名；确认术语表种子，并将策略注入翻译、术语统一和内容 QA。
- 新站点先试抓 1 章，记录内容容器 CSS 类、章末截断标记和 UI 残留，再全量抓。渲染脚本与版权头部按旧书脚本改写，先渲染 1 章验证 `# ` 分章与 EPUB 章数。

### 1. 抓取

- SB/SV 遇 Cloudflare：按序用 **Playwright**；仍被拦时向用户索要登录 cookie（如 `cf_clearance`）；最后用 wayback reader 存档。礼貌限速。不得把 cookie 写入报告或提交。
- 产出 `<book>_raw/chNN.txt`（标题首行、正文以 `\n\n` 分段）和 `manifest.tsv`（章号、post id、标题、词数）。SB/SV 用 threadmark 主类帖子、`bbWrapper` 平衡 div、`<br/>` 转段落，剔除引用/媒体/iframe/spoiler/图片，并从章末 `Authors note.` 截断；正文内嵌档案体文字保留。其他站点按试抓规则提取。
- 验收章数和总词数达到 threadmark/目录预估；抽查无 HTML/UI 残留，章尾自然收束。

### 1.5 输入为 EPUB

用户直接给 EPUB 时不重排、不重建，只在原结构上替换英文正文，保留封面、样式、目录和分章。`mimetype` 必须是第一个 ZIP 条目、`ZIP_STORED`、内容 `application/epub+zip`；`META-INF/container.xml` 指向 OPF，OPF manifest/spine 决定资源与顺序，`nav.xhtml`/`toc.ncx` 标题同步翻译。

1. 先完整备份并解包：
   `python3 -c "import zipfile; zipfile.ZipFile('book.epub').extractall('book_src/')"`
2. 按 spine 顺序用 **lxml** 提取 XHTML 正文，标题首行、正文 `\n\n` 分段，作为 `<book>_raw/chNN.txt`；随后复用 §2–§7。
3. 翻译后用 lxml 只替换文本节点（`element.text`/`.tail`），不动标签、属性、class、href、img src；按镜像段落映射，目录标题同步。
4. 重打包时其余条目 `ZIP_DEFLATED`，可将 `dc:language` 改为 `zh-CN`，其余元数据保留。DRM EPUB 需用户先解密。
5. 解包复核结构、用阅读器或 `ebooklib` 验证章数、样式、封面和正文。写盘须包含 `range(BODY_LAST+1, SPINE_LAST+1)` 的所有非正文 spine 页；核对 OPF/NCX 引用的每个 XHTML 确实存在。

EPUB 的长 XHTML 仍按“一个 XHTML = 一个 seg”处理；可在翻译批次中按字数续译，但段落必须一一对应。图片 alt 与链接文字可保持原文；不要用正则改 HTML。原书 DRM 无法直接处理，先由用户解密。

### 2. 切段与建基准

- 默认每章 1 段；超过 3000 词按段落边界拆分。输出 `<book>_zh/segs/chN_segM.txt`，立即验证切段拼接与整章一致（`rstrip` 比较），并复制原始整章到 segs 作为基准。
- 建立 `<book>_zh/json/` 与 `glossary_zh-CN.csv`。拆段章必须在任务中点名所有 seg；标题段只生成 md/md_zh 的 `# 第N章`，不进入正文段落对。

### 3. 术语表生命周期

1. **种子**：扫描 `\b[A-Z][a-zA-Z]{3,}\b`，裁定势力、种族、舰级、地名、军衔、科技名；优先 fandom 通行/官方译名。人名不建翻译条目，可写“人名→保留英文”提醒。
2. **翻译中收集**：任务报告表外新词及译文，提示固定；不把候选误写成决定。
3. **统一与校对回写**：按“铁律 > 术语表 > 主流译法 > fandom/官方译名”裁定，只改 `zh`，逐处核验 `en` 上下文，再将实际 source→target 决定追加/覆盖到 CSV。
4. **跨书继承**：同系列可维护 `shared_glossary_zh-CN.csv`；每书种子为 shared + 本书扫描，跨书共享专名以 shared 为准，人名仍按书独立处理。

术语提示是硬约束，memory/chapters/context 只是软参考，冲突以 glossary 为准。不要把同义词、尚未裁定的候选或仅出现一次的临时译法批量替换进全库。人名边界替换前逐处查看 `en`，同形词可能指不同 canon 对象。

术语统一完成后扫描全库残留并确认 `en`、ID、顺序和 schema 未改。校对发现的异译、错误专名或人名策略冲突，先作实际裁定，再只回写采用的 `source,target,zh-CN` 行；不因“可能有用”而扩充术语表。

### 3.5 译者注 canonical 合同（唯一位置）

新增注释只能写入卷目录 `translator_notes.jsonl`，每行一个严格 v1 对象，schema 为 `schemas/translator_notes.v1.schema.json`。这是 approval-only 产品选择：每行必须有稳定 `note_id`、canonical `unit_id`、精确 anchor/中文 SHA、`factual_scope`、`render` 及 adjudication/review provenance；没有 workflow `status`，`render:false` 不是候选存储。唯一 note-specific machine approval stage 产出 adjudication/review 两个严格五字段 JSONL artifact；普通 QA ledger 不是这两个 artifact。

先运行：
`python3 docs/skills/fic2zh/tools/validate_translator_notes.py --notes <translator_notes.jsonl> --translations <active-translations.jsonl> --workspace-root <approval-root>`

validator 拒绝 stale anchor、重复或移动 unit、非法文本及不符合机器格式的 provenance；不得跨 unit 搜索或重定位。每条渲染注释在 EPUB 中恰有一个 forward noteref 和 reciprocal backlink，编号由 renderer 按 owning chapter 的 canonical order 分配。`glossary_notes.csv` 仅为迁移期 legacy CSV，不得与 canonical 输入合用，也不得把候选 note 交给 renderer。

`adjudication`/`review` 的 `artifact` 必须是 approval root 下 LF-terminated JSONL；每对象严格只有 `key`, `note_id`, `unit_id`, `decision`, `subject_sha256` 五个字段，key 全文件唯一且精确引用。adjudication 的 decision 为 `CONFIRMED`，review 为 `APPROVE`；二者须匹配 note 的 ID、unit ID 和 subject digest。subject 是 note 的 `schema_version`, `kind`, `note_id`, `unit_id`, `order`, 完整 `anchor`, `note`, `factual_scope`, `render`（不含 provenance），按 `json.dumps(..., ensure_ascii=False, sort_keys=True, separators=(',', ':'))` 的 UTF-8 字节取 SHA-256；artifact 自身也须匹配 provenance SHA。validator 不接受 Markdown/自由文本审批。

路径 root 和各 artifact 组件以 `lstat` 拒绝 symlink；这不覆盖本地并发替换 race，敌对并发写入者需 descriptor-relative no-follow 适配器。结构化 JSON v1 无 protected-span：检测到既有 source noteref/anchor markup 时拒绝。需保留 parser spans 的适配器，先经 validator library API 传入严格 `{unit_id: [[start,end], ...]}`，再使用同一 literal 非重叠 occurrence locator；无 unit ID 的列表项 anchor 仍拒绝。

### 4. 并行翻译

开书写 `<book>_zh/book_summary.txt`（主角、设定、主线、进度，100–200 字），追更时更新。按目标字数切批，短章可合并，长章按段；每个 fixer 任务自包含。Main 先生成 context；不得并行翻译依赖尚未生成的后续 context。余额不足（HTTP 402）时只重派缺失段。

输出单元素数组 JSON：`{"id":"ch1-seg0","en":"<逐字节>","zh":"<镜像换行>","summary":"≤40字","status":"final|needs_review","uncertainties":[]}`。`zh` 镜像 `en` 的段落数、空行和末尾换行。旧 schema 用精确 ID sidecar；有 unresolved 不得 materialize。

### 5. 全量对账

canonical/translation JSONL 由 Main 提供路径，运行 `python3 docs/skills/fic2zh/tools/translate_tool.py validate`；一般 whole-segment JSON 运行 `python3 docs/skills/fic2zh/tools/validate_segment_json.py`。验收 `segs == jsons`、所有 `en` 一致、反向无多余，任何缺失/不一致都阻塞后续。

### 6. 术语统一

按 §3 的术语规则裁定并执行：只改 `zh`，人名用边界正则 `(?<![A-Za-z])X(?![A-Za-z])`，逐处核验 `en` 上下文，替换后确认残留为 0 且 `en` 未动。实际决定回写 glossary；共享 aggregate/EPUB 由 Main 串行重建，禁止并行写入。

### 6.5 批次与上下文执行细则

- 按英文词数切批，通常每批 1.5–2 万词；短章可合并，长章按段计。并行范围必须互斥，不能让两个 Agent 写同一 translation、glossary、aggregate 或 EPUB。
- 每个翻译任务必须自包含：输入 seg/canonical、输出 translation、权威格式、glossary、summary、context、status/uncertainties、机器翻译禁令和验证命令都由 Main 填入；Agent 不自行发现。
- 翻译前将 seg 输入复制到工作区；子代理读不到工作区外目录时不得自行重抓，否则会改变分段边界。抓取、切段、翻译和回填之间以 canonical snapshot 为准。
- `summary` 只作接力记忆，不是英文或中文真源；不得把摘要、memory、context 或 Markdown 当作 canonical 的替代品。摘要普通压缩不算缺陷，只有缺失、错章、错书、编造或会误导后续翻译才报告。
- 继续翻译时先检查已交付 JSON 的精确 ID、行数和 `en`，只补缺失单元；API 余额不足或中断不得重写已通过单元。
- Main 在 QA 前冻结 current-ZH、canonical-EN、ordered-ID 和派生产物快照。snapshot、路径、schema 或权威链变化时，先回到 preflight，不得用旧报告授权新写入。
- 普通 QA 只读产品文件；只有合同的 `WRITE_SCOPE` 与 `REPORT_PATH` 可写。QA 不得顺手统一术语、重生成 context、修正摘要或改 EPUB；这些变化必须成为明确任务。
- 报告只写实际发现。一个 finding 应绑定稳定 key、unit ID、source span、观察到的 EN/ZH、语义证据、决定和必要的 context；没有实质问题的 unit 只保留机械 scope 记录，不写辩护文字。
- 修复只针对批准的 confirmed keys，生成完整 row 以保护同 unit 的 false-positive 与 non-target 字段。禁止 silent scope expansion；新的缺陷要新建明确 key 并重新走裁决/复核。
- 修复后重新读取持久 translation 真源，再比较 canonical EN、current ZH、ID 顺序和派生产物；不能以 fixer 的聊天声明代替回归。共享输出最后由 Main 一次性串行重建。

### 7. 内容 QA（质量核心）

逐 unit 对照 canonical `en` 与当前 `zh`，检查漏译、错译、无依据增译、极性、施受关系、指代、时序、数字、专名/术语、对话声线、文化/专业事实、笑话铺垫与包袱、双关、反讽、缩写、标题和注释。结构/物化检查与翻译质量判断分开；准确的 ID/行覆盖是机械边界，不是质量证明。

**Oracle 只在明确的语义、自然度、人物声线、正典/专名或事实错误时阻塞；多种合理译法默认 PASS，不因个人偏好或“还能润色”要求重写。无法证明有实质缺陷时 PASS；只有证据确实不足且会改变含义/事实时才 UNRESOLVED。** Review 优化读者面对的质量，不优化 artifact 数量。缺陷必须有 source span、当前译文、证据、原因和决定；clean unit 不需 prose justification。

第一轮按精确 ID/行 exhaustive 覆盖。第二轮只在缺陷集中、context repair、风险传播或用户要求时启动，并按风险大小取样；不得设最低百分比或跨章节配额。普通 QA 不重建历史 context；若本任务确实修复 context、摘要、memory 或工具，只验证受影响 segment。

QA 要把普通语义错误与高风险语义分开核对：漏译、增译、极性、施受关系、指代、时序、数字、专名/术语；习语与多义、双关与典故、反讽、缩写、标题/词源、文化与专业事实。源文有意含糊时不得擅自具体化。原有注释必须完整，确有必要但缺失的译者注属于 editorial/suspect，不得自行发明渲染机制。

QA 发现实际缺陷后由 editor/fixer 按批准范围修改；只改 `zh`，保护 `en`/ID/summary 和 mixed false-positive 内容。修复后 Main 或独立 Oracle 读取持久文件逐条回归；术语实际裁定回写 §3。专门 summary/context QA 一卷一个 Agent，检查错书、错章、陈旧、空缺、未来信息泄漏和严重失真。

### 8. 渲染成品

标题忠实原作，多线标题不加工，专名按表、人名保留英文；两段章合并为一个 `# 标题`。`docs/skills/fic2zh/tools/render_otd.py`（或书内脚本）输出 `md/` 与 `md_zh/`；`python3 docs/skills/fic2zh/tools/build_full.py --md md --out <book>_zh_en.md --title ...` 生成全书，出版书显式加 `--copyright published`（可配 `--author`）。EPUB 推荐：
`uv run --with ebooklib python3 docs/skills/fic2zh/tools/build_epub.py --json ... --translator-notes ... --approval-root ... --out ...`

canonical book JSON 是章节/块结构输入，canonical unit-record JSONL 是翻译输入；使用 `build_epub.py --json book.json --translations translations.jsonl`，脚本按块中的 `unit_id` 精确替换 `en` 并按 canonical 顺序 join，拒绝缺失、多余、乱序或非 `final` unit。Markdown 是另一种独立的 legacy 输入格式，不与结构化 canonical 混称；不要把 Markdown 段落直接当作 canonical records。EPUB 验证 XHTML 数、章节标题、样式、封面、OPF/nav/NCX 引用；`# ` 必须用于分章，`## ` 会把全书并为一章。EPUB 译者注用 `--translator-notes ... --approval-root ...` 经 canonical v1 validator；Markdown 不物化译者注，anchor 过期、list-item anchor 或 legacy CSV 与 canonical 混用都拒绝。

### 9. 提交

仅用户明确要求时 `git add -A && git commit` 或 push；否则不提交。若要求提交，信息含书名、章/段数、字数、关键裁定和校验结论。

## 工具脚本速览

所有脚本通常用 `python3`（无第三方依赖）；`build_epub.py` 例外。核心 CLI `docs/skills/fic2zh/tools/translate_tool.py` 子命令为 `detect`、`extract`、`append`、`summarize`、`context`、`validate`、`render`。

| 脚本 | 用途与关键调用 |
|---|---|
| `docs/skills/fic2zh/tools/translate_tool.py` | 核心 CLI；canonical/translation JSONL 用 `python3 docs/skills/fic2zh/tools/translate_tool.py validate` |
| `docs/skills/fic2zh/tools/build_full.py` | 章 md → 全本 md：`python3 docs/skills/fic2zh/tools/build_full.py --md <book>_zh/md --out <book>_zh_en.md --title ...` |
| `docs/skills/fic2zh/tools/build_epub.py` | 结构化 JSON → EPUB；canonical v1 注释须先 validator，legacy CSV 仅迁移 |
| `docs/skills/fic2zh/tools/check_alignment.py` | legacy 对账：`python3 docs/skills/fic2zh/tools/check_alignment.py --json-dir <book>_zh/json` |
| `docs/skills/fic2zh/tools/validate_segment_json.py` | `python3 docs/skills/fic2zh/tools/validate_segment_json.py --source <segment.txt> --translation <segment.json> [--expected-id <ID>]` |
| `docs/skills/fic2zh/tools/culture_scan_terms.py` / `docs/skills/fic2zh/tools/scan_zh_terms.py` | 扫描术语冲突 |
| `docs/skills/fic2zh/tools/culture_unify_terms.py` | 按裁定表统一 `zh` |
| `docs/skills/fic2zh/tools/culture_render.py` / `docs/skills/fic2zh/tools/render_otd.py` | 渲染对照与纯中文 md |
| `docs/skills/fic2zh/tools/prep.py` / `docs/skills/fic2zh/tools/colossus_prep.py` | 论坛 HTML / 纯文本切段 |
| `docs/skills/fic2zh/tools/vote_parser.py` | 投票页 → votes.json/md |

## 任务合同与固定资料包

### Main preflight（每卷一次，任务复用）

Main 必须先轻量读取并把下列字段填入合同；不得让 SubAgent 用 `find`/`grep` 猜工具、真相源、章节键或文件名。资料包只有首次建立或工作区真实变化时重建。全局报告规则：`REPORT_PATH` 是该任务唯一报告路径；先写完整报告，再返回其 SHA 与摘要，聊天摘要永远不具权威性，Main 不得转录重建：

```text
VOLUME_ROOT
CANONICAL_FILES：全部 canonical JSON/JSONL 的精确路径和顺序
TRANSLATION_FILES：对应 translation JSON/JSONL 的精确路径和顺序
UNIT_SET：章节/segment 键、预期行数、首尾 ID、分段计数
AUTHORITY_CHAIN：canonical → translation → derived artifacts → EPUB
BOOK_SUMMARY / GLOSSARY / MEMORY / CHAPTERS：精确路径；不存在写 NONE
HISTORICAL_CONTEXT_FILES：翻译时保存的 context 路径；不存在写 NONE
CONTEXT_TOOL：Main 用 --help 确认的精确路径；不兼容写 NONE
CONTEXT_REPAIR_VERIFICATION_COMMANDS：普通 QA 为 NOT_NEEDED，否则填精确命令
MATERIALIZATION_FILES：精确路径
RENDERED_EPUB：路径及章节 XHTML/nav/NCX 目标
WRITE_SCOPE：允许写入的精确文件/字段；其余产品文件只读
EXCLUSIONS：明确不读取、不修改或不重建的路径/产物
REPORT_PATH：唯一报告路径（完整报告先写入此处，再返回 SHA）
VERIFICATION_COMMANDS：完整命令（Python 按项目要求指定缓存，如 UV_CACHE_DIR="$PWD/.uv-cache"）
```

也记录 `DERIVED_INDEX`、`HISTORICAL_CONTEXT_INDEX`、`CONTEXT_REPAIR_VERIFICATION_TEMPLATE`、`RENDER_TOOL`/`BUILD_TOOL`/`VALIDATION_TOOLS`、`EPUB_PATH + OPF/nav/NCX/chapter member 映射`、项目环境命令。资料包缺失或矛盾时返回 `TASK_PACKET_INCOMPLETE: <fields>`，不得自行探索；执行 Agent 禁止 broad `find`、目录遍历、读工具源码、运行 `--help` 或参考邻卷。

**资料包使用规则**：Main 只在资料包首次建立或工作区真实变化时重新发现；后续任务只填章节键、segment 列表、行数/首尾 ID、相邻文件、报告路径、抽样规则或 finding IDs。输入/输出、权威链和 exclusions 必须逐字写入任务合同，不得以文件名猜测真相源。canonical 的 exact join 是唯一英文证明，normalized EN、空值检查、渲染成功或 glossary 命中都不能代替它。

**Context 规则**：翻译前由 Main 用当前 glossary/memory/chapters 生成一次 context，译者只读合同指定文件；普通章节 QA 读取翻译时保存的历史 context，并直接核对当前 summary、glossary、memory/chapters、相邻 canonical/translation，不重新生成。专门 summary/context QA 一卷一个 Agent。只有任务确实修改 context 工具、摘要、memory 或历史 context 时，才对受影响 segment 使用合同中的 repair 命令；历史 context 不得被临时输出覆盖。

memory 保存 `{id, en_path, zh_path, summary}`；summarize 按章生成 `chapters.json`（L0 `one_line` + L1 段摘要），context 现场合并 book summary、命中术语、最近章节、章内回顾、最近译文和当前英文。术语表是硬约束，memory/chapters 是软参考。适用时 Main 预先填入并复用：
```sh
python3 docs/skills/fic2zh/tools/translate_tool.py append --id ch12-seg1 --en segs/ch12_seg1.txt --zh json/ch12-seg1.zh.txt --summary "战斗场景" --memory <book>_zh/memory.jsonl
python3 docs/skills/fic2zh/tools/translate_tool.py summarize --memory <book>_zh/memory.jsonl --out <book>_zh/chapters.json
python3 docs/skills/fic2zh/tools/translate_tool.py context --segment segs/ch13_seg0.txt --memory <book>_zh/memory.jsonl --chapters <book>_zh/chapters.json --chapter ch13 --glossary <book>_zh/glossary_zh-CN.csv
```
`append --summary` 必填且 ≤40 字；Main 复验翻译结果后才运行后两步，不得并行翻译依赖尚未生成的 context。

**报告内容规则**：报告必须列出精确 scope 的 ID/行、实际 finding keys、unresolved 和可复现 evidence；发现项至少含 source span、observed/current text、原因、confidence、decision。检测器不得写建议修复；Oracle 不写 replacement Chinese。clean rows 不写理由，不要求额外证明或配额。

### 翻译批合同（合并模板 A）

```text
Goal: 按 fic2zh 顺序流程亲自翻译 <VOLUME>/<SEGMENT>。
VOLUME_ROOT: <exact>
CANONICAL_FILES: <exact ordered paths>
TRANSLATION_FILES: <exact output paths>
UNIT_SET: <count, first/last IDs>
AUTHORITY_CHAIN: <exact>
BOOK_SUMMARY: <exact or NONE>
GLOSSARY: <exact>
MEMORY: <exact>
CHAPTERS: <exact>
GENERATED_CONTEXT: <Main 已运行 context 工具生成的精确路径>
CANON_OVERRIDES: <explicit list or NONE>
INPUT: <exact seg/canonical paths；先 ls 确认，点名所有拆段>
OUTPUT: <exact translation paths；每段一个>
FORMAT: 单元素数组 `{"id","en","zh","summary","status":"final|needs_review","uncertainties":[]}`；`en` 逐字节复制，`zh` 镜像段落/空行/末尾换行
TITLE_SEGMENT: 首段 `Chapter N` 只生成 md/md_zh 的 `# 第N章`，不进入正文段落对

先读 GENERATED_CONTEXT、canonical、全书梗概和 glossary；当前智能模型亲自逐单元翻译。严禁 Argos、本地/更小模型、MT 引擎/API/服务/脚本生成 zh，严禁参考污染旧译。只写 TRANSLATION_FILES；保持 ID/顺序/schema/en byte-exact。
REQUIREMENTS: 术语表先读全文并报告表外新词；人名保留英文；对话用「」/『』；en 拼写错误照实保留、zh 按正确含义译；原有注释保持内容、顺序、锚点和反向链接；确需译者注才登记 canonical v1；严格使用指定权威格式；from scratch 不读旧 zh；每 unit 必须返回 status/uncertainties，不确定就 needs_review。

完成后人工逐单元 EN↔ZH 复读，再运行：
<exact validation commands>
RETURN: summary / changes / verification / 每章 zh 字数 / 新造译名清单 / 新增 notes 条目清单 / 完整 uncertainties（逐 unit，含 category/source_span/competing_readings/chosen_provisional_wording/evidence/confidence/needs_review）/ 每 unit 的 status
```

验证边界固定：canonical/translation JSONL 用
`python3 docs/skills/fic2zh/tools/translate_tool.py validate --source <canonical.jsonl> --translation <translation.jsonl> --expected-count <N> --first-id <首个ID> --last-id <末个ID>`；whole-segment 单元素数组用
`python3 docs/skills/fic2zh/tools/validate_segment_json.py --source <segment.txt> --translation <segment.json> [--expected-id <ID>]`。不得拿 chapter-level `check_alignment.py` 或 JSONL validator 混用。全部 PASS 且人工逐单元复读后才算完成。Main 复验后运行预先写好的 memory append、chapters summarize、下一段 context 命令。

## 正式 QA 协议、状态机与 artifact schema

本节是唯一的五项 verdict 定义、Stage 0–8 及其 artifact 合同。适用现有 schema：能加字段就用 native fields，不能改旧 schema 就用 sidecar ledger；不新增工具或 schema。结构/物化 evidence 与 translation-quality judgment 分开。

**五项 verdict**：`MATERIALIZATION_VERDICT`（真相连接、写入 guard、derived/build/EPUB 物化）；`TRANSLATION_FIDELITY_VERDICT`（语义、事实、忠实度）；`NATURALNESS_VOICE_VERDICT`（自然度、人物声线、register）；`CULTURAL_PROFESSIONAL_VERDICT`（文化、双关、专业事实和正典/专名）；`NOTES_VERDICT`（原有注释及必要译者注的内容与锚点）。每项为 `PASS|FAIL|NOT_APPLICABLE`；整本必须所有适用项 PASS 且无 unresolved。前一项 structural/materialization PASS 不得贡献或暗示后三类质量 verdict；`TRANSLATION_QUALITY_PASS` 只由后四项适用 verdict 推导，`MATERIALIZATION_PASS` 只由物化/结构证据推导。

### QA 任务合同（合并模板 B）

```text
Goal: exhaustive QA <VOLUME>/<CHAPTER>；逐 unit 语义/canon QA，并按需审历史 context；普通章节 QA 不重新生成 context。
VOLUME_ROOT: <exact>
CANONICAL_FILES: <exact ordered paths>
TRANSLATION_FILES: <exact ordered paths>
UNIT_SET: <segment counts, total, first/last IDs>
AUTHORITY_CHAIN: <exact>
BOOK_SUMMARY: <exact or NONE>
GLOSSARY: <exact>
MEMORY: <exact or NONE>
CHAPTERS: <exact or NONE>
HISTORICAL_CONTEXT_FILES: <exact ordered paths or NONE>
CONTEXT_TOOL: <exact path or NONE; ordinary chapter QA does not run it>
CONTEXT_REPAIR_VERIFICATION_COMMANDS: <NOT_NEEDED for ordinary QA; exact commands only for repair>
ADJACENT_CANONICAL_FILES: <previous/next exact paths>
MATERIALIZATION_FILES: <exact paths>
RENDERED_EPUB: <exact path and chapter member/nav targets>
WRITE_SCOPE: <exact files/fields allowed to write>
EXCLUSIONS: <paths/artifacts not to read, modify, or rebuild>
REPORT_PATH: <sole allowed report path>
VERIFICATION_COMMANDS: <complete commands>

Phase 1: 冻结并核对 exact ID/order/schema/en、相邻 canonical/current translation。
Phase 2: 读取历史 context，并将 glossary、book summary、memory/chapters、相邻文件与当前真源核对；坏 context 不能替翻译免责。
Phase 3: 逐 unit 比较 EN↔ZH，记录实际 findings，并按本节五项 verdict 分开判断；clean rows 不写理由。按需读取 notes、EPUB 和 render evidence。
Phase 4: 检查 translation truth → TXT/aggregate → XHTML、navigation、notes/noteref/backlinks、anchors、CRC/XML；不得把这些结构证据混入翻译质量。

Path constraints: 按 WRITE_SCOPE 执行，其他 product files read-only；仅 REPORT_PATH 可写。
Return: exact scope、actual findings、unresolved、五项 verdict、历史 context 状态、artifact/build 结果和报告 SHA。
```

### Stage 0–8 职责与状态

0. **冻结输入**：冻结 canonical/current snapshot、资料包、路径、ID 顺序和 SHA；变化即回到 0。
1. **翻译**：逐 unit 写 translation 与 `status`/`uncertainties`；`needs_review` 或 unresolved 禁止物化。
2. **异常检测**：只读，只检测语义/文化 anomaly；不修复、改写、替换、裁决或把 provisional wording 当结论。
3. **Oracle**：只处理精确 nominated points，裁为 `CONFIRMED`、`FALSE-POSITIVE` 或 `UNRESOLVED`，提供 semantic evidence、理由和 note 判断，不提供 replacement Chinese。遵循本节开头的 Oracle 门槛：合理译法默认 PASS，无法证明实质缺陷就 PASS；证据不足且会改变含义/事实才 UNRESOLVED。
4. **中文编辑**：只针对 `CONFIRMED` 生成最小 full-row proposal，保护 mixed false positives 与 non-target 内容，不重译 clean rows、不扩大范围。
5. **独立复核**：只读检查 fidelity、自然中文、voice/register、terminology、culture/professional facts、collateral drift 和 notes；`CHANGES_REQUIRED` 只能回到 editor，`PASS` 不是写权限。
6. **持久化批准**：persistence-only Fixer 将 reviewer `PASS`、reviewed proposal SHA 和 counts 写入 standalone approval，不判断、不改 proposal、不代替 reviewer。
7. **产品物化**：product Fixer 仅在 preflight-before-write 后机械应用；验证 proposal/approval SHA、standalone `PASS`、current-ZH exact match、canonical EN joins、ordered IDs 和写前 snapshots。任一 guard failure 必须 zero writes；写后证明 exact target-only delta，再同步 derived、build/validation 和 cleanup。
8. **主验收**：Main 独立读取实际文件，重建/验证 aggregate、EPUB、omnibus；共享 derived outputs 串行写入。只有 `MATERIALIZATION_PASS` 与 `TRANSLATION_QUALITY_PASS` 同时成立才完成。

允许主路径 `0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8`。缺失、冲突或 `needs_review` 阻塞物化；`3 UNRESOLVED` 补证据后回到 `0/2 → 3`，不得先进 editor；`FALSE-POSITIVE` no-op；`CONFIRMED` 必须 editor edit + reviewer PASS。`CHANGES_REQUIRED` 仅 `5 → 4`；新 finding 必须以明确 key 重新进入，不得 silent scope expansion。false-only unit 不进 proposal；混合 unit 保留 false-positive 内容。

### 固定 artifact schema

- **AnomalyReport**：`{schema_version,snapshot_sha,canonical_sha,translation_sha,unit_ids:[ordered exact IDs],ledger:[{unit_id,outcome:"ANOMALY|CLEAN",finding_keys:[exact keys]}],findings:[{key,unit_id,source_span,category,observed_text,current_zh,competing_readings,evidence,confidence,needs_review}],report_sha}`。`unit_ids` 与 ledger ID 按序一一 exhaustive；每 unit 恰有一个 outcome；ANOMALY keys 与 findings exact 相等且各出现一次；CLEAN findings 为空；findings 引用 ANOMALY unit。detector 不写建议修复。
- **Adjudication**：`{schema_version,snapshot_sha,anomaly_report_sha,nominated_keys,decisions:{CONFIRMED:[exact keys],FALSE-POSITIVE:[exact keys],UNRESOLVED:[exact keys]},entries:[{key,semantic_evidence,decision,reason,note_required,note_reason}],report_sha}`。三数组对 nominated keys exact partition；entries 一一对应且不含 replacement Chinese。
- **EditorProposal**：`{schema_version,base_snapshot_sha,adjudication_sha,proposal_sha,units:[{unit_id,current_zh_sha,proposed_full_row,confirmed_keys,false_positive_keys_preserved,protected_non_target,uncertainties,notes}],counts}`。每 unit 是完整 row，只列 confirmed 修改，保留 false keys/non-target，并报告 uncertainties/notes；false-only 不列入 proposal。
- **Approval**：`{schema_version,standalone_decision:"PASS",reviewer_id,reviewed_proposal_sha,reviewed_snapshot_sha,unit_count,confirmed_count,false_positive_count,report_sha}`。独立自足，不能只引用聊天或 Markdown rationale；没有 standalone `PASS` 不得授权 Fixer。
- **ProductTransactionProof**：`{schema_version,preflight:{proposal_sha,approval_sha,pass,proposal_matches_approval,current_zh_exact,canonical_en_joins,ordered_ids,prewrite_snapshot_shas},writes,postwrite:{target_only_delta,changed_ids,derived_sync,build_validation,cleanup},proof_sha}`。证明 guards、写前快照、approved target rows、目标外 byte-identical，并记录实际命令和结果。

注释规则见 §3.5 canonical 合同：正文可读性优先，不重复正文，不把未经证实的 folklore/history 认证为事实，不剧透；必须挂 first-occurrence unit，用现有 renderer support 并有 forward/back links。renderer 不支持时为 `UNRESOLVED`，不得发明机制。任何 unresolved、缺 evidence、缺 note review 或无法 exact join 都禁止 final materialization。

## 已验证的坑

| 坑 | 对策 |
|---|---|
| 子代理读不到工作区外目录，自行重抓导致边界漂移 | 输入 seg 先复制进工作区；对账阶段抓 |
| 拆段章漏 seg1 | 任务点名所有 seg；第 5 步对账 |
| API 余额不足整批失败 | 查已交付/缺失，只补缺失段 |
| `## ` 章节导致 EPUB 并成一章 | 一律用 `# ` |
| 缺 ebooklib/lxml | `uv run --with ebooklib python3 docs/skills/fic2zh/tools/build_epub.py ...` |
| 校对与并行批次冲突 | 按章互斥写；Main 串行收尾 |
| 共享 aggregate/EPUB race | Main 串行重建 derived |
| EPUB 非正文 spine 页漏写 | 写全 `range(BODY_LAST+1, SPINE_LAST+1)`；核对 OPF/NCX 引用 |

## 验收标准（整本交付）

- §5 对账通过：segs 与 JSON 数量相等、`en` 逐字节一致、零漏段。
- §3 术语规则执行，实际裁定已回写，未采用候选不进入真源；§3.5 注释合同及 validator 通过。
- §5 对账、§7 scope 内 QA 和 Oracle 质量门槛通过；缺陷已修复或明确 unresolved，`needs_review`/`UNRESOLVED` 不物化。
- §8 渲染、EPUB parser-preserving、对照/纯中 Markdown + EPUB、章数、版权头部、OPF/nav/NCX 与样式检查通过。
- 正式 QA 的五项 verdict、standalone approval、proposal/approval SHA、current-ZH/canonical-EN/ordered-ID preflight、exact target-only delta、derived sync、build/validation 和 transaction proof 齐全；产品 Fixer 只改批准 target rows，目标外 byte-identical。
- 仅用户明确要求时 commit/push，否则不提交。
