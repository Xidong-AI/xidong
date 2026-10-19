# 贡献指北

## 项目版本管理

我们使用[语义化版本](https://semver.org/lang/zh-CN/)进行版本管理。
任何对代码库的更改都不应使用现有版本发布。

## 提交信息格式

提交信息应遵循 [PJ568 提交说明规范](https://github.com/PJ-568/git-commit-regulation)。
仓库自带校验钩子，首次克隆后启用：

```bash
git config core.hooksPath .githooks
```

`commit-msg` 钩子校验提交信息语法与排版（需要 `node`；已安装 `autocorrect` 时一并检查中英符号空格），`pre-commit` 钩子对暂存文件运行 `autocorrect --lint`。

## 项目结构

```plaintext
crates/
  yaa-cli/    二进制 yaa：参数解析、入口、展示渲染、stdout/stderr 策略
  yaa-core/   配置与路径、工具定义、模型调用、渲染与展示投影、agent loop
  yaa-store/  会话 SQLite 事件库读写、归档、文件锁、文件 inbox/spool（含 `file_queue` 原子发布原语）
  yaa-exec/   Shell 执行机制：进程组、超时、流式截断、完整输出落盘
  yaa-msg/    记录类型与序列化
```

设计背景见 [docs/design.md](docs/design.md)，实施计划见 [docs/plan.md](docs/plan.md)。

## 开发环境

1. 安装 cargo 与 Rust。
2. 配置 cargo 镜像源（可选）。
3. 编辑器：安装 rust-analyzer 扩展。

## 代码格式

- Rust 使用四个空格缩进，提交前运行 `cargo fmt`。
- Markdown、TOML 等文本文件使用 `autocorrect --fix <路径>` 规范中英符号间空格。

## 检查与测试

每次原子性修改后，以下命令都应通过：

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
autocorrect --lint crates docs README.md README.en.md CONTRIBUTING.md
```

仓库的提交钩子会拒绝包含警告或排版问题的代码。

端到端测试位于 `crates/yaa-cli/tests/e2e.rs`，它启动一个本地假 OpenAI 兼容服务并用真实二进制跑通「初始化注入 → 工具调用 → 最终回复」，不访问网络。

## 提交更改

1. Fork 此仓库，在您的仓库新建一个基于此仓库默认分支（branch）的分支。
2. 在该新建分支对项目进行更改，确保上述检查全部通过后推送您的仓库。
3. 向我们的仓库提交拉取请求。请确保您遵循了上述说明。
4. 我们将定期审查拉取请求，并告知您我们的问题以及在合并您的拉取请求之前需要进行的任何更改。
5. 我们希望您在 15 日内作出回应，之后如果没有显示活动，您的拉取请求可能会被关闭。

### 注意事项

- 单次 Pull Request 不应提交过多修改，请确保每次提交都针对特定的功能且务必说明本次改动的具体目的，例如：修复某 bug、优化某方法等，方便进行 Code Review；
- 对于 bug 的修复，应该将本次 Pull Request 和相对应 bug 的 issue 关联起来，让别人知道该问题已经被修复；
- 对于较大的新功能，你需要先提交 Issues，例如 "添加 XXX 功能"，确认该功能有被添加的必要后，再开始工作；
- 对于一些主观的样式、交互逻辑调整：如颜色、图标的使用，某些预设配置的增减修改等，一般不予通过。但可以在 Discussions 中进行讨论；
- 其他如简单的代码优化、文档修正等，只要修改合理都会被接受。
