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
const DEFAULT_COMPACTION_ENABLED: bool = true;
const DEFAULT_THRESHOLD_RATIO: f64 = 0.8;
const DEFAULT_RETAIN_RATIO: f64 = 0.16;
const DEFAULT_CHARS_PER_TOKEN: u32 = 4;
const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 8192;
const DEFAULT_PRUNE_THRESHOLD_CHARS: usize = 8192;
const DEFAULT_PRUNE_HEAD_CHARS: usize = 4096;
const DEFAULT_PRUNE_TAIL_CHARS: usize = 1024;
const DEFAULT_COMPACTION_RETRIES: u32 = 1;

/// 顶层配置。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    pub hooks: Hooks,
    pub shell: ShellConfig,
    pub agent: AgentConfig,
    pub approval: ApprovalConfig,
    pub compaction: CompactionConfig,
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

/// 渲染期压缩配置（阶段 3，D48–D58）。
///
/// `context_window` 缺失即「无从计算阈值」，压缩静默禁用（D49）；`enabled = false`
/// 同样禁用，且优先级最高。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionConfig {
    /// 是否启用压缩。
    pub enabled: bool,
    /// 模型上下文窗口（token）；`None` 时压缩静默禁用。
    pub context_window: Option<u32>,
    /// 触发阈值占窗口的比例：投影 token ≥ `floor(context_window × 该值)` 即压缩。
    pub threshold_ratio: f64,
    /// 逐字保留的近期尾部占窗口的比例；必须小于 `threshold_ratio`。
    pub retain_ratio: f64,
    /// token 估算密度（每个 token 的字符数）。
    pub chars_per_token: u32,
    /// 摘要调用的输出上限。
    pub max_tokens: u32,
    /// 工具消息正文超过此码点数才修剪。
    pub prune_threshold_chars: usize,
    /// 修剪保留的头部码点数。
    pub prune_head_chars: usize,
    /// 修剪保留的尾部码点数。
    pub prune_tail_chars: usize,
    /// 摘要后仍超阈值的额外压缩尝试次数。
    pub retries: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_COMPACTION_ENABLED,
            context_window: None,
            threshold_ratio: DEFAULT_THRESHOLD_RATIO,
            retain_ratio: DEFAULT_RETAIN_RATIO,
            chars_per_token: DEFAULT_CHARS_PER_TOKEN,
            max_tokens: DEFAULT_SUMMARY_MAX_TOKENS,
            prune_threshold_chars: DEFAULT_PRUNE_THRESHOLD_CHARS,
            prune_head_chars: DEFAULT_PRUNE_HEAD_CHARS,
            prune_tail_chars: DEFAULT_PRUNE_TAIL_CHARS,
            retries: DEFAULT_COMPACTION_RETRIES,
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
    compaction: Option<RawCompaction>,
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
struct RawCompaction {
    enabled: Option<bool>,
    context_window: Option<u32>,
    threshold_ratio: Option<f64>,
    retain_ratio: Option<f64>,
    chars_per_token: Option<u32>,
    max_tokens: Option<u32>,
    prune_threshold_chars: Option<usize>,
    prune_head_chars: Option<usize>,
    prune_tail_chars: Option<usize>,
    retries: Option<u32>,
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
    if let Some(compaction) = raw.compaction {
        if let Some(value) = compaction.enabled {
            config.compaction.enabled = value;
        }
        if let Some(value) = compaction.context_window {
            config.compaction.context_window = Some(value);
        }
        if let Some(value) = compaction.threshold_ratio {
            config.compaction.threshold_ratio = value;
        }
        if let Some(value) = compaction.retain_ratio {
            config.compaction.retain_ratio = value;
        }
        if let Some(value) = compaction.chars_per_token {
            config.compaction.chars_per_token = value;
        }
        if let Some(value) = compaction.max_tokens {
            config.compaction.max_tokens = value;
        }
        if let Some(value) = compaction.prune_threshold_chars {
            config.compaction.prune_threshold_chars = value;
        }
        if let Some(value) = compaction.prune_head_chars {
            config.compaction.prune_head_chars = value;
        }
        if let Some(value) = compaction.prune_tail_chars {
            config.compaction.prune_tail_chars = value;
        }
        if let Some(value) = compaction.retries {
            config.compaction.retries = value;
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

/// 校验一个比例字段落在 `(0, 1]` 内。
fn validate_ratio(name: &str, value: f64) -> anyhow::Result<()> {
    if !value.is_finite() || value <= 0.0 || value > 1.0 {
        anyhow::bail!("配置非法：{name} 必须是 (0, 1] 内的数，当前为 {value}。");
    }
    Ok(())
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
    if config.agent.max_steps < 1 {
        anyhow::bail!(
            "配置非法：agent.max_steps 必须 >= 1，当前为 {}。",
            config.agent.max_steps
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
    validate_ratio("compaction.threshold_ratio", config.compaction.threshold_ratio)?;
    validate_ratio("compaction.retain_ratio", config.compaction.retain_ratio)?;
    if config.compaction.retain_ratio >= config.compaction.threshold_ratio {
        anyhow::bail!(
            "配置非法：compaction.retain_ratio（{}）必须小于 compaction.threshold_ratio（{}）。",
            config.compaction.retain_ratio,
            config.compaction.threshold_ratio
        );
    }
    if config.compaction.chars_per_token < 1 {
        anyhow::bail!(
            "配置非法：compaction.chars_per_token 必须 >= 1，当前为 {}。",
            config.compaction.chars_per_token
        );
    }
    if config.compaction.max_tokens < 1 {
        anyhow::bail!(
            "配置非法：compaction.max_tokens 必须 >= 1，当前为 {}。",
            config.compaction.max_tokens
        );
    }
    if let Some(window) = config.compaction.context_window
        && window < 1
    {
        anyhow::bail!("配置非法：compaction.context_window 必须 >= 1，当前为 {window}。");
    }
    let prune_kept = config
        .compaction
        .prune_head_chars
        .saturating_add(config.compaction.prune_tail_chars)
        // 头尾之和加上修剪标记本身也不能超过阈值，才能保证修剪结果严格短于原文。
        .saturating_add(crate::compaction::prune::PRUNE_MARKER.chars().count());
    if prune_kept > config.compaction.prune_threshold_chars {
        anyhow::bail!(
            "配置非法：compaction.prune_head_chars + compaction.prune_tail_chars + 修剪标记（{}）不能超过 compaction.prune_threshold_chars（{}）。",
            prune_kept,
            config.compaction.prune_threshold_chars
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
    fn zero_max_steps_is_rejected() {
        let file = write_config("[agent]\nmax_steps = 0\n");
        let err = Config::load(file.path()).unwrap_err();
        assert!(
            err.to_string().contains("agent.max_steps"),
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

    #[test]
    fn compaction_defaults() {
        let config = Config::default();
        assert_eq!(
            config.compaction,
            CompactionConfig {
                enabled: true,
                context_window: None,
                threshold_ratio: 0.8,
                retain_ratio: 0.16,
                chars_per_token: 4,
                max_tokens: 8192,
                prune_threshold_chars: 8192,
                prune_head_chars: 4096,
                prune_tail_chars: 1024,
                retries: 1,
            }
        );
    }

    #[test]
    fn compaction_is_parsed() {
        let file = write_config(
            r#"
[compaction]
enabled = true
context_window = 64000
threshold_ratio = 0.75
retain_ratio = 0.2
chars_per_token = 3
max_tokens = 2048
prune_threshold_chars = 4096
prune_head_chars = 2048
prune_tail_chars = 512
retries = 2
"#,
        );
        let config = Config::load(file.path()).unwrap();
        assert_eq!(config.compaction.context_window, Some(64000));
        assert_eq!(config.compaction.threshold_ratio, 0.75);
        assert_eq!(config.compaction.retain_ratio, 0.2);
        assert_eq!(config.compaction.chars_per_token, 3);
        assert_eq!(config.compaction.max_tokens, 2048);
        assert_eq!(config.compaction.prune_threshold_chars, 4096);
        assert_eq!(config.compaction.prune_head_chars, 2048);
        assert_eq!(config.compaction.prune_tail_chars, 512);
        assert_eq!(config.compaction.retries, 2);
    }

    #[test]
    fn missing_compaction_section_keeps_defaults_and_disables() {
        let file = write_config("[agent]\nmax_steps = 3\n");
        let config = Config::load(file.path()).unwrap();
        assert_eq!(config.compaction, CompactionConfig::default());
        assert_eq!(
            config.compaction.context_window, None,
            "未配置窗口时无从计算阈值，应保持 None",
        );
    }

    #[test]
    fn compaction_retain_ratio_must_be_below_threshold() {
        let file = write_config("[compaction]\nthreshold_ratio = 0.5\nretain_ratio = 0.5\n");
        let err = Config::load(file.path()).unwrap_err();
        assert!(
            err.to_string().contains("retain_ratio"),
            "错误应指明字段：{err}"
        );
    }

    #[test]
    fn compaction_prune_budget_must_fit_threshold() {
        let file = write_config(
            "[compaction]\nprune_threshold_chars = 100\nprune_head_chars = 80\nprune_tail_chars = 40\n",
        );
        let err = Config::load(file.path()).unwrap_err();
        assert!(
            err.to_string().contains("prune_threshold_chars"),
            "错误应指明字段：{err}"
        );
    }

    #[test]
    fn compaction_prune_budget_includes_marker_in_threshold() {
        let marker = crate::compaction::prune::PRUNE_MARKER.chars().count();
        let threshold = 200usize;
        // head + tail 本身不超过阈值，但加上修剪标记就超过 → 应被拒。
        let file = write_config(&format!(
            "[compaction]\nprune_threshold_chars = {threshold}\nprune_head_chars = {}\nprune_tail_chars = 0\n",
            threshold - marker + 1
        ));
        let err = Config::load(file.path()).unwrap_err();
        assert!(
            err.to_string().contains("prune_threshold_chars"),
            "含标记超阈值应被拒：{err}"
        );

        // 恰好容纳标记时通过。
        let file = write_config(&format!(
            "[compaction]\nprune_threshold_chars = {threshold}\nprune_head_chars = {}\nprune_tail_chars = 0\n",
            threshold - marker
        ));
        assert!(Config::load(file.path()).is_ok(), "恰好含标记应通过");
    }

    #[test]
    fn compaction_zero_context_window_is_rejected() {
        let file = write_config("[compaction]\ncontext_window = 0\n");
        let err = Config::load(file.path()).unwrap_err();
        assert!(
            err.to_string().contains("context_window"),
            "错误应指明字段：{err}"
        );
    }

    #[test]
    fn compaction_zero_chars_per_token_is_rejected() {
        let file = write_config("[compaction]\nchars_per_token = 0\n");
        let err = Config::load(file.path()).unwrap_err();
        assert!(
            err.to_string().contains("chars_per_token"),
            "错误应指明字段：{err}"
        );
    }

    #[test]
    fn compaction_invalid_ratio_is_rejected() {
        let file = write_config("[compaction]\nthreshold_ratio = 1.5\n");
        let err = Config::load(file.path()).unwrap_err();
        assert!(
            err.to_string().contains("threshold_ratio"),
            "错误应指明字段：{err}"
        );
    }

    #[test]
    fn example_config_loads_and_enables_compaction() {
        // 示例配置是使用者的第一入口，必须始终与 schema 一致。
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../configs/config.example.toml");
        let config = Config::load(&path)
            .unwrap_or_else(|err| panic!("示例配置应可加载：{err:#}"));
        assert_eq!(config.compaction.context_window, Some(128000));
        assert!(config.compaction.enabled);
        assert_eq!(config.agent.max_steps, 30);
    }
}
