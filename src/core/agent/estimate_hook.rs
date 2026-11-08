//! `[hooks] estimate` 的协议层：收集待估文本、编码请求、解码响应。
//!
//! 本模块只做纯数据的转换，不发起进程调用；进程调用、缓存与禁用状态由 [`super`]
//! 负责，因此可脱离 I/O 独立单测。

use std::collections::{BTreeSet, HashMap};

use crate::msg::Record;

use crate::core::compaction::PrecomputedMap;

/// 收集所有记录投影出的非空文本字段，并附加前驱摘要文本。
///
/// 粒度与内置估算一致：每个会被投影为上下文的文本字段各算一个 key。用 `BTreeSet`
/// 去重并保证同一输入集合跨调用顺序稳定；空串不参与估算（内置估算对空串恒为 0）。
pub(super) fn collect_texts(records: &[Record], previous_summary: Option<&str>) -> Vec<String> {
    let mut texts = BTreeSet::new();
    for record in records {
        match record {
            Record::User(user) => {
                texts.insert(user.content.clone());
            }
            Record::Ai(ai) => {
                texts.insert(ai.content.clone());
                if let Some(reasoning) = &ai.reasoning {
                    texts.insert(reasoning.clone());
                }
            }
            Record::ToolCall(call) => {
                texts.insert(call.command.clone());
                if let Some(description) = &call.description {
                    texts.insert(description.clone());
                }
            }
            Record::ToolResult(result) => {
                texts.insert(result.stdout.clone());
                texts.insert(result.stderr.clone());
            }
            Record::System(system) => {
                texts.insert(system.content.clone());
                if let Some(command) = &system.command {
                    texts.insert(command.clone());
                }
            }
            Record::ApprovalPending(_) | Record::ApprovalResolved(_) => {}
        }
    }
    if let Some(summary) = previous_summary {
        texts.insert(summary.to_string());
    }
    texts.into_iter().filter(|text| !text.is_empty()).collect()
}

/// 把待估文本编码为钩子的 stdin：一个 JSON 字符串数组。
pub(super) fn encode(texts: &[String]) -> anyhow::Result<String> {
    Ok(serde_json::to_string(texts)?)
}

/// 解码钩子 stdout 的等长 JSON 整数数组。
///
/// 长度不匹配、负数、非整数或非法 JSON 都视为协议错误，由调用方回退内置估算。
pub(super) fn decode(stdout: &str, texts: &[String]) -> anyhow::Result<PrecomputedMap> {
    let values: Vec<i64> = serde_json::from_str(stdout)?;
    if values.len() != texts.len() {
        anyhow::bail!(
            "token 估算钩子返回 {} 个值，期望 {} 个",
            values.len(),
            texts.len()
        );
    }
    let mut map = HashMap::with_capacity(texts.len());
    for (text, value) in texts.iter().zip(values) {
        let value = usize::try_from(value)
            .map_err(|_| anyhow::anyhow!("token 估算钩子返回负数：{value}"))?;
        map.insert(text.clone(), value);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::{ApprovalDecision, ToolOrigin};

    fn records() -> Vec<Record> {
        vec![
            Record::system("init", "系统正文", Some("init-cmd".to_string()), None),
            Record::user("用户"),
            Record::ai("m", "h", "回复", Some("思考".into())),
            Record::tool_call(ToolOrigin::Ai, "c1", "echo", Some("说明".into())),
            Record::tool_result("c1", "标准输出", "标准错误", 0, false, false, None),
            Record::approval_pending("c1", "echo", "rule"),
            Record::approval_resolved("c1", ApprovalDecision::Approve),
            // 重复内容与空串都不应产生额外 key。
            Record::user("用户"),
            Record::ai("m", "h", "", None),
            Record::system("notify", "", Some(String::new()), None),
        ]
    }

    #[test]
    fn collects_all_projection_fields_without_duplicates() {
        let texts = collect_texts(&records(), None);
        let expected: Vec<String> = [
            "echo",
            "init-cmd",
            "回复",
            "思考",
            "标准错误",
            "标准输出",
            "用户",
            "系统正文",
            "说明",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
        assert_eq!(texts, expected);
    }

    #[test]
    fn includes_previous_summary() {
        let texts = collect_texts(&records(), Some("前驱摘要"));
        assert!(texts.iter().any(|text| text == "前驱摘要"));
    }

    #[test]
    fn filters_empty_texts() {
        let texts = collect_texts(&records(), Some(""));
        assert!(texts.iter().all(|text| !text.is_empty()));
    }

    #[test]
    fn encode_decode_round_trip() {
        let texts = vec!["你好".to_string(), "abc".to_string()];
        let payload = encode(&texts).unwrap();
        assert_eq!(payload, r#"["你好","abc"]"#);
        let map = decode("[4,2]", &texts).unwrap();
        assert_eq!(map.get("你好"), Some(&4));
        assert_eq!(map.get("abc"), Some(&2));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn decode_rejects_length_mismatch() {
        let texts = vec!["a".to_string(), "b".to_string()];
        let err = decode("[1]", &texts).unwrap_err();
        assert!(
            err.to_string().contains("期望 2 个"),
            "错误应说明长度：{err}"
        );
    }

    #[test]
    fn decode_rejects_negative_values() {
        let texts = vec!["a".to_string(), "b".to_string()];
        let err = decode("[-1,2]", &texts).unwrap_err();
        assert!(err.to_string().contains("负数"), "错误应说明负数：{err}");
    }

    #[test]
    fn decode_rejects_non_integers() {
        let texts = vec!["a".to_string()];
        assert!(decode("[1.5]", &texts).is_err());
        assert!(decode("[\"x\"]", &texts).is_err());
    }
}
