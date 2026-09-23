//! 唯一工具 `shell` 的全部知识：schema、提示词 section、参数解析、错误文案、
//! 调用编排与结果转换（见 `docs/plan.md` D14）。
//!
//! `yaa-exec` 只提供执行机制（进程组、超时、流式截断、完整输出落盘），本模块把
//! 解析后的命令交给它，并把输出转换为 `tool_result` 记录。

use std::collections::HashSet;
use std::path::Path;

use sha2::{Digest, Sha256};
use yaa_msg::Record;

use crate::message::AssistantToolCall;

/// 工具名。
pub const NAME: &str = "shell";

/// 唯一工具的 OpenAI function-tool schema。
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": NAME,
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

/// 本轮暴露的工具列表（含一个 `shell`）。
pub fn tools() -> serde_json::Value {
    serde_json::Value::Array(vec![schema()])
}

/// 工具列表的规范化 JSON 的 sha2-256 十六进制哈希。
///
/// `serde_json` 默认以有序 map 表示对象，因此同一 schema 的序列化结果稳定。
pub fn schema_hash() -> String {
    let canonical = serde_json::to_string(&tools()).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 供系统提示词拼装的工具说明，必须提及工具名与参数名。
pub fn prompt_section() -> &'static str {
    "可用工具只有 `shell` 一个函数。调用时参数为：

- `command`（必填）：要执行的 shell 命令字符串。
- `description`（可选）：一句话说明命令用途。"
}

/// 解析后的工具调用参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCall {
    /// 要执行的命令（合法时为参数值，非法时回退为原始文本，供展示）。
    pub command: String,
    /// 可选的命令用途说明。
    pub description: Option<String>,
    /// 参数非法时的错误文案；合法为 `None`。
    pub error: Option<String>,
}

/// 解析模型给出的 `arguments` JSON。
///
/// 仅当是 JSON 对象且含字符串 `command` 时合法；否则给出错误文案，并把原始文本
/// 保留在 `command` 中用于展示。
pub fn parse(arguments: &str) -> ParsedCall {
    let value = serde_json::from_str::<serde_json::Value>(arguments);
    match value {
        Ok(value) => {
            let command = value.get("command").and_then(|command| command.as_str());
            match command {
                Some(command) => ParsedCall {
                    command: command.to_string(),
                    description: value
                        .get("description")
                        .and_then(|description| description.as_str())
                        .filter(|description| !description.is_empty())
                        .map(str::to_string),
                    error: None,
                },
                None => ParsedCall {
                    command: arguments.to_string(),
                    description: None,
                    error: Some("工具参数缺少字符串字段 `command`。".to_string()),
                },
            }
        }
        Err(err) => ParsedCall {
            command: arguments.to_string(),
            description: None,
            error: Some(format!("工具参数不是合法 JSON：{err}")),
        },
    }
}

/// 执行一次工具调用，返回 `tool_result` 记录（错误也编码为记录）。
///
/// `name` 是模型给出的函数名：阶段 1 只支持 [`NAME`]，其他名字直接返回失败记录且
/// **不执行**命令。`extra_env` 注入子进程（如 `YAA_SESSION`），使工具内嵌的
/// `cmd | yaa --notify` 写回本会话。
pub async fn execute(
    name: &str,
    call_id: &str,
    parsed: &ParsedCall,
    limits: &yaa_exec::ShellLimits,
    output_dir: &Path,
    data_root: &Path,
    extra_env: &[(String, String)],
) -> Record {
    if name != NAME {
        return failure(
            call_id,
            &format!("不支持的工具：{name}（阶段 1 仅有 shell）。"),
        );
    }
    if let Some(error) = &parsed.error {
        return failure(call_id, error);
    }

    // 除调用方给的 `YAA_SESSION` 等变量外，把本次工具调用的限额以 `YAA_NOTIFY_*`
    // 透传给子进程：工具内嵌的 `yaa --notify` 据此采用同一套截断/超时参数（T3）。
    let mut env = extra_env.to_vec();
    env.push((
        "YAA_NOTIFY_TIMEOUT_SECS".to_string(),
        limits.timeout_secs.to_string(),
    ));
    env.push((
        "YAA_NOTIFY_MAX_STREAM_CHARS".to_string(),
        limits.max_stream_chars.to_string(),
    ));
    env.push((
        "YAA_NOTIFY_HEAD_CHARS".to_string(),
        limits.head_chars.to_string(),
    ));
    env.push((
        "YAA_NOTIFY_TAIL_CHARS".to_string(),
        limits.tail_chars.to_string(),
    ));

    let command = yaa_exec::ShellCommand {
        call_id: call_id.to_string(),
        command: parsed.command.clone(),
        env,
    };
    match yaa_exec::run(&command, limits, output_dir).await {
        Ok(output) => output_to_record(&output, data_root),
        Err(err) => failure(call_id, &format!("Shell 执行失败：{err}")),
    }
}

/// 失败结果：`code = -1`、错误进 stderr、无输出文件。
fn failure(call_id: &str, stderr: &str) -> Record {
    Record::tool_result(call_id, "", stderr, -1, false, false, None)
}

/// 把 Shell 输出转成 `tool_result` 记录，`output_files` 相对数据根。
fn output_to_record(output: &yaa_exec::ShellOutput, data_root: &Path) -> Record {
    let output_files = if output.stdout_file.is_some() || output.stderr_file.is_some() {
        let stdout = output
            .stdout_file
            .as_deref()
            .map(|path| relativize(path, data_root));
        let stderr = output
            .stderr_file
            .as_deref()
            .map(|path| relativize(path, data_root));
        Some(yaa_msg::OutputFiles { stdout, stderr })
    } else {
        None
    };

    Record::tool_result(
        output.call_id.clone(),
        output.stdout.clone(),
        output.stderr.clone(),
        output.code,
        output.timed_out,
        output.truncated,
        output_files,
    )
}

/// 清洗模型返回的工具调用 id。
///
/// 空串、批次内重复、或与历史任何 `call_id` 冲突的 id 都替换为本地生成的
/// `call-<ULID>`，保证渲染出的 `tool_call_id` 唯一且非空。
pub fn sanitize_tool_calls(
    history: &[yaa_msg::Record],
    calls: &[AssistantToolCall],
) -> Vec<AssistantToolCall> {
    let mut used: HashSet<String> = history
        .iter()
        .filter_map(|record| match record {
            yaa_msg::Record::ToolCall(call) => Some(call.call_id.clone()),
            _ => None,
        })
        .collect();
    calls
        .iter()
        .map(|call| {
            let id = if call.id.is_empty() || used.contains(&call.id) {
                format!("call-{}", ulid::Ulid::new())
            } else {
                call.id.clone()
            };
            used.insert(id.clone());
            AssistantToolCall {
                id,
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            }
        })
        .collect()
}

/// 把绝对路径相对化到数据根；不在数据根下时退化为绝对路径字符串。
///
/// `yaa_exec` 返回的是 `canonicalize` 后的路径，而 `data_root` 可能是未规范化的
/// 符号链接目录，因此先按原值匹配，再退回规范化后的数据根（`canonicalize` 失败则
/// 忽略）。两者都不匹配时（如 `--session` 指向数据根之外）保留绝对路径，绝不返回
/// `None`——否则 `truncated` 为真时模型会被误导去一个不存在的相对文件找完整输出。
pub fn relativize(path: &Path, data_root: &Path) -> String {
    path.strip_prefix(data_root)
        .ok()
        .or_else(|| {
            let canonical_root = std::fs::canonicalize(data_root).ok()?;
            path.strip_prefix(&canonical_root).ok()
        })
        .map(|relative| relative.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_shape_and_names() {
        let schema = schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], NAME);
        assert_eq!(schema["function"]["parameters"]["required"][0], "command");
        assert!(schema["function"]["parameters"]["properties"]["description"].is_object());
    }

    #[test]
    fn tools_wraps_single_shell() {
        let tools = tools();
        assert_eq!(tools.as_array().unwrap().len(), 1);
        assert_eq!(tools[0]["function"]["name"], NAME);
    }

    #[test]
    fn hash_is_stable_hex() {
        let a = schema_hash();
        assert_eq!(a, schema_hash());
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn prompt_section_mentions_tool_and_parameters() {
        let section = prompt_section();
        for needle in ["shell", "command", "description"] {
            assert!(section.contains(needle), "提示词 section 缺少 {needle}");
        }
    }

    #[test]
    fn parse_accepts_command_and_optional_description() {
        let parsed = parse(r#"{"command":"ls -la","description":"列目录"}"#);
        assert_eq!(parsed.error, None);
        assert_eq!(parsed.command, "ls -la");
        assert_eq!(parsed.description.as_deref(), Some("列目录"));
    }

    #[test]
    fn parse_drops_empty_description() {
        let parsed = parse(r#"{"command":"ls","description":""}"#);
        assert_eq!(parsed.error, None);
        assert_eq!(parsed.description, None);
    }

    #[test]
    fn parse_reports_invalid_json_and_keeps_raw_text() {
        let parsed = parse("not json");
        assert_eq!(parsed.command, "not json");
        assert_eq!(parsed.description, None);
        assert!(parsed.error.as_deref().unwrap().contains("不是合法 JSON"));
    }

    #[test]
    fn parse_reports_missing_command() {
        let parsed = parse(r#"{"description":"无命令"}"#);
        assert_eq!(parsed.command, r#"{"description":"无命令"}"#);
        assert!(
            parsed
                .error
                .as_deref()
                .unwrap()
                .contains("缺少字符串字段 `command`")
        );
    }

    #[tokio::test]
    async fn execute_invalid_arguments_returns_failed_record() {
        let dir = tempfile::tempdir().unwrap();
        let limits = test_limits();
        let parsed = parse("not json");
        let record = execute(NAME, "c1", &parsed, &limits, dir.path(), dir.path(), &[]).await;
        let yaa_msg::Record::ToolResult(result) = record else {
            panic!("应生成 tool_result");
        };
        assert_eq!(result.code, -1);
        assert!(result.stderr.contains("不是合法 JSON"));
        assert!(result.stdout.is_empty());
        assert_eq!(result.output_files, None);
    }

    #[tokio::test]
    async fn execute_unknown_tool_name_is_rejected_without_running() {
        let dir = tempfile::tempdir().unwrap();
        let limits = test_limits();
        let marker = dir.path().join("marker");
        let parsed = parse(&format!(r#"{{"command":"touch {}"}}"#, marker.display()));
        let record = execute("other", "c1", &parsed, &limits, dir.path(), dir.path(), &[]).await;
        let yaa_msg::Record::ToolResult(result) = record else {
            panic!("应生成 tool_result");
        };
        assert_eq!(result.code, -1);
        assert_eq!(result.stderr, "不支持的工具：other（阶段 1 仅有 shell）。");
        assert!(!marker.exists(), "不支持的工具名不得执行命令");
    }

    #[tokio::test]
    async fn execute_runs_command_and_records_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let limits = test_limits();
        let parsed = parse(r#"{"command":"echo hi"}"#);
        let record = execute(NAME, "c1", &parsed, &limits, dir.path(), dir.path(), &[]).await;
        let yaa_msg::Record::ToolResult(result) = record else {
            panic!("应生成 tool_result");
        };
        assert_eq!(result.code, 0);
        assert_eq!(result.stdout, "hi\n");
        assert_eq!(result.call_id, "c1");
    }

    #[tokio::test]
    async fn execute_injects_extra_env() {
        let dir = tempfile::tempdir().unwrap();
        let limits = test_limits();
        let parsed = parse(r#"{"command":"printf %s \"$YAA_TEST_ENV\""}"#);
        let extra = [("YAA_TEST_ENV".to_string(), "会话路径".to_string())];
        let record = execute(NAME, "c1", &parsed, &limits, dir.path(), dir.path(), &extra).await;
        let yaa_msg::Record::ToolResult(result) = record else {
            panic!("应生成 tool_result");
        };
        assert_eq!(result.stdout, "会话路径");
    }

    #[tokio::test]
    async fn execute_injects_notify_limit_env() {
        let dir = tempfile::tempdir().unwrap();
        let limits = test_limits();
        let parsed = parse(
            r#"{"command":"printf '%s,%s,%s,%s' \"$YAA_NOTIFY_TIMEOUT_SECS\" \"$YAA_NOTIFY_MAX_STREAM_CHARS\" \"$YAA_NOTIFY_HEAD_CHARS\" \"$YAA_NOTIFY_TAIL_CHARS\""}"#,
        );
        let record = execute(NAME, "c1", &parsed, &limits, dir.path(), dir.path(), &[]).await;
        let yaa_msg::Record::ToolResult(result) = record else {
            panic!("应生成 tool_result");
        };
        assert_eq!(
            result.stdout, "10,1000,100,50",
            "工具子进程应收到本次调用的限额"
        );
    }

    #[test]
    fn sanitize_regenerates_empty_and_duplicate_ids() {
        let history = vec![yaa_msg::Record::tool_call(
            yaa_msg::ToolOrigin::Ai,
            "old",
            "ls",
            None,
        )];
        let calls = vec![
            AssistantToolCall {
                id: "old".to_string(),
                name: NAME.to_string(),
                arguments: "{}".to_string(),
            },
            AssistantToolCall {
                id: String::new(),
                name: NAME.to_string(),
                arguments: "{}".to_string(),
            },
            AssistantToolCall {
                id: "fresh".to_string(),
                name: NAME.to_string(),
                arguments: "{}".to_string(),
            },
        ];
        let sanitized = sanitize_tool_calls(&history, &calls);
        assert_ne!(sanitized[0].id, "old");
        assert!(!sanitized[1].id.is_empty());
        assert_eq!(sanitized[2].id, "fresh");
        let unique: HashSet<&String> = sanitized.iter().map(|call| &call.id).collect();
        assert_eq!(unique.len(), 3);
    }

    #[test]
    fn relativize_inside_root_is_relative_and_outside_keeps_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        let inside = root.join("sessions").join("sid").join("a.stdout.out");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        std::fs::write(&inside, "x").unwrap();
        assert_eq!(relativize(&inside, &root), "sessions/sid/a.stdout.out");

        let outside = dir.path().join("elsewhere").join("b.stdout.out");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, "x").unwrap();
        assert_eq!(relativize(&outside, &root), outside.to_string_lossy());
    }

    fn test_limits() -> yaa_exec::ShellLimits {
        yaa_exec::ShellLimits {
            timeout_secs: 10,
            max_stream_chars: 1000,
            head_chars: 100,
            tail_chars: 50,
        }
    }
}
