<h1 align="center">
  <a href="https://github.com/Byte-tea/yaa/">
    <img src="assets/yaa.svg" width="150" height="150" alt="banner" /><br>
  </a>
</h1>

<p align="center"><a href="README.md">中文</a> | English</p>

## yaa

`yaa` means Yet Another Agent. A minimal command-line agent: **its only tool is the shell**, history is appended to JSONL, and the context is just a rendered view of that history.

## Highlights

- **The shell is the only tool**: no built-in file, search, or web tools. Write whatever commands you need; external commands (such as your own `webfetch`) are callable as long as they are on `PATH`.
- **Minimal hooks**: only an `init` hook for empty-session initialization and an `llm` hook for the model provider configuration; initialization injections enter the context through `yaa --notify`, and can be wired to `cat`, `pass`, `op`, and friends.
- **Auditable history**: each session is an append-only JSONL file; at render time records are mapped to model-interface messages.
- **Per-directory sessions**: the session id is a hash of the canonical working directory, so running `yaa` again in the same directory resumes the session.
- **Non-interactive**: one `yaa "..."` runs multiple steps internally until the model stops calling tools.

## Install

```bash
cargo build --release
# binary: target/release/yaa
```

Or install it onto `PATH`:

```bash
cargo install --path crates/yaa-cli
```

## Configuration

The only prerequisite is the model provider configuration; everything else has defaults.

```bash
mkdir -p ~/.config/yaa
cp configs/llm.example.toml ~/.config/yaa/llm.toml     # fill in api_key
cp configs/config.example.toml ~/.config/yaa/config.toml
```

- Config root `$YAA_CONFIG_HOME`, default `~/.config/yaa`, holds `config.toml`, `llm.toml`, and `AGENTS.md`.
- Data root `$YAA_DATA_HOME`, default `~/.local/share/yaa`, holds sessions, archives, and tool output files.

`config.toml` contains hook commands and limits; see `configs/config.example.toml` for every field.

## Usage

```bash
yaa 你好                          # a single run, multiple steps internally
yaa read the file here            # multiple positional args join with spaces
yaa                               # read the prompt from stdin
echo "summarize this repo" | yaa
yaa --clear                       # archive the current directory's session
yaa --status                      # print a session summary
yaa --get-prompt system           # print the built-in system prompt template
yaa --notify="comment" -- cmd      # run cmd and write its stdout as a notification
cmd | yaa --notify="comment"       # read stdin and write it as a notification
yaa --notify-timeout 30 --notify="comment" -- cmd  # override the notification command timeout (seconds)
yaa --cwd /path/to/project 你好    # set the working directory
yaa --session /path/to/default.jsonl 你好
yaa --config /path/to/config.toml 你好
yaa --verbose                     # also show reasoning and command output
yaa --quiet                       # print only the final reply
```

By default the process log goes to stderr: initialization injections, external notifications, and tool commands are printed as `# comment`/`# description` and `% command` blocks separated by `---`, together with the assistant text of non-final steps; the final reply goes to stdout only, so `yaa 你好 > reply.txt` stays clean.
`--verbose` adds the model's reasoning and tool command output; the `llm` hook is never persisted and never displayed. `--quiet` suppresses the whole process log.

Exit codes: `0` success, `1` generic error, `2` usage error, `3` config or hook error, `4` model error, `5` reached `max_steps`.

## How it works

1. **Initialization injection**: on an empty session the `init` hook runs once; the `yaa --notify` calls inside it write `system(source=notify)` events (system prompt, user-wide prompt, current environment).
2. The **`llm` hook** runs once per invocation; its stdout is parsed into the model provider configuration. It is never persisted and never enters the context.
3. **Layered rendering**: injections before the first `user` merge into one `system` message as a stable prefix; later out-of-band notifications merge into the adjacent `user`, keeping the prefix untouched.
4. **Main loop**: re-read history → render to model messages → call the model → run the shell tool → record the result, until the model stops calling tools or `max_steps` is reached.
5. **Truncation**: when one stream of tool output exceeds the limit its middle is cut; the full output is written to `sessions/<sid>/outputs/` and the path is recorded. A `--notify` command reuses the same truncation and dump path, while the stdin form truncates at the limit. Limit precedence: `--notify-timeout` → `YAA_NOTIFY_*` env vars → `[shell]` → defaults.
6. **Command visibility**: a notification's replayable command enters the model context with an `[执行命令]` prefix, telling the model which command produced it.

## Layout

| crate       | Responsibility                                                  |
| ----------- | --------------------------------------------------------------- |
| `yaa-cli`   | `yaa` binary: argument parsing, entry point, stdout/stderr policy |
| `yaa-core`  | Config and paths, tool definition, model calls, rendering and display projection, agent loop |
| `yaa-store` | Session JSONL read/write, offset reads, archiving                |
| `yaa-exec`  | Shell execution mechanism: process group, timeout, streaming truncation, full-output files |
| `yaa-msg`   | Record types and serialization                                   |

Background: [docs/design.md](docs/design.md); implementation plan: [docs/plan.md](docs/plan.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT License](LICENSE)

### Third-party dependencies

| Name         | License                                                         |
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

Test-only: `axum` ([MIT](https://github.com/tokio-rs/axum)), `tempfile` ([MIT OR Apache-2.0](https://github.com/Stebalien/tempfile)).
