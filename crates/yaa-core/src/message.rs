//! OpenAI 兼容的聊天消息类型。
//!
//! `ChatMessage` 的出站请求体由 [`ChatMessage::to_request_value`] 显式构造：它按
//! `endpoint_format` 决定是否回传 `reasoning_content`，不依赖结构体自身的 serde 派生。
//! `AssistantToolCall` 保留自定义 serde：反序列化用于解析模型响应，序列化则产出
//! `{id, type:"function", function:{name, arguments}}` 的线格式，供请求体拼装复用。

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 一条聊天消息。
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Option<Vec<AssistantToolCall>>,
    pub tool_call_id: Option<String>,
    pub reasoning: Option<String>,
}

/// 助手发起的工具调用。
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ChatMessage {
    /// 系统消息。
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain("system", content.into())
    }

    /// 用户消息。
    pub fn user(content: impl Into<String>) -> Self {
        Self::plain("user", content.into())
    }

    /// 助手消息；`content`、`tool_calls` 与 `reasoning` 均可缺省。
    pub fn assistant(
        content: Option<String>,
        tool_calls: Option<Vec<AssistantToolCall>>,
        reasoning: Option<String>,
    ) -> Self {
        Self {
            role: "assistant".to_string(),
            content,
            tool_calls,
            tool_call_id: None,
            reasoning,
        }
    }

    /// 工具结果消息。
    pub fn tool_result(tool_call_id: String, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id),
            reasoning: None,
        }
    }

    fn plain(role: &str, content: String) -> Self {
        Self {
            role: role.to_string(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
            reasoning: None,
        }
    }

    /// 出站请求体里的单条消息。
    ///
    /// 仅当 `role == "assistant"` 且 `replay_reasoning` 为真时写入 `reasoning_content`：
    /// 有思考内容用其值；无思考但带工具调用时写空串（DeepSeek 系在请求带 `tools` 时
    /// 即使思考为空也要求该字段）；否则不带该键。
    pub fn to_request_value(&self, replay_reasoning: bool) -> serde_json::Value {
        let mut value = serde_json::json!({ "role": self.role });
        let object = value.as_object_mut().expect("`json!` 构造的对象必为 map");
        if let Some(content) = &self.content {
            object.insert(
                "content".to_string(),
                serde_json::Value::String(content.clone()),
            );
        }
        if let Some(tool_calls) = &self.tool_calls {
            object.insert(
                "tool_calls".to_string(),
                serde_json::to_value(tool_calls).unwrap_or(serde_json::Value::Null),
            );
        }
        if let Some(tool_call_id) = &self.tool_call_id {
            object.insert(
                "tool_call_id".to_string(),
                serde_json::Value::String(tool_call_id.clone()),
            );
        }
        if self.role == "assistant" && replay_reasoning {
            if let Some(reasoning) = &self.reasoning {
                object.insert(
                    "reasoning_content".to_string(),
                    serde_json::Value::String(reasoning.clone()),
                );
            } else if self
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
            {
                object.insert(
                    "reasoning_content".to_string(),
                    serde_json::Value::String(String::new()),
                );
            }
        }
        value
    }
}

impl Serialize for AssistantToolCall {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("tool_call", 3)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("type", "function")?;
        state.serialize_field(
            "function",
            &FunctionCallRef {
                name: &self.name,
                arguments: &self.arguments,
            },
        )?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for AssistantToolCall {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(ToolCallVisitor)
    }
}

struct FunctionCallRef<'a> {
    name: &'a str,
    arguments: &'a str,
}

impl Serialize for FunctionCallRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("function", 2)?;
        state.serialize_field("name", self.name)?;
        state.serialize_field("arguments", self.arguments)?;
        state.end()
    }
}

#[derive(Deserialize)]
struct FunctionCall {
    name: String,
    #[serde(default)]
    arguments: String,
}

struct ToolCallVisitor;

impl<'de> Visitor<'de> for ToolCallVisitor {
    type Value = AssistantToolCall;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("一个工具调用对象")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut id = None;
        let mut function: Option<FunctionCall> = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "id" => id = Some(map.next_value()?),
                "function" => function = Some(map.next_value()?),
                // `type` 恒为 "function"，忽略即可。
                "type" => {
                    let _: String = map.next_value()?;
                }
                _ => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }
        let id = id.ok_or_else(|| de::Error::missing_field("id"))?;
        let function = function.ok_or_else(|| de::Error::missing_field("function"))?;
        Ok(AssistantToolCall {
            id,
            name: function.name,
            arguments: function.arguments,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> AssistantToolCall {
        AssistantToolCall {
            id: "call_1".to_string(),
            name: "shell".to_string(),
            arguments: "{\"command\":\"ls\"}".to_string(),
        }
    }

    #[test]
    fn system_message_omits_absent_fields() {
        let value = ChatMessage::system("hi").to_request_value(false);
        let object = value.as_object().unwrap();
        assert_eq!(object.get("role").unwrap(), "system");
        assert_eq!(object.get("content").unwrap(), "hi");
        assert!(!object.contains_key("tool_calls"));
        assert!(!object.contains_key("tool_call_id"));
        assert!(!object.contains_key("reasoning_content"));
    }

    #[test]
    fn tool_calls_use_openai_wire_shape() {
        let message = ChatMessage::assistant(None, Some(vec![call()]), None);
        let value = message.to_request_value(false);
        assert_eq!(value["tool_calls"][0]["id"], "call_1");
        assert_eq!(value["tool_calls"][0]["type"], "function");
        assert_eq!(value["tool_calls"][0]["function"]["name"], "shell");
        assert_eq!(
            value["tool_calls"][0]["function"]["arguments"],
            "{\"command\":\"ls\"}"
        );
        assert!(value.get("content").is_none());
    }

    #[test]
    fn tool_result_message_carries_tool_call_id() {
        let message = ChatMessage::tool_result("call_1".to_string(), "output");
        let value = message.to_request_value(false);
        assert_eq!(value["role"], "tool");
        assert_eq!(value["content"], "output");
        assert_eq!(value["tool_call_id"], "call_1");
    }

    #[test]
    fn reasoning_replayed_only_for_assistant_when_requested() {
        let message = ChatMessage::assistant(None, Some(vec![call()]), Some("思考".into()));
        assert_eq!(message.to_request_value(true)["reasoning_content"], "思考");
        // openai 端点（replay=false）不回传。
        assert!(
            message
                .to_request_value(false)
                .get("reasoning_content")
                .is_none()
        );
        // 非 assistant 角色不写入。
        let user = ChatMessage::user("hi");
        assert!(
            user.to_request_value(true)
                .get("reasoning_content")
                .is_none()
        );
    }

    #[test]
    fn assistant_with_tool_calls_and_empty_reasoning_replays_empty_string() {
        let message = ChatMessage::assistant(None, Some(vec![call()]), None);
        assert_eq!(message.to_request_value(true)["reasoning_content"], "");
    }

    #[test]
    fn assistant_without_tool_calls_and_without_reasoning_omits_key() {
        let message = ChatMessage::assistant(Some("回复".into()), None, None);
        assert!(
            message
                .to_request_value(true)
                .get("reasoning_content")
                .is_none()
        );
    }

    #[test]
    fn deserializes_tool_call_response() {
        let text = r#"{
            "id": "call_9",
            "type": "function",
            "function": {"name": "shell", "arguments": "{}"}
        }"#;
        let parsed: AssistantToolCall = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.id, "call_9");
        assert_eq!(parsed.name, "shell");
        assert_eq!(parsed.arguments, "{}");
    }
}
