<h1 align="center">
  <a href="https://github.com/Byte-tea/yaa/">
    <img src="assets/yaa.svg" width="150" height="150" alt="banner" /><br>
  </a>
</h1>

<p align="center">中文 | <a href="README.en.md">English</a></p>

## yaa

`yaa` 的意思是 Yet Another Agent。一个极致轻量的命令行智能体：**唯一的工具是 Shell**，历史以 JSONL 追加保存，上下文只是历史记录的一份渲染视图。

## 特点

- **Shell 即唯一工具**：不内置文件、搜索、网页等工具。需要什么就写什么命令，外部命令（如自备的 `webfetch`）只要在 `PATH` 中即可被调用。
- **钩子驱动**：系统提示词、用户提示词、模型供应配置全部由钩子命令产出，可以自由对接 `cat`、`pass`、`op` 等。
- **可审计历史**：每个会话是一个 JSONL 文件，记录只追加、不修改；渲染时按记录类型映射为模型接口消息。
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
yaa                               # 从 stdin 读提示词
echo "总结这个仓库" | yaa
yaa --clear                       # 归档当前目录的会话
yaa --status                      # 打印会话摘要
yaa --get-prompt system           # 打印内置系统提示词模板
yaa --cwd /path/to/project 你好    # 指定工作目录
yaa --session /path/to/default.jsonl 你好
yaa --config /path/to/config.toml 你好
yaa --verbose                     # 钩子与工具事件打到 stderr
yaa --trace                       # 工具命令打到 stderr
yaa --quiet                       # 只打印最终回复
```

退出码：`0` 成功，`1` 通用错误，`2` 用法错误，`3` 配置或钩子错误，`4` 模型错误，`5` 达到 `max_steps`。

## 工作方式

1. **提示词钩子**（`system_prompt`、`user_prompt`）每会话执行一次，输出作为 `system` 消息写入历史。
2. **`llm` 钩子**每次运行执行一次，stdout 解析为模型供应配置；该内容不落盘、不进入上下文。
3. **主循环**：重读历史 → 渲染为模型消息 → 调用模型 → 执行 Shell 工具 → 记录结果，直到模型不再调用工具或达到 `max_steps`。
4. **截断**：工具单流输出超过上限时中间截去，完整输出写入 `sessions/<sid>/outputs/`，记录中给出路径。

## 目录结构

| crate       | 职责                                                       |
| ----------- | ---------------------------------------------------------- |
| `yaa-cli`   | 二进制 `yaa`：参数解析、入口、stdout/stderr 策略           |
| `yaa-core`  | 配置与路径、钩子、模型调用、渲染、agent loop、事件          |
| `yaa-store` | 会话 JSONL 读写、按偏移读、归档                             |
| `yaa-shell` | 唯一工具：执行、超时、截断、完整输出落盘                   |
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
| serde        | [MIT OR Apache-2.0](https://github.com/serde-rs/serde)           |
| serde_json   | [MIT OR Apache-2.0](https://github.com/serde-rs/json)            |
| sha2         | [MIT OR Apache-2.0](https://github.com/RustCrypto/hashes)        |
| thiserror    | [MIT OR Apache-2.0](https://github.com/dtolnay/thiserror)        |
| tokio        | [MIT](https://github.com/tokio-rs/tokio)                         |
| toml         | [MIT OR Apache-2.0](https://github.com/toml-rs/toml)             |
| ulid         | [MIT](https://github.com/dylanhart/ulid-rs)                      |

仅测试使用：`axum`（[MIT](https://github.com/tokio-rs/axum)）、`tempfile`（[MIT OR Apache-2.0](https://github.com/Stebalien/tempfile)）。
