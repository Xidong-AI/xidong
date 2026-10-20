# YAA 重写实施计划

本文档把 `docs/design.md` 的愿景落成可执行的架构与阶段计划。冲突处以本文档的「决策记录」与「对 design.md 的更正」为准。

## 决策记录

| 编号 | 决策 | 理由 |
| ---- | ---- | ---- |
| D1 | 完全重写，旧 `client/`、actix 服务端、`rethink/finish/ask` 全部移除 | 旧产物是 web 服务端路线，与「轻量 CLI、Shell 即唯一工具」相反；Git 历史保留 |
| D2 | 分阶段交付，先跑通最小闭环 | inbox/跟随交接/断电恢复属并发一致性问题，一次性写完必带未验证缺陷 |
| D3 | 压缩复刻 DSH `compaction-basic` 真实行为（pruner + LLM 摘要） | 用户选择；已取得源码级参数 |
| D4 | 运行时 `tokio` + `reqwest` | 用户选择；便于将来并发与流式 |
| D5 | 5 个 crate：`yaa-msg`/`yaa-store`/`yaa-shell`/`yaa-core`/`yaa-cli`（`yaa-shell` 后于 D14 改名 `yaa-exec`） | 用户选择；依赖无环 |
| D6 | 模型供应配置与提示词经钩子获取，策略与限额留在 `config.toml` | 用户选择；原「所有配置」措辞不精确，按此执行 |
| D7 | 钩子输出结构化 stdout：`llm` 钩子按 TOML 解析，提示词钩子为纯文本 | 复用同一 TOML 解析器，避免 JSON 依赖 |
| D8 | 钩子时机：提示词钩子每会话一次，`llm` 钩子每次运行一次 | 避免每轮重复注入提示词污染历史 |
| D9 | 阶段 1 不含审批、压缩、inbox/单 runner/跟随、notify、断电恢复 | 用户选择 |
| D10 | 状态根按 XDG 拆分：`YAA_CONFIG_HOME` + `YAA_DATA_HOME`，`GLB_CFG` 指向配置根 | 用户选择 |
| D11 | 提示词钩子为替换语义：钩子 stdout 即完整提示词 | 用户选择；默认配置由 `yaa --get-prompt` 拼内置模板 |
| D12 | `ai` 与 `tool_call` 保持分离记录，以单批缓冲原子追加 | 用户选择；忠于 `design.md:31-33` |
| D13 | 过程记录默认打到 stderr（`# 描述` / `% 命令` / 块间 `---`，含非末步模型正文）；`--verbose` 追加思考与命令输出；删除 `--trace` | 用户要求对齐 `design.md` 示例观感；`--quiet` 保留为仅最终回复 |
| D14 | 工具知识内聚：function schema、提示词 section、参数解析、错误文案、调用编排与结果转换全部归 `yaa-core::tools::shell`；`yaa-shell` 降为纯 Shell 执行机制并改名 `yaa-exec` | 现状同一事实硬编码多处（工具名 4 处、参数名 5 处、描述两处措辞不一），schema 与提示词容易漂移；外部调研结论：单工具不引入 trait/registry/模板引擎，采用「多处呈现、一处数据源」 |
| D15 | 展示采用「同一事件日志派生」：模型消息与用户展示都从会话事件库投影，不维护第二套执行事件流 | 消除展示与模型上下文的漂移（Codex `/status` 的 `Agents.md: <none>` 系列 issue 为反例：两条独立计算路径必然漂移）；用户要求「用户看到的就是模型看到的」 |
| D16 | 展示不设专用格式：初始化注入与外部通知按与工具调用一致的块样式展示（默认 `# 注释` + `% 命令` 摘要，`--verbose` 追加内容全文）；`--quiet` 抑制全部过程输出 | 上一版「标签 · 来源 · 行数 · 大小 + 首行」的专用格式被用户判为多余；摘要与 `--verbose` 全文保留以便调试 |
| D17 | 内置模板只保留 `system`，删除 `USER_TEMPLATE`；`--get-prompt` 只认 `system`，输出「产品前导 + 工具 section」 | `user` 模板无运行时消费者，按「不定义无消费者的死分支」原则删除；工具说明随工具走 |
| D18 | 展示文案与渲染角色由 `yaa-core` 投影层决定，`yaa-cli` 只负责样式与 stdout/stderr 分流 | 消除 CLI 硬编码映射；展示语义属上下文投影，不属于终端样式 |
| D19 | 内容注入统一为 `yaa --notify` 外部接口（`yaa --notify="注释" -- cmd` / `cmd \| yaa --notify="注释"`），写入 `system(source=notify)` 事件、正文内联 `<注释>` 信封；`llm` 配置加载不写任何事件 | 取代 `system_prompt`/`user_prompt` 两个提示词钩子与 `origin=hook` 审计记录，注入内容成为一等事件，渲染不再需要「提升钩子」的间接层；`llm` 每次运行都执行却只累积 redacted 记录，是现状缺陷 |
| D20 | 空会话初始化钩子 `hooks.init`：会话**不存在 `system(source=init)` 记录**时、写入 `user` 之前执行一次；yaa 忽略其 stdout，内容由命令内的 `yaa --notify` 写入；成功后写一条 `system(source=init, content="")` 簿记保证幂等；失败仍写 `user` 并按配置错误退出 | 触发条件改用**簿记**而非「会话为空」，使 `yaa --notify` 先写入通知、或上轮 init 失败的会话仍会执行 init；簿记让不产出内容的钩子也不会反复执行 |
| D21 | 记录 schema 简化：移除 `tool_call.hook`、`tool_call.redacted`、`ToolOrigin::Hook`、`HookInfo` 与 `System.scope`；`System` 新增可选 `command` 供展示 | 没有 hook 记录后这些字段失去消费者；`command` 让通知块与工具块同构（默认摘要显示命令，`--verbose` 显示内容） |
| D22 | 上下文按稳定性分层装配（对齐 `docs/dsh-context.md`）：稳定前缀合并为一条 `system`，易变离群通知并入相邻 `user`，工具 schema 独立于消息 | 缓存友好（前缀字节稳定）；与 DSH 的 sections/contexts/messages/tools 分层一致 |
| D23 | `ai` 记录新增可选 `reasoning`（`Option<String>`，落盘） | 思考内容既要展示（`--verbose`）又要按供应商要求回传；不落盘则两者都失去来源。接受 schema 变动与存储/隐私成本 |
| D24 | `[llm]` 的 `provider` 字段删除，改为 `endpoint_format`（默认 `"openai"`，可选 `openai`/`deepseek`/`kimi`/`qwen`/`glm`），唯一驱动出站方言分支 | 原 `provider` 无任何消费者（死字段）。DeepSeek/Kimi/GLM/Qwen 在请求带 `tools` 时**必须**回传 `reasoning_content`（思考为空也要发 `""`），而 OpenAI 官方 Chat Completions 不认该字段，故必须按端点格式分支；见「对 design.md 的更正」第 12 条 |
| D25 | 阶段 1 引入 `YAA_SESSION` 环境变量；`yaa --notify` 目标解析顺序 `--session` → `YAA_SESSION` → 当前 cwd 默认会话 | `init` 钩子经 `yaa --notify` 侧写，而 `--notify` 子进程只看到 cwd；外层 `yaa --session <任意路径>` 时若不传目标会写错会话 |
| D26 | 旧会话不兼容：`load` 遇到无法解析的行报明确错误（提示 `--clear`），不静默跳过 | 新 schema 删除 `origin=hook` 等字段后旧行无法反序列化；pre-1.0 不做数据迁移，但数据损坏必须可见 |
| D27 | `yaa --notify -- cmd` 把 cmd 的 stderr 原样透传、以 cmd 的退出码退出；`init` 失败不写 `system(source=init)` 簿记、下次运行重试 | 钩子需要感知子命令失败；因触发条件由簿记判定（D20），不写簿记即让「下次运行自动重试」的承诺成立，代价是重试可能重复部分注入 |
| D28 | 离群 `system(notify)` 的分组取「双向最大连续段」（见「渲染管线」表） | 设计原文「与相邻 `user` 合并」未定义方向；golden 测试需要唯一解 |
| D29 | 「展示与模型消息同源」（D15）显式承认豁免：`reasoning` 与展示专用的块标记/标题不计入一致性不变量 | `reasoning` 落盘且展示，但是否投影由 `endpoint_format` 决定；块标记（`# `、`% `、`---`、`# 思考`）只服务终端排版。通知的 `command` 与工具调用的 `description` 现已同时出现在两侧（D31），不再豁免 |
| D30 | 默认 `init` 钩子中「用户总提示词」改为「文件存在才 `--notify`」，且脚本首行加 `set -e` | 原草案无条件 notify，`AGENTS.md` 缺失时会注入空信封，浪费 token；`set -e` 避免「前两条 notify 失败、末条成功 ⇒ 整钩子退出 0 却被当成功」 |
| D31 | 通知的 `command` 进入模型上下文：投影 `system` 时在最前拼 `[执行命令] <command>` | 命令是通知的因果来源，模型需要知道它从何而来；展示层仍以 `% 命令` 呈现，同一字段因此同时服务展示与上下文 |
| D32 | `system` 记录新增可选 `output_files`（与 `tool_result` 同构，字段顺序 `command?` 之后、`content` 之前） | `--notify` 的命令与 stdin 两种形态复用 `yaa-exec` 的截断/落盘后都需要一个与 `tool_result` 一致的路径载体；旧记录缺该字段解析为 `None`，不得报错 |
| D33 | `--notify` 的 `--` 形态复用 `yaa-exec`：截断 + 完整输出落盘 + 退出码沿用 + stderr 透传；`command` 改为 **shell 转义后的可重放命令** | 原实现 `Command::new(program).args` 绕过 shell、不截断也不落盘，`command` 只是展示摘要；统一执行路径后通知与工具行为一致，且命令可重放，`output_files` 得以成立 |
| D34 | `archive` 的 `sessions-<ts>.jsonl` 已存在时改为 `sessions-<ts>-<n>.jsonl`（n 从 1 递增；现仅用于旧 JSONL 的文件级归档） | 同毫秒连续归档会静默覆盖，破坏「归档即不可变历史」 |
| D35 | `timed_out` 只表示真正超出时限；读取宽限内放弃流只回填占位提示，不再 promote 为超时 | 逃逸进程组攥住管道与命令超时是两回事，误报会让调用方以为命令被超时击杀 |
| D36 | `--notify` 的 stdin 形态复用 `yaa-exec::capture_reader`（同一 `Sink`/`FullOutput`） | 与 `--` 形态保持同一截断标记与落盘语义，避免两套实现漂移；超限时同样产生 `output_files.stdout`，内存仍有界 |
| D37 | 模型请求超时 `[agent] request_timeout_secs`（默认 300 秒，0 值非法）；请求超时**不重试** | 端点接受连接却不响应时，无超时会无限挂起；重试只应覆盖「已收到响应」的可重试错误，超时重试会把等待拖成多轮 |
| D38 | SIGINT 处理：向当前工具进程组转发 SIGINT，2 秒宽限后 SIGKILL 整组，yaa 以 `130` 退出 | 工具在独立进程组，终端 SIGINT 打不到它；yaa 死亡后其超时监控消失，命令成为无人监管孤儿。仅覆盖 `shell` 工具进程组，钩子未覆盖 |
| D39 | 展示按水位增量去重：append 后重新 `load` 再 `emit_new` | 循环内 append 后直接展示却不推进水位，会使同一批记录在下一轮被二次展示；重新 load 同时覆盖「模型请求期间外部 `yaa --notify` 写入」的不丢不重边界 |
| D40 | 审批配置为 `[approval]` 规则数组（每项 `pattern` + `action`，动作取值 `allow`/`ask`/`deny`），无匹配默认 `allow`；单工具 `shell`，规则不按工具分键 | 保持阶段 1「默认 allow」行为；`[approval]` 与 `[shell]`/`[agent]` 并列，语义清晰 |
| D41 | 规则裁决 = **最具体优先 + deny 短路**：先全局扫 `deny`（任一命中即拒）；否则在命中的规则中按「首个通配符前的字面字符数」取最大者，同分取后者；无匹配为 `allow` | OpenCode 的 `findLast` 会让末尾 catch-all 覆盖具体 `deny`（其社区最大吐点），不可照抄 |
| D42 | 命令资源按 shell 分隔符（`;`、`&&`、`\|\|`、`\|`、换行）朴素拆分并剥离前导 `VAR=val`，逐段评估后聚合：任一 `deny` → deny；否则任一 `ask` → ask；否则 allow | `CI=true git commit` 一类 env 前缀可绕过 `git *`（OpenCode #16075）；解析仍是 best-effort，**不构成安全边界** |
| D43 | 非交互 `ask`：写 `approval_pending` → stderr 打印待批清单 → 以退出码 `6` 退出，**不写** `tool_result`；`yaa --approve`（无参）放行**最旧**未决 pending：执行其命令、写 `approval_resolved: approve` 与 `tool_result`，随后继续 agent loop | 非交互下 ask 绝不可阻塞（OpenCode #36762 教训）；无参逐一放行忠于 `design.md` |
| D44 | `--auto` 自动放行命中 `ask` 的调用（写 `approval_pending` + `approval_resolved: approve` 后执行）；显式 `deny` 不受影响 | 非交互批量场景的逃生舱；`deny` 永远强制（对齐 OpenCode V2 语义） |
| D45 | `deny` 命中即拒：写 `approval_pending` + `approval_resolved: deny` + 失败 `tool_result`（模型可见被拒），**不退出**；普通 run 启动时若存在未决 pending，先补 `approval_resolved: deny` | `design.md`：被拒不中断循环，模型可改命令；新提示词视为放弃旧待批 |
| D46 | 审批只作用于 `origin=ai` 的工具调用；`init`/`llm` 钩子与 `yaa --notify` 不经 `shell` 工具，天然豁免 | 与 plan 原定义一致 |
| D47 | 审批轮顺带修复 A4（畸形 `tool_call` 不再静默丢弃）、A5（非末步正文不上 stdout）、A6（`--config` 沿钩子链传递） | 同属「不静默降级 / 展示正确性」主题，避免二次开工 |
| D48 | 阶段 3 交付 = 压缩 + `--json` + `--no-store`；`outputs/` 保留策略顺延 | 三项各自独立可验收；混阶段会让验收标准互相稀释 |
| D49 | 压缩策略（比率、pruner 预算、`retries`）归 `[compaction]`；模型上下文窗口 `context_window` 随 `llm` 钩子输出（供应侧，与 `model` 同源，换模型只改一处），缺失即整支禁用（静默，不报错） | 窗口是模型属性而非策略；DSH 同样从路由适配器解析 capacity。策略与预算在 `[compaction]` 与 `[shell]`/`[agent]` 并列 |
| D50 | 压缩只发生在渲染期、**不改 `session.db`**；摘要产物写 `sessions/<sid>/summaries/<hash>.json` 侧车（`hash` = 被遮蔽记录 id 序列 + 模型） | 忠于 `design.md`「压缩只发生在渲染阶段，原始事件库不变」；key 含记录 id 使缓存精确，同内容不同划分不误命中 |
| D51 | 摘要**链式递进**：新摘要输入 = 旧摘要文本 + 新遮蔽区，`previous` 记录前驱；渲染时只应用链尾摘要 | 无状态重算会让长会话第二次压缩把已蔽原文整段重喂、可能直接超窗口；链式使每次摘要输入有界 |
| D52 | token 估算按**字段级二分类**：ASCII 码点按 `chars_per_token`（默认 4）取上整，非 ASCII 码点每码点计 1 token；各字段独立取整后求和，不含工具 schema | 修正旧固定密度对 CJK 约 4 倍的系统性低估（中文会话实际已近窗口仍不触发压缩）；字段级取整与钩子预计算粒度一致 |
| D53 | pruner 用独立 `[compaction]` 预算（默认 8192/4096/1024），只修剪 `tool` 消息正文、跳过 `system`；标记 `\n\n[... tool result middle pruned ...]\n\n`；按 Unicode 码点 | 与 `[shell]` 的执行期截断语义不同（历史再修剪 vs 执行即截断），复用会把两种阈值混为一谈；跳过 `system` 避免裁掉稳定前缀 |
| D54 | 摘要指令照搬 DSH `COMPACTION_INSTRUCTION`（英文 Markdown 结构）+ `CHECKPOINT_PREAMBLE` + `<compacted-summary>` 标签；摘要调用**不带 tools**、以被遮蔽消息 + 指令为输入 | 该指令是经 DSH 验证的工程化 prompt；不带 tools 避免模型返回 `tool_calls`、简化调用形状 |
| D55 | 摘要失败（网络/超时/空输出/不比原文小/重试耗尽）→ stderr 警告 + 用完整历史继续；不中止回合 | 对齐 DSH「警告并携带完整历史继续」；压缩是优化而非正确性前提 |
| D56 | 展示仍从原始记录增量投影（D15 不移）；压缩发生时额外展示一条摘要条目；D29 豁免新增「压缩摘要与被修剪的工具正文」 | 历史不可重写且展示增量，逐字一致在时间轴上不可能；以显式豁免取代静默漂移 |
| D57 | `--json`：`DisplayItem` 逐条序列化为 JSONL 到 stdout（含最终回复与诊断）；与 `--quiet`/`--verbose` 冲突 | 机器接口需要单一稳定流；人类模式的双流（过程 stderr / 回复 stdout）不适合结构化消费 |
| D58 | `--no-store`：本次运行指向临时目录会话，退出即淘；与 `--session` 冲突 | 「不留痕」要求连 `outputs/` 一并消失；临时目录复用全部既有写入路径，零特例 |
| D59 | 存储改为每会话 SQLite：`sessions/<sid>/session.db` 的 `events` 表 append-only 为唯一事实源；`payload` 存完整记录 JSON，`type`/`ts` 为生成列 | 每会话单库、可审计且事务原子；支持按 `seq` 增量读，拒绝全局单库与双存储 |
| D60 | 用 `rusqlite`（`bundled`）实现；打开时校验 `application_id=0x59414131` 与 `user_version=1`，非 yaa 库拒绝 | 绝不误写普通 SQLite 库；捆绑 SQLite 满足 WAL-reset 修复的最低版本（≥ 3.51.3） |
| D61 | WAL + `synchronous=FULL`；写路径用 `BEGIN IMMEDIATE` 短事务批量提交；`busy_timeout` 5 秒（`--notify` 外层预算约 30 秒），BUSY 按 50/100/200 ms 退避、最多 3 次额外尝试 | 断电不丢已提交批次；写者串行化；重试总是重跑完整事务，绝不重试已开始的事务 |
| D62 | 锁契约：`runner.lock`（run/approve/clear 互斥）、`clear.lock`（clear 排他 / `--notify` 写入共享）、`init.lock`（首次建库串行化）；锁文件永久保留 | 单 runner 选举与归档/通知互斥；固定顺序 runner → clear，notify 只拿 clear，无死锁 |
| D63 | `--clear` = `VACUUM INTO` 快照 → 完整性/计数校验 → rename 为 `history/<sid>/sessions-<ts>.db` → 清空 events → `generation+1`；任何一步失败都不清空主库 | 归档是自包含快照；generation 区分清空前后，`seq` 可从 1 重新使用 |
| D64 | 崩溃恢复职责在 core、store 保持哑：runner 锁内、`init`/`user` 之前补中断 `tool_result`；普通 run 作废旧 pending，`--approve` 只补孤儿；渲染器内存兜底保留 | store 不猜测业务语义；恢复幂等，重复运行不会重复补写 |
| D65 | 旧 JSONL 不自动迁移：`--session *.jsonl` 明确报错并提示 `--clear`；`--clear --session x.jsonl` 走文件级 rename 归档为 `history/<sid>/sessions-<ts>.jsonl` | 避免静默误写旧数据；用户显式归档后可重新开始 |
| D66 | `--session` 路径统一绝对化：目录 → `<目录>/session.db`；`.jsonl` → 旧格式错误；其余按数据库路径 | 规则单一可预测；裸文件名时 sid 回退为绝对路径 sha256 前 32 位 |
| D67 | 归档库标记 `archived=1`，`append` 拒绝写入；`--status` 仍可读 | 归档即不可变历史，只读访问保留审计价值 |
| D68 | runner 生命周期为**排空式单进程**：一次 `run` 处理到「历史空闲 + inbox 空 + 无未决审批」才退出，不做 daemon/常驻 runner | 常驻进程需要额外守护、信号与升级语义；排空式已覆盖「多客户端注入后继续跑」的需求，且退出即释放 `flock`，恢复模型简单 |
| D69 | inbox 采用**文件载体**：`<prefix>inbox/{tmp,pending,init,rejected}`，载荷为完整 `Record` JSON；发布 `tmp → fsync → rename → fsync 目录`；消费 `append_idempotent` 后删除文件，不建 `consumed/` | 无锁多进程发布；完整记录使消费端无需重建字段；崩溃在 append 与删除之间时文件重现，由 id 去重；`init/` 保证「notify → 簿记 → user」顺序 |
| D70 | `--notify` **只发布**：普通调用写 `pending/`，init 钩子内（`YAA_INBOX_PHASE=init`）写 `init/`；不抢 `runner.lock`、不跟随、不跑 `init`/`llm`、不执行 agent loop，发布带 generation 守卫 | 通知是外部副作用，不应因顺手调用而启动模型或阻塞；由运行中的 runner 在安全点落库，未运行时留给下一次 `run` |
| D71 | follower 用 `read_after_with_generation` + 20→250 ms 退避 + `DisplayProjector` 增量展示；抢到锁后复检 generation，接管**只尝试一次**；`PendingPolicy` 区分新 `User`（Abandon）与纯 notify（Keep） | 避免每轮全量重投影与重复展示；只接管一次防止失败后无限抢锁；纯 notify 不应替用户作废旧待批 |
| D72 | 原子水位与 generation 守卫发布：`watermark()` 在同一读快照返回 `(generation, last_seq)`，`publish_with_generation` 在 `clear.lock` 共享锁内校验；`0→1` 视为首次建库、不算归档 | 防止「旧 generation 的行 + 新 generation 值」拼接与归档后写入新库；建库窗口不应让 follower 误报「会话已归档」 |
| D73 | artifact 路径按**完整数据库文件名**唯一化：非 `session.db` 以安全化文件名作前缀，锁与 inbox 各自独立；`outputs/`、`summaries/` 仍按会话根共享 | 同目录多个显式 `--session` 不能共享 `runner.lock` 或 inbox；用完整文件名而非 `file_stem`，使 `a.db` 与 `a.sqlite` 不碰撞；阶段 6 由 D88 扩展至 `outputs/`、`summaries/` 与归档目录 |
| D74 | `--clear` 持 `runner.lock` + `clear.lock` 后检查 `pending/`/`init/`，有消息即拒绝归档；库不存在但 inbox 非空同样拒绝 | 归档会清空历史并 bump generation，若连同待处理消息一起归档会静默丢消息；持锁检查与发布互斥 |
| D75 | `max_steps` 计**模型调用次数**，按含 `User` 的 inbox 批次重置；纯 notify 不重置 | 一轮多用户合并时每个 user 批次都应有完整预算；通知不是新任务，不应无限延长步数 |
| D76 | 会话环境链收敛为 `yaa-core::session::yaa_env` 纯函数，钩子 `hook_env`、shell 工具 `extra_env` 与 `--approve` 执行路径共用；注入 `YAA_SESSION`（无条件自己）、`YAA_NOTIFY`（继承非空且 ≠ 自身时保留）、`YAA_PARENT`（优先继承中间层，否则取孵化者的 `YAA_SESSION`）、`YAA_NOTIFY_CALL_ID`（仅 shell 工具） | 原实现把会话变量与限额变量分散拼接，两处漂移会直接把通知写错会话；纯函数让环境链可单测且只有一处事实源 |
| D77 | `--to` 优先级 `--to` > `--session` > `YAA_NOTIFY` > `YAA_SESSION` > cwd；显式 `--to` 走严格校验阶梯（存在 → 目录内 `session.db` → 旧 `.jsonl` → yaa 库 → 未归档 → IO），不存在**拒绝且不创建**；普通 `--notify` 仍警告并创建；`--to` 与 `--session` 指向不同库、与 `--no-store` 同用、空串均为用法错误 | 显式目标写错会话的代价远高于「首次自动建库」的便利；严格模式让拼写错误立即暴露，普通路径保留阶段 1 的便利语义 |
| D78 | 通知归因写入 `System.origin`（发布者会话库绝对路径，总写）与可选 `System.call_id`（继承 `YAA_NOTIFY_CALL_ID` 合法才写）；`Option` + `skip_serializing_if`，旧记录兼容、无 SQLite 迁移；新增 `is_injectable_system` 统一 display/render/inbox 白名单 | 父子血缘需要可审计的事件字段，而不是额外的 edges 表；单一谓词修复 F12 的三处判定漂移，且不引入新 `source` |
| D79 | 通知发布失败且属于可重试类别（BUSY/IO）时降级写 `<会话根>/<artifact 前缀>spool/`；命令退出码不变、stderr 提示；`run_queued_inner` 在崩溃恢复后、`init` 前只消费 spool 自身（不合并、不提前消费 inbox），`append_idempotent` 后删除；follower 只提示滞留数 | 后台任务不能因为目标库短暂 BUSY 就丢结果；spool 复用 inbox 的原子发布原语，崩溃语义与幂等去重全部沿用，不需要 job 表或 watcher；只消费 spool 避免打乱 `init/` 与 `pending/` 的既有顺序 |
| D80 | 后台回注范式：整条后台作业必须重定向 stdout/stderr（或 `setsid` 脱离工具进程组），如 `{ ... \| yaa --notify="..." --to "$YAA_SESSION"; } >/dev/null 2>&1 &`；子回父用 `--to "$YAA_PARENT"`；`call_id` 沿环境链继承父的工具调用 id，子内部再 spawn 时覆盖为最近调用点 | `yaa-exec` 在直接子进程退出后读取输出管道，后台进程若继续持有管道会在宽限期（2 秒）后被整组 `SIGKILL`，原 `... \| yaa --notify=... &` 范式在 shell 工具内必然失败；重定向/`setsid` 让工具立即返回且后台通知能到达 |
| D81 | `--approve` 不认 `--to`：其解析链仍为 `--session` → `YAA_SESSION` → cwd；`--to` 与 `--approve` 组合因 `--to` 要求 `--notify` 而被 clap 拒绝；显式 `--to`/`YAA_NOTIFY` 目标一律按跨会话写 `pending/`，不写 `init/` | 审批是对本会话未决调用的恢复，不应被通知目标改写；跨会话通知也不应继承发起者的 init 相位 |
| D82 | `--clear` 除 pending/init 外还拒绝非空 spool；`--status` 只读显示 spool 计数；`spool/tmp/` 沿用 1 小时残留清理 | 归档会清空历史并 bump generation，若把尚未补投的 spool 一起归档就是静默丢通知；计数让滞留可见但不产生副作用 |
| D83 | 后台范式必须关闭/重定向 stdout/stderr（`{ ...; } >/dev/null 2>&1 &`）或 `setsid` 脱离工具进程组；`yaa-exec` 在直接子进程退出后读取管道，宽限期（2 秒）后对整组 `SIGKILL` | 原 `... \| yaa --notify=... &` 让后台进程继续持有工具管道，工具在 2 秒后整组击杀，内层通知永远不到达；重定向后管道立即 EOF，工具 <1s 返回，后台作业存活并投递 |
| D84 | spool 写入新增 generation/archived 守卫：持 `clear.lock` SH，与 `publish_with_generation` 同锁序（先 clear 后 init），复查 generation/archived；notify 预检 BUSY/IO 也降级 spool；可重试读取失败时仍写 spool（clear SH 已排除并发 `--clear`） | 避免 spool 落在已归档/新 generation 下绕过 `--clear` 的非空 spool 拒绝检查，同时不因目标库短暂 BUSY 丢通知 |
| D85 | CLI 冲突：`--notify` 与 `--clear`/`--status`/`--get-prompt` 由 clap 冲突拒绝；与位置提示词由解析后 `validate_combinations` 拒绝（clap 的 `conflicts_with` 会抵消 `--to` 的 `requires`）；`publisher_origin` 改为 best-effort，解析失败省略可选 `origin` | 短路写入路径不得静默忽略用户意图；可选归因不应成为合法 `--to` 发布的硬失败点；显式校验同时保住 `--to` 必须搭配 `--notify` |
| D86 | 新增 `yaa-core::test_support::{check_consistency, check_converged}` 纯查询检查器，复用 `agent` 的 `orphan_tool_calls`/`unresolved_pending`；`crates/yaa-cli/tests/crash.rs` 以 B 状态注入为主、A 随机 `kill -9` 烟雾为辅，每格结束调用检查器 | 崩溃恢复的正确性此前只靠分散断言与人工审查；把不变量集中成可复用谓词，既让崩溃矩阵有统一判据，也避免配对语义在 e2e 与单测间漂移 |
| D87 | 展示水位由 `usize` 记录下标机械迁至 `seq`：`History` 平行保存记录与 `seq`，`last_seq()`/`start_after()`（`partition_point`）按 `seq` 定界，`load` 仍全量读取 | 归档清空后 `seq` 从 1 重算，「记录下标 == seq」的隐含假设在跨 generation 时失效，会让展示误回放或重复；按 `seq` 定界消除该耦合 |
| D88 | artifact 路径唯一化：非默认会话以安全化的完整数据库文件名作 `artifact_name`，`outputs/`、`summaries/` 与归档目录 `history/<sid>/` 均在其下再嵌一层；默认会话 `artifact_name` 为空串 | 同目录多个显式 `--session` 会共用 `outputs`/`summaries` 并混放归档；统一 artifact 前缀让默认会话零变化、兼容既有路径 |
| D89 | env 入口绝对化：`YAA_SESSION` 输出前绝对化；`--to` 与继承的 `YAA_NOTIFY`/`YAA_PARENT` 的相对值在解析目标时按 cwd 绝对化 | 相对值会随后续子进程 cwd 漂移，指向错误会话；在入口处一次性绝对化，规则单一且不改变已绝对值的语义 |
| D90 | 断电保障分级：承诺「进程崩溃级 + fsync 约束下的断电级」，不承诺未 fsync 窗口与未落盘发布，真机器断电需 dm-log-writes；限定项（edges 表、daemon、向量检索、真断电、保留策略）明确为升级判据 | 避免把「SQLite 事务 + 原子 rename」误读为任意断电无损；把不可验证的承诺写成限制与升级条件，既诚实又给出何时引入重武器的判据 |
| D91 | 可选 `[hooks] estimate`：仅有纯启发式估算已超阈值时调用；把本次投影的全部文本字段去重后以 JSON 字符串数组写入钩子 stdin，钩子须在 stdout 返回等长 JSON 整数数组；结果按「记录数 + 末条 id + 前驱摘要 id」跨步缓存；超时/非零/解析失败回退内置并只告警一次 | 允许外接精确 tokenizer 而不引入重依赖；批处理避免逐字段启动进程；失败不阻断回合，且永久禁用避免每步重复付出失败开销 |

> D6–D8、D11 中「提示词钩子」的描述已被 D19 取代：现仅存 `init` 与 `llm` 两个钩子；D5 的 `yaa-shell` 已按 D14 改名 `yaa-exec`。D31–D35 为阶段 1 修复轮落地的行为修正，其中 D31、D32 涉及投影与记录 schema，D33–D35 为执行/存储语义。D48–D58 为阶段 3 设计讨论确认，D59–D67 为 SQLite 存储迁移确认，D68–D75 为阶段 4 单 runner 与多客户端注入确认，D76–D82 为阶段 5 notify 与后台任务确认，D83–D85 为阶段 5 终审修复确认，D86–D90 为阶段 6 断电恢复与不变量收口确认。

## 对 design.md 的更正

第 1–7 条必须在动代码前确认，因为它们决定了不可变的记录 schema；第 8–11 条为实现阶段 1 时发现、事后补记，不改变已冻结的 schema，但修正 design.md 对行为的描述；第 12–13 条为本次重设计讨论中确认，与 D23–D24、D28 对应。

1. **压缩并非无模型调用**（`design.md:44`）。`@deepseek-ai/dsh-compaction-basic` 出厂必发一次摘要 LLM 调用；真正无模型的是伴生包 `dsh-compaction-tool-result-pruner`（纯头尾截断）。按 D3 两者都实现。
2. **压缩归属**：`design.md:19` 把「压缩摘要」列在 `yaa-store`，但 `design.md:45` 说压缩只发生在渲染阶段。更正为：**压缩属 `yaa-core` 的渲染管线，`yaa-store` 只负责存储**。
3. **单根 `$GLB_CFG`**：`design.md:186` 让配置、会话、历史同根。更正为按 XDG 拆分配置根与数据根（见「状态根目录」），`GLB_CFG` 仅作为配置根暴露给钩子。
4. **`turn_end` 记录**：`design.md:342` 出现但不在记录清单内。约定：**无 `tool_call` 的 `ai` 记录即为回合结束**，不引入独立类型。
5. **记录缺 `id`**：`design.md:31,32,37` 的 `user`/`ai`/`system` 无 `id`，无法支撑阶段 4 的幂等去重（`design.md:361,366`）。更正为**所有记录都有 `id`**，工具结果另用 `call_id` 关联。
6. **截断产物无载体**：`design.md:50` 未定义完整输出的路径字段。更正为 `tool_result` 增加可选 `output_files`（stdout/stderr 分两个文件）与 `truncated`。
7. **`client_id`/`turn_id`**（`design.md:301`）预留进 schema；阶段 6 判定其无运行时消费者，生产路径恒为 `None`（见「已知限制」第 35 条）。
8. **钩子不经 `shell` 工具执行**（`design.md:12`）。design 说系统生命周期钩子「触发 Shell 工具调用」实现。更正为：`init`/`llm` 钩子由 `yaa-core` 的独立执行器（`crates/yaa-core/src/hook.rs`）以 `sh -c` 运行，与 `shell` 工具**不共用策略**——钩子单流超过 1 MiB 直接报错退出，不截断、不产生 `output_files`；「钩子就是同一个 Shell 工具」的表述不成立。
9. **初始化注入排在 `user` 之前**（`design.md:27-31`）。design 的记录顺序在新设计下成立：`init` 只在无 `system(source=init)` 簿记时于写入 `user` 之前执行，注入内容由命令内的 `yaa --notify` 写入，因而物理上位于 `user` 之前。`init` 失败时仍写入 `user` 并按配置错误退出。
10. **注入不再有专用文案**。旧设计「钩子 `tool_call` 不写 `description`、由展示层现算文案」随 D19 一并移除：注入内容现在是 `system(source=notify)` 事件，展示用与工具调用一致的块样式，标题即信封中的注释。
11. **中断 `tool_result` 有两种互补补法**（`design.md:427-431`、本文件「渲染管线」失配兜底）。渲染器在内存中合成中断 `tool` 消息，保证请求体合法，**不写回事件库**；阶段 6 的「补中断 `tool_result`」则是把它作为记录持久化落盘。两者不冲突：前者保证任意时刻请求合法，后者让历史事实源自身自洽。
12. **`ai` 记录含 `reasoning`，思考内容按端点格式回传**（`design.md:38,42` 的示例未列）。`design.md:63` 把压缩参考 DSH，而 DSH 的思考模型适配证明该字段必须可回传。更正为：`ai` 新增可选 `reasoning`，落盘、可展示；`[llm] endpoint_format` 决定它是否作为 assistant 消息顶层 `reasoning_content` 回传——`deepseek`/`kimi`/`qwen`/`glm` 在请求带 `tools` 时即使思考为空也必须发 `""`，`openai` 不回传。
13. **注入分组的方向**。`design.md:57` 只说「与相邻 `user` 事件合并」，未定义方向。更正为**双向最大连续段**：首条 `user` 起，`user` 与 `system(notify)` 的最大连续段合并为一条 `user`（`\n\n` 分隔，物理顺序）。
14. **通知的 `command` 进入上下文**（`design.md:48` 称「只用于展示，不进入上下文」）。更正为：投影 `system` 时把 `command` 以 `[执行命令] <command>` 前缀置于正文最前（D31）；`command` 现为 shell 转义后的**可重放命令**，不再是「仅展示摘要」。展示层仍以 `% 命令` 呈现，且不重复拼该前缀。
15. **`--notify` 与 `shell` 工具同一条执行路径**（`design.md` 只把截断/落盘描述给工具结果）。更正为：`--notify` 的 `--` 与 stdin 两种形态都经 `yaa-exec` 的截断/落盘策略（`run` 与 `capture_reader`），把完整输出落盘为 `output_files`，通知因此可携带丢失的完整内容；限额解析链见「配置与钩子协议」规则 8。`timed_out` 语义见 D35。

## 目标架构（阶段 1）

```plaintext
Cargo.toml                      # workspace
crates/
  yaa-msg/                      # 记录类型与 serde，无内部依赖
  yaa-store/                    # 会话 SQLite 事件库同步读写、会话解析、归档、文件锁
  yaa-exec/                     # Shell 执行机制：进程组、超时、流式截断、完整输出落盘
  yaa-core/                     # config、工具定义（tools）、agent loop、渲染投影、钩子、模型调用、压缩（后期）
  yaa-cli/                      # 二进制 yaa：clap 参数、展示渲染、stdout/stderr 策略、--notify 短路写入
```

```plaintext
yaa-msg  ←  yaa-store  ←  yaa-core  ←  yaa-cli
             yaa-exec   ↗
```

- `yaa-exec` 不依赖 `yaa-msg`：它自持 `ShellLimits`，返回纯输出结构，由 `yaa-core` 组装成记录。
- 唯一工具的全部知识集中在 `yaa-core::tools::shell`：function schema、提示词 section、参数解析、错误文案、调用编排与结果转换；`yaa-exec` 只被它调用，不含工具语义（见 D14）。
- `yaa-cli` 依赖 `yaa-core`，并直接依赖 `yaa-msg`/`yaa-store` 实现 `--clear`/`--status` 的会话运维与 `--notify` 的写入；stdout/stderr 策略完全在 cli。
- `config` 类型归 `yaa-core`；`yaa-store` 只接受 `Path`，不依赖 config。
- `yaa-core` 完成渲染投影，把展示条目交给 cli；stdout/stderr 与样式策略完全在 cli（见 D15）。

## 状态根目录与路径模型

按 XDG 拆分，避免把会话数据写进配置目录：

```plaintext
$YAA_CONFIG_HOME   默认 ${XDG_CONFIG_HOME:-$HOME/.config}/yaa
  config.toml
  AGENTS.md                    # 默认 init 钩子读取（可选）

$YAA_DATA_HOME     默认 ${XDG_DATA_HOME:-$HOME/.local/share}/yaa
  sessions/<sid>/session.db                        # 事件库（WAL 副产物 -wal/-shm 同目录）
  sessions/<sid>/{runner,clear,init}.lock          # runner 选举 / clear 互斥 / 建库串行化
  sessions/<sid>/outputs/<ulid>.{stdout,stderr}.out
  sessions/<sid>/summaries/<key>.json       # 阶段 3 摘要缓存
  history/<sid>/sessions-<ts>.db                   # 归档快照（VACUUM INTO）
```

- 启动时导出 `YAA_CONFIG_HOME`、`YAA_DATA_HOME`，并令 `GLB_CFG=$YAA_CONFIG_HOME` 兼容 `design.md` 的钩子写法。
- **会话根 = 数据库文件的父目录**，`outputs/` 与锁文件挂在会话根下。`output_files` 存相对**数据根**的路径，归档后仍可解析。
- `--session` 统一绝对化后解释：目录 → `<目录>/session.db`；`.jsonl` 视为旧格式并报错（`--clear` 可做文件级归档）；其余按数据库路径。
- `<sid>` = `hex(sha256(canonical_cwd))` 前 32 位十六进制（128 位）；裸文件名数据库路径回退为绝对路径哈希。
- `--cwd <path>` 的语义固定为：**改变进程工作目录**，同时用于 sid 计算、钩子 cwd 与 Shell cwd（三处一致）。不存在时明确报错。
- `canonicalize` 失败（目录已删、符号链接断裂）时回退词法规范化并给出警告，不静默。
- 会话不承诺跨机可迁移：`init` 注入的系统提示词含机器相关信息（工作目录等），移动目录或换机后新注入会不同；已有历史不重写。

## 配置与钩子协议

```toml
[hooks]
# 无 init 簿记时执行一次；stdout 不进入上下文，
# 内容由命令内的 `yaa --notify` 写入。
init = '''
"$YAA_BIN" --notify="系统提示词" -- "$YAA_BIN" --get-prompt system
if [ -f "$GLB_CFG/AGENTS.md" ]; then "$YAA_BIN" --notify="用户总提示词" -- cat "$GLB_CFG/AGENTS.md"; fi
"$YAA_BIN" --notify="当前环境" -- echo "工作目录：$(pwd)"
'''
llm  = 'cat $YAA_CONFIG_HOME/llm.toml'

[shell]
timeout_secs = 120
max_stream_chars = 32768
head_chars = 8192
tail_chars = 2048

[agent]
max_steps = 30
request_timeout_secs = 300

# 阶段 3：渲染期压缩。模型窗口在 llm 配置（context_window）提供，缺失即禁用整支压缩。
[compaction]
enabled = true
threshold_ratio = 0.8        # 投影超过窗口 × 该比例即触发压缩
retain_ratio = 0.16          # 逐字保留的近期尾部（窗口比例）；须 < threshold_ratio
chars_per_token = 4          # ASCII token 估算密度；非 ASCII（CJK/emoji 等）每码点 1 token
max_tokens = 8192            # 摘要调用输出上限
prune_threshold_chars = 8192 # 工具消息正文超过此码点数才修剪
prune_head_chars = 4096      # 修剪保留的头部码点数
prune_tail_chars = 1024      # 修剪保留的尾部码点数
retries = 1                  # 摘要后仍超阈值的额外压缩尝试次数
```

`llm` 钩子 stdout 按 TOML 解析：

```toml
endpoint_format = "deepseek"
base_url    = "https://api.deepseek.com"
api_key     = "..."
model       = "deepseek-chat"
temperature = 0.7
max_tokens  = 4096
# 模型上下文窗口（token）；供渲染期压缩用，缺失即禁用压缩（供应侧，随 model 走）。
context_window = 128000
```

规则与不变量：

1. 钩子以 `sh -c "<命令>"` 执行，cwd 为进程工作目录，环境含 `GLB_CFG`、`YAA_CONFIG_HOME`、`YAA_DATA_HOME`、`YAA_SESSION`（本会话数据库绝对路径）、`YAA_NOTIFY`（默认通知目标）与 `YAA_PARENT`（有父会话时）以及 `YAA_BIN`（当前 `yaa` 可执行文件路径，使默认钩子不依赖 `PATH`）。会话变量统一由 `yaa_env` 构造；钩子不注入 `YAA_NOTIFY_CALL_ID`（那是 shell 工具专有，见 D76）。
2. **`init` 钩子只做副作用**：yaa 运行它并忽略 stdout；写入上下文的唯一方式是在命令里调用 `yaa --notify`。默认配置用三条 notify 拼出「系统提示词 / 用户总提示词 / 当前环境」；内置模板的「工具 section」由 `yaa-core::tools::shell` 提供（见 D14、D17）。
3. **`llm` 钩子不落历史、不渲染、不展示**：其 stdout 含密钥，且每次运行都需执行以取得配置；结果只留在进程内，不写任何事件（见 D19）。
4. **`init` 幂等由簿记保证**：触发条件是「会话中不存在 `system(source=init)` 记录」，成功后追加一条 `system(source=init, content="")` 簿记；即使钩子不产出内容，下次也不会重跑。失败时不写簿记、仍写入 `user` 并按配置错误退出，已写入的 `notify` 记录保留；因此下次运行会自动重试。
5. **`yaa --get-prompt system` 必须在加载 config、解析会话、执行任何钩子之前短路**，只打印内置模板。否则 `init` 钩子内部调用它会形成递归环。补一条递归防护测试。
6. 钩子输出有**独立上限**（不复用 Shell 的截断）：超限直接报错退出，绝不静默裁掉提示词。
7. 钩子执行有**硬超时**，并以进程组方式整组击杀（与 Shell 同一套方案），避免配置钩子挂死拖住 `yaa`。
8. **`yaa --notify` 写入协议**：`yaa --notify="注释" -- cmd` 把 argv 逐个作 POSIX 单引号转义（简单 token 不加引号，含特殊字符/空格/单引号者用单引号并转义内部单引号）后经 `yaa-exec` 执行，**复用 Shell 的截断与完整输出落盘**：正文 `body` 取被截断的 stdout，stderr 原样透传到 yaa 的 stderr，退出码沿用 cmd 的退出码（被信号杀死或超时按 1）；超限时 `output_files` 记录相对数据根的完整输出路径。`command` 记为上述**转义后可重放**的命令串；`cmd | yaa --notify="注释"` 从 stdin 读取并**复用同一套截断/落盘策略**（`yaa-exec::capture_reader`）：未超限为全量正文，超限时正文为头尾拼接并带 `... [truncated] ...` 标记、完整输入落盘并记入 `output_files.stdout`，无 `command`；注释缺省「由用户执行」。写入一条 `system { source: "notify", command?, output_files?, content: "<注释>\n<内容>\n</注释>" }`，原子追加到目标会话。
   - **限额解析优先级**（高→低）：`--notify-timeout`（仅超时）→ 环境变量 `YAA_NOTIFY_TIMEOUT_SECS` / `YAA_NOTIFY_MAX_STREAM_CHARS` / `YAA_NOTIFY_HEAD_CHARS` / `YAA_NOTIFY_TAIL_CHARS`（能解析为数字才生效）→ 配置文件 `[shell]`（路径同 `--config`/默认；读取或解析失败只打警告到 stderr 并用默认值，**绝不让 notify 失败**）→ 内置默认。
   - `shell` 工具执行时把本次调用的限额以这 4 个 `YAA_NOTIFY_*` 变量注入子进程，使工具内嵌的 `yaa --notify` 采用同一套参数（调用者透传）。
    - 目标解析顺序 `--to` → `--session` → `YAA_NOTIFY`（非空且 ≠ 自身）→ `YAA_SESSION`（非空）→ 当前 cwd 默认会话（阶段 5，见 D77）。
   - `--notify` 是短路写入路径：不进入 agent loop、不触发 `init`；仅为解析限额按需读取配置，且失败不影响写入。

## 记录 schema（阶段 1 冻结）

每行一条 JSON，带标签枚举，`ts` 为 Unix 毫秒：

```plaintext
user        { type, id, ts, content, client_id?, turn_id? }
ai          { type, id, ts, model, tool_schema_hash, content, reasoning? }
tool_call   { type, id, ts, origin: ai|user, call_id, command, description? }
tool_result { type, id, ts, call_id, stdout, stderr, code, timed_out,
              truncated, output_files?: { stdout?, stderr? } }
system      { type, id, ts, source, origin?, call_id?, command?, output_files?, content }
approval_pending  { type, id, ts, call_id, command, matched_rule }
approval_resolved { type, id, ts, call_id, decision: approve|deny }
```

- `id` 为 ULID，全记录唯一，阶段 4 以此幂等去重。
- `call_id` 为模型返回的工具调用 id，`tool_result` 靠它配对。
- `ai` 与其后紧跟的 `tool_call(origin=ai)` 记录构成一个助手步骤，**两者以单次缓冲原子追加**（见下）。
- `system.source` 目前有 `notify`（`yaa --notify` 写入的通知，正文内联 `<注释>` 信封）与 `init`（初始化簿记，空内容不渲染）。
- `system.origin`/`system.call_id` 为阶段 5 归因字段（D78）：`origin` 是发布者会话库绝对路径，`--notify` 总是写；`call_id` 是产生通知的工具调用 id，仅 `YAA_NOTIFY_CALL_ID` 合法时写。二者 `Option` + `skip_serializing_if`，旧记录缺字段解析为 `None`，无需 SQLite 迁移，也不参与渲染分组。`is_injectable_system` 是 display/render/inbox 共用的唯一 `system` 白名单谓词。
- 可选 `command` 是产生该通知的 **shell 转义后可重放命令**（`sh -c` 重放等价原 argv；简单 token 不加引号，含特殊字符者用单引号并转义内部单引号）。展示层以 `% 命令` 呈现，投影时以 `[执行命令] <command>` 前缀进入上下文（D31）；stdin 形态与簿记无此字段。
- 可选 `output_files` 与 `tool_result` 同构（相对数据根），承载 `--notify`（命令或 stdin）超限截断后落盘的完整输出；正文自身只含 `yaa-exec` 的头尾截断标记，截断提示与相对路径由投影层 `display::system_text` 追加（D32）。旧记录缺该字段解析为 `None`。
- `ai.reasoning` 为可选思考内容，落盘、可由 `--verbose` 展示，并按 `endpoint_format` 决定是否回传（D23/D24）。
- 相对旧 schema 的移除项见 D21。
- `approval_pending` / `approval_resolved` 为阶段 2 引入的审批记录（D40–D46）：前者承载一次待批调用（`call_id` 指向已落盘的 `tool_call`，`matched_rule` 为命中的规则文本），后者承载决策（`decision: approve|deny`）。两者都用 `call_id` 与工具调用配对，**不参与模型消息投影**，也不改变 `assistant`/`tool` 配对。

### 追加的原子性

**BEFORE（JSONL，已废弃）**：整批序列化后一次 `O_APPEND` + `fsync`；断电可能留下撕裂尾行，`load`/`append` 需修补残段。

**AFTER（SQLite）**：`yaa-store::append(&[Record])` 在一个 `BEGIN IMMEDIATE` 事务中把整批记录逐条 `INSERT` 到 `events`，随后 `COMMIT`。一批记录要么全部可见、要么全部不可见，半行撕裂问题由事务消除；WAL + `synchronous=FULL` 保证已提交批次在断电后仍在。重复 `id` 使整批回滚并报 `DuplicateId`，`append_idempotent` 用 `ON CONFLICT(id) DO NOTHING` 供恢复/消费路径幂等补写；空批直接返回且不创建任何文件。BUSY 时从 `BEGIN IMMEDIATE` 重试整个事务，绝不续写已开始的事务。

## 渲染管线与分层装配

`yaa-core` 的渲染是**纯函数**，把事件日志投影为 OpenAI 消息序列（压缩与 pruner 都插在这一层，阶段 3）。装配按稳定性分层（D22、`docs/dsh-context.md`）：

| 日志中的位置 | 投影结果 |
| ------------ | -------- |
| 首条 `user` 之前的所有 `system` | 按物理顺序用空行拼接为**一条** `system` 消息（稳定前缀）；空内容与 `init` 簿记跳过 |
| `user` 与 `system(source=notify)` | 首条 `user` 起，二者组成的**最大连续段**按物理顺序用空行拼接为**一条** `user` 消息（双向合并，D28）；段外的通知自然单独成一条 `user` |
| `system` 正文 | 取 `display::system_text`（`content`，带 `output_files` 时追加统一截断提示与相对路径）；若 `command` 为 `Some`，在最前拼 `[执行命令] <command>`（D31） |
| `ai` + 其后 `tool_call(origin=ai)` | 一条 `assistant` 消息，工具调用进 `tool_calls` |
| 配对 `tool_result` | `tool` 消息，`tool_call_id` 关联；按 `call_id` **全日志一次建表**配对，不要求结果紧邻 `tool_call` |
| `tool_call(origin=user)` | 文本（`user` 或 `system`），**绝不投影为 `tool`** |
| `system(source=init)` | 不投影（簿记） |
| `ai.reasoning` | 落盘的思考内容；按 `endpoint_format` 决定是否作为 assistant 顶层 `reasoning_content` 回传（见下） |

**思考内容回传**：`endpoint_format` 为 `openai` 时 `reasoning` 只落盘与展示、不投影；为 `deepseek`/`kimi`/`qwen`/`glm` 时，把该 assistant 消息的 `reasoning_content` 设为 `reasoning` 的值（**带 `tool_calls` 时即使为空也写 `""`**，否则 DeepSeek 系返回 400）。未知的 `endpoint_format` 取值按配置错误处理（退出 3），不静默降级为 `openai`。请求侧开关（Qwen `preserve_thinking`、GLM `clear_thinking`）阶段 1 不引入，作为将来扩展点记录。Anthropic/Responses 的签名类结构化 metadata 属后续 provider，阶段 1 不预留字段。

- **分组规则**：按 `ai` 步骤切分，步骤内工具调用按出现顺序排列；`tool_result` 按 `call_id` **全日志一次建表**配对（首个同 `call_id` 结果生效），不要求紧邻 `tool_call`。
- **失配兜底**：`ai` 步骤里的 `call_id` 若无对应 `tool_result`（断电、中断），渲染器合成一条 `tool` 消息（内容为中断标记），保证 `assistant.tool_calls` 后紧跟配对的 `tool` 消息，请求体始终合法。
- **空回合**：既无文本又无工具调用的 `ai` 记录不投影消息（避免产生 `{"role":"assistant"}`）。
- 工具以 OpenAI function schema 暴露，仅一个 `shell` 函数，参数 `command`（必填）与 `description`（可选）。
- `tool_schema_hash` = hash(本轮暴露的 tools schema)，写入 `ai` 记录，用于历史兼容与缓存失效判定；**它只覆盖函数 schema，不覆盖系统提示词文本**。

### 缓存友好

- 稳定前缀（`system`）只在空会话初始化时一次成型，后续运行不重排，KV Cache 前缀可命中。
- 运行期新增内容只追加在历史尾部：模型工具调用产生 `assistant`/`tool`，外部通知产生尾部的 `system(source=notify)` 并渲染为尾部 `user` 消息。
- 不重写历史、不重新提升旧记录；任何注入都以追加事件表达。

### 展示

展示同样从这份日志派生，不引入第二套事件流：

- 展示条目与模型消息同源；`system(source=notify)` 用与工具调用一致的块样式：默认 `# 注释` + `% 命令`（可重放命令，无 `command` 时只给标题），`--verbose` 追加 `system_text` 全文（含截断提示）；不设专用格式（D16）。命令前缀 `[执行命令]` 只属于上下文投影，展示侧不重复拼。
- 会话首条 `user` 之前的注入只在空会话首轮出现一次；之后运行不再出现（`init` 不重跑）。
- `--quiet` 抑制全部过程输出，但**致命错误仍打印到 stderr**（否则用户看不到失败原因）；`--verbose` 追加 `ai.reasoning`（思考内容）、命令输出与注入内容全文。
- 展示文案与渲染角色由投影层决定，`yaa-cli` 只决定样式与流向。

### 一致性不变量

「展示中属于上下文的文本集合」恒等于「投影为模型消息的文本集合」；`reasoning`（展示且可能回传）与展示专用的块标记/标题（`# `、`% `、`---`、`# 思考`）不计入该集合；通知的 `command` 与工具调用的 `description` 同时出现在两侧，计入集合（D29、D31）。测试直接断言该不变量（见「测试与验证策略」）。

## 唯一工具：Shell

工具定义（function schema、提示词 section、参数解析、错误文案）在 `yaa-core::tools::shell`，其提示词 section 供内置系统模板拼装；本节描述 `yaa-exec` 提供的执行机制。两者以一个调用边界衔接：`tools::shell` 解析参数、调用 `yaa_exec::run`，并把输出转换为记录。

- 执行 `sh -c <command>`，`stdin = null`（否则 `echo ... | yaa` 时工具子进程会偷走 stdin）。
- **进程组**：`process_group(0)` 启动；超时时对整组 `killpg`，避免 `sh -c` 的孙进程持有管道导致读取挂死或孤儿继续运行。读取线程的等待也纳入同一时限：直接子进程退出但孙进程仍持有管道时不得无限等待，超时后放弃该流输出（`// PJ568 方案` 标注）。
- **流式有界读取**：边读边写完整输出文件，内存中只保留头 `head_chars` 与尾 `tail_chars`。禁止先 `wait_with_output` 再截断（`yes` 之类会 OOM）。
- 每个流一个完整输出文件，文件名 = 本地生成的 ULID + 流后缀（**不用模型给的 `call_id` 当文件名**，避免路径穿越与覆盖）；`output_files.stdout` / `output_files.stderr` 记相对**数据根**的路径；会话在数据根之外（`--session <任意路径>`）时回退记绝对路径，绝不静默留空。
- 截断按**流分别计**的字符数（stdout、stderr 各自独立），入记录时做 lossy UTF-8，原始字节写文件。
- 超时以 `timed_out: true` 标记，不借用 `code=-1`。
- 阶段 1 默认 allow，无规则（审批在阶段 2；审批只作用于 `origin=ai` 的工具调用，钩子与通知不经过 `shell` 工具）。
- 输出文件清理策略：**`--clear` 不移动 `outputs/`**，归档记录中的相对路径继续有效；磁盘无界增长作为已知限制记录，阶段 3 再引入保留策略。

## Agent loop 与并发形状（阶段 1 历史示意）

入口形状保持「先发布消息 → 再决定 runner/follow」；阶段 1 已用 `runner.lock` 实现逻辑单 runner，文件 inbox 已在阶段 4 落地（见 D68–D75 与「阶段 4」节）。以下伪代码保留阶段 1 的形状，具体行为以阶段 4 的实现为准。

```rust
async fn main_flow(cfg) -> Result<ExitCode> {
    let session = resolve_session(&cfg)?;          // canonical cwd -> sid / --session

    if !store::has_init_bookkeeping(&session)? {   // 无 init 簿记：先初始化，再收 user
        init_hook(&session, &cfg).await?;          // 经 `yaa --notify` 侧写事件 + init 簿记
    }

    let user = build_user_record(prompt);
    store::append(&session, &[user]);              // 阶段 1 直接追加；阶段 4 起先发布 inbox 再抢锁

    let guard = try_acquire_runner(&session);      // runner.lock 非阻塞选举
    match guard {
        Some(g) => run_agent_loop(&session, &cfg, g).await,
        None    => follow(&session).await,         // 阶段 1 不可达；阶段 4 已实现
    }
}

async fn run_agent_loop(session, cfg, guard) -> Result<ExitCode> {
    let llm = resolve_llm_hook(cfg)?;              // 每次运行一次，不写事件
    let mut watermark = store::last_seq(session)?;  // 展示增量水位：末条记录的 seq
    for step in 0..cfg.agent.max_steps {
        let records = store::load(session)?;       // 每步重读（design.md:59）
        let messages = render(&records);           // 事件日志 → ChatMessage（纯函数）
        sink.push(display::items_after(&records, watermark));  // 按 seq 定界展示新增记录，cli 只决定样式
        let resp = chat_with_retry(&llm, &messages, &[tools::shell::schema()]).await?;
        store::append(session, &ai_step_records(resp))?;   // ai + 其 tool_calls 原子一批
        if resp.tool_calls.is_empty() { return Ok(ExitCode::Success); }
        for call in &resp.tool_calls {
            let out = tools::shell::run(call, &cfg.shell).await?;
            store::append(session, &[out.into_record()])?;
        }
    }
    Ok(ExitCode::MaxSteps)
}
```

- `yaa-store` 保持**同步 API**（批量追加、按 `seq` 增量读、归档）。在 core 调用点用 `spawn_blocking` 包装，不做成假 async。
- `read_after(&session, after_seq) -> (Vec<(seq, Record)>, new_seq)` 为跟随/增量展示预留；当前 core 以 `load` 全量读取，展示水位按记录 `seq` 用 `partition_point` 定界（不再是记录下标）；归档重置后 `seq` 从 1 重算。`load` 按 `seq` 顺序读取全部事件。存储层只认 `Path`，不依赖配置类型。
- `load` 每步重读，阶段 4 的 inbox 插入与 `yaa --notify` 追加才能在下一轮被看到。
- **无初始化簿记才初始化**：会话中不存在 `system(source=init)` 记录时执行 `init` 钩子（D20）；钩子内经由 `yaa --notify` 写入的通知先落盘，随后才是 `init` 簿记与 `user` 记录。`init` 失败仍写入 `user`，再按配置错误退出；因不写簿记，下次运行自动重试。
- **`llm` 不写事件**：只解析配置供进程使用；解析失败按配置错误退出（见 D19）。
- **空回复**：无文本且无工具调用时，记一条空 `ai` 记录、向 stderr 发提示并结束回合（退出 `0`），且该记录不投影消息。

### 错误与退出策略

- 模型 429/5xx/网络错误：指数退避，总尝试最多 3 次（首发 + 2 次重试）；仍失败则以模型错误退出。此时历史停在「工具批次已完成」的合法状态，下次运行可续接。
- 退出码：`0` 成功，`1` 通用错误，`2` 用法错误，`3` 配置/钩子错误（含钩子执行失败），`4` 模型错误，`5` 达到 `max_steps`，`130` 被 SIGINT 中断（Ctrl-C）。
- 空回复（无内容且无工具调用）视为回合结束，退出 `0`，并给出 stderr 提示。
- 半批次收尾：模型失败或 Ctrl-C 后，已写入的 `tool_result` 保留；缺失的结果由渲染器兜底（见上）。

## 审批（阶段 2）

审批把「哪些 `origin=ai` 的 shell 命令要问、哪些禁」做成配置（D40–D46）。**它是防手滑与审计手段，不是安全边界**：命令资源是 best-effort 拆分的文本，无法穷举 shell 的全部等价写法，包装器、重定向、变量展开等绕过依然可达。真正想收紧时用窄 `allow` + 精准 `deny`，而不是指望审批流拦截一切。

### 配置

```toml
[approval]
rules = [
  { pattern = "rm *",    action = "deny" },
  { pattern = "git *",   action = "allow" },
  { pattern = "curl *",  action = "ask" },
]
```

- `pattern` 为 glob（`*` 跨任意字符含空格，`?` 单字符），对**整段命令**做整串匹配。
- `action` ∈ `allow` / `ask` / `deny`；无规则命中时默认 `allow`。
- 裁决顺序（D41）：先全局扫 `deny`，任一命中即拒；否则在命中的规则中按「首个通配符前的字面字符数」取最大者（更具体者胜），同分取靠后者；仍无命中为 `allow`。

### 命令资源与聚合

模型的一次 `shell` 调用可能含多个命令段。评估前先做**朴素拆分**：按 `;`、`&&`、`||`、`|`、换行切段，剥离每段前导的 `VAR=val`。逐段求裁决后聚合（D42）：任一 `deny` → deny；否则任一 `ask` → ask；否则 allow。任一步骤解析失败（如引号不配平）按 `ask` 处理，绝不静默放行。

### 非交互落地

| 裁决              | 记录                                                              | 行为                                            |
| ----------------- | ----------------------------------------------------------------- | ----------------------------------------------- |
| `allow`（含默认） | 不写审批记录                                                      | 直接执行                                        |
| `deny`            | `approval_pending` + `approval_resolved: deny` + 失败 `tool_result` | 提示模型被拒，**继续循环**；不退出              |
| `ask` + `--auto`  | `approval_pending` + `approval_resolved: approve`                  | 直接执行（`deny` 不受 `--auto` 影响）             |
| `ask`（默认）     | `approval_pending`                                                 | stderr 打印待批清单，**以退出码 6 退出**，不写 `tool_result` |

- **待批清单**：每条 pending 打印其 `call_id`、`matched_rule` 与 shell 转义后的可重放命令。
- **`yaa --approve`**（无参）：取**最旧**未决 pending（有 `approval_pending`、无对应 `approval_resolved`），执行其命令，写 `approval_resolved: approve` 与 `tool_result`，然后**继续 agent loop**（不写 `user`、不重跑 `init`）。一次调用只放行一条；多个命令需连续执行多次（忠于 `design.md`）。
- **普通 run 放弃旧待批**：启动若发现未决 pending，先补 `approval_resolved: deny` 并落一条失败 `tool_result`，再走正常流程（D45）。
- **`--auto`**：自动放行所有命中 `ask` 的调用；显式 `deny` 仍强制（D44）。

## CLI（阶段 1）

```plaintext
yaa [PROMPT]...              # 多个位置参数以空格连接；空则从 stdin 读
yaa --clear                  # 归档当前会话
yaa --status                 # 打印会话摘要
yaa --session <path>         # 指定会话文件
yaa --get-prompt system      # 打印内置系统提示词后退出
yaa --notify="注释" -- cmd   # 把命令输出作为通知写入当前会话后退出
cmd | yaa --notify="注释"    # 从 stdin 读并写入通知后退出
yaa --notify="注释" --to <path> -- cmd  # 写入显式目标会话（必须已存在，不创建）
yaa --config <path>
yaa --cwd <path>             # 改变进程工作目录
yaa --verbose                # 追加思考内容与命令输出
yaa --quiet                  # 只打印最终回复
yaa --approve                # 放行最旧一条待批命令并继续循环
yaa --auto                   # 自动放行命中 `ask` 的命令（`deny` 仍强制）
yaa --json                   # 以 JSONL 输出展示事件（与 --quiet/--verbose 冲突）
yaa --no-store               # 本次运行使用临时会话，退出即淘
```

- `--notify` 是短路路径：不加载 `llm` 配置、不进入 agent loop，只解析目标、执行命令或读 stdin、发布一条 `system(source=notify)` 到 inbox（发布失败可重试时降级 spool）。
- 过程记录默认打到 stderr（见 D13、D15、D16）：注入与通知用与工具调用一致的块样式（`# 注释` + 内容），工具调用用 `# 描述` + `% 命令`；块间以 `---` 分隔；非末步的模型正文也在此打印，末步回复只走 stdout。
- 展示条目与模型消息同源，由 `yaa-core` 的投影产出；`--quiet` 抑制全部过程输出。

`--json` 与 `--no-store` 已于阶段 3 落地（D57/D58）；`--notify` 的 `--to` 与跨会话解析已于阶段 5 落地（D77）。

## 阶段划分与验收

每阶段独立可验证、可提交。原子修改后必须通过 `cargo check --all-targets`、`cargo clippy`、`cargo test`。各阶段改动在独立 Git 工作树完成、验收后合并（阶段 1 的历史开发见 `rewrite/yaa-cli` 分支）。

### 阶段 1：最小闭环

内部顺序：**记录 schema（含 `reasoning`）+ store + 渲染分层 golden 测试 → `tools::shell` 内聚与提示词组装 → `--notify` 写入路径与 `YAA_SESSION` → init/llm 钩子与模型适配（含 `endpoint_format` 回传）→ agent loop → CLI 展示**。

验收：

1. `yaa "你好"` 在空会话先执行 `init`：`session.db` 的 `events` 表依次留下三条 `system(source=notify)`、一条 `system(source=init)`、`user`、`ai`；无工具调用时打印模型回复。
2. 再次 `yaa "继续"` 不重跑 `init`（无新增 notify 记录），能读到上一轮并正确渲染续接；**断言第二跳请求体**符合 OpenAI 消息序列。
3. 渲染分层：首条 `user` 之前的通知合并为一条 `system` 消息；`yaa --notify` 在首条 `user` 之后写入的通知与相邻 `user` 合并为一条 `user` 消息（信封保留）。
4. 构造需要 Shell 的多步任务，工具结果回填并循环至最终回答。
5. `--clear` 把当前事件库快照到 `history/<sid>/sessions-<ts>.db`，清空 `events` 并令 `generation+1`；`outputs/` 留在原地，归档记录的相对路径仍有效；同一 `ts` 再次归档时目标改为 `sessions-<ts>-<n>.db`（n 从 1 递增），不覆盖既有归档。
6. 集成测试用本地假 OpenAI 服务器，断言事件序列与 Shell 副作用；**断言请求体与事件库均不含 `api_key`**。
7. 递归防护测试：`init` 钩子内部调用 `yaa --get-prompt system` 能正常返回而非递归。
8. 中断测试：模型返回 `tool_calls` 后进程被杀，下次运行渲染器合成中断 `tool` 消息，请求体合法。
9. 展示一致性：对同一份记录，展示中属于上下文的文本集合与模型消息的文本集合完全一致（`reasoning` 与展示专用的块标记/标题豁免；通知的 `command`、工具调用的 `description` 两侧都有，计入，D29/D31）。
10. 初始化只展示一次：首次运行 stderr 出现三个注入块（默认 `# 注释` + `% 命令` 摘要，`--verbose` 追加内容全文）；第二次运行不再出现。
11. `llm` 钩子不写任何记录、不出现在 stderr（默认与 `--verbose` 均不展示）。
12. `--get-prompt user` 为用法错误（退出码 2）；`--get-prompt system` 输出的工具 section 与 `tools::shell` 的 schema 名称/参数一致（防漂移）。
13. `--notify` 两形态与限额：`yaa --notify="注释" -- cmd` 写入 `system(source=notify)`，`command` 为 shell 转义后的可重放命令，超限时经 `yaa-exec` 截断、正文含头尾标记且 `output_files.stdout` 为相对数据根的路径；`cmd | yaa --notify="注释"` 写入同名记录但不含 `command`、复用同一截断/落盘策略（超限时正文含头尾标记且完整输入落盘为 `output_files.stdout`，短输入无 `output_files`）；两者正文均为 `<注释>…</注释>`。再断言：`--notify-timeout` 生效（超时按 1 退出并提示）、环境变量与 `[shell]` 的限额解析链生效、命令对模型可见（第二跳请求含 `[执行命令]` 前缀）、旧 `system` 记录缺 `output_files` 仍可解析。
14. 思考回传：`--verbose` 展示 `ai.reasoning`；`endpoint_format="deepseek"` 时第二跳请求体中带 `tool_calls` 的 assistant 消息含 `reasoning_content`（思考为空时值为 `""`），`endpoint_format="openai"` 时不含该字段。
15. 会话目标：`yaa --session <path>` 在空会话触发 `init`，三条 notify 经 `YAA_SESSION` 写入该 `<path>`；旧 `.jsonl` 会话由 `yaa --status` 报旧格式错误并提示 `--clear`（D65）。

### 阶段 2：审批

内部顺序：**记录类型（`approval_pending`/`approval_resolved`）+ 规则解析与裁决单测 → 命令拆分与聚合 → agent loop 拦截与退出码 6 → `--approve` 恢复路径 → `--auto` → 展示与 `--config` 贯通 → A4/A5/A6 修复**。

范围（D40–D47）：配置与裁决、命令资源聚合、非交互待批与 `--approve` 恢复、`--auto`、`deny` 的模型可见拒绝、新提示词放弃旧待批。**不含** `--json`/`--no-store`（后移阶段 3）。审批只作用于 `origin=ai` 的工具调用；`init`/`llm` 钩子与 `yaa --notify` 不经 `shell` 工具，天然豁免。

验收：

1. 裁决单测：deny 短路（含 `*: ask` 尾随不覆盖具体 deny）；最具体优先（`git push *` 胜过 `git *`）；同分取后者；无匹配默认 allow；`?` 与 `*` 语义；命令拆分（`a && b`、`VAR=x b`）后按段聚合。
2. `ask` 拦截：命中 `ask` 时事件库出现 `approval_pending`、stderr 打印可重放命令与 `matched_rule`、进程以 `6` 退出、且**不写** `tool_result`。
3. `--approve`：对上述会话执行 `yaa --approve`，命令被执行并写 `approval_resolved: approve` + `tool_result`，随后继续循环直至最终回复；不写 `user`、不重跑 `init`；多个 pending 时每次只放行最旧一条。
4. `--auto`：`yaa --auto "……"` 自动放行命中 `ask` 的调用并完成循环；显式 `deny` 仍被拒。
5. `deny`：命中即写 `approval_pending` + `approval_resolved: deny` + 失败 `tool_result`，进程不退出，模型在下一跳能看到被拒结果。
6. 新提示词放弃旧待批：存在未决 pending 时执行普通 `yaa "新提示词"`，旧 pending 先补 `approval_resolved: deny` 与失败 `tool_result`。
7. 审批记录不进入模型消息投影；`approval_pending`/`approval_resolved` 不改变 `assistant`/`tool` 配对。
8. A4：模型返回 `tool_call.arguments` 非字符串或缺 `id` 时不再静默丢弃——报明确的模型错误（退出码 4），测试断言可观测。
9. A5：末步为空回复时 stdout 不打印本轮非末步正文；`yaa 你好 > 回复.txt` 只含最终回复。
10. A6：`yaa --config <path>` 在 `init` 钩子内 `yaa --notify` 的限额解析与主进程一致（同一 `[shell]`）。
11. `--get-prompt`/`--clear`/`--status`/`--notify` 等短路路径不因新增审批逻辑改变行为；`--approve`/`--auto` 与 `--notify` 组合按用法错误拒绝。

### 阶段 3：压缩、`--json` 与 `--no-store`

内部顺序：**配置与 token 估算 → pruner → 选区间 → 侧车摘要缓存 → 摘要调用与编排 → 渲染期注入 → 展示条目 → `--json` → `--no-store` → e2e**。

范围与行为（D48–D58）：

- **压缩**在渲染期进行，不改 `session.db`。触发链：投影 token ≥ `floor(context_window × threshold_ratio)` → 先 pruner（只修剪 `tool` 消息正文，跳过 `system`）→ 重测 → 仍超才摘要。
- **选区间照搬 DSH**：跳过稳定 `system` 头；从尾部累计 token 到 `retain_tokens`（= `floor(context_window × retain_ratio)`）；切点向头部回退到工具配对平衡处（不切分 `assistant.tool_calls` 与配对 `tool`）。
- **摘要链式递进**：输入 = 稳定前缀（可选）+ 被遮蔽区消息（含旧摘要消息）+ 指令；产物写侧车。渲染时只应用链尾摘要；`previous` 指向前驱，`covers` 为被遮蔽记录 id 序列。
- 摘要成功但**不比原文小**、或调用失败/超时/空输出 → 警告并降级（D55）。
- 展示保持一致（D56）：展示仍从原始记录增量投影，压缩额外展示一条摘要条目。

验收：

1. 配置：`[compaction]` 全字段解析与校验（比例 ∈ (0,1]、`retain_ratio < threshold_ratio`、`chars_per_token ≥ 1`、`prune_head + prune_tail + 标记 ≤ prune_threshold`、`enabled` 布尔）；`llm` 配置的 `context_window` 解析与校验，缺失时压缩静默禁用。
2. token 估算：ASCII 按 `chars_per_token`、非 ASCII 每码点 1 token，各字段独立取整后求和；`content`、`tool_calls` 参数与 `reasoning` 计入；含大量 CJK 的历史可在超出窗口前触发压缩；配置 `[hooks] estimate` 时预计算值优先、失败回退内置。
3. pruner：仅 `tool` 消息被修剪、`system` 不受影响；未超 `prune_threshold_chars` 不动；修剪后码点数 ≤ 阈值且严格小于原文；按 Unicode 码点（不拆代理项对）。
4. 选区间：稳定 `system` 头永不入区间；尾部按 `retain_tokens` 保留；切点回退到工具配对平衡；无可用区间返回 `None`（不产生空摘要）。
5. 触发：投影低于阈值不压缩、不产生侧车文件；跨过阈值先 pruner，修剪后已低于阈值则跳过摘要。
6. 摘要与侧车：`summaries/<hash>.json` 生成且含 `covers`/`previous`/`model`/`summary`；**断言 `session.db` 本次未新增压缩记录**；摘要不比原文小时不采用。
7. 链式：第二次压缩的摘要请求体含旧摘要文本、**不含**已被旧摘要遮蔽的原文。
8. 降级：摘要端点返回错误时，stderr 有警告、模型调用仍携带完整历史继续（退出码不为 4/3）。
9. 展示：压缩触发时 stderr 出现一条摘要条目（默认摘要，`--verbose` 全文）；展示不重放被遮蔽原文；D15 不变量测试在「无压缩生效」时逐字一致。
10. `--json`：stdout 为 JSONL，每条 `DisplayItem` 一行 JSON（含 `type`），最终回复在其中；退出码不变；与 `--quiet`/`--verbose` 组合为用法错误。
11. `--no-store`：不创建也不修改既有会话文件；退出后临时目录无残留；与 `--session` 组合为用法错误；`--notify`/`--clear`/`--status` 与 `--no-store` 组合为用法错误。
12. `--get-prompt`/`--clear`/`--status`/`--notify`/审批行为不因阶段 3 改动而改变。
13. 原子验证：`cargo check --all-targets`、`cargo clippy`、`cargo test` 全绿；`autocorrect --lint docs crates` 通过。

### 阶段 4：单 runner 与多客户端注入（已实现）

行为（D68–D75）：

- **排空式单 runner**：一次 `run` 处理到「历史空闲 + inbox 空 + 无未决审批」才退出；`runner.lock` 排他选举，不是常驻 daemon。
- **文件 inbox**：`<session.root>/<prefix>inbox/{tmp,pending,init,rejected}`；发布 = tmp → fsync → rename → fsync 目录，无锁多进程安全；载荷是完整 `Record` JSON，消费用 `append_idempotent` 后删除文件（不建 `consumed/`）。
- **先发布后抢锁**：`run` 在 `clear.lock` 共享临界区内做归档预检、原子捕获 `(generation, last_seq)` 并发布 own，释放共享锁后再抢 `runner.lock`。
- **`--notify` 只发布**：init 钩子内写 `init/`，否则写 `pending/`；不抢锁、不跟随、不跑钩子或 agent；发布带 generation 守卫。
- **follower/接管**：`read_after_with_generation` + 20→250 ms 退避 + `DisplayProjector` 增量展示 + generation 监控；旧 runner 退出后只尝试接管一次；`PendingPolicy` 区分新 `User`（Abandon）与纯 notify（Keep）。
- **展示与归属**：水位 `(generation, seq)`；最终回复按 own record id 定位其后第一条轮末 `ai`；`max_steps` 按 user 批次重置（模型调用计数）。
- **运维**：`--clear` 持锁后拒绝仍有 pending/init 的归档；`--status` 只读显示计数；锁与 inbox 路径按完整数据库文件名唯一化。

验收覆盖（关键测试，概括范围）：inbox 并发发布、幂等消费、非法载荷与超限隔离、路径唯一化；两进程同时 run 仅一个成为 leader；注入后 kill -9 重启不丢不重；注入不打断工具配对；init 顺序与展示恰好一次；follower 不回放历史、增量投影等价全量；接管不重放；`--notify` 不启动 agent；`--clear` 遇 pending 拒绝；generation 变化退出；最终回复归属；建库 `0→1` 不误判归档；`--status` 只读不 reject。

### 阶段 5：notify 与后台任务（已实现）

行为（D76–D85）：

- **环境链**：`yaa_env` 纯函数统一钩子、shell 工具与 `--approve` 执行路径；`YAA_SESSION` 总是自己，`YAA_NOTIFY` 只继承指向别处的值，`YAA_PARENT` 保留中间层或孵化者，`YAA_NOTIFY_CALL_ID` 仅 shell 工具注入。
- **`--to` 与优先级**：`--to` > `--session` > `YAA_NOTIFY` > `YAA_SESSION` > cwd；显式 `--to` 严格校验且绝不创建，普通 `--notify` 警告并创建；`--to` 空串、与 `--session` 指向不同库、与 `--no-store` 同用为用法错误；`--approve` 不认 `--to`。
- **归因**：`System.origin` 总写、`System.call_id` 合法才写；旧记录兼容、无 SQLite 迁移；`is_injectable_system` 统一 display/render/inbox 谓词。
- **spool**：可重试发布失败/预检 BUSY（BUSY/IO）降级暂存，命令退出码不变；runner 在崩溃恢复后、`init` 前只消费 spool 自身（不合并 inbox）；follower 只提示滞留；`--status` 显示计数；`--clear` 拒绝非空 spool。
- **后台回注**：shell 提示词给出 `{ ... | yaa --notify="..." --to "$YAA_SESSION"; } >/dev/null 2>&1 &` 与 `setsid ... >/dev/null 2>&1 &` 范式（后台作业必须重定向输出或脱离工具进程组），以及 `--to "$YAA_PARENT"`；`call_id` 沿环境链继承父的工具调用 id。

验收覆盖（关键测试，概括范围）：I1 环境链（含嵌套 `YAA_PARENT`）；I2 优先级矩阵；I3 严格拒绝且不创建目标；I4 子→父通知落库带 `origin`/`call_id`；I5 无 runner 时 pending 滞留 + status 计数 + 零模型请求；I6 spool 降级与补投幂等；I7 spool 非空拒绝 clear；I8 旧 JSON 兼容 + 非法 `call_id` 忽略；I9 display/render 谓词一致；I10 请求期间通知不丢不重；I11 双投幂等；I12 kill -9 后 append/删除窗口去重。

### 阶段 6：断电恢复与不变量（已实现）

行为（D86–D90）：

- **不变量检查器**：`yaa-core::test_support::{check_consistency, check_converged}` 供 e2e/harness 与单测共用，复用 `agent` 的 `orphan_tool_calls`/`unresolved_pending` 谓词，避免配对语义漂移。
- **崩溃注入矩阵**：`crates/yaa-cli/tests/crash.rs` 以 B 状态注入为主、A 随机 `kill -9` 烟雾为辅，覆盖 append 事务、工具结果补写、inbox 幂等重放、归档 `VACUUM`/`rename`/清空事务、`init` 建库自愈、模型请求中 kill、两次接管；每格结束调用检查器。
- **水位迁 `seq`**：`History` 平行保存记录与 `seq`，`emit_new` 用 `partition_point` 按 `seq` 定界；归档重置后 `seq` 从 1 重算，旧水位超前时按新历史重新展示。
- **artifact 唯一化**：`outputs/`、`summaries/` 与归档目录按安全化的完整数据库文件名（`artifact_name`）嵌套，同目录多个显式会话互不混放；默认会话路径不变。
- **env 入口绝对化**：`YAA_SESSION` 输出前绝对化；`--to` 与继承的 `YAA_NOTIFY`/`YAA_PARENT` 的相对值在解析目标时按 cwd 绝对化。
- **保障分级**：承诺「进程崩溃级 + fsync 约束下的断电级」，不承诺未 fsync 窗口与未落盘发布，真机器断电需 dm-log-writes（见「升级判据」）。

验收覆盖（关键测试，概括范围）：12 项崩溃矩阵全部通过并调用检查器；不变量检查器单测与 e2e 冒烟（`converged_run_passes_invariant_checks`）；水位跨归档重置单测（`emit_watermark_uses_seq_across_archive_reset`）；双显式会话 `outputs/`、`summaries/` 与归档互不混放 e2e（`explicit_sessions_keep_outputs_and_archives_separate`、`explicit_sessions_keep_summaries_separate`）；相对 env 落 cwd 会话 e2e（`relative_notify_env_lands_in_cwd_session`）。机械判据：`rg "watermark: &mut usize" crates` 为空；非默认会话的 `outputs`/`summaries` 路径带 `artifact_name` 前缀。

注入点 → 测试名映射：

| # | 注入点 | 手法 | 测试名 |
| - | ------ | ---- | ------ |
| 1 | append 事务中 | A | `kill_during_append_never_leaves_half_batch` |
| 2 | `ai`+`tool_call` 批后、工具执行中 | A | `restart_after_sigkill_recovers_interrupted_tool_call` |
| 3 | 工具执行中、结果前 | A | 同 #2（`restart_after_sigkill_recovers_interrupted_tool_call`） |
| 4 | `tool_result` 后、轮末 `ai` 前 | B | `existing_tool_result_is_not_duplicated_on_resume` |
| 5 | notify 发布中（tmp 后 rename 前） | A+B | `notify_cleans_stale_inbox_tmp` |
| 6 | inbox 消费 append 后、删除前 | B | `replayed_consumed_inbox_file_is_idempotent` |
| 7 | spool 写入/补投删除前 | A+B | `notify_falls_back_to_spool_and_next_run_replays_once` |
| 8 | 归档 `VACUUM` 中 | A | `kill_during_archive_vacuum_keeps_primary_readable` |
| 9 | 归档 `rename` 后、清空前 | B | `crash_after_archive_rename_before_clear_can_retry` |
| 10 | 归档清空事务中 | A | `kill_during_archive_clear_transaction_is_atomic` |
| 11 | `init` 建库中 | B+A | `zero_byte_database_self_heals`、`partial_database_reports_clear_error_then_retry`、`concurrent_first_runs_on_zero_byte_database` |
| 12 | 接管释放锁瞬间 | A | `follower_takes_over_after_leader_killed`、`second_takeover_after_two_kills_converges` |
| 13 | 模型请求中 kill（run） | A | `kill_during_model_request_loses_no_message` |
| 14 | 发布者写 `tmp` 前 `SIGKILL` | 文档格 | 不承诺（见「保障分级」） |

14 格均有归属：#2/#3、#7 复用既有 e2e 并在尾部追加收敛检查；#5 复用既有 e2e 的 tmp 清理断言；#14 为不承诺的文档格，列入「已知限制」与「升级判据」。INV7（发布者 kill 不影响已发布消息）另由 `publisher_killed_after_publish_is_consumed_exactly_once` 覆盖。

## 测试与验证策略

- 单元测试：记录序列化往返与 golden JSON（含 `system.output_files` 与旧记录缺字段的兼容）；渲染分层（稳定前缀合并、双向最大连续段并入 `user`、失配兜底、`origin=user`、`system` 的 `[执行命令]` 前缀与截断提示）；`ai.reasoning` 与 `endpoint_format` 的出站序列化；store 追加/加载/`read_after`/归档（含同 ts 序号与 `archived` 只读）/损坏 payload 报错/并发追加与 BUSY 重试；Shell 进程组、超时、流式截断、逃逸进程组放弃输出但不误报超时。
- 思考回传测试：`endpoint_format` 为 `deepseek` 等时 assistant 消息含 `reasoning_content`（带 `tool_calls` 且思考为空时为 `""`），为 `openai` 时不含。
- 通知测试：`--notify` 两种形态写入 `system(source=notify)` 与信封正文；`--` 形态记**转义后可重放**的 `command`、stdin 形态不记；注释缺省「由用户执行」；stderr 透传与退出码沿用；两种形态超限截断与 `output_files` 落盘（命令流与 stdin 同一 `Sink`）；`--notify-timeout` 与环境变量/`[shell]` 限额解析链；`shell_quote` 重放等价；`tools::shell` 注入 `YAA_NOTIFY_*`。
- 会话目标测试：`yaa_env` 环境链（`YAA_SESSION`/`YAA_NOTIFY`/`YAA_PARENT`/`YAA_NOTIFY_CALL_ID`）；`--notify` 解析顺序 `--to` → `--session` → `YAA_NOTIFY` → `YAA_SESSION` → cwd 默认；显式 `--to` 的严格校验与「不创建目标」；`origin` 总写与非法 `call_id` 忽略。
- 阶段 5 保底测试：可重试发布失败降级 spool、补投幂等与消费后删除、spool 非空拒绝 `--clear`、`--status` 只读计数、follower 只提示不补投。
- 初始化测试：无 `system(source=init)` 簿记时执行 `init` 并写簿记（含已有 `notify` 记录的会话）；已写簿记的会话不重跑；`init` 失败仍写 `user`、不写簿记且退出码为配置错误，改好后下次运行能重试成功。
- 展示测试：展示条目 golden；首轮注入展示一次；默认摘要与 `--verbose` 全文两种形态；`--quiet` 抑制；`llm` 不产生记录。
- 模型请求超时测试：无响应端点（accept 后不读不写）在配置超时内返回错误；`request_timeout_secs = 0` 被拒绝、缺省 300、可解析。
- SIGINT 测试：向运行中的 yaa 发 SIGINT 应清理工具进程组并以 130 退出。
- 展示唯一性测试：单次运行的工具步骤在 stderr 恰好展示一次；模型请求期间外部 notify 不丢不重。
- 防漂移测试：系统提示词 section 必须提及 schema 的工具名与参数名；展示 - 模型一致性不变量（见阶段 1 验收第 9 条）。
- 集成测试：`dev-dependencies` 起极简假 OpenAI 服务（`axum` 或 `hyper`，仅测试用），按脚本返回 `tool_calls`，端到端跑通并用临时目录隔离。
- 收尾运行 `autocorrect --fix` 与 `autocorrect --lint docs crates`。

## 已知限制

1. **网络文件系统不支持 WAL**：`session.db` 必须位于本地文件系统；NFS/SMB 等挂载上的 WAL 与 `flock` 语义不可靠。
2. **单条 `payload` 建议 ≤64 KiB**：现状由 `yaa-exec` 的流式截断保证；更大的记录会放大事务与 WAL 开销。
3. **归档崩溃残留**：可能留下 0 字节归档占位与 `.tmp.<pid>` 临时文件；超过 1 小时的临时文件由下次归档尽力清理，占位需人工确认后删除。
4. **`append` 每次调用约 1 ms 固定开销**：打开连接、设置 PRAGMA 与事务提交的成本，不适合逐条高频写入；批量追加可摊薄。
5. 仅 POSIX（`sh -c`、`killpg`）；Windows 不在阶段 1 范围。
6. **（永久限制）`outputs/` 无保留策略**：截断落盘的完整输出只增不减，磁盘可能无界增长；阶段 6 明确不做自动保留策略，用户报告磁盘问题时先用 `du`/`find` 手动查看占用再 prune（见「升级判据」）。
7. 会话不承诺跨机迁移。
8. token 估算默认按字段级二分类（ASCII 按 `chars_per_token`，非 ASCII 每码点 1 token，D52）；未配置 `[hooks] estimate` 时，对高频中文词可能略高估、对多码点 emoji 可能高估，二者方向均偏保守；需要精确值时接入外部估算钩子（D91）。
9. `reasoning` 落盘：思考内容进入会话文件，增大存储与隐私面（D23）。
10. 思考回传仅覆盖 `reasoning_content` 系端点（DeepSeek/Kimi/Qwen/GLM）；Anthropic/Responses/Gemini 的签名类结构化 metadata 不支持（D24）。
11. **（F12，阶段 5 已修复）展示与渲染对 `system` 的谓词已统一**：display、render 与 inbox 载荷校验都调用 `is_injectable_system`（当前仅 `source == "notify"`），不再各自判断 `source` 与 `content`/`output_files`。
12. **逃逸进程组攥管道时输出仍可能被放弃**：后台进程若 `setsid` 脱离工具进程组并继续持有 stdout/stderr 管道，读取会在宽限后被放弃，输出以占位提示自解释；但这**不再误报**为 `timed_out`（D35）。长任务应用 `nohup <cmd> >/dev/null 2>&1 &` 主动脱离工具管道，而不是依赖读取宽限。
13. **SIGINT 只清理 `shell` 工具进程组**：`init`/`llm` 钩子的进程组未纳入信号转发；钩子自身有硬超时兜底。
14. **审批不是安全边界**（阶段 2）：命令资源为 best-effort 文本拆分，包装器（`xargs`/`find -exec`/`sudo`）、重定向目标、变量展开等仍可绕过；收紧应使用窄 `allow` + 精准 `deny`，不要指望审批流拦截一切（D42）。
15. **压缩使 KV cache 部分失效**：摘要替换稳定前缀之后的早期消息，provider 从第一个被替换 token 起的复用失效（压缩的本质代价，DSH 同样如此）。
16. **摘要会再次发送被遮蔽内容**：摘要调用把被遮蔽消息发给模型，隐私面与主调用相同，不新增泄露面，但敏感历史会被多送一次。
17. **（永久限制）`outputs/` 仍无保留策略**：与第 6 条同源；D48 顺延后于阶段 6 定为永久限制，理由见第 6 条。
18. **`--notify` 不立即落库**：它只发布到 inbox，通知要等运行中的 runner 在安全点消费，或下一次 `run` 才进入历史；`--status` 能看到待处理计数，但 `--notify` 返回后立刻读取历史不会包含该通知。发布失败且可重试时会先落 spool，更要等下一次非跟随运行补投。
19. **纯 follower 遇未决审批只等待**：不会自动接管或拒绝，会提示 `--approve` 并继续跟随；需要用户显式放行，或发布新的 `User` 触发 `PendingPolicy::Abandon`。
20. **（永久限制）inbox/spool 顺序是 ULID 近似序**：按文件名字典序消费，同一毫秒内的 ULID 不保证单调；同毫秒到达的多条消息只保证大致按时间，不保证严格因果。阶段 6 明确不做 ULID 严格同毫秒序，需严格顺序时应串行发布或改用更高精度 id。spool 补投只消费 spool 自身、不再与 inbox 合并（见第 25 条）。
21. **（永久限制）follower 每轮全量 `load`**：判断 own 是否落库、历史是否空闲、有无未决审批都需要全量历史，长会话为 O(n)；实测空轮询约 0.5 ms，增量读取本身是 O(增量)。阶段 6 明确不引入增量视图以保持配对与投影语义简单，故永久记录。
22. **（阶段 6 已修复）显式会话 artifact 唯一化**：锁、inbox、spool 已按完整数据库文件名唯一化；阶段 6 起 `outputs/`、`summaries/` 与归档目录 `history/<sid>/` 也在安全化后的完整文件名（`artifact_name`）下再嵌一层，同目录多个显式会话互不混放。默认会话 `artifact_name` 为空串，路径与旧行为一致。
23. **run 竞态 e2e 是烟雾测试**：真实进程同时启动、慢模型、`kill -9` 等覆盖主路径，不证明所有调度交错；inbox 并发与幂等由 store 单测覆盖，follow 的时序正确性仍依赖不变量测试与代码审查。
24. **（永久限制）发布者在落盘前 `SIGKILL` 会丢通知**：spool 只兜发布失败/预检 BUSY；若发布者进程在通知成功发布到 inbox/spool 之前被 `SIGKILL`，该条通知仍会丢失，且不留下脏状态。这是「进程崩溃级」保障的边界，需外部重试或改用可靠队列，阶段 6 明确不做。
25. **spool 补投只消费 spool 自身**：补投发生在崩溃恢复之后、`init` 之前，只扫描并消费 spool，不合并、不提前消费 inbox，因此不会打乱 `init/` 与 `pending/` 的既有消费顺序（F11）。代价是 spool 中的通知会先于本轮 `init/` 落库，与旧 spec 的「与 inbox 按 id 合并」不同。
26. **跨数据根时 `output_files` 基准按发布者**：通知记录的相对路径以发布者数据根解析；目标会话在另一数据根时，接收方可能无法按自己的数据根解析该路径。
27. **（永久限制）ULID 同毫秒排序只是近似**：inbox/spool 的合并与消费都以 ULID 字典序近似时间顺序，同毫秒内不保证严格因果；阶段 6 明确不做 ULID 严格同毫秒序，需要严格顺序时应串行发布或使用更高精度 id。
28. **（阶段 6 已修复）env 入口绝对化**：`YAA_SESSION` 在输出前绝对化；`--to` 与继承的 `YAA_NOTIFY`/`YAA_PARENT` 的相对值在解析通知目标时按当前 cwd 绝对化，不再随后续子进程的 cwd 漂移。
29. **非 UTF-8 会话路径会被 lossy 化**：`YAA_SESSION`/`origin` 是字符串，`to_string_lossy` 会把无法解码的字节替换为 U+FFFD；非 UTF-8 路径仍可用于本地文件操作，但环境变量与归因字段不再逐字节可逆。
30. **`--to` 的非普通文件被拒绝**：显式目标必须是普通文件；FIFO/设备等按「不是 yaa 会话数据库」退出 1，不会打开或阻塞。
31. **大库 `--clear` 期间 notify 预检/发布会等待**：`--clear` 持 `clear.lock` EX 并在归档时执行 `VACUUM INTO`；notify 的预检与发布持 SH，可能等待数秒（受 busy_timeout 约束），期间命令尚未开始执行。
32. **保障分级：进程崩溃 vs 机器断电**：承诺覆盖「进程崩溃级（`kill -9`）」与「fsync 约束下的断电级」，不承诺未 `fsync` 窗口（提交前、`rename` 前、写 spool 前）内的记录与发布，也不承诺真机器断电——后者需 dm-log-writes 验证，列为升级项（见「升级判据」）。
33. **跨会话归因 `origin` 是弱引用**：`System.origin` 保存发布者会话库绝对路径（字符串），归档后可能指向已清空的主库或旧 generation，不能反查历史；阶段 6 明确不做跨会话归因解析，跨会话因果/树查询若成为需求再评估 edges 表（见「升级判据」）。
34. **接管至少一次、单工具非 exactly-once**：follower 只在旧 runner 退出后尝试接管一次，失败即退出并保留 inbox 消息，由后续 `run` 处理（至少一次投递）；被中断的 `shell` 工具恢复时补中断结果，重试可能再次执行命令，单工具调用不保证 exactly-once，结果幂等靠 `call_id` 与同批事务约束。
35. **`User.client_id`/`turn_id` 恒为 `None`**：schema 保留这两个可选字段但无运行时消费者，阶段 6 明确不填充；旧记录缺字段解析为 `None`。

## 升级判据

以下能力在阶段 6 定为「不做/不承诺」，出现对应信号时再评估，避免过度设计：

| 能力 | 触发条件 | 备注 |
| ---- | -------- | ---- |
| edges 表 | 出现跨会话因果/树查询或自动路由需求，且 `origin`/`call_id` 字符串无法支撑 | `origin` 现为弱引用（见「已知限制」第 33 条） |
| 常驻 daemon | 需要无人值守定时执行，或「通知必须即时消费且无人开 `yaa`」 | 现为排空式单进程模型（D68） |
| 向量检索 | 历史搜索成为主要用法 | 优先做独立工具，不塞进 runner |
| 真机器断电保障 | 数据重要且环境真断电 | 先补 dm-log-writes 测试 |
| `outputs/` 保留策略 | 用户报告磁盘问题 | 先用 `du`/`find` 手动查看占用，再 prune |
| ULID 严格同毫秒序 | 同毫秒因果顺序确有必要 | 现为近似序（见「已知限制」第 20、27 条） |
| 单工具 exactly-once | 命令重复执行不可接受 | 现为至少一次投递（见「已知限制」第 34 条） |

## 已确认项与默认假设

已确认：XDG 双根（D10）、`ai` 与 `tool_call` 分离记录 + 单批原子写（D12）、统一通知注入（D19）、簿记驱动的初始化（D20）、分层装配（D22）、`ai.reasoning` 落盘（D23）、`endpoint_format` 取代 `provider`（D24）、阶段 1 引入 `YAA_SESSION`（D25）、离群通知双向最大连续段（D28）。

默认假设（未特别反对即执行）：截断阈值 `max_stream_chars=32768`、`head_chars=8192`、`tail_chars=2048`；`endpoint_format` 默认 `"openai"`；注入合并用空行分隔；包 `yaa-cli` 产出二进制 `yaa`；`request_timeout_secs` 默认 300；workspace 版本从 `0.1.0` 起。

阶段 3 已确认（D48–D58）：压缩策略与默认值（`threshold_ratio=0.8`、`retain_ratio=0.16`、`chars_per_token=4`、`max_tokens=8192`、pruner `8192/4096/1024`、`retries=1`）、模型窗口 `context_window` 随 llm 配置（缺失即禁用）、渲染期压缩与侧车缓存、链式摘要、展示豁免、`--json` 与 `--no-store`。

阶段 5 已确认（D76–D85）：`yaa_env` 环境链、`--to` 优先级与严格校验、`System.origin`/`call_id` 归因与统一谓词、spool 降级与补投、后台回注范式与 `call_id` 继承、`--approve` 不认 `--to`、归档拒绝非空 spool；终审补充提示词后台范式（D83）、spool 锁序与 generation 复查（D84）、CLI 冲突与 best-effort origin（D85）。

阶段 6 已确认（D86–D90）：不变量检查器与崩溃矩阵、水位迁 `seq`、artifact 路径唯一化、env 入口绝对化、断电保障分级与升级判据。
