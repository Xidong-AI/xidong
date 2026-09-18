//! 配置加载与默认值。
//!
//! `config.toml` 缺失时返回默认配置并由调用方告警；文件存在时按「缺失键保留默认值」
//! 合并。类型定义与 `docs/plan.md` 的配置章节一致。

use std::path::Path;

use serde::Deserialize;

use crate::paths;

/// 默认系统提示词钩子：调用 `yaa --get-prompt system` 拼入内置模板。
pub const DEFAULT_SYSTEM_PROMPT: &str = r#"echo "工作目录：$(pwd)" && yaa --get-prompt system"#;
/// 默认用户提示词钩子。
pub const DEFAULT_USER_PROMPT: &str = "cat $GLB_CFG/AGENTS.md";
/// 默认模型配置钩子。
pub const DEFAULT_LLM: &str = "cat $YAA_CONFIG_HOME/llm.toml";

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_STREAM_CHARS: usize = 32768;
const DEFAULT_HEAD_CHARS: usize = 8192;
const DEFAULT_TAIL_CHARS: usize = 2048;
const DEFAULT_MAX_STEPS: u32 = 30;

/// 顶层配置。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    pub hooks: Hooks,
    pub shell: ShellConfig,
    pub agent: AgentConfig,
}

/// 钩子命令。
#[derive(Debug, Clone, PartialEq)]
pub struct Hooks {
    pub system_prompt: String,
    pub user_prompt: String,
    pub llm: String,
}

/// Shell 工具限额，可直接转成 `yaa_shell::ShellLimits`。
#[derive(Debug, Clone, PartialEq)]
pub struct ShellConfig {
    pub timeout_secs: u64,
    pub max_stream_chars: usize,
    pub head_chars: usize,
    pub tail_chars: usize,
}

/// Agent loop 限额。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentConfig {
    pub max_steps: u32,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            user_prompt: DEFAULT_USER_PROMPT.to_string(),
            llm: DEFAULT_LLM.to_string(),
        }
    }
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            max_stream_chars: DEFAULT_MAX_STREAM_CHARS,
            head_chars: DEFAULT_HEAD_CHARS,
            tail_chars: DEFAULT_TAIL_CHARS,
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
        }
    }
}

impl ShellConfig {
    /// 转换为 `yaa_shell` 的限额结构。
    pub fn to_limits(&self) -> yaa_shell::ShellLimits {
        yaa_shell::ShellLimits {
            timeout_secs: self.timeout_secs,
            max_stream_chars: self.max_stream_chars,
            head_chars: self.head_chars,
            tail_chars: self.tail_chars,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    hooks: Option<RawHooks>,
    shell: Option<RawShell>,
    agent: Option<RawAgent>,
}

#[derive(Debug, Deserialize)]
struct RawHooks {
    system_prompt: Option<String>,
    user_prompt: Option<String>,
    llm: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawShell {
    timeout_secs: Option<u64>,
    max_stream_chars: Option<usize>,
    head_chars: Option<usize>,
    tail_chars: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct RawAgent {
    max_steps: Option<u32>,
}

impl Config {
    /// 从路径加载配置。
    ///
    /// 文件不存在时返回默认配置（调用方负责告警）；其他 IO 或 TOML 语法错误向上传播。
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Config::default());
            }
            Err(err) => {
                return Err(
                    anyhow::Error::new(err).context(format!("读取配置失败：{}", path.display()))
                );
            }
        };
        let raw: RawConfig = toml::from_str(&text)
            .map_err(|err| anyhow::anyhow!("解析配置失败 {}：{err}", path.display()))?;
        Ok(merge(raw))
    }

    /// 用 `$YAA_CONFIG_HOME`/`XDG` 规则定位并加载配置。
    pub fn load_from_env(
        env: &impl Fn(&str) -> Option<String>,
        home: &Path,
    ) -> anyhow::Result<Config> {
        Config::load(&paths::config_file(env, home))
    }
}

/// 把可能缺键的原始配置合并到默认值上。
fn merge(raw: RawConfig) -> Config {
    let mut config = Config::default();
    if let Some(hooks) = raw.hooks {
        if let Some(value) = hooks.system_prompt {
            config.hooks.system_prompt = value;
        }
        if let Some(value) = hooks.user_prompt {
            config.hooks.user_prompt = value;
        }
        if let Some(value) = hooks.llm {
            config.hooks.llm = value;
        }
    }
    if let Some(shell) = raw.shell {
        if let Some(value) = shell.timeout_secs {
            config.shell.timeout_secs = value;
        }
        if let Some(value) = shell.max_stream_chars {
            config.shell.max_stream_chars = value;
        }
        if let Some(value) = shell.head_chars {
            config.shell.head_chars = value;
        }
        if let Some(value) = shell.tail_chars {
            config.shell.tail_chars = value;
        }
    }
    if let Some(agent) = raw.agent
        && let Some(value) = agent.max_steps
    {
        config.agent.max_steps = value;
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_config(text: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(text.as_bytes()).unwrap();
        file
    }

    #[test]
    fn full_toml_is_parsed() {
        let file = write_config(
            r#"
[hooks]
system_prompt = "echo system"
user_prompt = "echo user"
llm = "cat llm.toml"

[shell]
timeout_secs = 5
max_stream_chars = 100
head_chars = 40
tail_chars = 10

[agent]
max_steps = 7
"#,
        );
        let config = Config::load(file.path()).unwrap();
        assert_eq!(config.hooks.system_prompt, "echo system");
        assert_eq!(config.hooks.user_prompt, "echo user");
        assert_eq!(config.hooks.llm, "cat llm.toml");
        assert_eq!(
            config.shell,
            ShellConfig {
                timeout_secs: 5,
                max_stream_chars: 100,
                head_chars: 40,
                tail_chars: 10,
            }
        );
        assert_eq!(config.agent.max_steps, 7);
    }

    #[test]
    fn partial_toml_keeps_defaults() {
        let file = write_config(
            r#"
[shell]
timeout_secs = 9
"#,
        );
        let config = Config::load(file.path()).unwrap();
        assert_eq!(config.shell.timeout_secs, 9);
        assert_eq!(config.shell.head_chars, DEFAULT_HEAD_CHARS);
        assert_eq!(config.hooks.system_prompt, DEFAULT_SYSTEM_PROMPT);
        assert_eq!(config.agent.max_steps, DEFAULT_MAX_STEPS);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&dir.path().join("absent.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn invalid_toml_is_an_error() {
        let file = write_config("this is not = = toml");
        assert!(Config::load(file.path()).is_err());
    }

    #[test]
    fn shell_limits_mapping() {
        let config = ShellConfig::default();
        let limits = config.to_limits();
        assert_eq!(limits.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(limits.max_stream_chars, DEFAULT_MAX_STREAM_CHARS);
        assert_eq!(limits.head_chars, DEFAULT_HEAD_CHARS);
        assert_eq!(limits.tail_chars, DEFAULT_TAIL_CHARS);
    }
}
