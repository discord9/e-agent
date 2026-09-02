---
name: fic2zh
description: Translate English fanfiction and web novels into Simplified Chinese through a continuous literary editorial workflow: prepare a canonical manuscript, translate and revise by chapter/scene, render standalone reader outputs, read the book continuously, resolve concise editorial queries, and proof the released EPUB/Markdown. Use for a book, volume, catch-up chapter, or rebuild of an existing translation artifact.
---

# 中文同人小说文学翻译与编辑流程（fic2zh）

本 Skill 管理从英文小说到简体中文成品的编辑流程。目标是每卷独立可读的**独立 EPUB**、干净纯中文 Markdown，以及**中文在前、英文在后的双语 Markdown**；必要时再把已完成的独立 EPUB 打包为 omnibus。工具脚本在 `docs/skills/fic2zh/tools/`，书籍数据在数据仓库相应的 `<book>_zh/` 中。术语表、人物表和查询表按书/卷维护，不随 Skill 分发。

“质量”首先来自从头到尾的连续阅读，而不是异常数量、覆盖率或审计表。脚本只证明可机械证明的完整性；读者反应是发现证据；任何完成声明都必须说明实际读过什么、运行了什么检查，以及证据没有证明什么。

## When to use

用于新书或新卷、追更补章、已有 canonical 翻译的修订、重建独立成品和最终校对。不用于零星句子、非小说材料，或向平台发布/上传译文。

## 铁律与权威边界

1. **人名默认保留英文，不音译**（如 Perturabo、Gottfried、Ramirez）；开译前确认人名、专名、首次出现是否附英文原名，并写入已有的书籍约定或 `book_summary.txt`。舰名、地名、组织、军衔等按已裁定术语处理。
2. **源文不可改。** canonical `en` 必须与固化源文件逐字节一致，包括换行和末尾换行。原文拼写错误在 `en` 照录，按语义在 `zh` 中正确翻译。
3. 对话使用「」，嵌套对话使用『』。译文应是自然的现代简体中文，但不得用中文顺滑掩盖信息、语气、指代或叙事视角的损失。
4. 原有脚注、尾注、作者注、编者注、弹窗注释、注号、反向链接和注释标题属于源文内容及 EPUB 结构：必须保留其内容、顺序、编号和 note↔noteref 跳转关系。译者注只在正文确实无法自然承载且有文化、语言、双关或典故门槛时使用；不得用注释代替翻译、解释显而易见内容、剧透或编造资料。
5. 术语表按书维护，CSV 既有格式为 `source,target,zh-CN`；实际采用的裁定才回写。候选、同义词和临时译法不得批量写成已定术语。人物可在既有表中记录“保留英文”的规则，而不是制造译名。
6. 必须亲自翻译和修订。禁止 Argos、机器翻译引擎/API/服务、浏览器翻译、本地或更小模型生成 `zh`，也不得把机器译文后编辑后冒充亲译。程序只能抓取、切段、序列化、渲染和执行机械检查。
7. 不新增 monolithic 格式或框架。继续使用项目已有的 mapping、translations、canonical structural slot data、source-footnote records 和 canonical translator-note records；这五类记录共同组成一个**逻辑 canonical manuscript package**，是单一真相源。它们可以继续分文件、使用现有 schema，不能被合并成新格式。
8. 任何 Markdown、EPUB、aggregate、memory、context、读者报告或 omnibus 都是输出/参考，不得反向覆盖 canonical package，也不能成为下一轮写入的权威。canonical package 的 mapping 必须精确连接 slot、source、translation、footnote 和 approved translator note。
9. 结构化 unit record 继续使用工作区已有 schema；通常要求 `unit_id`、精确 `en`、`zh`、摘要/状态和 `uncertainties`。有未解决不确定性时保持既有 `needs_review` 语义，不物化为最终成品。旧格式只能按工具明确要求的 legacy 模式验证，不能冒充 canonical。
10. 任何修订都先冻结要读的 canonical package、当前 translations、顺序和输出快照；按明确的写入范围修改，并在写后重新从持久 canonical records 渲染和验证。不得用聊天摘要、哈希或 fixer 声明替代实际文件回读。

## 工作流

### 1. 源文、canonical manuscript 与编辑原则

先确认数据仓库、书/卷边界、目标 URL 或输入 EPUB、章节顺序、已有 canonical 文件及其 mapping。论坛正文与讨论混在帖子流中时，SpaceBattles/SufficientVelocity 按作者 threadmark 取目标帖子；FFN/AO3/Webnovel 等按独立章节目录取文。新站点先试抓一章，记录内容容器、截断标记和 UI 残留，再全量抓取；礼貌限速，Cloudflare 依次使用 Playwright、用户提供的登录 cookie、最后才用 Wayback reader，cookie 不写入报告或提交。

固化 `<book>_raw/chNN.txt`（标题首行、正文以 `\n\n` 分段）和 `manifest.tsv`（章号、post id、标题、词数）。剔除引用、媒体、iframe、spoiler 和 UI；正文内嵌档案体保留；按约定截断章末 Authors note。验收章数、目录/threadmark、词数、章尾和 HTML/UI 残留。

用户提供 EPUB 时先备份并解包，不随意重排：

```sh
python3 -c "import zipfile; zipfile.ZipFile('book.epub').extractall('book_src/')"
```

按 OPF spine 顺序用 lxml 提取 XHTML 正文为已有的源/segment 记录；不动标签、属性、class、href 或 img src。重打包须保持 `mimetype` 为第一个 ZIP 条目、`ZIP_STORED`、内容为 `application/epub+zip`；保留封面、样式、目录、分章和所有非正文 spine 页，核对 OPF、nav、NCX 引用。DRM 由用户先解密。不要用正则改 HTML。

在正式翻译前，主代理和译者共同建立一页**编辑简报**：

- 目标读者、体裁和本卷的阅读边界；
- 从源文抽取的代表性段落/场景 sample；
- 叙述视角、时态、句长、对话 register、人物声线、幽默/讽刺/粗俗程度和专名策略；
- 哪些含糊必须保留，哪些文化信息可在正文自然化处理，何时才需要译者注；
- 本卷已知的正典、人物关系、时间线和不可擅自解释的查询。

sample 和原则是全程可回看的编辑基准，不是替代逐章阅读的风格模板。源文改变、读者证据或编辑裁定改变原则时，记录变更并从受影响的场景重新检查。

### 2. 连续翻译与工作记录

先切段并建立现有的 source segments/canonical unit records；默认一章一个连续工作单元，超过约 3000 词时只在段落或场景边界切分。切段拼接须与整章相符；拆段任务点名所有 segment，不能遗漏 seg0/seg1。标题按现有 renderer 约定作为 `#` 章节标题，不把标题伪装成正文段落。

译者按章节和场景顺序连续阅读、翻译，不以孤立异常为主要工作单元。每次任务合同应明确：卷和章节、精确输入路径、精确输出路径、canonical authority chain、编辑简报、glossary、相邻场景/已有 context、允许写入范围和验证命令。多个译者可以处理不重叠的章节，但不得并行写同一 translation、glossary、notes、aggregate 或 EPUB；短章合并时也必须保持阅读顺序。不得为追求并行或表格覆盖率打断本应连续的场景。

翻译中同步维护已有的四类工作记录：

- **glossary**：实际裁定的术语、译法和适用范围；逐处核对同形词；
- **character/voice record**：人物关系、称谓、代词、声线和本卷有效的身份信息；
- **query log**：真正影响含义、正典或成品的疑问，注明 source span、当前读法和所需决定；
- **translator-note register**：仅登记可能需要译者注的具体 first occurrence，不直接把候选渲染进成品。

memory、summary、chapters/context 只作连续阅读的接力材料，不是英文或中文真源。新词报告不等于术语裁定；只有实际决定才写回 glossary。若使用现有工具，例：

```sh
python3 docs/skills/fic2zh/tools/translate_tool.py append --id ch12-seg1 --en segs/ch12_seg1.txt --zh json/ch12-seg1.zh.txt --summary "战斗场景" --memory <book>_zh/memory.jsonl
python3 docs/skills/fic2zh/tools/translate_tool.py summarize --memory <book>_zh/memory.jsonl --out <book>_zh/chapters.json
python3 docs/skills/fic2zh/tools/translate_tool.py context --segment segs/ch13_seg0.txt --memory <book>_zh/memory.jsonl --chapters <book>_zh/chapters.json --chapter ch13 --glossary <book>_zh/glossary_zh-CN.csv
```

只有在译文复读后才更新这些接力记录；历史 context 不得被临时输出覆盖。中断或余额不足时只补缺失 unit，不重写已通过 unit。

### 3. 译者目标语自修订

每章/场景翻译完，译者脱离逐句生成状态，以中文读者视角完整重读自己的目标语，再回看编辑简报和相邻场景。修订重点是中文句法、节奏、叙述距离、人物声线、称谓一致、动作主体、段落转场、对话标点和可读性；同时确认没有为求自然而删掉、补出或具体化源文没有的信息。

修订者重新检查段落/场景的开合和上下文，而非只找关键词。确需改动时写回 translation canonical record，保护精确 `en`、unit ID、slot mapping、summary/status 及 source-footnote 记录；术语实际裁定回写 glossary。自修订不等于 QA 通过，也不替代后面的双语连续编辑阅读。

### 4. 确定性渲染独立成品

只从冻结的逻辑 canonical manuscript package 确定性渲染，不从上一次 EPUB/Markdown 回读写入。结构化 book JSON 继续提供章节/块/slot 顺序，unit-record JSONL 提供精确英中记录；mapping 精确 join 后才可生成成品。保留 source footnotes 的原始记录和结构；canonical translator notes 由既有审批/validator 机制控制，不能以 legacy CSV 混用。

现有工具示例：

```sh
# 如项目的既有 renderer 需要先生成章节 Markdown（核验其中文优先顺序）
python3 docs/skills/fic2zh/tools/render_otd.py ...

# 将已按“中文在前、英文在后”生成的章节 Markdown 拼成全书（build_full 保留输入顺序）
python3 docs/skills/fic2zh/tools/build_full.py --md <book>_zh/md_zh_first --out <book>_zh_en.md --title ...

# 结构化 canonical package → standalone EPUB
uv run --with ebooklib python3 docs/skills/fic2zh/tools/build_epub.py \
  --json <book>.json --translations <book>_zh/translations.jsonl \
  --translator-notes <book>_zh/translator_notes.jsonl \
  --approval-root <book>_zh/approval --out <book>_zh/<book>_zh_only.epub
```

同时渲染干净纯中文 Markdown（例如 `<book>_zh_only.md`）和中文优先双语 Markdown（例如 `<book>_zh_en.md`）。最终成品命名沿用 `<book>_zh_only.*` 与 `<book>_zh_en.*`。EPUB 的 `#` 分章、XHTML 数、章节标题、样式、封面、OPF/nav/NCX、脚注锚点、note↔noteref 往返链接和所有 spine 成员都要验证；`##` 不得误作全书章节分隔。

`tools/export_reader_markdown.py` 是读者导出所需的演进中实现：工具尚未具备全卷/所有 EPUB 变体支持时，不得声称已支持所有卷，须注明适用范围或阻塞发布。实现/使用接口应包含中文和双语模式，例如：

```sh
python3 docs/skills/fic2zh/tools/export_reader_markdown.py \
  --epub <book>_zh/<book>_zh_only.epub --mode zh \
  --out <book>_zh/reader/<book>_zh_only.md
python3 docs/skills/fic2zh/tools/export_reader_markdown.py \
  --epub <book>_zh/<book>_zh_only.epub --mode bilingual \
  --out <book>_zh/reader/<book>_zh_en.md
```

命令参数以实际工具为准；没有脚本或验证能力时写“evolving implementation requirement”，不把示例误报为已经运行。

### 5. 独立中文盲读：只收反应

每卷用当前**独立 standalone EPUB** 导出干净纯中文 Markdown，再交给没有英文、术语表、历史 QA、其他卷、网页或 canonical 文件的首读者。首读者把本卷当作第一次接触的书，按正常速度连续阅读；记录真实反应：困惑、动作/指代/空间/时间连续性断裂、依赖查找才能理解之处、明显打断阅读的中文，以及后文是否自然化解这些摩擦。

这是发现阶段，不是编辑阶段。首读输出只能是阅读反应和必要的前后文定位：不改正文、不改术语、不写译者注、不提出替换方案、不做覆盖率/严重度/配额/审计矩阵。读者反馈是 discovery evidence，不是直接写入授权；没有反应也不证明全书无问题。

### 6. 中文优先的连续双语编辑阅读

编辑先以中文优先连续阅读整卷（使用刚刚渲染的中文输出），保持小说的节奏和累积信息；随后按段落和场景边界回看英文，检查读者反应点及可能影响意义的地方。源文检查以边界、转场、对话轮次、段落功能和场景整体为中心，不退化为逐行机械比对，也不以 monolingual “感觉好”替代双语 fidelity review。

编辑关注漏译/误译、极性和施受关系、指代、时序、数字、人物称谓、术语、叙述声线、文化/专业事实、幽默、双关、反讽、标题以及原有 source footnotes。源文有意含糊时保持含糊；多种忠实且自然的译法不因个人偏好重写。发现问题时只记录可复核的 source span、当前中译、上下文证据、影响和决定；先处理真正破坏理解或声线的项，不制造异常清单来替代阅读。

### 7. 简洁编辑查询、决议与有限对抗复核

编辑把需要决定的问题写成简洁 query：问题、最小必要上下文、当前读法、可能影响和需要谁决定。译者/编辑/审阅者共同解决 query，并将**决定**（而非所有候选）回写适用的 glossary、character record、translation 或 note register。新问题不得静默扩大已有写入范围；超出范围就开一个明确的小任务。

对实际提出的正文或译者注修改，可选做一次**有界 adversarial review**：只审这次具体 proposed change、其 source span、前后段落/场景和相关术语，寻找意义漂移、过度具体化、声线损失、剧透或注释越权。它不是第二套全书 QA，不要求重复审阅、平行裁决或逐行多人批准。实际采用的修改须由有权限的 editor/fixer 写回 canonical package，并重新渲染受影响输出；未采用的候选不进入真源。写入前必须重做 exact mapping/join、ordered-ID、当前译文快照和 schema/anchor guards；任一 guard 失败时零写入。写入后重新读取持久 records，证明只改变批准的 target fields/units，再串行重建 derived outputs；不能用 proposal、approval 或哈希本身代替回读。

### 8. 译者注编辑与物化

全局 knowledge registry（系列/项目层已有资料）只能生成每卷的**候选**；它不是本卷真源，也不能自动生成最终注释。逐候选明确 scope：

- **portable**：可跨卷复用、事实范围稳定且不依赖本卷尚未发生的情节；
- **local**：只对本卷或本场景成立，依赖本卷上下文、首次出现或阅读时机。

候选必须绑定 canonical first-occurrence unit 和精确 anchor。每卷编辑先结合本卷上下文审核候选，再做有界 adversarial pruning：删掉可由正文理解、重复、未经证实、过度解释、剧透或把事实争议写死的项。通过 pruning 的 note 才进入已有 `translator_notes.jsonl` canonical records，并经过现有 validator、审批和 renderer。source footnotes/author notes/editor notes 永远单独保留，不能改写成 translator note；translator note 也不能吞并或替代 source footnote。

继续使用现有安全约束：拒绝 stale anchor、跨 unit 重定位、重复 note、非法 provenance、legacy CSV 与 canonical notes 混用；每条渲染注释恰有一个 forward noteref 和 reciprocal backlink。没有证据、审批、有效 anchor 或 renderer 支持的候选保持未物化，而不是发明新机制。

### 9. 窄范围脚本检查

机械检查在连续阅读之后服务于已知风险，不能重新成为 QA 中心。只运行与本卷和实际改动相关的窄检查，至少覆盖：

- source/translation mapping 的遗漏、重复、多余 unit、顺序和 slot join；
- 数字、计量、日期、否定/极性和明显施受关系的定向检查；
- 人名、称谓和已裁定术语残留/误替换；
- raw English/source/UI residue（允许的专名、引用、source-footnote 内容按项目规则排除）；
- JSONL schema、字段类型、canonical `en` byte-exact、换行、notes anchor/provenance；
- 结构完整性、脚注往返链接、EPUB ZIP/OPF/nav/NCX/spine 和 Markdown 章节边界。

可用既有脚本，例如：

```sh
python3 docs/skills/fic2zh/tools/translate_tool.py validate \
  --source <canonical.jsonl> --translation <translations.jsonl> \
  --expected-count <N> --first-id <FIRST> --last-id <LAST>
python3 docs/skills/fic2zh/tools/validate_translator_notes.py \
  --notes <translator_notes.jsonl> --translations <translations.jsonl> \
  --workspace-root <approval-root>
```

`check_alignment.py` 等 legacy 检查只在其输入格式确实适用时使用，不能拿 chapter-level 对账替代 canonical exact join。数字、否定、名字和术语脚本报告候选风险，编辑必须回看 source 和上下文；机械 PASS 不等于语义正确。

### 10. Copyedit、最终 proof 与发布包装

完成 query resolution 和 notes materialization 后，对实际将交付的 EPUB、纯中文 Markdown 和中文优先双语 Markdown 做 copyedit；再从文件本身进行 final proof，而不是只检查 source rows 或构建日志。检查章节顺序与标题、断段、标点、空白、目录、脚注跳转、封面/元数据、样式、中文残留英文和双语顺序，并抽读开头、中段、场景转折、注释附近和结尾。发现问题回到相应 canonical record 或 renderer 输入，重新确定性渲染并 proof。

每卷独立 EPUB 完成且通过本卷 release validation 后，omnibus **只负责包装这些已完成的 standalone EPUB**，按声明的卷序合并/归档，不在 omnibus 层重新决定正文、术语、译者注或 QA。omnibus 不得成为内容 authority，也不要求额外的全局内容 QA。

## 角色、上下文、工具、输出矩阵与交接协议

以下矩阵是每个任务包的最小边界。`READ` 是允许使用的输入，`DO_NOT_READ` 是明确排除的输入，`TOOLS` 是本角色可用的工具，`RETURN` 是必须交回的结果，`WRITE` 是唯一写入权限。没有列出的资料不因“可能有帮助”而自动开放；缺资料就停止并报告 `TASK_PACKET_INCOMPLETE: <fields>`。

| 角色 | READ（允许输入） | DO_NOT_READ（禁止输入） | TOOLS | RETURN（输出） | WRITE（写入权限） |
|---|---|---|---|---|---|
| **项目编辑/主代理** | 固化源文、项目约束、已有 canonical package、卷边界；确认目标读者、编辑简报和现有决议 | 把任何导出物、反馈、memory/context、registry 或聊天摘要当 authority；未经授权扩展卷边界 | `read_file`、`write_file`/`edit_file`、已有确定性脚本/renderer、`delegate`（仅判断性任务） | canonical package、卷边界、目标读者、编辑简报、最终决议和各任务包 | 建立/裁定 canonical package、结构与工作记录及最终决定；不得手改 derived 输出冒充真源 |
| **译者** | canonical English、mapping/structure、编辑简报、glossary、character/voice records、相邻场景 context、query/note registers | 其他卷或未授权源、历史 QA/读者反应、导出物作为 authority、无关工具源码；不得自行猜缺失资料 | `read_file`、`write_file`/`edit_file`；`web_search` 仅作有边界的事实/文化核验 | 指定 unit 的译文、work records、未决 query/note 候选及准确验证结果 | 仅写 assigned translation/work records（指定 `translations` unit、glossary/character/query/note 字段）；不写 source、结构 authority、derived 输出或最终决议 |
| **译者自修订者** | 完整 target-language chapter/volume；随后 source、完整 translation brief、既有决定、相关 records | 只看孤立段落、只看 QA/summary、其他卷或把导出物当 authority；不得跳过完整目标语阅读 | `read_file`、`write_file`/`edit_file`；现有窄检查脚本按包指定运行 | 连续阅读后的修订、保留问题和所依据的决定 | 首选原译者同一 session；若为新 agent，必须收到完整 brief/decision context；可更新 canonical translation 及已决定的 glossary/character/query/voice records，不改 source/derived 输出 |
| **盲读首读者** | 仅干净、独立、纯中文 standalone Markdown；最小 premise/阅读边界 | English、glossary、其他卷、QA、canonical files、repo browsing、网页/web search、任何编辑/译者反馈 | 只读任务包指定 Markdown（可用 `read_file`）；不使用 web/delegate/脚本作内容判断 | 按阅读顺序的真实 reactions：困惑、断裂、摩擦及足够定位的前后文；不提供改写方案 | 仅写 reactions；不得改正文、术语、notes、canonical 或 derived 输出 |
| **双语编辑** | 中文优先 bilingual Markdown、编辑简报；先独立读；完成后才读 blind reactions；诊断时才查 glossary、character/voice records 和必要 source context | 独立阅读完成前的 blind reactions；无关卷、全量历史 QA、repo 漫游；不得把个人偏好或反馈直接当决定 | `read_file`、`write_file`/`edit_file` 写 findings；不使用 web 作开放式研究 | 简洁 findings/queries：source span、当前中译、上下文、影响和建议检查方向 | 仅写 editorial findings/query register（若任务包明确）；不写 product、canonical translation、glossary final 或 notes |
| **查询解决者/译者** | 仅实际 query，以及解决该 query 所需的最小 source/target context 和相关已定术语 | 全书、无关 query、历史 QA/候选大表、其他卷或不必要上下文 | `read_file`、`write_file`/`edit_file`；不作开放式 web search | 每个 query 的 accepted/rejected/minimal decision、理由和必要的后续字段 | 仅记录 query decision；若决定本身是术语/人物裁定，可写对应已定 record；不借机重写 product，获准的正文改动另发有界任务 |
| **对抗复核者** | 仅 concrete proposed change、exact source/current/proposed text、必要的相邻 scene context、相关已定 terms | 全书重扫、未提出的候选、无关 QA/feedback、其他卷；不扩展为逐行或覆盖率审查 | `read_file`；不写文件、不用 delegate，不作开放式 web search | 对该 proposal 的 accept/reject/needs-query 与简短理由，专找过度翻译、信息泄漏、声线损失、错误 premise、注释过量 | 无写入权限；任何采用须由有权限者写回 canonical package |
| **译者注编辑** | global registry candidates、standalone-volume context、canonical EN/ZH、已有 source notes/translator notes | registry 以外的自动注释结果、无关卷全文、历史 QA/盲读反应、未绑定本卷 anchor 的资料；不改 source footnotes | `read_file`、`write_file`/`edit_file`；`web_search` 仅事实核验 | pruning 后每候选的 accepted/rejected、scope/anchor/provenance 理由及 approved canonical notes | 只在 pruning 后写 candidate decisions 和 approved canonical translator-note records；不写 source notes、正文或 derived 输出 |
| **Copyeditor** | 实际 target-language deliverables、house style；只有 wording 暗示语义风险时才读对应 source/context | 不以历史 QA 噪音或全书重扫替代实际成品；不读取无关卷；不把风格偏好当语义决定 | `read_file`、`write_file`/`edit_file`；必要时运行包指定的窄检查 | 机械/风格缺陷及语义风险 query；说明实际文件和修复范围 | 可按任务包修 canonical inputs 中的机械性目标语缺陷；语义风险必须转 query，不得静默改变 meaning，不直接把 derived 输出当真源 |
| **Proofreader** | 确定性 render 后实际 final EPUB、final zh-only Markdown、final Chinese-first bilingual Markdown、release checklist | historical QA noise、旧导出物、仅 source rows/日志；不把非最终 artifact 当交付物 | `read_file` 检查实际产物；既有 renderer/validation scripts；`write_file`/`edit_file` 仅修 canonical inputs | production defect report，以及已修复 defect 的实际文件/重渲染验证结果 | 只通过 canonical inputs 修 production defects；不手修最终导出物，不改变文学意义或内容政策 |
| **确定性脚本/renderer** | 仅 canonical package（mapping、translations、structural slots、source footnotes、canonical translator notes）及命令参数 | outputs、feedback、registry 候选、聊天摘要、QA 结论作为输入或 authority；不读来作内容决定 | 直接运行现有 scripts/renderers 和机械 validators；不使用 `delegate` | deterministic Markdown/EPUB/aggregate 或 validation result | 不作内容写入或裁定；只生成 derived outputs/报告。export/propagation/validation 由主代理直接运行，不委托给 agent |

### 交接协议与阶段闸门

1. **先定 authority。** 项目编辑/主代理先冻结 canonical package、volume boundary、target reader、editorial brief、ordered unit set、当前决定和本任务的 write scope；每个下游包都引用这些精确路径。canonical package 仍由 mapping、translations、structural slots、source-footnote records、canonical translator-note records 五类既有记录组成，不新建合并格式。
2. **只传最小必要上下文。** 发包方列出 `HANDOFF_FROM`、`HANDOFF_TO`、`STAGE`、精确 `READ`/`DO_NOT_READ`、`WRITE`、`TOOLS`、`RETURN` 和 `VALIDATION`。下游不得自行补读；发现缺口或矛盾即停止，不重抓、不猜文件名、不参考邻卷。
3. **保持阶段顺序。** 翻译 → 目标语整章/整卷自修订 → 确定性 render → blind standalone Chinese read → 中文优先 bilingual read → query resolution → concrete proposal 的 adversarial review → translator-note pruning/materialization → copyedit → 确定性重渲染 → final proof/release。相应工作流的十个阶段不变；本协议只规定资料释放和写入边界。
4. **反馈延迟释放。** blind reader 必须先完成并交回 reactions，之后才向 bilingual editor 暴露 reactions；bilingual editor 必须先完成独立中文优先阅读，之后才可看 reactions 并查必要 source/records。未达到闸门，不传播反馈或诊断上下文。
5. **提案后才对抗复核。** adversarial reviewer 只接收一个或一组已形成的 concrete proposed changes，以及 exact source/current/proposed text 和必要场景；没有 concrete proposal 不发起 review，也不让 reviewer 全书重扫。
6. **先 render 后 proof。** copyedit 或已批准的 canonical 修复完成后，由主代理直接运行现有 deterministic renderer；proofreader 只检查这次 render 出来的实际 EPUB/Markdown/bilingual Markdown。production defect 回写 canonical inputs，再重渲染、复读和验证，不手改输出物。
7. **delegate 的边界。** `delegate` 只用于上述判断性阅读/翻译/编辑/复核，并随任务包给出 bounded write scope；不能用它运行本可直接运行的 deterministic export、propagation 或 validation，也不能借 delegate 扩大上下文或引入 nested/并行审批流程。

### 通用任务包（所有角色）

```text
VOLUME_ROOT: <exact path>
HANDOFF_FROM: <role/session>
HANDOFF_TO: <role/session>
STAGE: <1-10 stage and subtask>
CANONICAL_PACKAGE: <exact mapping/source/translation/slot/source-footnote/note paths>
UNIT_SET: <ordered chapter/scene/unit IDs and expected counts>
EDITORIAL_BRIEF: <sample, reader, voice, register, ambiguity and naming rules>
DECISIONS: <exact decided glossary/character/query/note paths or NONE>
READ: <exact allowed paths and required reading order>
DO_NOT_READ: <exact excluded paths, volumes, outputs and feedback>
WRITE: <exact files/fields allowed to change, or NONE>
TOOLS: <allowed tools; bounded web/delegate rules, if any>
RETURN: <required result fields and evidence limits>
OUTPUTS: <derived outputs, if this stage consumes or produces them>
VALIDATION: <exact applicable commands, after-write checks and render requirement>
EXCLUSIONS: <anything else not to read, write, rebuild or treat as authority>
```

### 可复制的最小任务包

这些包只覆盖四种容易越界的交接；路径、卷和 unit 必须由主代理替换，不能照例子猜测。`WRITE: NONE` 是真实权限，不是建议。

#### Blind reader

```text
ROLE: blind first reader
STAGE: 5 — standalone Chinese discovery
READ:
  - <exact clean standalone Chinese Markdown path>
  - MINIMAL_PREMISE: <one short premise and standalone volume boundary>
DO_NOT_READ:
  - any English/source, glossary, character/voice records, canonical package, repo files/browsing
  - other volumes, QA/history, bilingual Markdown, editor/translator feedback, web pages
WRITE:
  - <exact reactions report path only>
TOOLS:
  - read_file on the supplied Markdown only; no web_search, delegate, scripts, or renderers
RETURN:
  - ordered reactions with enough local before/after context to locate each moment
  - confusion, continuity break, reading friction, and later natural resolution when observed
  - no fixes, replacements, notes, severity/coverage/quality metrics, or claims beyond this read
```

#### Bilingual editor

```text
ROLE: bilingual editor
STAGE: 6 — Chinese-first independent read, then diagnosis
READ:
  PHASE_1: <exact Chinese-first bilingual Markdown path> and <editorial brief path>
  PHASE_2_AFTER_INDEPENDENT_READ: blind reader reactions; only then the listed
    <glossary path>, <character/voice path>, and necessary <source/context paths>
DO_NOT_READ:
  - blind reactions during PHASE_1; unrelated volumes, whole historical QA, repo browsing
  - any source/context not needed to diagnose a finding; no output is a canonical authority
WRITE:
  - <exact findings/query register path only>; no product files
TOOLS:
  - read_file; write_file/edit_file for findings only; no open-ended web_search or delegate
RETURN:
  - concise finding/query: source span, current Chinese, relevant context, impact, and
    question or bounded direction for resolution
  - record that PHASE_1 was completed before reactions were read; no silent rewrite
```

#### Adversarial review

```text
ROLE: adversarial reviewer
STAGE: 7 — bounded review after a concrete proposal
READ:
  - <exact proposal path>
  - exact source text: <path/span>
  - exact current text: <path/span>
  - exact proposed text: <path/span or packet>
  - <only necessary surrounding scene path/span>
  - <relevant decided terms path>
DO_NOT_READ:
  - any whole-book/whole-volume rescan, unproposed candidates, unrelated QA or feedback
  - other volumes, broad repo browsing, or files outside this proposal context
WRITE:
  - NONE
TOOLS:
  - read_file only; no write_file/edit_file, web_search, delegate, scripts, or renderer
RETURN:
  - ACCEPT, REJECT, or NEEDS_QUERY for this proposal, with concise evidence
  - check specifically for overtranslation, information leakage, voice loss, wrong premise,
    and excess notes; do not approve by silence or by a coverage count
```

#### Translator-note review

```text
ROLE: translator-note editor
STAGE: 8 — per-volume note pruning and materialization
READ:
  - <global registry candidate path>
  - <standalone-volume context path>
  - <canonical EN path> and <canonical ZH path>
  - <existing source-footnote records path> and <existing translator-note records path>
DO_NOT_READ:
  - unbound auto-generated notes, unrelated volumes as authority, historical QA/reader reactions
  - candidates without canonical first-occurrence anchor, and any source footnote as a translator note
WRITE:
  - <candidate decision register fields>
  - <approved canonical translator-note records only, after pruning>
  - NEVER source-footnote records, source text, translations, or derived outputs
TOOLS:
  - read_file; write_file/edit_file for listed records; web_search only for bounded factual verification
RETURN:
  - per candidate: ACCEPT/REJECT, portable/local scope, exact anchor, provenance, concise reason
  - approved notes and unresolved candidates clearly separated; no note for obvious/redundant,
    unverified, overexplaining, spoiler, or renderer-unsupported material
```

### 证据边界

连续盲读只证明读者在给定 clean output 上产生了所记录的反应；双语编辑阅读支持具体的翻译诊断；脚本、哈希、count、schema 和 build 只证明其可机械证明的 mapping/发布结构属性。它们都不单独证明文学质量、语义正确、可读性、翻译忠实度或读者理解。完成报告必须写明 scope、实际读过的最终输出、已解决/未解决 query，以及证据不能证明什么。

## 保留的操作要点

| 情况 | 对策 |
|---|---|
| 子代理无法访问工作区外输入 | 先将合同指定的 source/canonical 输入复制到工作区，不得自行重抓 |
| 拆段章漏掉 segment | 在合同点名完整 ordered unit set，并在 exact join 检查 |
| API 余额不足或中断 | 只补缺失 unit，不重写已通过记录 |
| `## ` 造成 EPUB 并章 | 章节始终使用 `# `，并检查实际输出 |
| 缺 ebooklib/lxml | `uv run --with ebooklib python3 ...`，缺依赖时明确记录 |
| 共享产物竞争写入 | 按卷/输出互斥写；由主代理串行最终渲染 |
| EPUB 非正文 spine 页漏写 | 保留并核对 `BODY_LAST+1` 到 `SPINE_LAST` 的所有成员 |

## Explicit non-goals

- 不在 omnibus 层制定内容政策、译名政策或质量门槛；
- 不实行逐行多 agent approval bureaucracy、强制重复审阅或广泛平行 adjudication；
- 不把 blind reader 变成直接编辑者；
- 不用单语“读起来顺”替代中文优先的连续双语 fidelity review；
- 不以 hashes、counts、coverage、schema 或 builds 单独作质量声明；
- 不以 anomaly sweep、candidate matrix、审计式 coverage metrics 或异常数量作为 QA 中心；
- 不新增 monolithic manuscript format、翻译框架、调度器或并行 worker pool；
- 不让输出、读者反馈、memory、context 或 registry 取代 canonical package；
- 不将 source footnotes 与 translator notes 合并，不让全局 registry 自动物化本卷注释；
- 不在本 Skill 中修改代码、工具、测试或数据文件。

## 验收标准（整卷交付）

- 源文已固化，canonical package 的 mapping、translations、structural slots、source footnotes 和 canonical translator notes 可精确连接；`en` 未变，缺失/多余/乱序 unit 为零。
- 编辑简报、连续章节/场景翻译、译者目标语自修订、glossary/character/query/note 记录和实际决议可追溯；未解决项没有被伪装成 final。
- standalone EPUB、纯中文 Markdown、中文优先双语 Markdown 均由 canonical package 确定性生成；实际输出已检查结构、脚注、章节和导航。exporter 若仍是 evolving implementation，只声称已验证的卷/格式，不声称全卷支持。
- blind standalone Chinese first-reader pass 只产生反应；中文优先连续双语 editorial read 已覆盖本卷，且在段落/场景边界做了必要 source checks。
- queries 已解决或明确保留；实际提议的改动如做 adversarial review，其范围有界；notes 候选经过 per-volume context review、scope 判断和 pruning，source footnotes 仍分离。
- 已运行适用的窄脚本检查，并完成实际 EPUB/Markdown 的 copyedit 与 final proof；发布声明明确证据及其限度。omnibus 只包装已完成的 standalone EPUB。
- 除非用户明确要求，不提交或推送。
