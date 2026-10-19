<h1 align="center">
  <a href="https://github.com/Byte-tea/yaa/">
    <img src="assets/yaa.svg" width="150" height="150" alt="banner" /><br>
  </a>
</h1>

<p align="center">中文 | <a href="README.en.md">English</a></p>

## yaa

`yaa` 的意思是 Yet Another Agent。一个极致轻量的命令行智能体：**唯一的工具是 Shell**，历史以每会话 SQLite 事件库追加保存（只插入、不修改，渲染是它的投影），上下文只是历史记录的一份渲染视图。

## 特点

- **Shell 即唯一工具**：不内置文件、搜索、网页等工具。需要什么就写什么命令，外部命令（如自备的 `webfetch`）只要在 `PATH` 中即可被调用。
- **钩子极简**：只有空会话初始化的 `init` 钩子与取模型供应配置的 `llm` 钩子；初始化注入经 `yaa --notify` 写入上下文，可与 `cat`、`pass`、`op` 自由对接。
- **可审计历史**：每个会话是一个 SQLite 事件库（`sessions/<sid>/session.db`），记录只追加、不修改；渲染时按记录类型映射为模型接口消息。
- **会话按目录隔离**：会话 id 由工作目录规范化后哈希得到，在同一目录再次运行默认续接。
- **非交互**：一次 `yaa "……"` 在内部循环多步，直到模型不再调用工具。

## 安装

```bash
cargo build --release
# 产物：target/release/yaa
```

或安装到 `PATH`：

```bash
cargo install --path crates/yaa-cli
```

## 配置

先决条件是模型供应配置，其余都有默认值。

```bash
mkdir -p ~/.config/yaa
cp configs/llm.example.toml ~/.config/yaa/llm.toml     # 填入 api_key
cp configs/config.example.toml ~/.config/yaa/config.toml
```

- 配置根 `$YAA_CONFIG_HOME`，默认 `~/.config/yaa`，存放 `config.toml`、`llm.toml` 与 `AGENTS.md`。
- 数据根 `$YAA_DATA_HOME`，默认 `~/.local/share/yaa`，存放会话、归档与工具输出。

`config.toml` 里都是钩子命令与限额，字段说明见 `configs/config.example.toml`。

## 使用

```bash
yaa 你好                          # 单次运行，内部可多步
yaa 检查目录 /path                # 多个位置参数以空格连接为提示词
yaa                               # 从 stdin 读提示词
echo "总结这个仓库" | yaa
yaa --clear                       # 归档当前目录的会话
yaa --status                      # 打印会话摘要
yaa --get-prompt system           # 打印内置系统提示词模板
yaa --notify="注释" -- cmd         # 执行 cmd，把 stdout 作为通知写入当前会话
cmd | yaa --notify="注释"          # 从 stdin 读内容并写入通知
yaa --notify-timeout 30 --notify="注释" -- cmd  # 覆盖该通知命令的超时（秒）
yaa --cwd /path/to/project 你好    # 指定工作目录
yaa --session /path/to/session.db 你好
yaa --config /path/to/config.toml 你好
yaa --verbose                     # 追加思考内容与命令输出
yaa --quiet                       # 只打印最终回复
```

默认把过程记录打到 stderr：初始化注入与外部通知、工具命令按 `# 注释`/`# 描述` 与 `% 命令` 成块打印，块间以 `---` 分隔，并打印非末步的模型正文；末步回复只走 stdout，因此 `yaa 你好 > 回复.txt` 仍然只得到最终回复。
`--verbose` 在此基础上追加模型的思考内容（`reasoning`）与工具命令输出；`llm` 钩子的加载过程不落盘、不展示。`--quiet` 则抑制全部过程记录。

退出码：`0` 成功，`1` 通用错误，`2` 用法错误，`3` 配置或钩子错误，`4` 模型错误，`5` 达到 `max_steps`，`130` 被 Ctrl-C 中断。

## 工作方式

1. **初始化注入**：空会话时执行一次 `init` 钩子，钩子内的 `yaa --notify` 写入 `system(source=notify)` 事件（系统提示词、用户总提示词、当前环境）。
2. **`llm` 钩子**每次运行执行一次，stdout 解析为模型供应配置；该内容不落盘、不进入上下文。
3. **渲染分层**：首条 `user` 之前的注入合并为一条 `system` 消息作为稳定前缀；之后的离群通知并入相邻 `user`，不污染前缀。
4. **主循环**：重读历史 → 渲染为模型消息 → 调用模型 → 执行 Shell 工具 → 记录结果，直到模型不再调用工具或达到 `max_steps`。
5. **截断**：工具单流输出超过上限时中间截去，完整输出写入 `sessions/<sid>/outputs/`，记录中给出路径；`--notify` 的命令与 stdin 形态复用同一套截断与落盘。限额解析优先级为 `--notify-timeout` → `YAA_NOTIFY_*` 环境变量 → `[shell]` → 默认。
6. **命令可见**：通知记录的可重放命令会以 `[执行命令]` 前缀进入模型上下文，让模型知道通知由哪条命令产生。
7. **并发与注入**：同一会话同时只有一个 runner；另一个 `yaa "内容"` 会先把消息发布到会话的 inbox，再跟随已有 runner 的输出。`--notify` 也只写入 inbox，不启动模型，由运行中的 yaa 在安全点消费；没有 runner 时留待下次 `yaa` 处理。`--clear` 遇到待处理消息会拒绝归档。

## 目录结构

| crate       | 职责                                                       |
| ----------- | ---------------------------------------------------------- |
| `yaa-cli`   | 二进制 `yaa`：参数解析、入口、stdout/stderr 策略           |
| `yaa-core`  | 配置与路径、工具定义、模型调用、渲染与展示投影、agent loop |
| `yaa-store` | 会话 SQLite 事件库读写、按序号读、归档、文件锁、文件 inbox 发布与扫描 |
| `yaa-exec`  | Shell 执行机制：进程组、超时、流式截断、完整输出落盘       |
| `yaa-msg`   | 记录类型与序列化                                            |

设计背景见 [docs/design.md](docs/design.md)，实施计划见 [docs/plan.md](docs/plan.md)。

## 贡献

详见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

[MIT License](LICENSE)

### 第三方依赖

| 名称         | 协议                                                            |
| ------------ | --------------------------------------------------------------- |
| anyhow       | [MIT OR Apache-2.0](https://github.com/dtolnay/anyhow)           |
| clap         | [MIT OR Apache-2.0](https://github.com/clap-rs/clap)             |
| libc         | [MIT OR Apache-2.0](https://github.com/rust-lang/libc)           |
| reqwest      | [MIT OR Apache-2.0](https://github.com/seanmonstar/reqwest)      |
| rusqlite     | [MIT](https://github.com/rusqlite/rusqlite)                      |
| serde        | [MIT OR Apache-2.0](https://github.com/serde-rs/serde)           |
| serde_json   | [MIT OR Apache-2.0](https://github.com/serde-rs/json)            |
| sha2         | [MIT OR Apache-2.0](https://github.com/RustCrypto/hashes)        |
| thiserror    | [MIT OR Apache-2.0](https://github.com/dtolnay/thiserror)        |
| tokio        | [MIT](https://github.com/tokio-rs/tokio)                         |
| toml         | [MIT OR Apache-2.0](https://github.com/toml-rs/toml)             |
| ulid         | [MIT](https://github.com/dylanhart/ulid-rs)                      |

仅测试使用：`axum`（[MIT](https://github.com/tokio-rs/axum)）、`tempfile`（[MIT OR Apache-2.0](https://github.com/Stebalien/tempfile)）。

`rusqlite` 以 `bundled` 特性静态链接 SQLite；其构建依赖 `libsqlite3-sys`（[MIT](https://github.com/rusqlite/rusqlite)）与公有领域的 SQLite 源码。
