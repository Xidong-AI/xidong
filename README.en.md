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
- **Hook-driven**: the system prompt, the user prompt, and the model provider configuration are all produced by hook commands, so they can be wired to `cat`, `pass`, `op`, and friends.
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
yaa --cwd /path/to/project 你好    # set the working directory
yaa --session /path/to/default.jsonl 你好
yaa --config /path/to/config.toml 你好
yaa --verbose                     # also show reasoning and command output
yaa --quiet                       # print only the final reply
```

By default the process log goes to stderr: hook and tool commands are printed as `# description` / `% command` blocks separated by `---`, together with the assistant text of non-final steps; the final reply goes to stdout only, so `yaa 你好 > reply.txt` stays clean.
`--verbose` adds the model's reasoning and tool command output; hooks only show their description and command, and hook output is never printed. `--quiet` suppresses the whole process log.

Exit codes: `0` success, `1` generic error, `2` usage error, `3` config or hook error, `4` model error, `5` reached `max_steps`.

## How it works

1. **Prompt hooks** (`system_prompt`, `user_prompt`) run once per session; their output is written to history as `system` messages.
2. The **`llm` hook** runs once per invocation; its stdout is parsed into the model provider configuration. It is never persisted and never enters the context.
3. **Main loop**: re-read history → render to model messages → call the model → run the shell tool → record the result, until the model stops calling tools or `max_steps` is reached.
4. **Truncation**: when one stream of tool output exceeds the limit its middle is cut; the full output is written to `sessions/<sid>/outputs/` and the path is recorded.

## Layout

| crate       | Responsibility                                                  |
| ----------- | --------------------------------------------------------------- |
| `yaa-cli`   | `yaa` binary: argument parsing, entry point, stdout/stderr policy |
| `yaa-core`  | Config and paths, hooks, model calls, rendering, agent loop, events |
| `yaa-store` | Session JSONL read/write, offset reads, archiving                |
| `yaa-shell` | The only tool: execution, timeout, truncation, full-output files |
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
