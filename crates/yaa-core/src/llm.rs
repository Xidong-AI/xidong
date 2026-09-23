//! 模型供应配置与 OpenAI Chat Completions 适配。
//!
//! `llm` 钩子的 stdout 是 TOML，经 `LlmConfig::from_toml` 解析；`api_key` 永不写入
//! 日志或错误信息（自定义 `Debug` 亦做遮蔽）。`endpoint_format` 是唯一驱动出站
//! 方言分支的字段：它决定 `reasoning` 是否作为顶层 `reasoning_content` 回传（D24）。

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::message::{AssistantToolCall, ChatMessage};

/// 单次 HTTP 错误的响应片段上限（字符）。
const ERROR_SNIPPET_CHARS: usize = 512;
/// 总尝试次数（1 次首发 + 2 次重试）。
const MAX_ATTEMPTS: usize = 3;
/// 首次重试退避（毫秒），之后按 ×3 指数增长（200ms、600ms）。
const BASE_BACKOFF_MS: u64 = 200;

/// 出站端点方言。决定是否回传思考内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointFormat {
    /// OpenAI 官方 Chat Completions：不认 `reasoning_content`，不回传。
    #[default]
    OpenAi,
    /// DeepSeek：请求带 `tools` 时须回传 `reasoning_content`。
    DeepSeek,
    /// Kimi：同 DeepSeek 系。
    Kimi,
    /// Qwen：同 DeepSeek 系。
    Qwen,
    /// GLM：同 DeepSeek 系。
    Glm,
}

impl EndpointFormat {
    /// 是否把 `ai.reasoning` 作为 assistant 顶层 `reasoning_content` 回传。
    pub fn replays_reasoning(self) -> bool {
        !matches!(self, EndpointFormat::OpenAi)
    }
}

/// 模型供应配置。
#[derive(Clone, PartialEq)]
pub struct LlmConfig {
    pub endpoint_format: EndpointFormat,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
}

impl std::fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmConfig")
            .field("endpoint_format", &self.endpoint_format)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct RawLlmConfig {
    #[serde(default)]
    endpoint_format: EndpointFormat,
    base_url: String,
    api_key: String,
    model: String,
    #[serde(default = "default_temperature")]
    temperature: f32,
    #[serde(default = "default_max_tokens")]
    max_tokens: u32,
}

fn default_temperature() -> f32 {
    0.7
}

fn default_max_tokens() -> u32 {
    4096
}

impl LlmConfig {
    /// 解析 `llm` 钩子的 stdout。
    ///
    /// `base_url`、`model`、`api_key` 必填；`endpoint_format`、`temperature`、
    /// `max_tokens` 有默认值。
    pub fn from_toml(text: &str) -> Result<Self> {
        let raw: RawLlmConfig =
            toml::from_str(text).context("解析 llm 钩子输出失败（应为 TOML）")?;
        Ok(Self {
            endpoint_format: raw.endpoint_format,
            base_url: raw.base_url,
            api_key: raw.api_key,
            model: raw.model,
            temperature: raw.temperature,
            max_tokens: raw.max_tokens,
        })
    }
}

/// 由 `base_url` 推导 Chat Completions 端点。
///
/// 去掉末尾 `/`；若已以 `/v1` 结尾则只补 `/chat/completions`，否则补
/// `/v1/chat/completions`。
pub fn endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        format!("{trimmed}/chat/completions")
    } else {
        format!("{trimmed}/v1/chat/completions")
    }
}

/// 构造 OpenAI Chat Completions 请求体。请求体绝不含 `api_key`。
///
/// 每条消息经 [`ChatMessage::to_request_value`] 组装，是否回传 `reasoning_content`
/// 由 `endpoint_format` 决定。
pub fn build_request(
    cfg: &LlmConfig,
    messages: &[ChatMessage],
    tools: &serde_json::Value,
) -> serde_json::Value {
    let replay_reasoning = cfg.endpoint_format.replays_reasoning();
    let messages: Vec<serde_json::Value> = messages
        .iter()
        .map(|message| message.to_request_value(replay_reasoning))
        .collect();
    serde_json::json!({
        "model": cfg.model,
        "temperature": cfg.temperature,
        "max_tokens": cfg.max_tokens,
        "messages": messages,
        "tools": tools,
    })
}

/// 一次模型回复。
#[derive(Debug, Clone, PartialEq)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<AssistantToolCall>,
    pub finish_reason: Option<String>,
}

/// 解析非流式 Chat Completions 响应体。
pub fn parse_response(body: &serde_json::Value) -> Result<ChatResponse> {
    let choice = body
        .get("choices")
        .and_then(|value| value.get(0))
        .context("模型响应缺少 choices[0]")?;
    let message = choice.get("message").context("模型响应缺少 message")?;

    let content = message
        .get("content")
        .and_then(|value| value.as_str())
        .map(str::to_string);

    // 思考内容优先读 `reasoning_content`，缺失时回退 `reasoning`；两者都非字符串或
    // 都没有则为 `None`。
    let reasoning = message
        .get("reasoning_content")
        .and_then(|value| value.as_str())
        .or_else(|| message.get("reasoning").and_then(|value| value.as_str()))
        .map(str::to_string);

    // `tool_calls` 缺失或为 `null` 视为空列表；一旦是数组，则其中任一元素都必须
    // 成功反序列化，否则报错并指明元素索引——不静默丢弃非法工具调用。
    let tool_calls = match message.get("tool_calls") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(value) => {
            let calls = value.as_array().context("模型响应的 tool_calls 不是数组")?;
            calls
                .iter()
                .enumerate()
                .map(|(index, call)| {
                    serde_json::from_value::<AssistantToolCall>(call.clone())
                        .with_context(|| format!("模型返回的第 {} 个 tool_call 非法", index + 1))
                })
                .collect::<Result<Vec<_>>>()?
        }
    };

    let finish_reason = choice
        .get("finish_reason")
        .and_then(|value| value.as_str())
        .map(str::to_string);

    Ok(ChatResponse {
        content,
        reasoning,
        tool_calls,
        finish_reason,
    })
}

/// 调用模型，返回解析后的回复。
///
/// 对 HTTP 429/5xx 与网络错误最多重试到 3 次总尝试，指数退避 200ms、600ms；
/// 其他 HTTP 状态立即失败，错误信息含状态码与有界响应片段，绝不含 `api_key`。
///
/// `timeout` 覆盖连接与响应体读取；请求超时**不重试**，立即返回错误，避免把
/// 「端点无响应」拖成多轮退避。
pub async fn chat(
    cfg: &LlmConfig,
    messages: &[ChatMessage],
    tools: &serde_json::Value,
    timeout: Duration,
) -> Result<ChatResponse> {
    let url = endpoint(&cfg.base_url);
    let body = build_request(cfg, messages, tools);
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .context("构造 HTTP 客户端失败")?;
    let mut last_error: Option<anyhow::Error> = None;

    for attempt in 0..MAX_ATTEMPTS {
        let response = client
            .post(&url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", cfg.api_key),
            )
            .json(&body)
            .send()
            .await;

        match response {
            Ok(response) if response.status().is_success() => {
                let value: serde_json::Value =
                    response.json().await.context("解析模型响应 JSON 失败")?;
                return parse_response(&value);
            }
            Ok(response) => {
                let status = response.status();
                if !is_retryable(status) {
                    let snippet = bounded_snippet(response.text().await.unwrap_or_default());
                    return Err(anyhow::anyhow!("模型服务返回 {status}：{snippet}"));
                }
                last_error = Some(anyhow::anyhow!("模型服务返回 {status}"));
            }
            // 超时不进入指数退避：端点接受连接却不响应时，重试只会白等更久。
            Err(err) if err.is_timeout() => {
                return Err(anyhow::anyhow!("模型请求超时：{err}"));
            }
            Err(err) => {
                last_error = Some(anyhow::anyhow!("模型请求网络错误：{err}"));
            }
        }

        if attempt + 1 < MAX_ATTEMPTS {
            let delay = BASE_BACKOFF_MS.saturating_mul(3u64.pow(attempt as u32));
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("模型调用失败")))
}

fn is_retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn bounded_snippet(text: String) -> String {
    text.chars().take(ERROR_SNIPPET_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LlmConfig {
        LlmConfig {
            endpoint_format: EndpointFormat::OpenAi,
            base_url: "https://api.example.com".to_string(),
            api_key: "secret".to_string(),
            model: "gpt-test".to_string(),
            temperature: 0.7,
            max_tokens: 4096,
        }
    }

    fn shell_call() -> AssistantToolCall {
        AssistantToolCall {
            id: "call_1".to_string(),
            name: crate::tools::shell::NAME.to_string(),
            arguments: "{\"command\":\"ls\"}".to_string(),
        }
    }

    #[test]
    fn endpoint_format_parses_all_values() {
        for (text, expected) in [
            ("openai", EndpointFormat::OpenAi),
            ("deepseek", EndpointFormat::DeepSeek),
            ("kimi", EndpointFormat::Kimi),
            ("qwen", EndpointFormat::Qwen),
            ("glm", EndpointFormat::Glm),
        ] {
            let cfg = LlmConfig::from_toml(&format!(
                "endpoint_format = \"{text}\"\nbase_url = \"u\"\napi_key = \"k\"\nmodel = \"m\""
            ))
            .unwrap();
            assert_eq!(cfg.endpoint_format, expected, "endpoint_format={text}");
        }
    }

    #[test]
    fn endpoint_format_defaults_to_openai() {
        let cfg = LlmConfig::from_toml("base_url=\"u\"\napi_key=\"k\"\nmodel=\"m\"").unwrap();
        assert_eq!(cfg.endpoint_format, EndpointFormat::OpenAi);
    }

    #[test]
    fn replays_reasoning_only_for_non_openai() {
        assert!(!EndpointFormat::OpenAi.replays_reasoning());
        assert!(EndpointFormat::DeepSeek.replays_reasoning());
        assert!(EndpointFormat::Kimi.replays_reasoning());
        assert!(EndpointFormat::Qwen.replays_reasoning());
        assert!(EndpointFormat::Glm.replays_reasoning());
    }

    #[test]
    fn from_toml_full() {
        let cfg = LlmConfig::from_toml(
            r#"
endpoint_format = "deepseek"
base_url = "https://api.deepseek.com"
api_key = "sk-x"
model = "deepseek-chat"
temperature = 0.2
max_tokens = 1024
"#,
        )
        .unwrap();
        assert_eq!(cfg.endpoint_format, EndpointFormat::DeepSeek);
        assert_eq!(cfg.base_url, "https://api.deepseek.com");
        assert_eq!(cfg.model, "deepseek-chat");
        assert_eq!(cfg.temperature, 0.2);
        assert_eq!(cfg.max_tokens, 1024);
    }

    #[test]
    fn from_toml_defaults() {
        let cfg = LlmConfig::from_toml(
            r#"
base_url = "https://api.example.com"
api_key = "sk-x"
model = "m"
"#,
        )
        .unwrap();
        assert_eq!(cfg.endpoint_format, EndpointFormat::OpenAi);
        assert_eq!(cfg.temperature, 0.7);
        assert_eq!(cfg.max_tokens, 4096);
    }

    #[test]
    fn legacy_provider_field_is_ignored() {
        // `provider` 已不是字段：写了也不影响，仍取默认端点格式。
        let cfg = LlmConfig::from_toml(
            "provider = \"deepseek\"\nbase_url=\"u\"\napi_key=\"k\"\nmodel=\"m\"",
        )
        .unwrap();
        assert_eq!(cfg.endpoint_format, EndpointFormat::OpenAi);
    }

    #[test]
    fn from_toml_missing_field_fails() {
        let err = LlmConfig::from_toml("model = \"m\"").unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("base_url") || text.contains("api_key"));
    }

    #[test]
    fn debug_redacts_api_key() {
        let text = format!("{:?}", sample());
        assert!(text.contains("<redacted>"));
        assert!(!text.contains("secret"));
    }

    #[test]
    fn endpoint_appends_v1() {
        assert_eq!(
            endpoint("https://api.example.com"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            endpoint("https://api.example.com/"),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn endpoint_respects_existing_v1() {
        assert_eq!(
            endpoint("https://api.example.com/v1"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            endpoint("https://api.example.com/v1/"),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn build_request_shape() {
        let messages = vec![ChatMessage::user("hi")];
        let tools = crate::tools::shell::tools();
        let value = build_request(&sample(), &messages, &tools);
        assert_eq!(value["model"], "gpt-test");
        let temperature = value["temperature"].as_f64().unwrap();
        assert!((temperature - 0.7).abs() < 1e-6);
        assert_eq!(value["max_tokens"], 4096);
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(
            value["tools"][0]["function"]["name"],
            crate::tools::shell::NAME
        );
        assert!(value.get("api_key").is_none());
    }

    #[test]
    fn deepseek_replays_empty_reasoning_for_tool_calls() {
        let mut cfg = sample();
        cfg.endpoint_format = EndpointFormat::DeepSeek;
        let messages = vec![ChatMessage::assistant(None, Some(vec![shell_call()]), None)];
        let value = build_request(&cfg, &messages, &serde_json::json!([]));
        assert_eq!(value["messages"][0]["reasoning_content"], "");
        // 带工具调用的 assistant 消息仍保留 tool_calls。
        assert_eq!(value["messages"][0]["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn deepseek_replays_reasoning_when_present() {
        let mut cfg = sample();
        cfg.endpoint_format = EndpointFormat::DeepSeek;
        let messages = vec![ChatMessage::assistant(
            Some("回复".to_string()),
            None,
            Some("思考".to_string()),
        )];
        let value = build_request(&cfg, &messages, &serde_json::json!([]));
        assert_eq!(value["messages"][0]["reasoning_content"], "思考");
    }

    #[test]
    fn openai_omits_reasoning_content() {
        let messages = vec![ChatMessage::assistant(None, Some(vec![shell_call()]), None)];
        let value = build_request(&sample(), &messages, &serde_json::json!([]));
        assert!(value["messages"][0].get("reasoning_content").is_none());
    }

    #[test]
    fn parse_content_only() {
        let body = serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "你好"},
                "finish_reason": "stop"
            }]
        });
        let response = parse_response(&body).unwrap();
        assert_eq!(response.content.as_deref(), Some("你好"));
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn parse_reasoning_content() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "答案",
                    "reasoning_content": "先想一下"
                },
                "finish_reason": "stop"
            }]
        });
        let response = parse_response(&body).unwrap();
        assert_eq!(response.reasoning.as_deref(), Some("先想一下"));
    }

    #[test]
    fn parse_reasoning_fallback_field() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "答案",
                    "reasoning": "回退思考"
                },
                "finish_reason": "stop"
            }]
        });
        let response = parse_response(&body).unwrap();
        assert_eq!(response.reasoning.as_deref(), Some("回退思考"));
    }

    #[test]
    fn parse_reasoning_missing_or_non_string_is_none() {
        let missing = serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "答案"},
                "finish_reason": "stop"
            }]
        });
        assert_eq!(parse_response(&missing).unwrap().reasoning, None);

        let non_string = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "答案",
                    "reasoning_content": {"nested": true}
                },
                "finish_reason": "stop"
            }]
        });
        assert_eq!(parse_response(&non_string).unwrap().reasoning, None);
    }

    #[test]
    fn parse_tool_calls() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "shell", "arguments": "{\"command\":\"ls\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let response = parse_response(&body).unwrap();
        assert!(response.content.is_none());
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "shell");
        assert_eq!(response.tool_calls[0].arguments, "{\"command\":\"ls\"}");
    }

    #[test]
    fn parse_tool_call_with_object_arguments_fails() {
        // 模型偶尔把 `arguments` 返回为 JSON 对象而非字符串：不得静默丢弃。
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "shell", "arguments": {"command": "ls"}}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let err = parse_response(&body).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains('1'), "错误信息应指明元素索引：{text}");
    }

    #[test]
    fn parse_tool_call_missing_id_or_function_fails() {
        let missing_id = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "type": "function",
                        "function": {"name": "shell", "arguments": "{\"command\":\"ls\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        assert!(parse_response(&missing_id).is_err());

        let missing_function = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{"id": "call_1", "type": "function"}]
                },
                "finish_reason": "tool_calls"
            }]
        });
        assert!(parse_response(&missing_function).is_err());
    }

    #[test]
    fn parse_tool_calls_valid_still_parses() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "shell", "arguments": "{\"command\":\"ls\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let response = parse_response(&body).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_1");
        assert_eq!(response.tool_calls[0].name, "shell");
    }

    #[test]
    fn parse_tool_calls_missing_or_null_is_empty() {
        let missing = serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "你好"},
                "finish_reason": "stop"
            }]
        });
        assert!(parse_response(&missing).unwrap().tool_calls.is_empty());

        let null = serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "你好", "tool_calls": null},
                "finish_reason": "stop"
            }]
        });
        assert!(parse_response(&null).unwrap().tool_calls.is_empty());
    }

    #[test]
    fn parse_malformed_fails() {
        assert!(parse_response(&serde_json::json!({})).is_err());
        assert!(parse_response(&serde_json::json!({"choices": []})).is_err());
        assert!(
            parse_response(&serde_json::json!({"choices": [{"finish_reason": "stop"}]})).is_err()
        );
    }

    #[tokio::test]
    async fn request_timeout_returns_within_bounds() {
        use std::io::Read;

        // 接受连接后既不读也不写，保持打开，模拟「端点接受连接但不响应」。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                // 连接保持打开直到超时后测试结束；期间不产生任何响应。
                let mut buffer = [0u8; 1024];
                let _ = socket.read(&mut buffer);
                std::thread::sleep(Duration::from_secs(30));
            }
        });

        let mut cfg = sample();
        cfg.base_url = format!("http://{addr}");
        let start = std::time::Instant::now();
        let result = chat(
            &cfg,
            &[ChatMessage::user("hi")],
            &serde_json::json!([]),
            Duration::from_millis(200),
        )
        .await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "无响应端点应在超时后返回错误：{result:?}");
        assert!(
            elapsed >= Duration::from_millis(200),
            "应真的等待到超时：{elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "不应无限挂起：{elapsed:?}"
        );
    }
}
