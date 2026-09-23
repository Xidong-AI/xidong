//! 内置系统提示词。
//!
//! `builtin` 支撑 `yaa --get-prompt`：该命令必须在加载配置与执行任何钩子之前短路，
//! 只打印这里的文本，否则 `init` 钩子内部调用它会形成递归环。工具说明随工具走，
//! 由 [`crate::tools::shell::prompt_section`] 提供，避免 schema 与提示词漂移（D14、D17）。

use crate::tools::shell;

/// 产品前导：规则要点，工具 section 由工具模块拼上。
const SYSTEM_PREAMBLE: &str = "当前智能体环境是 YAA，一个以 Shell 为唯一工具的智能体。

规则：

1. 需要执行命令时调用 `shell` 工具，一次只调用一个，等待结果返回后再决定下一步。
2. 不要编造命令输出；观察真实结果后再回答。
3. 任务完成后用自然语言给出最终答复，不要继续调用工具。";

/// 内置系统提示词：产品前导加工具说明 section。
pub fn system_prompt() -> String {
    format!("{SYSTEM_PREAMBLE}\n\n{}", shell::prompt_section())
}

/// 返回内置模板；未知名称返回 `None`。阶段 1 只保留 `system`。
pub fn builtin(name: &str) -> Option<String> {
    match name {
        "system" => Some(system_prompt()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_mentions_tool_and_parameters() {
        let text = system_prompt();
        for needle in ["shell", "command", "description"] {
            assert!(text.contains(needle), "系统提示词缺少 {needle}");
        }
    }

    #[test]
    fn system_prompt_embeds_tool_section() {
        assert!(system_prompt().contains(shell::prompt_section()));
    }

    #[test]
    fn builtin_only_knows_system() {
        assert!(builtin("system").is_some());
        assert!(builtin("user").is_none());
        assert!(builtin("nope").is_none());
    }
}
