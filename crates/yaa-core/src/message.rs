//! OpenAI 兼容的聊天消息类型。
//!
//! `ChatMessage` 以 `None` 字段省略的方式序列化，得到合法的 OpenAI 请求体；
//! `AssistantToolCall` 自定义 serde，以线格式
//! `{id, type:"function", function:{name, arguments}}` 表示。

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 一条聊天消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<AssistantToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// 助手发起的工具调用。
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain("system", content.into())
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::plain("user", content.into())
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::plain("assistant", content.into())
    }

    /// 带工具调用的助手消息（无文本内容）。
    pub fn assistant_tool_calls(tool_calls: Vec<AssistantToolCall>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    /// 工具结果消息。
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    fn plain(role: &str, content: String) -> Self {
        Self {
            role: role.to_string(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
        }
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

    #[test]
    fn system_message_omits_tool_fields() {
        let value = serde_json::to_value(ChatMessage::system("hi")).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.get("role").unwrap(), "system");
        assert_eq!(object.get("content").unwrap(), "hi");
        assert!(!object.contains_key("tool_calls"));
        assert!(!object.contains_key("tool_call_id"));
    }

    #[test]
    fn tool_calls_use_openai_wire_shape() {
        let message = ChatMessage::assistant_tool_calls(vec![AssistantToolCall {
            id: "call_1".to_string(),
            name: "shell".to_string(),
            arguments: "{\"command\":\"ls\"}".to_string(),
        }]);
        let value = serde_json::to_value(&message).unwrap();
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
    fn tool_result_roundtrips() {
        let original = ChatMessage::tool_result("call_1", "output");
        let text = serde_json::to_string(&original).unwrap();
        let parsed: ChatMessage = serde_json::from_str(&text).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn deserializes_tool_call_response() {
        let text = r#"{
            "id": "call_9",
            "type": "function",
            "function": {"name": "shell", "arguments": "{}"}
        }"#;
        let call: AssistantToolCall = serde_json::from_str(text).unwrap();
        assert_eq!(call.id, "call_9");
        assert_eq!(call.name, "shell");
        assert_eq!(call.arguments, "{}");
    }
}
