//! 配置加载与默认值。
//!
//! `config.toml` 缺失时返回默认配置并由调用方告警；文件存在时按「缺失键保留默认值」
//! 合并。类型定义与 `docs/plan.md` 的配置章节一致。

use std::path::Path;

use serde::Deserialize;

use crate::approval::{Action, Rule};
use crate::paths;

/// 默认初始化钩子：空会话时执行一次，用三条 `yaa --notify` 拼出「系统提示词 /
/// 用户总提示词 / 当前环境」。stdout 不进入上下文，写入上下文的唯一方式就是
/// 命令内的 `--notify`。
///
/// 用 `$YAA_BIN`（当前可执行文件绝对路径）而非裸 `yaa`，否则 PATH 中没有 yaa 时
/// 钩子会静默失败；`hook_env` 负责注入该变量。`AGENTS.md` 缺失时跳过 notify，
/// 避免注入空信封（D30）。首行 `set -e` 保证任一条 notify 失败即整体失败，不会出现
/// 「前两条失败、末条成功 ⇒ 退出 0 被当成功」的假成功。
pub const DEFAULT_INIT: &str = r#"set -e
"$YAA_BIN" --notify="系统提示词" -- "$YAA_BIN" --get-prompt system
if [ -f "$GLB_CFG/AGENTS.md" ]; then "$YAA_BIN" --notify="用户总提示词" -- cat "$GLB_CFG/AGENTS.md"; fi
"$YAA_BIN" --notify="当前环境" -- echo "工作目录：$(pwd)""#;
/// 默认模型配置钩子。
pub const DEFAULT_LLM: &str = "cat $YAA_CONFIG_HOME/llm.toml";

const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 30;
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_STREAM_CHARS: usize = 32768;
const DEFAULT_HEAD_CHARS: usize = 8192;
const DEFAULT_TAIL_CHARS: usize = 2048;
const DEFAULT_MAX_STEPS: u32 = 30;
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 300;

/// 顶层配置。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    pub hooks: Hooks,
    pub shell: ShellConfig,
    pub agent: AgentConfig,
    pub approval: ApprovalConfig,
}

/// 审批规则配置。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ApprovalConfig {
    pub rules: Vec<Rule>,
}

/// 钩子命令与执行限额。
///
/// 阶段 1 只有两个钩子：`init`（空会话初始化）与 `llm`（模型供应配置，不进入
/// 上下文、不写记录）。
#[derive(Debug, Clone, PartialEq)]
pub struct Hooks {
    pub init: String,
    pub llm: String,
    /// 单次钩子执行的硬超时（秒），覆盖启动与读尽输出的全过程。
    pub timeout_secs: u64,
}

/// Shell 工具限额，可直接转成 `yaa_exec::ShellLimits`。
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
    /// 单次模型请求硬超时（秒），覆盖连接与响应体读取；超时不重试。
    pub request_timeout_secs: u64,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            init: DEFAULT_INIT.to_string(),
            llm: DEFAULT_LLM.to_string(),
            timeout_secs: DEFAULT_HOOK_TIMEOUT_SECS,
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
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
        }
    }
}

impl ShellConfig {
    /// 转换为 `yaa_exec` 的限额结构。
    pub fn to_limits(&self) -> yaa_exec::ShellLimits {
        yaa_exec::ShellLimits {
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
    approval: Option<RawApproval>,
}

#[derive(Debug, Deserialize)]
struct RawHooks {
    init: Option<String>,
    llm: Option<String>,
    timeout_secs: Option<u64>,
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
    request_timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawApproval {
    rules: Option<Vec<RawRule>>,
}

#[derive(Debug, Deserialize)]
struct RawRule {
    pattern: String,
    action: String,
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
        let config = merge(raw)?;
        validate(&config)?;
        Ok(config)
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
///
/// `action` 需映射为枚举，未知取值在此报错（配置错误语义），因此返回 `Result`。
fn merge(raw: RawConfig) -> anyhow::Result<Config> {
    let mut config = Config::default();
    if let Some(hooks) = raw.hooks {
        if let Some(value) = hooks.init {
            config.hooks.init = value;
        }
        if let Some(value) = hooks.llm {
            config.hooks.llm = value;
        }
        if let Some(value) = hooks.timeout_secs {
            config.hooks.timeout_secs = value;
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
    if let Some(agent) = raw.agent {
        if let Some(value) = agent.max_steps {
            config.agent.max_steps = value;
        }
        if let Some(value) = agent.request_timeout_secs {
            config.agent.request_timeout_secs = value;
        }
    }
    if let Some(approval) = raw.approval
        && let Some(rules) = approval.rules
    {
        config.approval.rules = rules
            .into_iter()
            .enumerate()
            .map(|(index, rule)| {
                let action = parse_action(&rule.action).ok_or_else(|| {
                    anyhow::anyhow!(
                        "配置非法：approval.rules[{index}].action 取值未知：{}（应为 allow / ask / deny）。",
                        rule.action
                    )
                })?;
                Ok(Rule {
                    pattern: rule.pattern,
                    action,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
    }
    Ok(config)
}

/// 把配置中的动作字符串映射为枚举。
fn parse_action(value: &str) -> Option<Action> {
    match value {
        "allow" => Some(Action::Allow),
        "ask" => Some(Action::Ask),
        "deny" => Some(Action::Deny),
        _ => None,
    }
}

/// 校验限额，非法时返回清晰错误而不是静默错误行为。
fn validate(config: &Config) -> anyhow::Result<()> {
    if config.hooks.timeout_secs < 1 {
        anyhow::bail!(
            "配置非法：hooks.timeout_secs 必须 >= 1，当前为 {}。",
            config.hooks.timeout_secs
        );
    }
    if config.shell.timeout_secs < 1 {
        anyhow::bail!(
            "配置非法：shell.timeout_secs 必须 >= 1，当前为 {}。",
            config.shell.timeout_secs
        );
    }
    if config.agent.request_timeout_secs < 1 {
        anyhow::bail!(
            "配置非法：agent.request_timeout_secs 必须 >= 1，当前为 {}。",
            config.agent.request_timeout_secs
        );
    }
    let kept = config
        .shell
        .head_chars
        .saturating_add(config.shell.tail_chars);
    if kept > config.shell.max_stream_chars {
        anyhow::bail!(
            "配置非法：shell.head_chars + shell.tail_chars（{}）不能超过 shell.max_stream_chars（{}）。",
            kept,
            config.shell.max_stream_chars
        );
    }
    Ok(())
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
    fn default_hooks_are_init_and_llm() {
        let hooks = Hooks::default();
        assert_eq!(hooks.init, DEFAULT_INIT);
        assert_eq!(hooks.llm, DEFAULT_LLM);
        // 旧的两条提示词钩子已删除：默认值里不再出现它们的名字。
        assert!(!DEFAULT_INIT.contains("system_prompt"));
        assert!(!DEFAULT_INIT.contains("user_prompt"));
    }

    #[test]
    fn default_init_contains_set_e_and_three_notify_lines() {
        assert_eq!(DEFAULT_INIT.lines().count(), 4);
        assert_eq!(DEFAULT_INIT.lines().next(), Some("set -e"));
        assert_eq!(DEFAULT_INIT.matches("--notify=").count(), 3);
        assert!(DEFAULT_INIT.contains("--get-prompt system"));
        assert!(DEFAULT_INIT.contains("AGENTS.md"));
    }

    #[test]
    fn full_toml_is_parsed() {
        let file = write_config(
            r#"
[hooks]
init = "echo init"
llm = "cat llm.toml"
timeout_secs = 7

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
        assert_eq!(config.hooks.init, "echo init");
        assert_eq!(config.hooks.llm, "cat llm.toml");
        assert_eq!(config.hooks.timeout_secs, 7);
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
        assert_eq!(config.hooks.init, DEFAULT_INIT);
        assert_eq!(config.hooks.llm, DEFAULT_LLM);
        assert_eq!(config.hooks.timeout_secs, DEFAULT_HOOK_TIMEOUT_SECS);
        assert_eq!(config.agent.max_steps, DEFAULT_MAX_STEPS);
    }

    #[test]
    fn missing_hooks_section_uses_defaults() {
        let file = write_config("[agent]\nmax_steps = 3\n");
        let config = Config::load(file.path()).unwrap();
        assert_eq!(config.hooks.init, DEFAULT_INIT);
        assert_eq!(config.hooks.llm, DEFAULT_LLM);
        assert_eq!(config.hooks.timeout_secs, DEFAULT_HOOK_TIMEOUT_SECS);
        assert_eq!(config.agent.max_steps, 3);
    }

    #[test]
    fn hooks_init_and_llm_can_be_overridden() {
        let file = write_config("[hooks]\ninit = \"run-init\"\nllm = \"run-llm\"\n");
        let config = Config::load(file.path()).unwrap();
        assert_eq!(config.hooks.init, "run-init");
        assert_eq!(config.hooks.llm, "run-llm");
    }

    #[test]
    fn provider_is_not_a_hooks_field() {
        // 未知键被忽略：`provider` 已删除，写进配置也不产生任何效果。
        let file = write_config("[hooks]\nprovider = \"deepseek\"\n");
        let config = Config::load(file.path()).unwrap();
        assert_eq!(config.hooks.init, DEFAULT_INIT);
        assert_eq!(config.hooks.llm, DEFAULT_LLM);
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
    fn zero_timeout_is_rejected() {
        let file = write_config("[shell]\ntimeout_secs = 0\n");
        let err = Config::load(file.path()).unwrap_err();
        assert!(
            err.to_string().contains("timeout_secs"),
            "错误应指明字段：{err}"
        );
    }

    #[test]
    fn zero_hook_timeout_is_rejected() {
        let file = write_config("[hooks]\ntimeout_secs = 0\n");
        let err = Config::load(file.path()).unwrap_err();
        assert!(
            err.to_string().contains("hooks.timeout_secs"),
            "错误应指明字段：{err}"
        );
    }

    #[test]
    fn zero_request_timeout_is_rejected() {
        let file = write_config("[agent]\nrequest_timeout_secs = 0\n");
        let err = Config::load(file.path()).unwrap_err();
        assert!(
            err.to_string().contains("agent.request_timeout_secs"),
            "错误应指明字段：{err}"
        );
    }

    #[test]
    fn request_timeout_defaults_to_300() {
        let file = write_config("[agent]\nmax_steps = 3\n");
        let config = Config::load(file.path()).unwrap();
        assert_eq!(DEFAULT_REQUEST_TIMEOUT_SECS, 300);
        assert_eq!(
            config.agent.request_timeout_secs,
            DEFAULT_REQUEST_TIMEOUT_SECS
        );
    }

    #[test]
    fn request_timeout_is_parsed() {
        let file = write_config("[agent]\nrequest_timeout_secs = 5\n");
        let config = Config::load(file.path()).unwrap();
        assert_eq!(config.agent.request_timeout_secs, 5);
    }

    #[test]
    fn head_plus_tail_exceeding_max_is_rejected() {
        let file = write_config(
            r#"
[shell]
max_stream_chars = 10
head_chars = 8
tail_chars = 5
"#,
        );
        let err = Config::load(file.path()).unwrap_err();
        assert!(
            err.to_string().contains("max_stream_chars"),
            "错误应指明字段：{err}"
        );
    }

    #[test]
    fn head_plus_tail_equal_to_max_is_accepted() {
        let file = write_config(
            r#"
[shell]
max_stream_chars = 10
head_chars = 8
tail_chars = 2
"#,
        );
        let config = Config::load(file.path()).unwrap();
        assert_eq!(config.shell.max_stream_chars, 10);
    }

    #[test]
    fn approval_defaults_to_empty_rules() {
        let config = Config::default();
        assert!(config.approval.rules.is_empty());
    }

    #[test]
    fn approval_rules_are_parsed() {
        let file = write_config(
            r#"
[approval]
rules = [
  { pattern = "rm *",  action = "deny" },
  { pattern = "git *", action = "allow" },
  { pattern = "curl *", action = "ask" },
]
"#,
        );
        let config = Config::load(file.path()).unwrap();
        assert_eq!(
            config.approval.rules,
            vec![
                Rule {
                    pattern: "rm *".into(),
                    action: Action::Deny,
                },
                Rule {
                    pattern: "git *".into(),
                    action: Action::Allow,
                },
                Rule {
                    pattern: "curl *".into(),
                    action: Action::Ask,
                },
            ]
        );
    }

    #[test]
    fn approval_empty_rules_is_valid() {
        let file = write_config("[approval]\nrules = []\n");
        let config = Config::load(file.path()).unwrap();
        assert!(config.approval.rules.is_empty());
    }

    #[test]
    fn approval_unknown_action_is_rejected() {
        let file =
            write_config("[approval]\nrules = [{ pattern = \"rm *\", action = \"block\" }]\n");
        let err = Config::load(file.path()).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("approval"),
            "错误应指明 approval：{message}"
        );
        assert!(message.contains("block"), "错误应含非法取值：{message}");
    }

    #[test]
    fn missing_approval_section_keeps_defaults() {
        let file = write_config("[shell]\ntimeout_secs = 9\n");
        let config = Config::load(file.path()).unwrap();
        assert!(config.approval.rules.is_empty());
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
