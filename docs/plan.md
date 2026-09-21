# YAA 重写实施计划

本文档把 `docs/design.md` 的愿景落成可执行的架构与阶段计划。冲突处以本文档的「决策记录」与「对 design.md 的更正」为准。

## 决策记录

| 编号 | 决策 | 理由 |
| ---- | ---- | ---- |
| D1 | 完全重写，旧 `client/`、actix 服务端、`rethink/finish/ask` 全部移除 | 旧产物是 web 服务端路线，与「轻量 CLI、Shell 即唯一工具」相反；Git 历史保留 |
| D2 | 分阶段交付，先跑通最小闭环 | inbox/跟随交接/断电恢复属并发一致性问题，一次性写完必带未验证缺陷 |
| D3 | 压缩复刻 DSH `compaction-basic` 真实行为（pruner + LLM 摘要） | 用户选择；已取得源码级参数 |
| D4 | 运行时 `tokio` + `reqwest` | 用户选择；便于将来并发与流式 |
| D5 | 5 个 crate：`yaa-msg`/`yaa-store`/`yaa-shell`/`yaa-core`/`yaa-cli` | 用户选择；依赖无环 |
| D6 | 模型供应配置与提示词经钩子获取，策略与限额留在 `config.toml` | 用户选择；原「所有配置」措辞不精确，按此执行 |
| D7 | 钩子输出结构化 stdout：`llm` 钩子按 TOML 解析，提示词钩子为纯文本 | 复用同一 TOML 解析器，避免 JSON 依赖 |
| D8 | 钩子时机：提示词钩子每会话一次，`llm` 钩子每次运行一次 | 避免每轮重复注入提示词污染历史 |
| D9 | 阶段 1 不含审批、压缩、inbox/单 runner/跟随、notify、断电恢复 | 用户选择 |
| D10 | 状态根按 XDG 拆分：`YAA_CONFIG_HOME` + `YAA_DATA_HOME`，`GLB_CFG` 指向配置根 | 用户选择 |
| D11 | 提示词钩子为替换语义：钩子 stdout 即完整提示词 | 用户选择；默认配置由 `yaa --get-prompt` 拼内置模板 |
| D12 | `ai` 与 `tool_call` 保持分离记录，以单批缓冲原子追加 | 用户选择；忠于 `design.md:31-33` |
| D13 | 过程记录默认打到 stderr（`# 描述` / `% 命令` / 块间 `---`，含非末步模型正文）；`--verbose` 追加思考与命令输出；删除 `--trace` | 用户要求对齐 `design.md` 示例观感；`--quiet` 保留为仅最终回复 |

## 对 design.md 的更正

第 1–7 条必须在动代码前确认，因为它们决定了不可变的 JSONL schema；第 8–11 条为实现阶段 1 时发现、事后补记，不改变已冻结的 schema，但修正 design.md 对行为的描述。

1. **压缩并非无模型调用**（`design.md:44`）。`@deepseek-ai/dsh-compaction-basic` 出厂必发一次摘要 LLM 调用；真正无模型的是伴生包 `dsh-compaction-tool-result-pruner`（纯头尾截断）。按 D3 两者都实现。
2. **压缩归属**：`design.md:19` 把「压缩摘要」列在 `yaa-store`，但 `design.md:45` 说压缩只发生在渲染阶段。更正为：**压缩属 `yaa-core` 的渲染管线，`yaa-store` 只负责存储**。
3. **单根 `$GLB_CFG`**：`design.md:186` 让配置、会话、历史同根。更正为按 XDG 拆分配置根与数据根（见「状态根目录」），`GLB_CFG` 仅作为配置根暴露给钩子。
4. **`turn_end` 记录**：`design.md:342` 出现但不在记录清单内。约定：**无 `tool_call` 的 `ai` 记录即为回合结束**，不引入独立类型。
5. **记录缺 `id`**：`design.md:31,32,37` 的 `user`/`ai`/`system` 无 `id`，无法支撑阶段 4 的幂等去重（`design.md:361,366`）。更正为**所有记录都有 `id`**，工具结果另用 `call_id` 关联。
6. **截断产物无载体**：`design.md:50` 未定义完整输出的路径字段。更正为 `tool_result` 增加可选 `output_files`（stdout/stderr 分两个文件）与 `truncated`。
7. **`client_id`/`turn_id`**（`design.md:301`）预留进 schema，阶段 4 使用。
8. **钩子不经 `shell` 工具执行**（`design.md:12`）。design 说系统生命周期钩子「触发 Shell 工具调用」实现。更正为：钩子由 `yaa-core` 的独立执行器（`crates/yaa-core/src/hook.rs`）以 `sh -c` 运行，与 `shell` 工具**不共用策略**——钩子单流超过 1 MiB 直接报错退出，不截断、不产生 `output_files`。只有审计记录以 `origin=hook` 的 `tool_call`/`tool_result` 表示，供渲染与缓存判定；「钩子就是同一个 Shell 工具」的表述不成立。
9. **用户记录先于提示词钩子落盘**（`design.md:27-31`）。design 的 JSONL 示例把钩子注入记录排在 `user` 之前；更正为 `user` 最先落盘、提示词钩子记录在其后（见「Agent loop」的用户记录先落盘）。渲染时钩子仍被提升到消息列表最前，模型上下文不变；好处是钩子失败也不丢用户输入。
10. **钩子 `tool_call` 不写 `description`**（`design.md:27`）。design 示例带 `"description":"注入系统提示词"`；更正为钩子记录不落 `description`，该文案是 `yaa-cli` 展示层按钩子名现算的（`crates/yaa-cli/src/printer.rs`），不入 JSONL。
11. **中断 `tool_result` 有两种互补补法**（`design.md:427-431`、本文件「渲染管线」失配兜底）。阶段 1 由渲染器在内存中合成中断 `tool` 消息，保证请求体合法，**不写回** JSONL；阶段 6 的「补中断 `tool_result`」则是把它作为记录持久化落盘。两者不冲突：前者保证任意时刻请求合法，后者让历史事实源自身自洽。

## 目标架构（阶段 1）

```plaintext
Cargo.toml                      # workspace
crates/
  yaa-msg/                      # 记录类型与 serde，无内部依赖
  yaa-store/                    # JSONL 同步读写、会话解析、归档、按偏移读
  yaa-shell/                    # 唯一工具：执行、超时、流式截断、完整输出落盘
  yaa-core/                     # config、agent loop、渲染、钩子、模型调用、压缩（后期）
  yaa-cli/                      # 二进制 yaa：clap 参数、事件渲染、stdout/stderr 策略
```

```plaintext
yaa-msg  ←  yaa-store  ←  yaa-core  ←  yaa-cli
             yaa-shell  ↗
```

- `yaa-shell` 不依赖 `yaa-msg`：它自持 `ShellLimits`，返回纯输出结构，由 `yaa-core` 组装成记录。
- `yaa-cli` 依赖 `yaa-core`，并直接依赖 `yaa-msg`/`yaa-store` 实现 `--clear`/`--status` 的会话运维；stdout/stderr 策略完全在 cli。
- `config` 类型归 `yaa-core`；`yaa-store` 只接受 `Path`，不依赖 config。
- `yaa-core` 通过 `EventSink`（trait 或闭包）向 cli 输出事件，stdout/stderr 策略完全在 cli。

## 状态根目录与路径模型

按 XDG 拆分，避免把会话数据写进配置目录：

```plaintext
$YAA_CONFIG_HOME   默认 ${XDG_CONFIG_HOME:-$HOME/.config}/yaa
  config.toml
  AGENTS.md                    # 默认用户提示词钩子的输入（可选）

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
- 会话不承诺跨机可迁移：提示词钩子缓存含机器相关信息，移动目录或换机可能拿到不同提示词，写入文档。

## 配置与钩子协议

```toml
[hooks]
system_prompt = 'echo "工作目录：$(pwd)" && yaa --get-prompt system'
user_prompt   = 'cat $GLB_CFG/AGENTS.md'
llm           = 'cat $YAA_CONFIG_HOME/llm.toml'

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
provider    = "openai"
base_url    = "https://api.deepseek.com"
api_key     = "..."
model       = "deepseek-chat"
temperature = 0.7
max_tokens  = 4096
```

规则与不变量：

1. 钩子以 `sh -c "<命令>"` 执行，cwd 为进程工作目录，环境含 `GLB_CFG`、`YAA_CONFIG_HOME`、`YAA_DATA_HOME`、`YAA_BIN`（当前 `yaa` 可执行文件路径，使默认钩子不依赖 `PATH`）。
2. **提示词钩子输出 = 完整系统/用户提示词**（替换语义，不叠加内置模板）。默认配置的钩子通过调用 `"$YAA_BIN" --get-prompt system` 主动拼入内置模板。因此纯 `echo hi` 的钩子不会得到工具说明——这是使用者责任，默认配置给出可运行范例。
3. **`llm` 钩子不落历史、不渲染**。其 stdout 含密钥，若按 `design.md` 记为 `tool_call`/`tool_result` 会同时污染 JSONL 与模型上下文。只记录一条 redacted 记录（不含 stdout 与命令，仅退出码与配置指纹），且渲染器永不将其变成消息。
4. **钩子缓存判定键 = (钩子名，fingerprint, 成功)**。`fingerprint` = hash(命令 + cwd + 配置根 + 数据根 + `yaa` 版本)：换目录或升级都会失效并重跑。失败不写成功标记，下次运行重试。
5. **`yaa --get-prompt <name>` 必须在加载 config、解析会话、执行任何钩子之前短路**，只打印内置模板。否则 `system_prompt` 钩子内部调用它会形成递归环。补一条递归防护测试。
6. 钩子输出有**独立上限**（不复用 Shell 的截断）：超限直接报错退出，绝不静默裁掉提示词。
7. 钩子执行有**硬超时**，并以进程组方式整组击杀（与 Shell 同一套方案），避免配置钩子挂死拖住 `yaa`。
8. 提示词钩子的执行记为 `tool_call(origin="hook")` + `tool_result`；只有 `code == 0` 且是该钩子名最新的一条才会被渲染成 `system` 消息，失败与被取代的记录仅作审计保留。

## 记录 schema（阶段 1 冻结）

每行一条 JSON，带标签枚举，`ts` 为 Unix 毫秒：

```plaintext
user        { type, id, ts, content, client_id?, turn_id? }
ai          { type, id, ts, model, tool_schema_hash, content }
tool_call   { type, id, ts, origin: hook|ai|user, call_id, command, description,
              hook?: {name, fingerprint}, redacted? }
tool_result { type, id, ts, call_id, stdout, stderr, code, timed_out,
              truncated, output_files?: { stdout?, stderr? } }
system      { type, id, ts, source, scope, content }
```

- `id` 为 ULID，全记录唯一，阶段 4 以此幂等去重。
- `call_id` 为模型返回的工具调用 id，`tool_result` 靠它配对。
- `ai` 与其后紧跟的 `tool_call(origin=ai)` 记录构成一个助手步骤，**两者以单次缓冲原子追加**（见下）。
- `approval_pending`/`approval_resolved` 阶段 2 引入。

### 追加的原子性

`yaa-store::append(&[Record])` 把整批记录序列化到一个 `Vec<u8>`，一次 `write_all`（`O_APPEND`）+ `fsync`。禁止对文件句柄逐条 `serde::to_writer`（多 syscall 会在同会话并发时交织出半行）。断电只会留下撕裂的尾行；**`append` 在写入前先修补尾行**：尾段能解析则补一个换行保留，否则截断到最后一个换行之后。`load` 同样容忍撕裂尾行。

## 渲染管线

`yaa-core` 的渲染是**纯函数**，返回 owned 的 `Vec<ChatMessage>`，压缩与 pruner 都插在这一层（阶段 3）。

| 记录 | 渲染结果 |
| ---- | -------- |
| 每个钩子名最新一条 `tool_call(origin=hook)`，且配对 `tool_result` 的 `code == 0` | `system` 消息（内容取 `stdout`），置于消息列表最前 |
| `user` | `user` 消息 |
| `ai` + 其后 `tool_call(origin=ai)` | 一条 `assistant` 消息，工具调用进 `tool_calls` |
| 配对 `tool_result` | `tool` 消息，`tool_call_id` 关联 |
| `tool_call(origin=user)` | 文本（`user` 或 `system`），**绝不渲染为 `tool`** |
| `system` | `system` 消息 |

- 分组规则：按 `ai` 步骤切分，步骤内工具调用按出现顺序排列，`tool_result` 按 `call_id` 配对。
- **失配兜底**：`ai` 步骤里的 `call_id` 若无对应 `tool_result`（断电、中断），渲染器合成一条 `tool` 消息（内容为中断标记），保证 `assistant.tool_calls` 后紧跟配对的 `tool` 消息，请求体始终合法。
- **钩子去重**：每个钩子名只取最新一条 `code == 0` 且非 redacted 的调用，旧提示词与失败输出不进入上下文。
- **空回合**：既无文本又无工具调用的 `ai` 记录不渲染消息（避免产生 `{"role":"assistant"}`）。
- 工具以 OpenAI function schema 暴露，仅一个 `shell` 函数，参数 `command`（必填）与 `description`（可选）。
- `tool_schema_hash` = hash(本轮暴露的 tools schema)，写入 `ai` 记录，用于历史兼容与缓存失效判定。

## 唯一工具：Shell

- 执行 `sh -c <command>`，`stdin = null`（否则 `echo ... | yaa` 时工具子进程会偷走 stdin）。
- **进程组**：`process_group(0)` 启动；超时时对整组 `killpg`，避免 `sh -c` 的孙进程持有管道导致读取挂死或孤儿继续运行。读取线程的等待也纳入同一时限：直接子进程退出但孙进程仍持有管道时不得无限等待，超时后放弃该流输出（`// PJ568 方案` 标注）。
- **流式有界读取**：边读边写完整输出文件，内存中只保留头 `head_chars` 与尾 `tail_chars`。禁止先 `wait_with_output` 再截断（`yes` 之类会 OOM）。
- 每个流一个完整输出文件，文件名 = 本地生成的 ULID + 流后缀（**不用模型给的 `call_id` 当文件名**，避免路径穿越与覆盖）；`output_files.stdout` / `output_files.stderr` 记相对**数据根**的路径；会话在数据根之外（`--session <任意路径>`）时回退记绝对路径，绝不静默留空。
- 截断按**流分别计**的字符数（stdout、stderr 各自独立），入记录时做 lossy UTF-8，原始字节写文件。
- 超时以 `timed_out: true` 标记，不借用 `code=-1`。
- 阶段 1 默认 allow，无规则（审批在阶段 2；审批规则必须豁免 `origin=hook`，否则钩子会被卡死）。
- 输出文件清理策略：**`--clear` 不移动 `outputs/`**，归档记录中的相对路径继续有效；磁盘无界增长作为已知限制记录，阶段 3 再引入保留策略。

## Agent loop 与并发形状（阶段 1）

即使阶段 1 无锁，入口形状必须现在就是「先发布消息 → 再决定 runner/follow」，否则阶段 4 要重写入入口。

```rust
async fn main_flow(cfg) -> Result<ExitCode> {
    let session = resolve_session(&cfg)?;          // canonical cwd -> sid / --session
    let user = build_user_record(prompt);
    store::append(&session, &[user]);              // 阶段 1：直接追加；阶段 4：改为 inbox 发布

    let guard = try_acquire_runner(&session);      // 阶段 1：恒成功，无锁
    match guard {
        Some(g) => run_agent_loop(&session, &cfg, g).await,
        None    => follow(&session).await,         // 阶段 1：不可达
    }
}

async fn run_agent_loop(session, cfg, guard) -> Result<ExitCode> {
    let llm = resolve_llm_hook(cfg)?;              // 每次运行一次
    store::ensure_prompt_hooks(session, cfg)?;     // 每会话一次，属 core 而非 store
    for step in 0..cfg.agent.max_steps {
        let records = store::load(session)?;       // 每步重读（design.md:59）
        let ctx = render(&records);                // 纯函数
        let resp = chat_with_retry(&llm, &ctx, &[shell_tool()]).await?;
        store::append(session, &ai_step_records(resp))?;   // ai + 其 tool_calls 原子一批
        if resp.tool_calls.is_empty() { return Ok(ExitCode::Success); }
        for call in &resp.tool_calls {
            let out = shell::run(call, &cfg.shell).await?;
            store::append(session, &[out.into_record()])?;
        }
    }
    Ok(ExitCode::MaxSteps)
}
```

- `yaa-store` 保持**同步 API**（追加、按偏移读、归档）。在 core 调用点用 `spawn_blocking` 包装，不做成假 async。
- store 预留 `read_from(&session, offset) -> (Vec<Record>, new_offset)` 供阶段 4 跟随使用；`load` 容忍撕裂尾行。
- `load` 每步重读，阶段 4 的 inbox 插入才能在下一轮被看到。
- `store::ensure_prompt_hooks` 实为 core 函数（命名保留，但归属 core）。
- **用户记录先落盘**：`run` 一进入就 append `user`，再执行钩子与模型调用；钩子或 `llm` 配置失败也不会丢失用户输入。
- **空回复**：无文本且无工具调用时，记一条空 `ai` 记录、向 stderr 发提示并结束回合（退出 `0`），且该记录不渲染消息。

### 错误与退出策略

- 模型 429/5xx/网络错误：指数退避重试 3 次；仍失败则以模型错误退出。此时历史停在「工具批次已完成」的合法状态，下次运行可续接。
- 退出码：`0` 成功，`1` 通用错误，`2` 用法错误，`3` 配置/钩子错误（含钩子执行失败），`4` 模型错误，`5` 达到 `max_steps`。
- 空回复（无内容且无工具调用）视为回合结束，退出 `0`，并给出 stderr 提示。
- 半批次收尾：模型失败或 Ctrl-C 后，已写入的 `tool_result` 保留；缺失的结果由渲染器兜底（见上）。

## CLI（阶段 1）

```plaintext
yaa [PROMPT]...           # 多个位置参数以空格连接；空则从 stdin 读
yaa --clear               # 归档当前会话
yaa --status              # 打印会话摘要
yaa --session <path>      # 指定会话文件
yaa --get-prompt system|user
yaa --config <path>
yaa --cwd <path>          # 改变进程工作目录
yaa --verbose             # 追加思考内容与命令输出
yaa --quiet               # 只打印最终回复
```

过程记录默认打到 stderr：钩子与工具命令为 `# 描述` + `% 命令`，块间 `---`；非末步的模型正文也在此打印，末步回复只走 stdout（见 D13）。

后移到阶段 2 及以后：`--json`（事件 schema 未定，随 core 的 `EventSink` 一起定义）、`--no-store`、`--approve`、`--notify --to`。

## 阶段划分与验收

每阶段独立可验证、可提交。原子修改后必须通过 `cargo check --all-targets`、`cargo clippy`、`cargo test`。所有改动在 `rewrite/yaa-cli` 分支。

### 阶段 1：最小闭环

内部顺序：**记录 schema + store + render 的 golden 测试 → 钩子与模型适配 → agent loop → CLI 表面**。

验收：

1. `yaa "你好"` 在无工具调用时打印模型回复，`default.jsonl` 留下提示词钩子的 `tool_call`/`tool_result`、`user`、`ai`。
2. 再次 `yaa "继续"` 能读到上一轮并正确渲染续接；**断言第二跳请求体**符合 OpenAI 消息序列。
3. 构造需要 Shell 的多步任务，工具结果回填并循环至最终回答。
4. `--clear` 把 `default.jsonl` 移入 `history/<sid>/sessions-<ts>.jsonl` 后开新历史；`outputs/` 留在原地，归档记录的相对路径仍有效。
5. 集成测试用本地假 OpenAI 服务器，断言 JSONL 序列与 Shell 副作用；**断言请求体与 JSONL 均不含 `api_key`**。
6. 递归防护测试：`system_prompt` 钩子内部调用 `yaa --get-prompt system` 能正常返回而非递归。
7. 中断测试：模型返回 `tool_calls` 后进程被杀，下次运行渲染器合成中断 `tool` 消息，请求体合法。

### 阶段 2：审批

规则（allow/deny/ask 通配符）、`approval_pending`/`approval_resolved`、`--approve` 同意一次、用户发新提示词时 pending 自动 deny、**`origin=hook` 豁免审批**。同阶段补 `--json`/`--no-store`。

### 阶段 3：压缩

pruner 先跑（排除 `origin=hook` 的结果，避免裁掉系统提示词），仍超阈值再走 LLM 摘要。边界选择照搬 DSH：system 头不压缩、按 token 预算从尾部保留、切点向头部回退到工具配对平衡处。摘要产物写入 `sessions/<sid>/summaries/<key>.json` 侧车缓存（key = 被遮蔽区间 id + 模型），不修改 `default.jsonl`。参数 `thresholdRatio=0.8`、`retainRatio=0.16`、`CHARS_PER_TOKEN=4`（已知低估 CJK）改为读配置，不硬编码。

### 阶段 4：单 runner 与多客户端注入

`runner.lock` 选举、`inbox/{tmp,pending,consumed}` 无锁发布、`consume_inbox`（append → fsync → rename 顺序）、跟随与按偏移读取、安全点消费。`--clear` 在 runner 运行中的语义在此定义。

### 阶段 5：notify 与后台任务

`YAA_SESSION`/`YAA_PARENT`/`YAA_NOTIFY` 解析链、`--notify --to`、后台结果作为 `system` 消息回注，复用阶段 3 的截断与压缩。

### 阶段 6：断电恢复与不变量

截断半行、补中断 `tool_result`、补 `approval_resolved: deny`、按 `id` 去重；不变量测试。

## 测试与验证策略

- 单元测试：记录序列化往返与 golden JSONL；渲染映射（含多工具、中断、hook 配对、`origin=user`）；store 追加/加载/归档/撕裂尾行；Shell 进程组、超时、流式截断。
- 集成测试：`dev-dependencies` 起极简假 OpenAI 服务（`axum` 或 `hyper`，仅测试用），按脚本返回 `tool_calls`，端到端跑通并用临时目录隔离。
- 钩子测试：临时 `config.toml` + `echo`/`cat` 钩子，断言记录、fingerprint 失效与递归防护。
- 收尾运行 `autocorrect --fix` 与 `autocorrect --lint docs crates`。

## 已知限制（阶段 1）

1. 无锁：同会话并发运行的写入可能交织（虽然单次 `write_all` 保证不出现半行，但顺序不可控）；阶段 4 解决。
2. 仅 POSIX（`sh -c`、`killpg`）；Windows 不在阶段 1 范围。
3. `outputs/` 无保留策略，磁盘可能无界增长。
4. 会话不承诺跨机迁移。
5. token 估算低估 CJK。

## 已确认项与默认假设

已确认：XDG 双根（D10）、提示词钩子替换语义（D11）、`ai` 与 `tool_call` 分离记录 + 单批原子写（D12）。

默认假设（未特别反对即执行）：截断阈值 `max_stream_chars=32768`、`head_chars=8192`、`tail_chars=2048`；包 `yaa-cli` 产出二进制 `yaa`；workspace 版本从 `0.1.0` 起。
