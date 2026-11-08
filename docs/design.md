# XIDONG 智能体设计

设想一个基于 Rust 的极致轻量的智能体（AI Agent）。
每个模块都分 crate 开发，先用 Cargo workspace 管理，边界稳定后考虑拆分仓库。

> 本文是构想稿。实现现状，以及本文与实现的已知不一致处，以 [`docs/plan.md`](plan.md) 为准。
> 其在“对 design.md 的更正”部分逐条列出。

用 `xidong` 运行的非交互式命令。

## 模块设计

对每个模块做最小化设计。智能体只有 Shell 命令这一个工具调用。
为确保高度可维护性，模块数量和功能数量应做到最少。

可能先做三到四个 crate 吧：

- `xidong::cli`：参数解析、入口、展示渲染、stdout/stderr 策略；`xidong --notify` 的写入路径也在此短路。
- `xidong::core`：智能体循环（Agent loop）、工具定义（`tools`）、模型调用、消息模型、渲染与展示投影、上下文分层装配（见 [`dsh-context.md`](dsh-context.md)）。
- `xidong::store`：会话 SQLite 事件库、归档、加载、文件锁，以及文件 inbox/spool 的原子发布与扫描。
- `xidong::exec`：Shell 执行机制，负责进程组、超时、流式截断、完整输出落盘；不含工具定义。
- `xidong::msg`：消息类型。

唯一工具的全部知识——function schema、提示词片段、参数解析、错误文案、调用编排与结果转换——集中在 `xidong::core::tools`，不跨 crate 维护；`xidong::exec` 只是被它调用的执行机制。单工具场景不引入 trait/registry 与模板引擎。

向上下文注入内容只有一条外部接口：`xidong --notify` 把外部产出（初始化脚本、后台任务、用户管道）写成 `system` 事件。系统提示词不再走专用的钩子记录，而是空会话初始化脚本调用 `xidong --notify` 写入的普通事件；`llm` 配置加载不进入上下文，也不留记录。

## 历史消息

历史消息管理框架模块把历史消息作为事件序列存储：每会话一个 SQLite 库，`events` 表 append-only，每行 `payload` 保存一条完整记录的 JSON，`type`/`ts` 由 SQLite 生成列从 `payload` 派生。

```json
{"type":"system","ts":...,"id":"01J...","source":"notify","origin":"/abs/path/to/sessions/<sid>/session.db","command":"\"$XIDONG_BIN\" --get-prompt system","content":"<系统提示词>\n当前智能体环境是 XIDONG，一个以 Shell 为唯一工具的智能体。……\n</系统提示词>"}
{"type":"system","ts":...,"id":"01J...","source":"notify","command":"cat \"$GLB_CFG/AGENTS.md\"","content":"<用户总提示词>\n……AGENTS.md 内容……\n</用户总提示词>"}
{"type":"system","ts":...,"id":"01J...","source":"notify","command":"echo \"工作目录：$(pwd)\"","content":"<当前环境>\n工作目录：/home/pj568/项目/xidong\n</当前环境>"}
{"type":"system","ts":...,"id":"01J...","source":"init","content":""}
{"type":"user","ts":...,"id":"01J...","content":"你好"}
{"type":"ai","ts":...,"id":"01J...","model":"...","tool_schema_hash":"...","content":"...","reasoning":"..."}
{"type":"tool_call","origin":"ai|user","ts":...,"id":"01J...","call_id":"call_xxx","command":"...","description":"……"}
{"type":"approval_pending","ts":...,"id":"01J...","call_id":"call_xxx","command":"...","matched_rule":"..."}
{"type":"approval_resolved","ts":...,"id":"01J...","call_id":"call_xxx","decision":"approve|deny"}
{"type":"tool_result","ts":...,"id":"01J...","call_id":"call_xxx","stdout":"...","stderr":"...","code":0,"timed_out":false,"truncated":true,"output_files":{"stdout":"sessions/<sid>/outputs/01J....stdout.out"}}
{"type":"system","ts":...,"id":"01J...","source":"notify","command":"git status --short","content":"<由用户执行>\n……外部通知……\n</由用户执行>"}
```

> 上面是各事件的 `payload` 内容，在库中按 `events.seq` 顺序存放；`seq` 单调递增，是事件在会话内的唯一位置。
>
> `ts` 为 Unix 毫秒；`id` 为 ULID，全记录唯一；`tool_result` 用 `call_id` 与 `tool_call` 配对。
>
> `system` 事件是内容进入上下文的唯一通道：`source=notify` 承载 `xidong --notify` 写入的通知，正文内联 `<注释>` 信封（呈献给模型时即此文本）；`source=init` 是空会话初始化的簿记，`content` 为空、不渲染。可选 `command` 保存产生该通知的命令，展示时以 `% 命令` 呈现，投影时以 `[执行命令]` 前缀进入上下文（D31）；stdin 形态与簿记没有此项。阶段 5 另加归因字段：`origin` 为发布者会话库绝对路径（`--notify` 总是写），`call_id` 为产生本次通知的工具调用 id（仅继承值合法时写），二者均可缺省。注入内容的渲染角色由事件在日志中的**物理位置**决定，不依赖额外字段；哪些 `system` 可注入统一由 `is_injectable_system` 判定（见「环境链与多智能体通知」）。

过往历史消息不可变，上下文只是基于历史消息构造的渲染视图。
渲染时语义转换为对应接口格式的消息，所以模型始终看到的是结构和内容合规的上下文。
如开头的初始化注入被格式化为 OpenAI 的 `system`，而非 `assistant.tool_calls` 和 `tool`。

上下文按稳定性分层装配（对齐 [`dsh-context.md`](dsh-context.md)）：稳定前缀放消息列表最前的 `system`，易变内容追加在历史尾部，工具 schema 独立于消息。渲染规则：

- 首条 `user` 事件之前的所有 `system` 通知按物理顺序合并为**一条** `system` 消息，作为初始化注入的稳定前缀。
- 首条 `user` 之后的离群 `system` 通知与相邻 `user` 事件合并为一条 `user` 消息；其后没有 `user` 时单独成为一条 `user` 消息。易变内容因此留在历史尾部，不污染前缀。
- 空内容的 `system`（初始化簿记）不产生消息。
- `ai` 与其 `tool_call`、配对 `tool_result` 分别投影 `assistant` 与 `tool` 消息；`tool_call(origin=user)` 绝不做成 `tool` 消息。

渲染视图是模型消息与用户展示的共同事实源：两者都从同一份事件日志派生，不维护第二套执行事件流。`llm` 配置加载不写事件，既不渲染、也不展示。

上下文压缩参考 DSH (DeepSeek Harness) 的 `compaction-basic`：先跑无模型的头尾 pruner，仍超阈值再发一次摘要 LLM 调用。
压缩只发生在渲染阶段，原始事件库不变。
压缩保留未完成工具调用。

每一次执行 `xidong "……"` 都默认接着当前目录下已存在的会话历史消息继续，除非 `xidong --clear`。

工具调用输出过长时，中间部分被截去，并产生包含完整输出的外部文件。

目录结构：

```plaintext
$XIDONG_CONFIG_HOME/config.toml                                      # 配置根
$XIDONG_DATA_HOME/sessions/<sid>/session.db                          # 主历史事件库（WAL 副产物 -wal/-shm 同目录）
$XIDONG_DATA_HOME/sessions/<sid>/{runner,clear,init}.lock            # runner 选举 / clear 互斥 / 建库串行化
$XIDONG_DATA_HOME/sessions/<sid>/outputs/<ulid>.{stdout,stderr}.out  # 截断后的完整输出
$XIDONG_DATA_HOME/history/<sid>/sessions-<ts>.db                     # 主历史归档快照（VACUUM INTO）
```

> 状态根按 XDG 拆分：配置根 `$XIDONG_CONFIG_HOME`，数据根 `$XIDONG_DATA_HOME`（见 plan.md D10）。
>
> 主循环每一步重读会话库；写入以一个 `BEGIN IMMEDIATE` 事务批量提交（WAL + `synchronous=FULL`），不逐条写。事务保证一批记录要么全部可见、要么全部不可见，因此不存在半行撕裂。
>
> 当前工作目录规范化后哈希得 `<sid>`。

## 使用示例

`xidong "……"` 一次运行内部可以循环多步，使用效果伪示例：

```zsh
$ xidong 你好吗？
# 系统提示词
% "$XIDONG_BIN" --get-prompt system

---
# 用户总提示词
% cat "$GLB_CFG/AGENTS.md"

---
# 当前环境
% echo "工作目录：$(pwd)"

---
我很好，谢谢，您还有什么需要帮助的吗？

$ xidong 不用了，只是确认是否能正常回复。
好的，需要帮助时随时叫我。

$ xidong --clear
# 归档历史消息（内部完成，不调用工具或钩子）
$XIDONG_DATA_HOME/history/<sid>/sessions-<ts>.db

$ cd /home/pj568/桌面
$ xidong 不用子智能体，帮我查未来五天的天气并写入到文件 \`/home/pj568/桌面/temp.txt\`。
# 系统提示词
% "$XIDONG_BIN" --get-prompt system

---
# 用户总提示词
% cat "$GLB_CFG/AGENTS.md"

---
# 当前环境
% echo "工作目录：$(pwd)"

---
当前工作目录已在 `/home/pj568/桌面/`。
请告诉我未来五天天气要查询的城市或具体地点；我拿到地点后会查询并写入 `/home/pj568/桌面/temp.txt`。

可选回复：

1. 根据 IP 获取的档期位置；
2. 你告诉我要查询的具体地点；
3. 其它，你来说。

$ xidong 按网络定位。
# 获取 IP 所在位置
% xidong-webfetch https://ipinfo.io/json

---
# 获取所在位置未来天气预报
% xidong-webfetch https://api.open-meteo.com/v1/forecast?latitude=22.4821&longitude=113.8850&daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max,precipitation_sum,wind_speed_10m_max&timezone=Asia%2FShanghai&forecast_days=6

---
# 检查文件存在情况
% ls -la

---
# 编写指定文件
% cat > temp.txt <<'EOF'
未来五天天气（网络定位：广东赤湾，坐标约 22.4821, 113.8850）
数据来源：Open-Meteo，时区：Asia/Shanghai

2026-09-18：晴，25.3–33.0 °C，降水概率 0%，降水量 0.0 mm，最大风速 15.7 km/h
2026-09-19：小毛毛雨，25.1–32.7 °C，降水概率 35%，降水量 0.4 mm，最大风速 14.2 km/h
2026-09-20：小毛毛雨，25.2–32.8 °C，降水概率 87%，降水量 0.3 mm，最大风速 12.0 km/h
2026-09-21：中等毛毛雨，24.9–30.3 °C，降水概率 94%，降水量 4.8 mm，最大风速 8.5 km/h
2026-09-22：雷暴，23.7–29.3 °C，降水概率 75%，降水量 14.7 mm，最大风速 9.8 km/h' > ./temp.txt
EOF

---
已按网络定位（广东赤湾）查询未来五天天气，并已写入：

`/home/pj568/桌面/temp.txt`

$ xidong 我已查看，删掉它。
# 删除 temp.txt（阶段 2：命中规则后拦截）
% rm /home/pj568/桌面/temp.txt
匹配到 `rm`，是否批准执行 `rm /home/pj568/桌面/temp.txt`？

$ xidong --approve
# 删除 temp.txt
% rm /home/pj568/桌面/temp.txt

---
# 检查状态
% ls -la

---
删除完毕。
```

遇到拦截时退出，`--approve` 允许执行并继续，直接写提示词等情况默认拒绝并提示已被拒绝（审批整体属阶段 2）。

初始化注入与外部通知都用与工具调用一致的块样式展示：默认 `# 注释` + `% 命令`（摘要），`--verbose` 追加内容全文；初始化只在空会话触发一次，`llm` 配置加载不展示。

本节是使用效果示意，非逐字规范：具体参数以下文“参数（待定）”为准，实现状态见 `docs/plan.md`。示例里的 `xidong-webfetch` 是外部辅助命令，不属于 `xidong` 自带工具；提示词打印已定为 `xidong --get-prompt` 子命令。

## 参数（待定）

阶段 1 已实现 `--status`/`--session`/`--clear`/`--verbose`/`--quiet`/`--get-prompt`/`--cwd`/`--config`，以及 `--notify` 的写入形态；`--json`/`--no-store`/`--approve` 属阶段 2；`--notify` 的跨会话目标解析（`--to`/`XIDONG_NOTIFY`/`XIDONG_PARENT`）已于阶段 5 实现。

```plaintext
xidong [PROMPT]...              # 多个位置参数以空格连接；空则从 stdin 读
xidong -- "[PROMPT]"            # 同上
xidong --status                 # 看会话摘要
xidong --session $path_to/session.db
xidong --clear                  # 归档当前目录的会话；若已有 runner 在跑，此时再运行 `xidong "内容"` 会把消息排入 inbox 并跟随输出到循环结束（阶段 4 已实现）。
xidong --verbose                # 追加思考内容与命令输出
xidong --quiet                  # 仅打印最终结果输出
xidong --get-prompt system      # 仅打印内置系统提示词
xidong --notify="注释" -- cmd   # 执行 cmd，把输出作为通知写入当前会话
cmd | xidong --notify="注释"    # 从 stdin 读，写入当前会话
xidong --notify="注释" --to $path_to/session.db -- cmd  # 写入显式目标会话（必须已存在）
cmd | xidong --notify="注释" --to "$XIDONG_PARENT"         # 回注父会话
xidong --json                   # 结构化事件
xidong --approve                # 同意一次
xidong --cwd="……"               # 设置会话目录
xidong --no-store
xidong --config $path_to/config.toml
……
```

## 审批（阶段 2）

`xidong --approve` 同意最旧的未决（尚未批准或拒绝）`pending` 命令执行并继续智能体循环，一次性调用多个命令都被拦截时需连续执行多次此命令。

用户直接发新提示词时，pending 自动 deny。

## 配置文件

配置文件 `$XIDONG_CONFIG_HOME/config.toml` 内配置钩子命令、命令和目录拦截放行规则、压缩触发条件、timeout 和 max-steps 什么的。
钩子只有两个：`init`（空会话初始化，内容经 `xidong --notify` 写入上下文）与 `llm`（模型供应配置，不进入上下文）。默认命令均可替换。`llm` 输出的配置含 `endpoint_format`，用于决定思考内容是否回传（见 [`plan.md`](plan.md) D24）。

阶段 1 已实现钩子命令、timeout 与 max-steps；拦截放行规则属阶段 2，压缩触发条件属阶段 3。

### 规则

和 OpenCode 相似，默认 allow，可配置 deny 和 ask 名单通配符什么的。

## 通知与后台任务

`xidong --notify` 是内容进入会话的外部接口（写入形态属阶段 1，后台与跨会话语义已于阶段 5 实现）：

- `xidong --notify="注释" -- cmd`：执行命令，把 stdout 作为通知写入会话。
- `cmd | xidong --notify="注释"`：从 stdin 读内容写入。
- 注释可省略，缺省为「由用户执行」；通知正文内联 `<注释>` 信封。

`system(source=notify)` 事件不被夹在模型的 `assistant.tool_calls` 与配对 `tool` 消息之间，因此是「离群」的；渲染时自动转换为呈现层信封，见上文渲染规则。

模型可以让 shell 工具执行后台作业，但**必须把整条作业的 stdout/stderr 关闭或重定向**：`xidong::exec` 给工具自建进程组，直接子进程 `sh` 退出后仍会读取输出管道；后台进程若继续持有管道，读取宽限期（2 秒）到期后工具会对整组发 `SIGKILL`，内层 `xidong --notify` 永远来不及发布。因此推荐：

```shellscript
{ some-cmd | xidong --notify="注释"; } >/dev/null 2>&1 &
# 或让后台作业脱离工具进程组
setsid sh -c 'some-cmd | xidong --notify="注释"' >/dev/null 2>&1 &
```

两种形态都让 `shell` 工具立即拿到空输出；后台任务结束后，`xidong --notify` 进程把结果发布到会话的 inbox，由运行中的 runner 在安全点落库，模型在下一轮才看到它。通知结果的压缩复用工具调用的过长截断与外部完整输出文件。

后台子智能体复用同一方法，如：

```shellscript
{ xidong --quiet --cwd .workspace/temp -- "你作为子智能体做某些任务。" \
  | xidong --notify="子智能体" --to "$XIDONG_SESSION"; } >/dev/null 2>&1 &
# 或让后台作业脱离工具进程组
setsid sh -c 'xidong --quiet --cwd .workspace/temp -- "你作为子智能体做某些任务。" | xidong --notify="子智能体" --to "$XIDONG_SESSION"' >/dev/null 2>&1 &
```

### 环境链与多智能体通知（阶段 5）

> 状态：阶段 5 已实现。裁决理由见 `plan.md` 的 D76–D85。

身份拆分为三者：`self`（本会话）、`target`（本次通知目标）、`parent`（父会话）。通知只写目标会话的 inbox（发布失败且可重试时写 spool），不启动目标会话的模型。

#### 环境变量与 `xidong_env`

会话变量收敛到纯函数 `xidong::core::session::xidong_env(self_db, incoming_env, call_id)`：钩子（`hook_env`）、shell 工具 `extra_env` 与 `--approve` 执行路径共用它，消除「会话变量一处拼、限额变量另一处拼」的漂移；`XIDONG_NOTIFY_*` 限额仍由 shell 工具追加，不在本函数内。

| 变量                  | 含义                       | 设置规则                                                                 |
| --------------------- | -------------------------- | ------------------------------------------------------------------------ |
| `XIDONG_SESSION`         | 本会话库绝对路径           | 每次 xidong 无条件重设为自己                                                |
| `XIDONG_NOTIFY`          | 默认通知目标               | 默认本会话；继承值非空且 ≠ 自身 `XIDONG_SESSION` 时保留                     |
| `XIDONG_PARENT`          | 父会话库绝对路径           | 优先保留继承的 `XIDONG_PARENT`（非空且 ≠ 自身，中间层）；否则取继承的 `XIDONG_SESSION`（非空且 ≠ 自身，孵化者）；都没有则不输出 |
| `XIDONG_NOTIFY_CALL_ID`  | 产生当前工具调用的 id      | 仅 shell 工具执行时注入本次 `call.id`；钩子不注入                         |

- `run` 自身仍只按 `--session`/cwd 决定会话，不因继承的 `XIDONG_SESSION` 改变自己；对子进程，`XIDONG_SESSION` 无条件重设为本会话。
- 继承的 `XIDONG_NOTIFY`（非空且 ≠ 自身）会保留：父的 shell 工具默认注入 `XIDONG_NOTIFY=父`，因此子智能体默认通知父；只继承 `XIDONG_SESSION` 而没有 `XIDONG_NOTIFY` 时默认通知自己。回自己用 `--to "$XIDONG_SESSION"`，回父用 `--to "$XIDONG_PARENT"`。
- `XIDONG_PARENT` 为空时执行 `--to "$XIDONG_PARENT"` 得到空串，是用法错误（退出码 2），不会静默写入错误目标。
- 子内部再次 spawn 时，`XIDONG_NOTIFY_CALL_ID` 覆盖为最近调用点；`origin` 始终是发布者自己的会话库。

#### `--to` 优先级与严格校验

目标优先级从高到低：显式 `--to` → `--session` → `XIDONG_NOTIFY`（非空且 ≠ 自身）→ `XIDONG_SESSION`（非空）→ 当前 cwd 默认会话。

显式 `--to` 走固定阶梯，只报第一个失败（退出码 1，**绝不创建**目标目录或文件）：

1. 绝对化后存在（文件或目录）；
2. 目录 → 其中必须已有 `session.db`；
3. 旧 `.jsonl` 报旧格式错误并提示 `--clear`；
4. 文件必须是 xidong 会话库（SQLite 文件头与 store 的 `application_id`/`user_version`）；
5. 会话未归档；
6. 其余 IO 错误原样返回。

没有 `--to` 的普通 `--notify` 保持既有语义：目标不存在时 stderr 警告并创建。用法错误（退出码 2）：

- `--to` 为空串；
- `--to` 与 `--session` 同时显式给出且绝对化后的数据库路径不同；
- `--to` 与 `--no-store` 同用；
- `--to` 未与 `--notify` 同用（clap 校验）。

显式 `--to` 或 `XIDONG_NOTIFY` 选中的目标一律按跨会话处理，写目标 `pending/`，即使它指向自身也不写 `init/`。`--approve` 不认 `--to`：其解析链是 `--session` → `XIDONG_SESSION` → cwd，`--to` 与 `--approve` 组合因 `--to` 要求 `--notify` 而被 clap 拒绝。

#### 父子血缘与归因

通知记录新增两个可选字段：

- `System.origin`：发布者会话库的绝对路径。`--notify` **总是**写；有继承的 `XIDONG_SESSION` 时用其值，否则用解析出的自发会话。
- `System.call_id`：产生本次通知的工具调用 id。仅当继承的 `XIDONG_NOTIFY_CALL_ID` 匹配 `[A-Za-z0-9_-]{1,128}` 时写，非法值静默忽略、不报错。

两者都是 `Option` 且 `skip_serializing_if = "Option::is_none"`：旧 JSON 记录缺字段解析为 `None`，不需要 SQLite schema 迁移。归因字段只说明「谁发的」，不改变渲染分组。

`is_injectable_system` 是唯一白名单谓词（当前仅 `source == "notify"`），display、render 与 inbox 载荷校验共用它，避免三处对 `system` 的判定漂移。

#### spool 保底与补投

发布失败时不能静默丢通知，也不应把命令的退出码改掉：

- 位置：`<会话根>/<artifact 前缀>spool/<记录 id>.json`（`tmp/` 与 `rejected/` 是其子目录），载荷、命名、id 校验与 1 MiB 上限和 inbox 完全一致。
- 降级：`publish_with_generation` 或 notify 预检失败且属于**可重试**类别（BUSY/IO）时写 spool；generation 变化（真归档）属终态，不 spool；命令退出码保持不变，stderr 提示已暂存；spool 也失败才报错。spool 写入自身持 `clear.lock` 共享锁并复查 generation/archived，与 inbox 发布同锁序，避免落在已归档/新 generation 下。
- 补投：runner 在崩溃恢复之后、`init` 之前**只扫描 spool 自身**（不合并、不提前消费 inbox），按记录 id 顺序 `append_idempotent` 补写后删除文件；失败保留文件，下轮重试。follower 不补投，只提示滞留数量。
- 清理：消费即删；`spool/tmp/` 沿用 1 小时残留清理；不设过期丢弃。
- `--status` 显示 spool 计数；`--clear` 遇到非空 spool 拒绝归档。

#### 后台回注范式

```shellscript
# 父的 shell 工具内：整条后台作业重定向输出，子智能体完成后把结果回注到父会话
{ xidong --quiet --cwd .workspace/dir -- "作为子智能体完成 <任务>" \
  | xidong --notify="子智能体完成" --to "$XIDONG_SESSION"; } >/dev/null 2>&1 &

# 或让后台作业脱离工具进程组
setsid sh -c 'xidong --quiet --cwd .workspace/dir -- "作为子智能体完成 <任务>" | xidong --notify="子智能体完成" --to "$XIDONG_SESSION"' >/dev/null 2>&1 &

# 或子自己回父
xidong --notify="..." --to "$XIDONG_PARENT"
```

后台作业必须关闭或重定向 stdout/stderr：`xidong::exec` 在直接子进程退出后读取输出管道，后台进程若继续持有管道，读取宽限期（2 秒）后会被整组 `SIGKILL`，内层通知永远不到达；`setsid` 形态则让作业脱离工具进程组。`&` 让任务立即转入后台，shell 工具马上返回；任务结束后通知进入 inbox，运行中的会话在下一个安全点消费，没有 runner 时留待下次 `xidong` 处理。`call_id` 沿环境链继承父的工具调用 id，子内部再 spawn 时覆盖为最近调用点。跨数据根允许：`output_files` 的相对路径基准按发布者的数据根解析（已知限制）。

### 主循环召回

`xidong --notify` 发布时持 `clear.lock` 共享锁做 generation 守卫并写文件 inbox；运行中的 runner 在安全点消费落库，未运行时留待下次启动。发布失败且可重试时降级 spool，由下次非跟随运行补投。`--clear` 持该锁的排他锁，因此归档与通知发布不会交错。

| 状态         | 收到 `system` 消息的时机 |
| ------------ | ------------------------ |
| 主循环运行中 | 主循环的下一个循环开始时 |
| 主循环已退出 | 下次该目录 `xidong` 启动时  |

## 单 runner 与多客户端注入（阶段 4，已实现）

> 状态：阶段 4 已实现并合入。本节描述实际行为；裁决理由见 `plan.md` 的 D68–D75。

### 目标

- 同一会话同一时刻只有一个 runner（`runner.lock` 排他 `flock`）。
- 已有 runner 运行时，再执行 `xidong "内容"` 不启动第二个 runner，而是把消息发布到 inbox 并跟随。
- 新消息先进入 inbox，由 runner 在安全点插入主历史。
- 插入发生在当前工具调用批次全部结束后，不打断 `tool_call` / `tool_result` 配对。
- 注入客户端默认跟随主历史输出，直到本轮工作排空。
- 断电重启后不丢消息、不重复插入、不破坏历史语义。

### 文件布局

```plaintext
$XIDONG_DATA_HOME/sessions/<sid>/
  session.db             # 主历史事件库，只由 runner 写
  runner.lock            # runner 选举锁（flock 排他）
  clear.lock             # clear 排他 / inbox 发布共享
  init.lock              # 首次建库串行化
  outputs/               # 截断工具的完整输出
  summaries/             # 渲染期压缩的侧车摘要
  inbox/
    tmp/                 # 发布临时文件，不参与扫描；崩溃残留由 cleanup 清理
    pending/             # 普通 user 与外部 notify
    init/                # init 钩子期间的 notify
    rejected/            # 解析失败、超限或非法载荷
  spool/                 # 发布失败（BUSY/IO）的通知暂存区，下次运行补投
    tmp/                 # 发布临时文件，1 小时残留清理
    rejected/            # 解析失败、超限或非法载荷
```

锁文件永久保留，不删除，不用「文件是否存在」判断 runner，只使用 `flock` 判断。inbox 与 spool 消费后直接删除消息文件，**不建 `consumed/`**。

默认会话 `session.db` 使用上述无前缀路径。显式 `--session` 且数据库文件名不是 `session.db` 时，锁、inbox 与 spool 按**完整文件名**安全化后加前缀，例如 `a.db.runner.lock`、`a.db.clear.lock`、`a.db.init.lock`、`a.db.inbox/`、`a.db.spool/`。安全化保留 `[A-Za-z0-9._-]`，其余字符替换为 `_`，超长名追加短 hash；因此同目录的 `a.db` 与 `a.sqlite` 不会共享锁、inbox 或 spool。`outputs/`、`summaries/` 与归档目录同样以 `artifact_name` 在会话根下再嵌一层唯一化（阶段 6；见「断电恢复与保障分级」）。

### 启动流程（`run`）

```rust
fn run(prompt) {
    let session = resolve_session(cwd);
    create_dir_all(session.root);
    cleanup_inbox_tmp(session, 1h);

    let own = Record::user(prompt);           // 含全局唯一 id

    // 发布与水位在同一个 clear.lock SH 临界区，且不覆盖 notify 命令执行。
    let wm = {
        let _clear = clear_lock_shared(session);
        if is_archived(session) { return 1; }
        let wm = watermark(session);          // 同一快照的 (generation, last_seq)
        publish_with_generation(session, Pending, &own, wm.generation);
        wm
    };

    // 先发布，再抢锁；释放 clear.lock SH 后才抢 runner.lock，避免与 --clear 死锁。
    match try_exclusive(runner_lock(session)) {
        Some(_guard) => run_queued(session, own, PendingPolicy::Abandon),
        None => follow(session, own, wm.last_seq, wm.generation),
    }
}
```

先发布再抢锁，这样无论自己成为 runner，还是已有 runner，消息都不会丢。归档预检与发布处于同一个 `clear.lock` 共享临界区，`--clear` 的排他锁无法在两者之间插入。

`--approve` 不发布 user、不跟随；抢锁失败仍按阶段 2 语义报错退出 1。

### inbox 发布

注入方只做以下步骤，无需锁：

1. 把**完整 `Record` JSON** 写入 `inbox/tmp/<唯一名>`（不是仅正文；载荷必须是 `user` 或 `system(source=notify)`）。
2. 对临时文件 `sync_all`。
3. 原子 `rename` 到 `inbox/pending/<record.id()>.json` 或 `inbox/init/<record.id()>.json`。
4. `fsync` 目标目录。

`rename` 在同一文件系统内原子，`pending/` 中不会出现半条消息；多个客户端可同时发布。`record.id()` 必须匹配 `[A-Za-z0-9_-]{1,128}`，否则拒绝发布且不拼接路径。扫描时还要求文件名等于 id、载荷类型合法、文件不超过 1 MiB；不满足者尽力移入 `rejected/` 并跳过。`count_inbox` 只读统计 `*.json` 数量，供 `--status` 使用。

消息文件示例（实际载荷就是 `Record`）：

```json
{"type":"user","id":"01J...","ts":...,"client_id":"cli_x","turn_id":"turn_x","content":"你好"}
```

`--notify` 的 system 消息走同一 inbox：普通调用写 `pending/`，init 钩子内（环境变量 `XIDONG_INBOX_PHASE=init`）写 `init/`。

`--notify` **只发布**：执行命令前短暂持 `clear.lock` 共享锁做归档预检并捕获 generation，命令执行期间不持锁（避免内嵌 `xidong --clear` 死锁）；发布时再由 `publish_with_generation` 持共享锁校验 generation。预检或发布遇 BUSY/IO 时降级写 spool，命令退出码保持不变；真归档（generation 变化且不是 0→1 建库）等终态拒绝写入并返回 1。它不抢 `runner.lock`、不跟随、不跑 `init`/`llm` 钩子、不执行 agent loop；退出码来自命令本身。

### 主循环安全点

安全点定义：

- 当前 AI 发起的全部 `tool_call` 都已有对应 `tool_result`；
- 或当前没有未完成工具调用；
- 下一轮模型调用之前。

`run_queued` 的启动消费顺序：

1. 普通 run / 含新 `User` 的接管先作废旧待批（`PendingPolicy::Abandon`），再补崩溃孤儿 `tool_result`；
2. 补投 spool：只消费 spool 自身（不合并、不提前消费 inbox），按记录 id 顺序幂等追加并删除，失败留待下轮（阶段 5）；
3. 历史中没有 `system(source=init)` 簿记时执行 `init` 钩子，随后消费 `inbox/init/` 并写簿记；`init` 失败仍消费 `init/`、落 own 并按配置错误退出（不消费其他 pending）；
4. 消费文件名（ULID）小于 `own.id()` 的 `inbox/pending/` 注入；
5. 消费 own；
6. 解析 `llm` 钩子后进入主循环。

主循环（示意；实际实现见 `src/core.rs/src/agent.rs` 的 `run_loop`）：

```rust
fn run_loop(session, runner_guard) {
    loop {
        // 安全点：上一轮工具批次已完整落盘，合并消费 init/ 与 pending/（按 ULID 升序）
        let saw_user = consume_inbox(session); // append_idempotent + 删除文件
        if saw_user {
            step = 0;                          // 新 user 批次重置步数预算
        }
        if step >= max_steps {
            break;                             // 退出码 5
        }

        let history = load(session);
        let ctx = render_context(history);     // 压缩只发生在这里
        let resp = model.chat(ctx);

        // `ai` 与其全部 `tool_call` 以单批原子追加（见 plan.md D12）
        append_history(session, ai_step_records(resp));

        if resp.has_tool_calls() {
            for call in resp.tool_calls {
                let result = shell_execute(call);
                append_history(session, tool_result(result));
            }
            // 工具批次结束，下一轮开头才是下一个安全点
        } else {
            // 无 `tool_call` 的 `ai` 记录即为回合结束，不引入独立 `turn_end` 类型。
            // 但最终模型调用期间可能到达 notify：inbox 非空则继续下一轮消费，
            // 不重置 step，仍由 max_steps 兜底。
        }
    }
}
```

效果：

- runner 执行工具时，第二个 `xidong` 把消息发布到 `inbox/pending/`。
- runner 不在工具中间插入消息。
- 当前工具批次全部 `tool_result` 写完后，下一轮循环开头消费 inbox。
- 新 user 消息被追加到 `session.db` 的 `tool_result` 之后，下一次模型调用自然看到它。
- `max_steps` 按**模型调用次数**计；消费到含 `User` 的批次时重置，纯 notify 不重置。

### inbox 消费

```rust
fn consume_inbox(session) -> bool {
    let mut messages = scan(session, Init);
    messages.extend(scan(session, Pending));
    messages.sort_by_key(|m| m.id()); // ULID 时间近似有序

    let saw_user = messages.iter().any(|m| is_user(m));
    let records: Vec<Record> = messages.iter().map(record).collect();
    append_idempotent(session, &records);          // 整批幂等追加
    for message in messages {
        remove_file(message.path);                 // 消费后删除，不建 consumed/
    }
    saw_user
}
```

顺序与崩溃语义：

1. 用 `append_idempotent` 把消息追加到 `session.db`（WAL + `synchronous=FULL`，提交即持久）；
2. 删除对应 inbox 文件。

若在 1 和 2 之间断电，重启后消息文件会重现并被再次扫描，但 `append_idempotent` 按记录 `id` 去重，不会重复插入。若发布方在 `rename` 前崩溃，只会留下 `tmp/` 残留（下次尽力清理），不会出现半条已发布消息。

### 跟随与接管

已有 runner 时，第二个 `xidong` 进入跟随模式，只读、不写历史、不跑钩子、不执行工具：

- 启动水位是发布 own 时同一 `clear.lock` 临界区内捕获的 `(baseline_gen, start_seq)`，因此不回放历史；
- 用 `read_after_with_generation` 增量读取，退避从 20 ms 起翻倍至 250 ms 封顶，有记录时重置为 20 ms，并加 ±20% 抖动；
- 每批记录交给 `DisplayProjector` 增量投影：它跨批维护 `call_id → command` 命令表与末尾待定 `ai` 正文，行为等价于全量投影；启动时用 `start_seq` 之前的历史回填命令表但不产出旧条目，保证之后到达的 `approval_resolved` 仍能显示命令；
- 每次读取都校验返回的 `generation`：真归档（变化且不是 `0→1` 建库）提示「会话已归档」并以 0 退出；`0→1` 视为首次建库，重置 `next_seq = 0` 后继续；
- 非阻塞尝试获取 `runner.lock`；抢到后**复检 generation**，再补读旧 runner 释放前的最后一批，消除「读水位后、抢锁前被 clear」的竞态；
- 若 own 已落库、inbox 为空、无未决审批且历史空闲 → 释放锁退出 0；最终回复由 CLI 按 own id 定位其后的第一条轮末 `ai` 打印；
- 若 own 已落库、存在未决审批且 inbox 不含新 `User` → 用 `PendingPolicy::Keep`，提示运行 `xidong --approve` 并留在跟随，**不替用户拒绝**；
- 其余情形调用 `run_takeover` 接管。接管**只尝试一次**：有新的 `User` 时用 `PendingPolicy::Abandon` 作废旧待批，否则用 `Keep`；接管失败即退出并保留 inbox 消息，不丢消息。

接管复用 `run_queued` 的消费顺序。own 可能已被旧 runner 消费，按 id 幂等去重，不会重复插入。

最终回复归属：每个客户端按自己的 user record id 定位「其后第一条轮末 `ai`」，多客户端合并成一轮时各自打印同一条答案。若该答案为空，follower 与 leader 一样发出空回复提示，不静默退出。

### 退出与竞态

runner 退出条件（排空式，一次启动处理到满足为止）：

- 主历史处于空闲（空、末条为轮末 `ai`、或仅含 init 簿记/空 `system`）；
- `inbox/pending/` 与 `inbox/init/` 均无可消费消息；
- 没有未决 `approval_pending`；
- 持有 `runner.lock` 时做出该判断，释放锁后不再写历史。

存在未决 `approval_pending` 时 runner 以退出码 6 退出（保留阶段 2 语义）；follower 不自动接管。末条是 `user` 或 `tool_result` 表示轮次未完成，接管者继续跑而不是退出。

若释放锁后、退出前有新注入：

- 注入方发布消息后尝试 `runner.lock`；
- 若成功，它成为新 runner，处理自己的消息；
- 若失败，它进入跟随，旧 runner 释放锁后它尝试接管。

只要默认注入客户端都会「跟随并尝试接管」，消息最终会被处理。这是排空式单进程模型，不是常驻 daemon。

### `--clear` 与 `--status`

- `--clear` 先取 `runner.lock` 排他锁，再取 `clear.lock` 排他锁；持锁后检查 `inbox/pending/`、`inbox/init/` 与 `spool/`，只要还有消息就拒绝归档并返回 1，文件原样保留。数据库文件不存在但 inbox 或 spool 非空时同样拒绝，避免静默丢消息。检查期间 notify 发布持 `clear.lock` 共享锁，因此不会有新消息插入。
- `--status` 用只读的 `count_inbox`/`count_spool` 显示 pending/init 与 spool 计数（不解析、不 reject、不创建目录）；数据库不存在但 inbox 或 spool 非空时仍显示计数。
- `inbox/tmp/` 与 `spool/tmp/` 中超 1 小时的临时文件由下次 `run`/`--notify` 尽力清理。

### 断电恢复与保障分级（阶段 6）

> 状态：阶段 6 已实现。裁决理由见 `plan.md` 的 D86–D90。

#### 保障分级

承诺按「进程崩溃级」与「fsync 约束下的断电级」两档给出，并显式列出不承诺的窗口：

| 档位 | 覆盖 | 依据 |
| ---- | ---- | ---- |
| 进程崩溃级（`kill -9`） | 全覆盖：已原子发布的记录与文件在后续成功 `run` 后仍在 events；同一 `call_id` 至多一条成功结果；重放幂等；收敛态无孤儿 | SQLite `BEGIN IMMEDIATE` 事务、inbox/spool 的 `rename` 原子替换、`append_idempotent` 按记录 `id` 去重 |
| fsync 约束下的断电级 | 已 `fsync` 的批次与文件在断电后仍在 | WAL + `synchronous=FULL`、发布对临时文件与目标目录 `fsync` |
| 不承诺 | 未 `fsync` 窗口（提交前、`rename` 前、写 spool 前）内的记录与发布会丢失；真机器断电的验证需 dm-log-writes | 升级项，见 `plan.md` 的「升级判据」 |

「不承诺未落盘发布」的含义：发布者在写 `tmp/` 或 spool 之前被 `SIGKILL`，或进程在 `fsync` 窗口内断电，该条通知会丢失，但不会留下半条或脏状态（见「崩溃语义与恢复流程」）。

#### 不变量检查器与崩溃矩阵

`xidong::core::test_support` 提供两个纯查询入口，供 e2e/harness 与单测共用：

- `check_consistency(&Session)`：任意时刻应成立的结构不变量——无重复记录 `id`、`seq` 严格递增、同一 `call_id` 至多一条成功（`code == 0`）`tool_result`、`approval_resolved` 必有同 `call_id` 的 `approval_pending`、`tool_call` 的 `call_id` 唯一。
- `check_converged(&Session)`：在一致性之上追加收敛态要求——无孤儿 `tool_call`、无未决 `approval_pending`、`inbox/{pending,init}` 与 `spool` 无待消费 `.json`、`runner.lock` 可被独占获取（获取后立即释放）。

两者复用 `src/core.rs/src/agent.rs` 的 `orphan_tool_calls`/`unresolved_pending` 谓词，避免配对语义漂移。`./tests/crash.rs` 以手工构造崩溃后磁盘状态（B 状态注入）为主、随机 `kill -9` 烟雾（A）为辅，用真实二进制重试；每个场景结束时调用检查器——未收敛态用 `check_consistency`，成功重试后用 `check_converged`。

注入点 → 测试名映射（与 `crash.rs` 文件头一致）：

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

14 格均有归属：#2/#3、#7 复用既有 e2e 并在尾部追加收敛检查；#5 复用既有 e2e 的 tmp 清理断言；#14 为不承诺的文档格。INV7（发布者 kill 不影响已发布消息）另由 `publisher_killed_after_publish_is_consumed_exactly_once` 覆盖。

#### 水位、artifact 与 env

- **展示水位改为 `seq`**：`History` 平行保存记录与其 `seq`，`last_seq()` 取末条 `seq`，`emit_new` 用 `partition_point` 定位第一条 `seq >` 水位的记录。归档清空后主库 `seq` 从 1 重新起算（`generation` 递增），因此水位比较基于 `seq` 值而非记录下标，旧水位超前于当前最大 `seq` 时从 0 起按全量新历史展示。
- **artifact 路径唯一化**：非默认会话（数据库文件名不是 `session.db`）把安全化后的完整文件名作为 `artifact_name`，`outputs/`、`summaries/` 与归档目录 `history/<sid>/` 都在其下再嵌一层，使同一目录下的多个显式会话互不混放；默认会话 `artifact_name` 为空串，路径与旧行为完全一致。
- **env 入口绝对化**：`XIDONG_SESSION` 在输出前绝对化；`--to` 与继承的 `XIDONG_NOTIFY`/`XIDONG_PARENT` 若是相对路径，在解析通知目标时按当前 cwd 绝对化，不再随后续子进程的 cwd 漂移。

#### 崩溃语义与恢复流程

SQLite 事务消除了「撕裂尾行」问题：一批记录要么全部提交、要么全部不可见，断电只可能丢掉尚未提交的批次。缺失的配对仍由渲染器在内存中合成中断 `tool` 消息兜底（见 plan.md 更正 11）；持久化补写由 `xidong::core` 的恢复逻辑完成（store 保持哑，不猜测业务语义）。

文件 inbox 协议已在阶段 4 实现：发布先写 `tmp/`、`fsync` 后原子 `rename`，已发布消息不会因断电变成半条；消费是「`append_idempotent` 后删除文件」，若在两者之间断电，消息文件会重现并由 id 去重。普通 `run` 先发布 own 再抢锁，因此断电时已发布但未消费的消息仍在 inbox，下次启动继续处理。

阶段 5 的 spool 复用同一原子发布原语，因此断电语义相同：发布失败的通知要么完整落在 spool、要么完全不出现；补投是「`append_idempotent` 后删除」，在两者之间断电只会让文件重现并被 id 去重。spool 只兜「发布失败」，发布者进程在写 spool 之前被 SIGKILL 的丢失不在保障范围内（见 `plan.md` 的「已知限制」）。

断电后所有进程死亡，内核自动释放 `flock`。`runner.lock` 文件残留无害。新 `xidong` 启动后：

1. 尝试获取 `runner.lock`，成功则成为 runner；随后按阶段 4 的顺序执行崩溃恢复、spool 补投、`init` 与 inbox 消费（先只补投 spool 自身，再 `init/`，再 own 之前的 `pending/`，再 own）。
2. 在 `init` 钩子与写入 `user` 之前执行恢复：
   - 为每个无对应 `tool_result` 的 `tool_call` 补一条中断结果（`stderr="runner interrupted"`，`code=-1`），幂等；
   - 普通 run 先把未决 `approval_pending` 补成 `approval_resolved: deny` 与失败结果；`--approve` 只补孤儿 `tool_call`，把最旧 pending 留给本次放行。
3. 进入主循环。

补写仍走同一 `append` 批量事务；重复运行因记录 `id` 与配对关系天然幂等。

补中断结果示例：

```json
{"type":"tool_result","id":"01J...","call_id":"call_xxx","stdout":"","stderr":"runner interrupted","code":-1,"timed_out":false,"truncated":false,"ts":...}
```

### 不变量

- `session.db` 是唯一历史事实源，只由 runner 写。
- 历史消息不可变，上下文只是渲染视图；压缩只发生在渲染阶段。
- `inbox/` 与 `spool/` 多写无锁，每消息一文件，`rename` 原子发布；载荷是完整 `Record`。
- 消息 `id` 全局唯一；消费用 `append_idempotent` 按 id 去重，消费后删除文件。
- `runner.lock` 只用于 runner 选举，不用于数据保护；任何时刻同一会话最多一个 runner。
- 发布先于抢锁；发布与归档预检在同一 `clear.lock` 共享临界区。
- 注入不打断工具批次，只在安全点插入。
- 展示水位是 `(generation, seq)` 且只前进；generation 真变化即停止跟随（`0→1` 首次建库除外）。
- `--clear` 不归档仍有待处理 inbox 或非空 spool 的会话；锁、inbox 与 spool 路径按完整数据库文件名唯一化，`outputs/`、`summaries/` 与归档目录同法按 `artifact_name` 唯一化。
- 环境链由 `xidong_env` 单一构造：`XIDONG_SESSION` 总是自己，`XIDONG_NOTIFY` 只继承指向别处的值，`XIDONG_PARENT` 只保留中间层或孵化者，`XIDONG_NOTIFY_CALL_ID` 仅 shell 工具注入。
- 显式 `--to` 只读校验、绝不创建；普通 `--notify` 才警告并创建。
- 通知归因 `origin` 总写、`call_id` 合法才写；两者缺省不影响旧记录解析，也不改变渲染分组。
- `is_injectable_system` 是 display/render/inbox 的唯一 `system` 白名单谓词。
- 断电重启后，消息不丢、不重复、不破坏工具调用配对。
