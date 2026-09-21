# YAA 智能体设计

设想一个基于 Rust 的极致轻量的智能体（AI Agent）。
每个模块都分 crate 开发，先用 Cargo workspace 管理，边界稳定后考虑拆分仓库。

> 本文是构想稿。实现现状，以及本文与实现的已知不一致处，以 [`docs/plan.md`](plan.md) 为准。
> 其在“对 design.md 的更正”部分逐条列出。

用 `yaa` 运行的非交互式命令。

## 模块设计

对每个模块做最小化设计。智能体只有 Shell 命令这一个工具调用。
为确保高度可维护性，模块数量和功能数量应做到最少。
系统生命周期钩子：比如上下文和提示词注入，也由系统内部钩子触发 Shell 命令实现；钩子命令与智能体工具调用分开执行、策略不同，但同样以 `origin=hook` 的 `tool_call`/`tool_result` 记录，供审计与缓存判定。

可能先做三到四个 crate 吧：

- `yaa-cli`：参数解析、入口、stdout/stderr 策略。
- `yaa-core`：智能体循环（Agent loop）、提示词组装、模型调用、消息模型、压缩并渲染上下文视图。
- `yaa-store`：会话 JSONL、归档、加载。
- `yaa-shell`：唯一工具，负责执行命令、超时、截断、策略。
- `yaa-msg`：消息类型。

## 历史消息

历史消息管理框架模块以消息队列的形式存储历史消息。

```jsonl
{"type":"user","ts":...,"id":"01J...","content":"你好"}
{"type":"tool_call","origin":"hook","ts":...,"id":"01J...","call_id":"call_xxx","command":"echo \"工作目录：$(pwd)\" && \"$YAA_BIN\" --get-prompt \"system\"","hook":{"name":"system_prompt","fingerprint":"..."},"redacted":false}
{"type":"tool_result","ts":...,"id":"01J...","call_id":"call_xxx","stdout":"...","stderr":"...","code":0,"timed_out":false,"truncated":false}
{"type":"tool_call","origin":"hook","ts":...,"id":"01J...","call_id":"call_xxx","command":"cat $YAA_CONFIG_HOME/AGENTS.md","hook":{"name":"user_prompt","fingerprint":"..."},"redacted":false}
{"type":"tool_result","ts":...,"id":"01J...","call_id":"call_xxx","stdout":"...","stderr":"...","code":0,"timed_out":false,"truncated":false}
{"type":"ai","ts":...,"id":"01J...","model":"...","tool_schema_hash":"...","content":"..."}
{"type":"tool_call","origin":"ai|user","ts":...,"id":"01J...","call_id":"call_xxx","command":"...","description":"……","redacted":false}
{"type":"approval_pending","ts":...,"id":"01J...","call_id":"call_xxx","command":"...","matched_rule":"..."}
{"type":"approval_resolved","ts":...,"id":"01J...","call_id":"call_xxx","decision":"approve|deny"}
{"type":"tool_result","ts":...,"id":"01J...","call_id":"call_xxx","stdout":"...","stderr":"...","code":0,"timed_out":false,"truncated":true,"output_files":{"stdout":"sessions/<sid>/outputs/01J....stdout.out"}}
{"type":"system","source":"notify|error|...","scope":"ai|shell|store|...","ts":...,"id":"01J...","content":"（系统警告、后台任务返回或错误等的消息）"}
```

> `ts` 为 Unix 毫秒；`id` 为 ULID，全记录唯一；`tool_result` 用 `call_id` 与 `tool_call` 配对。
>
> 提示词钩子记录在 `user` 之后写入（用户输入先落盘），渲染时才提升为消息列表最前的 `system`；钩子的 `tool_call` 不写 `description`，该文案由 CLI 展示层现算。

过往历史消息不可变，上下文只是基于历史消息构造的渲染视图。
渲染时语义转换为对应接口格式的消息，所以模型始终看到的是结构和内容合规的上下文。
如开头的提示词注入被格式化为 OpenAI 的 `system` 而非 `assistant.tool_calls` 和 `tool`。

上下文压缩参考 DSH (DeepSeek Harness) 的 `compaction-basic`：先跑无模型的头尾 pruner，仍超阈值再发一次摘要 LLM 调用。
压缩只发生在渲染阶段，原始 JSONL 不变。
压缩保留未完成工具调用。

每一次执行 `yaa "……"` 都默认接着当前目录下已存在的会话历史消息继续，除非 `yaa --clear`。

工具调用输出过长时，中间部分被截去，并产生包含完整输出的外部文件。

目录结构：

```plaintext
$YAA_CONFIG_HOME/config.toml                                      # 配置根
$YAA_DATA_HOME/sessions/<sid>/default.jsonl                       # 主历史，主要由前台 yaa 维护
$YAA_DATA_HOME/sessions/<sid>/outputs/<ulid>.{stdout,stderr}.out  # 截断后的完整输出
$YAA_DATA_HOME/history/<sid>/sessions-<ts>.jsonl                  # 主历史归档后存放的位置
```

> 状态根按 XDG 拆分：配置根 `$YAA_CONFIG_HOME`，数据根 `$YAA_DATA_HOME`（见 plan.md D10）。
>
> 主循环每一步重读会话文件；写入以整批单次 `O_APPEND` + `fsync` 完成，不逐条写。
>
> 当前工作目录规范化后哈希得 `<sid>`。

## 使用示例

`yaa "……"` 一次运行内部可以循环多步，使用效果伪示例：

```zsh
$ yaa 你好吗？
# 注入系统提示词
% echo "工作目录：$(pwd)" && "$YAA_BIN" --get-prompt system

---
# 注入用户提示词
% cat $YAA_CONFIG_HOME/AGENTS.md

---
我很好，谢谢，您还有什么需要帮助的吗？

$ yaa 不用了，只是确认是否能正常回复。
好的，需要帮助时随时叫我。

$ yaa --clear
# 归档历史消息（内部完成，不经过 Shell 钩子）
$YAA_DATA_HOME/history/<sid>/sessions-<ts>.jsonl

$ yaa 不用子智能体，帮我查未来五天的天气并写入到文件 \`/home/pj568/桌面/temp.txt\`。
# 注入系统提示词
% echo "工作目录：$(pwd)" && "$YAA_BIN" --get-prompt system

---
# 注入用户提示词
% cat $YAA_CONFIG_HOME/AGENTS.md

---
当前工作目录已在 `/home/pj568/桌面/`。
请告诉我未来五天天气要查询的城市或具体地点；我拿到地点后会查询并写入 `/home/pj568/桌面/temp.txt`。

可选回复：

1. 根据 IP 获取的档期位置；
2. 你告诉我要查询的具体地点；
3. 其它，你来说。

$ yaa 按网络定位。
# 获取 IP 所在位置
% yaa-webfetch https://ipinfo.io/json

---
# 获取所在位置未来天气预报
% yaa-webfetch https://api.open-meteo.com/v1/forecast?latitude=22.4821&longitude=113.8850&daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max,precipitation_sum,wind_speed_10m_max&timezone=Asia%2FShanghai&forecast_days=6

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

$ yaa 我已查看，删掉它。
# 删除 temp.txt（阶段 2：命中规则后拦截）
% rm /home/pj568/桌面/temp.txt
匹配到 `rm`，是否批准执行 `rm /home/pj568/桌面/temp.txt`？

$ yaa --approve
# 删除 temp.txt
% rm /home/pj568/桌面/temp.txt

---
# 检查状态
% ls -la

---
删除完毕。
```

遇到拦截时退出，`--approve` 允许执行并继续，直接写提示词等情况默认拒绝并提示已被拒绝（审批整体属阶段 2）。

本节是使用效果示意，非逐字规范：具体参数以下文“参数（待定）”为准，实现状态见 `docs/plan.md`。示例里的 `yaa-webfetch` 是外部辅助命令，不属于 `yaa` 自带工具；提示词打印已定为 `yaa --get-prompt` 子命令。

## 参数（待定）

阶段 1 已实现 `--status`/`--session`/`--clear`/`--verbose`/`--quiet`/`--get-prompt`/`--cwd`/`--config`；`--json`/`--no-store`/`--approve` 属阶段 2，`--notify` 属阶段 5。

```plaintext
yaa [PROMPT]...              # 多个位置参数以空格连接；空则从 stdin 读
yaa -- "[PROMPT]"            # 同上
yaa --status                 # 看会话摘要
yaa --session $path_to/session.jsonl
yaa --clear                  # 归档当前目录的会话；若已有 runner 在跑，此时再运行 `yaa "内容"` 会把消息排入 inbox 并跟随输出到循环结束（阶段 4）。
yaa --verbose                # 追加思考内容与命令输出
yaa --quiet                  # 仅打印最终结果输出
yaa --get-prompt system|……   # 仅打印相关提示词
yaa --json                   # 结构化事件
yaa --approve                # 同意一次
yaa --cwd="……"               # 设置会话目录
yaa --no-store
yaa --notify --to $SESSION   # 从 stdin 读，完毕后作为 system 消息原子追加进“通知目标会话”。
yaa --notify -- some-command --args
yaa --config $path_to/config.toml
……
```

## 审批（阶段 2）

`yaa --approve` 同意第一个未批准或拒绝的 `pending` 命令执行并继续智能体循环，一次性调用多个命令都被拦截时需连续执行多次此命令。

用户直接发新提示词时，pending 自动 deny。

## 配置文件

配置文件 `$YAA_CONFIG_HOME/config.toml` 内配置各部分的钩子调用的命令、命令和目录拦截放行规则、压缩触发条件、timeout 和 max-steps 什么的。
钩子调用的命令包括默认的系统命令也可替换，模型供应各项配置也是通过钩子命令获取的。

阶段 1 已实现钩子命令、timeout 与 max-steps；拦截放行规则属阶段 2，压缩触发条件属阶段 3。

### 规则

和 OpenCode 相似，默认 allow，可配置 deny 和 ask 名单通配符什么的。

## 后台命令（阶段 5）

`<任何命令> | yaa --notify &` 执行后不阻塞主循环，待后台任务完毕后能把消息和压缩的执行结果以系统消息发回 inbox。
其中执行结果的压缩复用工具调用的过长截断压缩加外部完整原始输出文件。

后台子智能体也复用同一方法，如：

```shellscript
cd .workspace/temp && yaa --quiet -- "你作为子智能体做某些任务。" | yaa --notify &
# 或可取得更多信息的包装模式
cd .workspace/temp && yaa --notify -- aa --quiet -- "你作为子智能体做某些任务。"
```

### 通知目标

维护两个环境变量和一个规则：

| 变量          | 含义                 | 谁设置                          |
| ------------- | -------------------- | ------------------------------- |
| `YAA_SESSION` | 我自己所在的会话路径 | 当前 yaa 启动时设置（覆盖父值） |
| `YAA_PARENT`  | 父会话路径           | 由父传给子，子不覆盖            |
| `YAA_NOTIFY`  | 默认通知目标         | 启动时默认设为自己              |

解析顺序：`--to <path>` 参数 → `YAA_NOTIFY` → `YAA_SESSION` → 当前 cwd 默认会话。

```zsh
# 子智能体内部后台任务，默认回子自己
some-cmd | yaa --notify &

# 子智能体想显式通知父
some-cmd | yaa --notify --to "$YAA_PARENT" &
```

`yaa --notify` 发现目标不存在时 stderr 警告。

### 主循环召回

`yaa --notify` “召回”时也是拿文件锁（flock 或 `O_APPEND`）写一行，写完释放。

| 状态         | 收到 `system` 消息的时机 |
| ------------ | ------------------------ |
| 主循环运行中 | 主循环的下一个循环开始时 |
| 主循环已退出 | 下次该目录 `yaa` 启动时  |

## 单 runner 与多客户端注入（阶段 4）

### 目标

- 同一会话同一时刻只有一个 runner。
- 已有 runner 运行时，再执行 `yaa "内容"` 不启动第二个 runner。
- 新消息先进入 inbox，由 runner 在安全点插入主历史。
- 插入发生在当前工具调用批次全部结束后，不打断 `tool_call` / `tool_result` 配对。
- 注入客户端默认跟随主历史输出，直到循环结束。
- 断电重启后不丢消息、不重复插入、不破坏历史语义。

### 文件布局

```plaintext
$YAA_DATA_HOME/sessions/<sid>/
  default.jsonl          # 主历史，只由 runner 写
  runner.lock            # runner 长期 flock/fcntl 锁
  outputs/               # 截断工具的完整输出
  inbox/
    tmp/                 # 临时文件，不参与消费
    pending/             # 待消费消息，每个消息一个文件
    consumed/            # 已消费消息
```

`runner.lock` 文件永久保留，不删除，不用“文件是否存在”判断 runner。只使用 `flock` / `fcntl` 排他锁判断。

### 启动流程

```rust
fn start(prompt) {
    let session = resolve_session(cwd);

    let user_msg = build_user_msg(prompt); // 含全局唯一 id
    write_inbox_message(session, user_msg); // tmp -> fsync -> rename 到 pending

    let lock = open_runner_lock(session);

    match lock.try_lock_exclusive() {
        Ok(guard) => {
            recover_default_jsonl(session.default_jsonl);
            run_agent_loop(session, guard);
        }
        Err(WouldBlock) => {
            follow(session.default_jsonl, start_offset, user_msg.id);
        }
    }
}
```

先写 inbox，再尝试获取 runner 锁。这样无论自己成为 runner，还是已有 runner，消息都不会丢。

### inbox 发布

注入方只做三件事：

1. 写临时文件到 `inbox/tmp/`。
2. `fsync` 临时文件。
3. 原子 `rename` 到 `inbox/pending/<ulid>.json`。
4. `fsync` `pending/` 目录。

多个客户端可同时发布，不需要锁。`rename` 是原子的，`pending/` 中不会出现半条消息。

消息文件示例：

```json
{"type":"user","id":"01J...","ts":...,"client_id":"cli_x","turn_id":"turn_x","content":"你好"}
```

`--notify` 的 system 消息也走同一 inbox，只是 `type` 为 `system`。

### 主循环安全点

安全点定义：

- 当前 AI 发起的全部 `tool_call` 都已有对应 `tool_result`；
- 或当前没有未完成工具调用；
- 下一轮模型调用之前。

主循环：

```rust
fn run_agent_loop(session, runner_guard) {
    loop {
        // 安全点：消费 inbox，追加到 default.jsonl
        consume_inbox(session);

        let history = read_jsonl(session.default_jsonl);

        if is_idle(history) && pending_is_empty(session) {
            // 释放 runner.lock 后退出；若之后有注入，跟随者会接管
            drop(runner_guard);
            break;
        }

        let ctx = render_context(history); // 压缩只发生在这里
        let resp = model.chat(ctx);

        // `ai` 与其全部 `tool_call` 以单批原子追加（见 plan.md D12）
        append_history(session, ai_step_records(resp));

        if resp.has_tool_calls() {
            for call in resp.tool_calls {
                let result = shell_execute(call);
                append_history(session, tool_result(result));
            }
            // 工具批次结束，下一轮开头消费 inbox
        } else {
            // 无 `tool_call` 的 `ai` 记录即为回合结束，不引入独立 `turn_end` 类型
        }
    }
}
```

效果：

- runner 执行工具时，第二个 `yaa` 把消息写入 `inbox/pending/`。
- runner 不在工具中间插入消息。
- 当前工具批次全部 `tool_result` 写完后，下一轮循环开头消费 inbox。
- 新 user 消息被追加到 `default.jsonl` 的 `tool_result` 之后。
- 下一次模型调用自然看到它，继续输出。

### inbox 消费

```rust
fn consume_inbox(session) {
    let mut files = list_files(session.inbox.pending);
    files.sort_by_key(|f| f.name); // ULID 时间有序

    for file in files {
        let msg = read_json(file);

        if !history_contains_id(session.default_jsonl, msg.id) {
            append_jsonl(session.default_jsonl, msg.into_history());
            fsync(session.default_jsonl);
        }

        rename(file, session.inbox.consumed.join(file.name));
        fsync(session.inbox.consumed);
    }
}
```

顺序：

1. append 到 `default.jsonl`；
2. `fsync`；
3. rename 到 `consumed/`。

若在 1 和 3 之间断电，重启后会重复消费，但 `id` 去重保证不会重复插入。

### 跟随与接管

已有 runner 时，第二个 `yaa` 进入跟随模式：

- 从启动时记录的 `default.jsonl` 偏移开始 tail；
- 增量读取并渲染新消息；
- 周期性尝试非阻塞获取 `runner.lock`；
- 若获取成功，说明旧 runner 已退出，自己成为新 runner，调用 `run_agent_loop` 接管；
- 接管时会消费 `inbox/pending/`，包括自己刚注入的消息；
- 直到空闲且 pending 为空，再释放锁退出。

这样即使 runner 在“检查 pending 为空”和“释放锁退出”之间发生注入竞态，跟随者也会接管处理，消息不会卡死。

### 退出与竞态

runner 退出条件：

- 主历史处于空闲；
- `inbox/pending/` 为空；
- 持有 `runner.lock` 时做出该判断；
- 释放 `runner.lock` 后退出。

若释放锁后、退出前有新注入：

- 注入方写完 pending 后尝试 `runner.lock`；
- 若成功，它成为新 runner，处理自己的消息；
- 若失败，它进入跟随，旧 runner 释放锁后它会尝试接管。

只要默认注入客户端都会“跟随并尝试接管”，消息最终会被处理。

### 断电恢复（阶段 6）

阶段 1 只做到「容忍撕裂尾行」：`load`/`append` 会把最后一个换行之后的残段解析或截断，缺失的配对由渲染器在内存中合成中断 `tool` 消息以保证请求体合法（见 plan.md 更正 11）。下面的落盘补写属阶段 6。

断电后所有进程死亡，内核自动释放 `flock`。`runner.lock` 文件残留无害。新 `yaa` 启动后：

1. 写入自己的 inbox 消息。
2. 尝试获取 `runner.lock`，成功则成为新 runner。
3. 执行 `recover_default_jsonl`：
   - 截断最后一个不完整行，保留最后一个换行之前的内容；
   - 为缺失 `tool_result` 的 `tool_call` 补一条中断结果；
   - 为未解决的 `approval_pending` 补 `approval_resolved: deny`。
4. 消费 `inbox/pending/`，按 `id` 去重。
5. 进入主循环。

补中断结果示例：

```json
{"type":"tool_result","id":"01J...","call_id":"call_xxx","stdout":"","stderr":"runner interrupted by power loss","code":-1,"timed_out":false,"truncated":false,"ts":...}
```

### 不变量

- `default.jsonl` 是唯一历史事实源，只由 runner 写。
- 历史消息不可变，上下文只是渲染视图。
- 压缩只发生在渲染阶段，原始 JSONL 不变。
- `inbox/` 多写无锁，每消息一文件，`rename` 原子发布。
- 消息 `id` 全局唯一，消费幂等。
- `runner.lock` 只用于 runner 选举，不用于数据保护。
- 任何时刻同一会话最多一个 runner。
- 注入不打断工具批次，只在安全点插入。
- 断电重启后，消息不丢、不重复、不破坏工具调用配对。
