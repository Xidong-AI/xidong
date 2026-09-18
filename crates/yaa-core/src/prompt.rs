//! 内置提示词模板与工具 schema。
//!
//! `builtin` 支撑 `yaa --get-prompt`：该命令必须在加载配置与执行任何钩子之前短路，
//! 只打印这里的常量，否则 `system_prompt` 钩子内部调用它会形成递归环。

use sha2::{Digest, Sha256};

const SYSTEM_TEMPLATE: &str = r#"当前智能体环境是 YAA，一个以 Shell 为唯一工具的智能体。

可用工具只有 `shell` 一个函数。需要执行命令时，调用 `shell`，参数为：

- `command`（必填）：要执行的 shell 命令字符串。
- `description`（可选）：一句话说明命令用途。

规则：

1. 一次只调用一个 `shell`，等待结果返回后再决定下一步。
2. 不要编造命令输出；观察真实结果后再回答。
3. 任务完成后用自然语言给出最终答复，不要继续调用工具。
"#;

const USER_TEMPLATE: &str = "请根据当前工作目录与已有上下文，完成用户的请求。";

/// 返回内置模板；未知名称返回 `None`。
pub fn builtin(name: &str) -> Option<&'static str> {
    match name {
        "system" => Some(SYSTEM_TEMPLATE),
        "user" => Some(USER_TEMPLATE),
        _ => None,
    }
}

/// 唯一工具 `shell` 的 OpenAI function-tool schema。
pub fn shell_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "shell",
            "description": "执行 shell 命令并返回 stdout、stderr 与退出码。",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "要执行的 shell 命令。"
                    },
                    "description": {
                        "type": "string",
                        "description": "命令用途的可选说明。"
                    }
                },
                "required": ["command"]
            }
        }
    })
}

/// 本轮暴露的工具列表。
pub fn shell_tools() -> serde_json::Value {
    serde_json::Value::Array(vec![shell_tool_schema()])
}

/// 工具列表的规范化 JSON 的 sha2-256 十六进制哈希。
///
/// `serde_json` 默认以有序 map 表示对象，因此同一 schema 的序列化结果稳定。
pub fn tool_schema_hash() -> String {
    let canonical = serde_json::to_string(&shell_tools()).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_template_mentions_shell() {
        let text = builtin("system").expect("内置 system 模板应存在");
        assert!(text.contains("shell"));
        assert!(text.contains("command"));
    }

    #[test]
    fn user_template_exists() {
        assert!(builtin("user").is_some());
    }

    #[test]
    fn unknown_template_is_none() {
        assert!(builtin("nope").is_none());
    }

    #[test]
    fn schema_shape() {
        let schema = shell_tool_schema();
        assert!(schema.is_object());
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "shell");
        assert_eq!(schema["function"]["parameters"]["required"][0], "command");
    }

    #[test]
    fn hash_is_stable() {
        let a = tool_schema_hash();
        assert_eq!(a, tool_schema_hash());
        assert_eq!(a.len(), 64);
    }
}
