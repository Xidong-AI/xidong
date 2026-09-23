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
| D15 | 展示采用「同一事件日志派生」：模型消息与用户展示都从会话 JSONL 投影，不维护第二套执行事件流 | 消除展示与模型上下文的漂移（Codex `/status` 的 `Agents.md: <none>` 系列 issue 为反例：两条独立计算路径必然漂移）；用户要求「用户看到的就是模型看到的」 |
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
| D34 | `archive` 的 `sessions-<ts>.jsonl` 已存在时改为 `sessions-<ts>-<n>.jsonl`（n 从 1 递增） | 同毫秒连续归档会静默覆盖，破坏「归档即不可变历史」 |
| D35 | `timed_out` 只表示真正超出时限；读取宽限内放弃流只回填占位提示，不再 promote 为超时 | 逃逸进程组攥住管道与命令超时是两回事，误报会让调用方以为命令被超时击杀 |
| D36 | `--notify` 的 stdin 形态复用 `yaa-exec::capture_reader`（同一 `Sink`/`FullOutput`） | 与 `--` 形态保持同一截断标记与落盘语义，避免两套实现漂移；超限时同样产生 `output_files.stdout`，内存仍有界 |

> D6–D8、D11 中「提示词钩子」的描述已被 D19 取代：现仅存 `init` 与 `llm` 两个钩子；D5 的 `yaa-shell` 已按 D14 改名 `yaa-exec`。D31–D35 为阶段 1 修复轮落地的行为修正，其中 D31、D32 涉及投影与记录 schema，D33–D35 为执行/存储语义。

## 对 design.md 的更正

第 1–7 条必须在动代码前确认，因为它们决定了不可变的 JSONL schema；第 8–11 条为实现阶段 1 时发现、事后补记，不改变已冻结的 schema，但修正 design.md 对行为的描述；第 12–13 条为本次重设计讨论中确认，与 D23–D24、D28 对应。

1. **压缩并非无模型调用**（`design.md:44`）。`@deepseek-ai/dsh-compaction-basic` 出厂必发一次摘要 LLM 调用；真正无模型的是伴生包 `dsh-compaction-tool-result-pruner`（纯头尾截断）。按 D3 两者都实现。
2. **压缩归属**：`design.md:19` 把「压缩摘要」列在 `yaa-store`，但 `design.md:45` 说压缩只发生在渲染阶段。更正为：**压缩属 `yaa-core` 的渲染管线，`yaa-store` 只负责存储**。
3. **单根 `$GLB_CFG`**：`design.md:186` 让配置、会话、历史同根。更正为按 XDG 拆分配置根与数据根（见「状态根目录」），`GLB_CFG` 仅作为配置根暴露给钩子。
4. **`turn_end` 记录**：`design.md:342` 出现但不在记录清单内。约定：**无 `tool_call` 的 `ai` 记录即为回合结束**，不引入独立类型。
5. **记录缺 `id`**：`design.md:31,32,37` 的 `user`/`ai`/`system` 无 `id`，无法支撑阶段 4 的幂等去重（`design.md:361,366`）。更正为**所有记录都有 `id`**，工具结果另用 `call_id` 关联。
6. **截断产物无载体**：`design.md:50` 未定义完整输出的路径字段。更正为 `tool_result` 增加可选 `output_files`（stdout/stderr 分两个文件）与 `truncated`。
7. **`client_id`/`turn_id`**（`design.md:301`）预留进 schema，阶段 4 使用。
8. **钩子不经 `shell` 工具执行**（`design.md:12`）。design 说系统生命周期钩子「触发 Shell 工具调用」实现。更正为：`init`/`llm` 钩子由 `yaa-core` 的独立执行器（`crates/yaa-core/src/hook.rs`）以 `sh -c` 运行，与 `shell` 工具**不共用策略**——钩子单流超过 1 MiB 直接报错退出，不截断、不产生 `output_files`；「钩子就是同一个 Shell 工具」的表述不成立。
9. **初始化注入排在 `user` 之前**（`design.md:27-31`）。design 的 JSONL 顺序在新设计下成立：`init` 只在无 `system(source=init)` 簿记时于写入 `user` 之前执行，注入内容由命令内的 `yaa --notify` 写入，因而物理上位于 `user` 之前。`init` 失败时仍写入 `user` 并按配置错误退出。
10. **注入不再有专用文案**。旧设计「钩子 `tool_call` 不写 `description`、由展示层现算文案」随 D19 一并移除：注入内容现在是 `system(source=notify)` 事件，展示用与工具调用一致的块样式，标题即信封中的注释。
11. **中断 `tool_result` 有两种互补补法**（`design.md:427-431`、本文件「渲染管线」失配兜底）。阶段 1 由渲染器在内存中合成中断 `tool` 消息，保证请求体合法，**不写回** JSONL；阶段 6 的「补中断 `tool_result`」则是把它作为记录持久化落盘。两者不冲突：前者保证任意时刻请求合法，后者让历史事实源自身自洽。
12. **`ai` 记录含 `reasoning`，思考内容按端点格式回传**（`design.md:38,42` 的示例未列）。`design.md:63` 把压缩参考 DSH，而 DSH 的思考模型适配证明该字段必须可回传。更正为：`ai` 新增可选 `reasoning`，落盘、可展示；`[llm] endpoint_format` 决定它是否作为 assistant 消息顶层 `reasoning_content` 回传——`deepseek`/`kimi`/`qwen`/`glm` 在请求带 `tools` 时即使思考为空也必须发 `""`，`openai` 不回传。
13. **注入分组的方向**。`design.md:57` 只说「与相邻 `user` 事件合并」，未定义方向。更正为**双向最大连续段**：首条 `user` 起，`user` 与 `system(notify)` 的最大连续段合并为一条 `user`（`\n\n` 分隔，物理顺序）。
14. **通知的 `command` 进入上下文**（`design.md:48` 称「只用于展示，不进入上下文」）。更正为：投影 `system` 时把 `command` 以 `[执行命令] <command>` 前缀置于正文最前（D31）；`command` 现为 shell 转义后的**可重放命令**，不再是「仅展示摘要」。展示层仍以 `% 命令` 呈现，且不重复拼该前缀。
15. **`--notify` 与 `shell` 工具同一条执行路径**（`design.md` 只把截断/落盘描述给工具结果）。更正为：`--notify` 的 `--` 与 stdin 两种形态都经 `yaa-exec` 的截断/落盘策略（`run` 与 `capture_reader`），把完整输出落盘为 `output_files`，通知因此可携带丢失的完整内容；限额解析链见「配置与钩子协议」规则 8。`timed_out` 语义见 D35。

## 目标架构（阶段 1）

```plaintext
Cargo.toml                      # workspace
crates/
  yaa-msg/                      # 记录类型与 serde，无内部依赖
  yaa-store/                    # JSONL 同步读写、会话解析、归档、按偏移读
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
  sessions/<sid>/default.jsonl
  sessions/<sid>/outputs/<ulid>.{stdout,stderr}.out
  sessions/<sid>/summaries/<key>.json      # 阶段 3 摘要缓存
  history/<sid>/sessions-<ts>.jsonl
```

- 启动时导出 `YAA_CONFIG_HOME`、`YAA_DATA_HOME`，并令 `GLB_CFG=$YAA_CONFIG_HOME` 兼容 `design.md` 的钩子写法。
- **会话根 = JSONL 文件的父目录**，`outputs/` 挂在会话根下。`output_files` 存相对**数据根**的路径，归档后仍可解析。
- `<sid>` = `hex(sha256(canonical_cwd))` 前 32 位十六进制（128 位）。
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
```

`llm` 钩子 stdout 按 TOML 解析：

```toml
endpoint_format = "deepseek"
base_url    = "https://api.deepseek.com"
api_key     = "..."
model       = "deepseek-chat"
temperature = 0.7
max_tokens  = 4096
```

规则与不变量：

1. 钩子以 `sh -c "<命令>"` 执行，cwd 为进程工作目录，环境含 `GLB_CFG`、`YAA_CONFIG_HOME`、`YAA_DATA_HOME`、`YAA_SESSION`（本会话 JSONL 绝对路径）与 `YAA_BIN`（当前 `yaa` 可执行文件路径，使默认钩子不依赖 `PATH`）。
2. **`init` 钩子只做副作用**：yaa 运行它并忽略 stdout；写入上下文的唯一方式是在命令里调用 `yaa --notify`。默认配置用三条 notify 拼出「系统提示词 / 用户总提示词 / 当前环境」；内置模板的「工具 section」由 `yaa-core::tools::shell` 提供（见 D14、D17）。
3. **`llm` 钩子不落历史、不渲染、不展示**：其 stdout 含密钥，且每次运行都需执行以取得配置；结果只留在进程内，不写任何事件（见 D19）。
4. **`init` 幂等由簿记保证**：触发条件是「会话中不存在 `system(source=init)` 记录」，成功后追加一条 `system(source=init, content="")` 簿记；即使钩子不产出内容，下次也不会重跑。失败时不写簿记、仍写入 `user` 并按配置错误退出，已写入的 `notify` 记录保留；因此下次运行会自动重试。
5. **`yaa --get-prompt system` 必须在加载 config、解析会话、执行任何钩子之前短路**，只打印内置模板。否则 `init` 钩子内部调用它会形成递归环。补一条递归防护测试。
6. 钩子输出有**独立上限**（不复用 Shell 的截断）：超限直接报错退出，绝不静默裁掉提示词。
7. 钩子执行有**硬超时**，并以进程组方式整组击杀（与 Shell 同一套方案），避免配置钩子挂死拖住 `yaa`。
8. **`yaa --notify` 写入协议**：`yaa --notify="注释" -- cmd` 把 argv 逐个作 POSIX 单引号转义（简单 token 不加引号，含特殊字符/空格/单引号者用单引号并转义内部单引号）后经 `yaa-exec` 执行，**复用 Shell 的截断与完整输出落盘**：正文 `body` 取被截断的 stdout，stderr 原样透传到 yaa 的 stderr，退出码沿用 cmd 的退出码（被信号杀死或超时按 1）；超限时 `output_files` 记录相对数据根的完整输出路径。`command` 记为上述**转义后可重放**的命令串；`cmd | yaa --notify="注释"` 从 stdin 读取并**复用同一套截断/落盘策略**（`yaa-exec::capture_reader`）：未超限为全量正文，超限时正文为头尾拼接并带 `... [truncated] ...` 标记、完整输入落盘并记入 `output_files.stdout`，无 `command`；注释缺省「由用户执行」。写入一条 `system { source: "notify", command?, output_files?, content: "<注释>\n<内容>\n</注释>" }`，原子追加到目标会话。
   - **限额解析优先级**（高→低）：`--notify-timeout`（仅超时）→ 环境变量 `YAA_NOTIFY_TIMEOUT_SECS` / `YAA_NOTIFY_MAX_STREAM_CHARS` / `YAA_NOTIFY_HEAD_CHARS` / `YAA_NOTIFY_TAIL_CHARS`（能解析为数字才生效）→ 配置文件 `[shell]`（路径同 `--config`/默认；读取或解析失败只打警告到 stderr 并用默认值，**绝不让 notify 失败**）→ 内置默认。
   - `shell` 工具执行时把本次调用的限额以这 4 个 `YAA_NOTIFY_*` 变量注入子进程，使工具内嵌的 `yaa --notify` 采用同一套参数（调用者透传）。
   - 目标解析顺序 `--session` → `YAA_SESSION` → 当前 cwd 默认会话（跨会话显式 `--to` 见阶段 5）。
   - `--notify` 是短路写入路径：不进入 agent loop、不触发 `init`；仅为解析限额按需读取配置，且失败不影响写入。

## 记录 schema（阶段 1 冻结）

每行一条 JSON，带标签枚举，`ts` 为 Unix 毫秒：

```plaintext
user        { type, id, ts, content, client_id?, turn_id? }
ai          { type, id, ts, model, tool_schema_hash, content, reasoning? }
tool_call   { type, id, ts, origin: ai|user, call_id, command, description? }
tool_result { type, id, ts, call_id, stdout, stderr, code, timed_out,
              truncated, output_files?: { stdout?, stderr? } }
system      { type, id, ts, source, command?, output_files?, content }
```

- `id` 为 ULID，全记录唯一，阶段 4 以此幂等去重。
- `call_id` 为模型返回的工具调用 id，`tool_result` 靠它配对。
- `ai` 与其后紧跟的 `tool_call(origin=ai)` 记录构成一个助手步骤，**两者以单次缓冲原子追加**（见下）。
- `system.source` 目前有 `notify`（`yaa --notify` 写入的通知，正文内联 `<注释>` 信封）与 `init`（初始化簿记，空内容不渲染）。
- 可选 `command` 是产生该通知的 **shell 转义后可重放命令**（`sh -c` 重放等价原 argv；简单 token 不加引号，含特殊字符者用单引号并转义内部单引号）。展示层以 `% 命令` 呈现，投影时以 `[执行命令] <command>` 前缀进入上下文（D31）；stdin 形态与簿记无此字段。
- 可选 `output_files` 与 `tool_result` 同构（相对数据根），承载 `--notify`（命令或 stdin）超限截断后落盘的完整输出；正文自身只含 `yaa-exec` 的头尾截断标记，截断提示与相对路径由投影层 `display::system_text` 追加（D32）。旧记录缺该字段解析为 `None`。
- `ai.reasoning` 为可选思考内容，落盘、可由 `--verbose` 展示，并按 `endpoint_format` 决定是否回传（D23/D24）。
- 相对旧 schema 的移除项见 D21。
- `approval_pending`/`approval_resolved` 阶段 2 引入。

### 追加的原子性

`yaa-store::append(&[Record])` 把整批记录序列化到一个 `Vec<u8>`，一次 `write_all`（`O_APPEND`）+ `fsync`。禁止对文件句柄逐条 `serde::to_writer`（多 syscall 会在同会话并发时交织出半行）。断电只会留下撕裂的尾行；**`append` 在写入前先修补尾行**：尾段能解析则补一个换行保留，否则截断到最后一个换行之后。`load` 同样容忍撕裂尾行。

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

## Agent loop 与并发形状（阶段 1）

即使阶段 1 无锁，入口形状必须现在就是「先发布消息 → 再决定 runner/follow」，否则阶段 4 要重写入入口。

```rust
async fn main_flow(cfg) -> Result<ExitCode> {
    let session = resolve_session(&cfg)?;          // canonical cwd -> sid / --session

    if !store::has_init_bookkeeping(&session)? {   // 无 init 簿记：先初始化，再收 user
        init_hook(&session, &cfg).await?;          // 经 `yaa --notify` 侧写事件 + init 簿记
    }

    let user = build_user_record(prompt);
    store::append(&session, &[user]);              // 阶段 1：直接追加；阶段 4：改为 inbox 发布

    let guard = try_acquire_runner(&session);      // 阶段 1：恒成功，无锁
    match guard {
        Some(g) => run_agent_loop(&session, &cfg, g).await,
        None    => follow(&session).await,         // 阶段 1：不可达
    }
}

async fn run_agent_loop(session, cfg, guard) -> Result<ExitCode> {
    let llm = resolve_llm_hook(cfg)?;              // 每次运行一次，不写事件
    let watermark = store::len(session)?;          // 展示增量水位
    for step in 0..cfg.agent.max_steps {
        let records = store::load(session)?;       // 每步重读（design.md:59）
        let messages = render(&records);           // 事件日志 → ChatMessage（纯函数）
        sink.push(display::items(&records[watermark..]));  // 同源展示条目，cli 只决定样式
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

- `yaa-store` 保持**同步 API**（追加、按偏移读、归档）。在 core 调用点用 `spawn_blocking` 包装，不做成假 async。
- store 预留 `read_from(&session, offset) -> (Vec<Record>, new_offset)` 供阶段 4 跟随使用；`load` 容忍撕裂尾行。
- `load` 每步重读，阶段 4 的 inbox 插入与 `yaa --notify` 追加才能在下一轮被看到。
- **无初始化簿记才初始化**：会话中不存在 `system(source=init)` 记录时执行 `init` 钩子（D20）；钩子内经由 `yaa --notify` 写入的通知先落盘，随后才是 `init` 簿记与 `user` 记录。`init` 失败仍写入 `user`，再按配置错误退出；因不写簿记，下次运行自动重试。
- **`llm` 不写事件**：只解析配置供进程使用；解析失败按配置错误退出（见 D19）。
- **空回复**：无文本且无工具调用时，记一条空 `ai` 记录、向 stderr 发提示并结束回合（退出 `0`），且该记录不投影消息。

### 错误与退出策略

- 模型 429/5xx/网络错误：指数退避，总尝试最多 3 次（首发 + 2 次重试）；仍失败则以模型错误退出。此时历史停在「工具批次已完成」的合法状态，下次运行可续接。
- 退出码：`0` 成功，`1` 通用错误，`2` 用法错误，`3` 配置/钩子错误（含钩子执行失败），`4` 模型错误，`5` 达到 `max_steps`。
- 空回复（无内容且无工具调用）视为回合结束，退出 `0`，并给出 stderr 提示。
- 半批次收尾：模型失败或 Ctrl-C 后，已写入的 `tool_result` 保留；缺失的结果由渲染器兜底（见上）。

## CLI（阶段 1）

```plaintext
yaa [PROMPT]...              # 多个位置参数以空格连接；空则从 stdin 读
yaa --clear                  # 归档当前会话
yaa --status                 # 打印会话摘要
yaa --session <path>         # 指定会话文件
yaa --get-prompt system      # 打印内置系统提示词后退出
yaa --notify="注释" -- cmd   # 把命令输出作为通知写入当前会话后退出
cmd | yaa --notify="注释"    # 从 stdin 读并写入通知后退出
yaa --config <path>
yaa --cwd <path>             # 改变进程工作目录
yaa --verbose                # 追加思考内容与命令输出
yaa --quiet                  # 只打印最终回复
```

- `--notify` 是短路路径：不加载 `llm` 配置、不进入 agent loop，只解析会话、执行命令或读 stdin、原子追加一条 `system(source=notify)`。
- 过程记录默认打到 stderr（见 D13、D15、D16）：注入与通知用与工具调用一致的块样式（`# 注释` + 内容），工具调用用 `# 描述` + `% 命令`；块间以 `---` 分隔；非末步的模型正文也在此打印，末步回复只走 stdout。
- 展示条目与模型消息同源，由 `yaa-core` 的投影产出；`--quiet` 抑制全部过程输出。

后移到阶段 2 及以后：`--json`（展示条目 schema 未定）、`--no-store`、`--approve`、`--notify` 的 `--to` 与跨会话解析。

## 阶段划分与验收

每阶段独立可验证、可提交。原子修改后必须通过 `cargo check --all-targets`、`cargo clippy`、`cargo test`。各阶段改动在独立 Git 工作树完成、验收后合并（阶段 1 的历史开发见 `rewrite/yaa-cli` 分支）。

### 阶段 1：最小闭环

内部顺序：**记录 schema（含 `reasoning`）+ store + 渲染分层 golden 测试 → `tools::shell` 内聚与提示词组装 → `--notify` 写入路径与 `YAA_SESSION` → init/llm 钩子与模型适配（含 `endpoint_format` 回传）→ agent loop → CLI 展示**。

验收：

1. `yaa "你好"` 在空会话先执行 `init`：`default.jsonl` 依次留下三条 `system(source=notify)`、一条 `system(source=init)`、`user`、`ai`；无工具调用时打印模型回复。
2. 再次 `yaa "继续"` 不重跑 `init`（无新增 notify 记录），能读到上一轮并正确渲染续接；**断言第二跳请求体**符合 OpenAI 消息序列。
3. 渲染分层：首条 `user` 之前的通知合并为一条 `system` 消息；`yaa --notify` 在首条 `user` 之后写入的通知与相邻 `user` 合并为一条 `user` 消息（信封保留）。
4. 构造需要 Shell 的多步任务，工具结果回填并循环至最终回答。
5. `--clear` 把 `default.jsonl` 移入 `history/<sid>/sessions-<ts>.jsonl` 后开新历史；`outputs/` 留在原地，归档记录的相对路径仍有效；同一 `ts` 再次归档时目标改为 `sessions-<ts>-<n>.jsonl`（n 从 1 递增），不覆盖既有归档。
6. 集成测试用本地假 OpenAI 服务器，断言 JSONL 序列与 Shell 副作用；**断言请求体与 JSONL 均不含 `api_key`**。
7. 递归防护测试：`init` 钩子内部调用 `yaa --get-prompt system` 能正常返回而非递归。
8. 中断测试：模型返回 `tool_calls` 后进程被杀，下次运行渲染器合成中断 `tool` 消息，请求体合法。
9. 展示一致性：对同一份记录，展示中属于上下文的文本集合与模型消息的文本集合完全一致（`reasoning` 与展示专用的块标记/标题豁免；通知的 `command`、工具调用的 `description` 两侧都有，计入，D29/D31）。
10. 初始化只展示一次：首次运行 stderr 出现三个注入块（默认 `# 注释` + `% 命令` 摘要，`--verbose` 追加内容全文）；第二次运行不再出现。
11. `llm` 钩子不写任何记录、不出现在 stderr（默认与 `--verbose` 均不展示）。
12. `--get-prompt user` 为用法错误（退出码 2）；`--get-prompt system` 输出的工具 section 与 `tools::shell` 的 schema 名称/参数一致（防漂移）。
13. `--notify` 两形态与限额：`yaa --notify="注释" -- cmd` 写入 `system(source=notify)`，`command` 为 shell 转义后的可重放命令，超限时经 `yaa-exec` 截断、正文含头尾标记且 `output_files.stdout` 为相对数据根的路径；`cmd | yaa --notify="注释"` 写入同名记录但不含 `command`、复用同一截断/落盘策略（超限时正文含头尾标记且完整输入落盘为 `output_files.stdout`，短输入无 `output_files`）；两者正文均为 `<注释>…</注释>`。再断言：`--notify-timeout` 生效（超时按 1 退出并提示）、环境变量与 `[shell]` 的限额解析链生效、命令对模型可见（第二跳请求含 `[执行命令]` 前缀）、旧 `system` 记录缺 `output_files` 仍可解析。
14. 思考回传：`--verbose` 展示 `ai.reasoning`；`endpoint_format="deepseek"` 时第二跳请求体中带 `tool_calls` 的 assistant 消息含 `reasoning_content`（思考为空时值为 `""`），`endpoint_format="openai"` 时不含该字段。
15. 会话目标：`yaa --session <path>` 在空会话触发 `init`，三条 notify 经 `YAA_SESSION` 写入该 `<path>`；旧 schema 会话由 `yaa --status` 报明确错误并提示 `--clear`（D26）。

### 阶段 2：审批

规则（allow/deny/ask 通配符）、`approval_pending`/`approval_resolved`、`--approve` 同意一次、用户发新提示词时 pending 自动 deny。审批只作用于 `origin=ai` 的工具调用；`init`/`llm` 钩子与 `yaa --notify` 不经过 `shell` 工具，天然豁免。同阶段补 `--json`/`--no-store`。

### 阶段 3：压缩

pruner 先跑（跳过 `system` 事件与初始化注入，避免裁掉稳定前缀），仍超阈值再走 LLM 摘要。边界选择照搬 DSH：system 头不压缩、按 token 预算从尾部保留、切点向头部回退到工具配对平衡处。摘要产物写入 `sessions/<sid>/summaries/<key>.json` 侧车缓存（key = 被遮蔽区间 id + 模型），不修改 `default.jsonl`。参数 `thresholdRatio=0.8`、`retainRatio=0.16`、`CHARS_PER_TOKEN=4`（已知低估 CJK）改为读配置，不硬编码。

### 阶段 4：单 runner 与多客户端注入

`runner.lock` 选举、`inbox/{tmp,pending,consumed}` 无锁发布、`consume_inbox`（append → fsync → rename 顺序）、跟随与按偏移读取、安全点消费。`--clear` 在 runner 运行中的语义在此定义。

### 阶段 5：notify 与后台任务

`YAA_SESSION`/`YAA_PARENT`/`YAA_NOTIFY` 解析链、`--notify="注释" --to <path>`、后台结果作为 `system(source=notify)` 事件回注，复用阶段 3 的截断与压缩。

### 阶段 6：断电恢复与不变量

截断半行、补中断 `tool_result`、补 `approval_resolved: deny`、按 `id` 去重；不变量测试。

## 测试与验证策略

- 单元测试：记录序列化往返与 golden JSONL（含 `system.output_files` 与旧记录缺字段的兼容）；渲染分层（稳定前缀合并、双向最大连续段并入 `user`、失配兜底、`origin=user`、`system` 的 `[执行命令]` 前缀与截断提示）；`ai.reasoning` 与 `endpoint_format` 的出站序列化；store 追加/加载/归档（含同 ts 序号）/撕裂尾行/无法解析行报错；Shell 进程组、超时、流式截断、逃逸进程组放弃输出但不误报超时。
- 思考回传测试：`endpoint_format` 为 `deepseek` 等时 assistant 消息含 `reasoning_content`（带 `tool_calls` 且思考为空时为 `""`），为 `openai` 时不含。
- 通知测试：`--notify` 两种形态写入 `system(source=notify)` 与信封正文；`--` 形态记**转义后可重放**的 `command`、stdin 形态不记；注释缺省「由用户执行」；stderr 透传与退出码沿用；两种形态超限截断与 `output_files` 落盘（命令流与 stdin 同一 `Sink`）；`--notify-timeout` 与环境变量/`[shell]` 限额解析链；`shell_quote` 重放等价；`tools::shell` 注入 `YAA_NOTIFY_*`。
- 会话目标测试：hook 环境注入 `YAA_SESSION`；`--notify` 解析顺序 `--session` → `YAA_SESSION` → cwd 默认。
- 初始化测试：无 `system(source=init)` 簿记时执行 `init` 并写簿记（含已有 `notify` 记录的会话）；已写簿记的会话不重跑；`init` 失败仍写 `user`、不写簿记且退出码为配置错误，改好后下次运行能重试成功。
- 展示测试：展示条目 golden；首轮注入展示一次；默认摘要与 `--verbose` 全文两种形态；`--quiet` 抑制；`llm` 不产生记录。
- 防漂移测试：系统提示词 section 必须提及 schema 的工具名与参数名；展示 - 模型一致性不变量（见阶段 1 验收第 9 条）。
- 集成测试：`dev-dependencies` 起极简假 OpenAI 服务（`axum` 或 `hyper`，仅测试用），按脚本返回 `tool_calls`，端到端跑通并用临时目录隔离。
- 收尾运行 `autocorrect --fix` 与 `autocorrect --lint docs crates`。

## 已知限制（阶段 1）

1. 无锁：同会话并发运行的写入可能交织（虽然单次 `write_all` 保证不出现半行，但顺序不可控）；阶段 4 解决。
2. 仅 POSIX（`sh -c`、`killpg`）；Windows 不在阶段 1 范围。
3. `outputs/` 无保留策略，磁盘可能无界增长。
4. 会话不承诺跨机迁移。
5. token 估算低估 CJK。
6. `reasoning` 落盘：思考内容进入会话文件，增大存储与隐私面（D23）。
7. 思考回传仅覆盖 `reasoning_content` 系端点（DeepSeek/Kimi/Qwen/GLM）；Anthropic/Responses/Gemini 的签名类结构化 metadata 不支持（D24）。
8. 旧 schema 会话不兼容，需 `--clear`（D26）。
9. **（F12）展示与渲染对 `system` 的谓词不对称**：展示只看 `source == "notify"`，渲染看「非 `init` 且（`content` 非空或 `output_files` 存在）」。将来出现新 `source`（如阶段 5 的后台结果）时必须统一谓词；阶段 1 暂不修，以免在只有两种 `source` 时引入无消费者的抽象。
10. **逃逸进程组攥管道时输出仍可能被放弃**：后台进程若 `setsid` 脱离工具进程组并继续持有 stdout/stderr 管道，读取会在宽限后被放弃，输出以占位提示自解释；但这**不再误报**为 `timed_out`（D35）。长任务应用 `nohup <cmd> >/dev/null 2>&1 &` 主动脱离工具管道，而不是依赖读取宽限。

## 已确认项与默认假设

已确认：XDG 双根（D10）、`ai` 与 `tool_call` 分离记录 + 单批原子写（D12）、统一通知注入（D19）、簿记驱动的初始化（D20）、分层装配（D22）、`ai.reasoning` 落盘（D23）、`endpoint_format` 取代 `provider`（D24）、阶段 1 引入 `YAA_SESSION`（D25）、离群通知双向最大连续段（D28）。

默认假设（未特别反对即执行）：截断阈值 `max_stream_chars=32768`、`head_chars=8192`、`tail_chars=2048`；`endpoint_format` 默认 `"openai"`；注入合并用空行分隔；包 `yaa-cli` 产出二进制 `yaa`；workspace 版本从 `0.1.0` 起。
