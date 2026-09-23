//! 阶段 1 的 agent loop。
//!
//! 流程（见 `docs/plan.md`「Agent loop 与并发形状」）：
//!
//! 1. 先追加用户记录：配置/钩子失败也不丢用户输入；
//! 2. 每会话一次执行两个提示词钩子，成功的缓存按 `(名称，指纹，成功)` 判定；
//! 3. 每次运行一次执行 `llm` 钩子，解析配置并只记一条 redacted 审计记录（stdout
//!    含密钥，绝不落盘、绝不渲染）；
//! 4. 循环：重读历史 → 渲染 → 调用模型 → 原子追加 `ai` 及其 `tool_call` → 执行
//!    工具并追加 `tool_result`，直到无工具调用或达到 `max_steps`。
//!
//! 退出码：`0` 成功，`1` 通用错误，`2` 用法错误，`3` 配置/钩子错误，`4` 模型错误，
//! `5` 达到 `max_steps`。`run` 只在非预期 I/O 失败时返回 `Err`。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use yaa_msg::{HookInfo, Record, ToolOrigin};

use crate::config::Config;
use crate::events::{Event, EventSink};
use crate::llm::{self, ChatResponse, LlmConfig};
use crate::message::{AssistantToolCall, ChatMessage};
use crate::render;
use crate::session;

/// 进程退出码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    /// 成功。
    Success,
    /// 用法错误。
    Usage,
    /// 配置或钩子错误。
    Config,
    /// 模型错误。
    Model,
    /// 达到最大步数。
    MaxSteps,
    /// 通用错误。
    Error,
}

impl ExitCode {
    /// 映射到进程退出码。
    pub fn code(self) -> i32 {
        match self {
            ExitCode::Success => 0,
            ExitCode::Usage => 2,
            ExitCode::Config => 3,
            ExitCode::Model => 4,
            ExitCode::MaxSteps => 5,
            ExitCode::Error => 1,
        }
    }
}

/// 模型抽象。
///
/// 阶段 1 只需要一次「请求→回复」，不引入流式与重试抽象（重试在 [`llm::chat`] 内），
/// 因此保持最小形状并兼容 `dyn`。
pub trait Model: Send + Sync {
    /// 发送一次对话请求。
    fn chat<'a>(
        &'a self,
        messages: &'a [ChatMessage],
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ChatResponse>> + Send + 'a>>;

    /// 记录使用的模型名；默认空串（测试替身无需实现）。
    fn name(&self) -> &str {
        ""
    }
}

/// 真实的 OpenAI 兼容模型。
pub struct OpenAiModel {
    /// 供应配置。
    pub cfg: LlmConfig,
}

impl Model for OpenAiModel {
    fn chat<'a>(
        &'a self,
        messages: &'a [ChatMessage],
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ChatResponse>> + Send + 'a>> {
        Box::pin(async move {
            let tools = crate::prompt::shell_tools();
            llm::chat(&self.cfg, messages, &tools).await
        })
    }

    fn name(&self) -> &str {
        &self.cfg.model
    }
}

/// agent loop 的运行上下文。
pub struct Runner<'a> {
    /// 策略与限额配置。
    pub config: &'a Config,
    /// 配置根（钩子环境 `GLB_CFG` / `YAA_CONFIG_HOME`）。
    pub config_root: &'a Path,
    /// 数据根（`output_files` 相对路径的基准）。
    pub data_root: &'a Path,
    /// 钩子与 Shell 的工作目录。
    pub cwd: &'a Path,
    /// 已解析的会话。
    pub session: yaa_store::Session,
    /// 会话解析时是否因 `canonicalize` 失败而回退。
    pub cwd_warned: bool,
    /// 事件上报器。
    pub sink: &'a dyn EventSink,
}

impl Runner<'_> {
    /// 运行一个回合。
    ///
    /// `make_model` 在 `llm` 钩子解析成功后恰好被调用一次，模型由本次循环独占，
    /// 从而保证 `llm` 钩子每次 `run` 只执行一次（D8）。
    pub async fn run<M, F>(&self, prompt_text: &str, make_model: F) -> anyhow::Result<ExitCode>
    where
        F: FnOnce(LlmConfig) -> M,
        M: Model,
    {
        if self.cwd_warned {
            self.sink.emit(Event::Notice {
                content: "警告：工作目录无法规范化，已回退到词法绝对路径计算会话 id。".to_string(),
            });
        }

        // 1. 用户记录先落盘：配置/钩子失败也不能丢失用户输入。
        yaa_store::append(&self.session, &[Record::user(prompt_text)])?;

        // 2. 提示词钩子（每会话一次）。
        if let Some(exit) = self.ensure_prompt_hooks().await? {
            return Ok(exit);
        }

        // 3. llm 钩子（每次运行一次，只记 redacted 审计），并用解析出的配置构造模型。
        let llm = match self.resolve_llm_hook().await? {
            Ok(config) => config,
            Err(code) => return Ok(code),
        };
        let model = make_model(llm);

        // 4. 循环。
        let schema_hash = crate::prompt::tool_schema_hash();
        for _ in 0..self.config.agent.max_steps {
            let records = yaa_store::load(&self.session)?;
            let messages = render::render(&records);

            let response = match model.chat(&messages).await {
                Ok(response) => response,
                Err(err) => {
                    self.sink.emit(Event::Notice {
                        content: format!("模型调用失败：{err:#}"),
                    });
                    return Ok(ExitCode::Model);
                }
            };
            self.sink.emit(Event::Ai {
                content: response.content.clone().unwrap_or_default(),
                reasoning: response.reasoning.clone(),
                // `final_step` 用**原始**工具调用判断，避免清洗结果影响「是否末步」。
                final_step: response.tool_calls.is_empty(),
            });

            // 既无文本也无工具调用：不算错误（退出码仍为 0），但要明确告知用户，
            // 否则 CLI 会静默结束，用户误以为卡死。
            if response.tool_calls.is_empty()
                && response.content.as_deref().is_none_or(str::is_empty)
            {
                self.sink.emit(Event::Notice {
                    content: "模型返回了空回复（既无文本也无工具调用）。".to_string(),
                });
            }

            // 清洗模型返回的 call_id：空串、批次内重复、或与历史冲突者一律重生成，
            // 否则 OpenAI 会因重复/空 `tool_call_id` 拒绝请求。
            let tool_calls = sanitize_tool_calls(&records, &response.tool_calls);

            // `ai` 与其 `tool_call` 以单批原子追加。
            let mut batch = Vec::with_capacity(1 + tool_calls.len());
            batch.push(Record::ai(
                model.name(),
                schema_hash.clone(),
                response.content.clone().unwrap_or_default(),
            ));
            for call in &tool_calls {
                batch.push(Record::tool_call(
                    ToolOrigin::Ai,
                    call.id.clone(),
                    persisted_arguments(&call.arguments),
                    None,
                    None,
                    false,
                ));
            }
            yaa_store::append(&self.session, &batch)?;

            if tool_calls.is_empty() {
                return Ok(ExitCode::Success);
            }

            for call in &tool_calls {
                let result = self.execute_tool_call(call).await;
                yaa_store::append(&self.session, &[result])?;
            }
        }

        // 达到步数上限不在此发事件：`ExitCode::MaxSteps` 已是明确信号，具体警告与
        // 「是否仍打印部分回复」的显示策略完全由 CLI 决定（避免 stderr 重复告警）。
        Ok(ExitCode::MaxSteps)
    }

    /// 执行两个提示词钩子；缓存命中则跳过。钩子失败返回 `Some(ExitCode::Config)`。
    async fn ensure_prompt_hooks(&self) -> anyhow::Result<Option<ExitCode>> {
        let env = self.hook_env();
        let context = self.hook_context();
        for (name, command) in [
            ("system_prompt", self.config.hooks.system_prompt.as_str()),
            ("user_prompt", self.config.hooks.user_prompt.as_str()),
        ] {
            let fingerprint = crate::hook::fingerprint(name, command, &context);
            if prompt_hook_cached(&yaa_store::load(&self.session)?, name, &fingerprint) {
                continue;
            }

            self.sink.emit(Event::HookStarted {
                name: name.to_string(),
                command: command.to_string(),
            });
            // 钩子执行失败（spawn/IO/超限）是配置问题：报事件并按退出码 3 处理，
            // 而不是作为非预期 `Err` 冒泡成通用错误 1。
            let outcome =
                match crate::hook::run(command, self.cwd, &env, self.config.hooks.timeout_secs)
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        self.sink.emit(Event::Notice {
                            content: format!("执行提示词钩子 {name} 失败：{err}"),
                        });
                        return Ok(Some(ExitCode::Config));
                    }
                };

            // 成功与否都成对写入：失败不写成功标记，下次运行按指纹重试。
            let call_id = format!("hook-{name}-{}", ulid::Ulid::new());
            let call = Record::tool_call(
                ToolOrigin::Hook,
                call_id.clone(),
                command,
                None,
                Some(HookInfo {
                    name: name.to_string(),
                    fingerprint,
                }),
                false,
            );
            let result = Record::tool_result(
                call_id,
                outcome.stdout,
                outcome.stderr,
                outcome.code,
                false,
                false,
                None,
            );
            yaa_store::append(&self.session, &[call, result])?;

            if outcome.code != 0 {
                self.sink.emit(Event::Notice {
                    content: format!("提示词钩子 {name} 以退出码 {} 失败。", outcome.code),
                });
                return Ok(Some(ExitCode::Config));
            }
        }
        Ok(None)
    }

    /// 执行 `llm` 钩子：解析配置并写一条 redacted 审计记录。
    ///
    /// 成功返回 `Ok(Ok(config))`；钩子失败、退出码非零或 TOML 非法时返回
    /// `Ok(Err(ExitCode::Config))`，绝不返回 `Err`（除非写审计记录发生非预期 I/O）。
    async fn resolve_llm_hook(&self) -> anyhow::Result<Result<LlmConfig, ExitCode>> {
        let name = "llm";
        let command = self.config.hooks.llm.as_str();
        let fingerprint = crate::hook::fingerprint(name, command, &self.hook_context());
        let env = self.hook_env();

        self.sink.emit(Event::HookStarted {
            name: name.to_string(),
            command: command.to_string(),
        });
        let outcome =
            match crate::hook::run(command, self.cwd, &env, self.config.hooks.timeout_secs).await {
                Ok(outcome) => outcome,
                Err(err) => {
                    self.sink.emit(Event::Notice {
                        content: format!("执行 llm 钩子失败：{err}"),
                    });
                    return Ok(Err(ExitCode::Config));
                }
            };
        // 只记指纹，命令与 stdout 都可能内联密钥，一律不落盘；也不配 tool_result，
        // 渲染器永不消费它。
        let audit = Record::tool_call(
            ToolOrigin::Hook,
            format!("hook-llm-{}", ulid::Ulid::new()),
            "",
            None,
            Some(HookInfo {
                name: name.to_string(),
                fingerprint,
            }),
            true,
        );
        yaa_store::append(&self.session, &[audit])?;

        if outcome.code != 0 {
            self.sink.emit(Event::Notice {
                content: format!("llm 钩子以退出码 {} 失败。", outcome.code),
            });
            return Ok(Err(ExitCode::Config));
        }
        match LlmConfig::from_toml(&outcome.stdout) {
            Ok(config) => Ok(Ok(config)),
            Err(err) => {
                self.sink.emit(Event::Notice {
                    content: format!("解析 llm 钩子输出失败：{err:#}"),
                });
                Ok(Err(ExitCode::Config))
            }
        }
    }

    /// 解析并执行一次模型工具调用，返回 `tool_result` 记录（错误也编码为记录）。
    async fn execute_tool_call(&self, call: &AssistantToolCall) -> Record {
        // 先解析参数，解析完成后才上报一次 `ToolStarted`：
        // - shell 且 JSON 合法且有字符串 `command`：命令用解析值，描述取非空 `description`；
        // - 其余情况（非 shell、JSON 非法、缺 `command`）：命令回退为原始参数，描述置空。
        let (parsed, parse_error) = if call.name == "shell" {
            match serde_json::from_str::<serde_json::Value>(&call.arguments) {
                Ok(value) => (Some(value), None),
                Err(err) => (None, Some(err)),
            }
        } else {
            (None, None)
        };
        let command = parsed
            .as_ref()
            .and_then(|value| value.get("command"))
            .and_then(|value| value.as_str());
        // 只在命令可用时才展示描述，避免「缺 command」的失败调用仍带出描述。
        let description = command.and_then(|_| {
            parsed
                .as_ref()
                .and_then(|value| value.get("description"))
                .and_then(|value| value.as_str())
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        });

        self.sink.emit(Event::ToolStarted {
            call_id: call.id.clone(),
            command: command.unwrap_or(call.arguments.as_str()).to_string(),
            description,
        });

        if call.name != "shell" {
            let message = format!("不支持的工具：{}（阶段 1 仅有 shell）。", call.name);
            return self.finish_tool_call(call, -1, false, false, message);
        }

        let Some(command) = command else {
            let message = match &parse_error {
                Some(err) => format!("工具参数不是合法 JSON：{err}"),
                None => "工具参数缺少字符串字段 `command`。".to_string(),
            };
            return self.finish_tool_call(call, -1, false, false, message);
        };

        let shell_cmd = yaa_exec::ShellCommand {
            call_id: call.id.clone(),
            command: command.to_string(),
        };
        let limits = self.config.shell.to_limits();
        let output_dir = session::outputs_dir(&self.session);

        match yaa_exec::run(&shell_cmd, &limits, &output_dir).await {
            Ok(output) => {
                self.sink.emit(Event::ToolFinished {
                    call_id: output.call_id.clone(),
                    code: output.code,
                    timed_out: output.timed_out,
                    truncated: output.truncated,
                    stdout: output.stdout.clone(),
                    stderr: output.stderr.clone(),
                });
                shell_output_to_record(&output, self.data_root)
            }
            Err(err) => {
                let message = format!("Shell 执行失败：{err}");
                self.finish_tool_call(call, -1, false, false, message)
            }
        }
    }

    /// 生成一个失败 `tool_result` 并上报 `ToolFinished`。
    fn finish_tool_call(
        &self,
        call: &AssistantToolCall,
        code: i32,
        timed_out: bool,
        truncated: bool,
        stderr: String,
    ) -> Record {
        self.sink.emit(Event::ToolFinished {
            call_id: call.id.clone(),
            code,
            timed_out,
            truncated,
            stdout: String::new(),
            stderr: stderr.clone(),
        });
        Record::tool_result(
            call.id.clone(),
            "",
            stderr,
            code,
            timed_out,
            truncated,
            None,
        )
    }

    /// 钩子进程的环境变量。
    fn hook_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        let config = self.config_root.to_string_lossy().into_owned();
        env.insert("GLB_CFG".to_string(), config.clone());
        env.insert("YAA_CONFIG_HOME".to_string(), config);
        env.insert(
            "YAA_DATA_HOME".to_string(),
            self.data_root.to_string_lossy().into_owned(),
        );
        // 当前可执行文件路径，供默认提示词钩子不依赖 PATH 地调用自身；取不到则省略。
        if let Ok(exe) = std::env::current_exe() {
            env.insert("YAA_BIN".to_string(), exe.to_string_lossy().into_owned());
        }
        env
    }

    /// 钩子指纹的稳定执行上下文：工作目录、配置根、数据根，以 NUL 无歧义连接。
    fn hook_context(&self) -> String {
        format!(
            "{}\u{0}{}\u{0}{}",
            self.cwd.display(),
            self.config_root.display(),
            self.data_root.display()
        )
    }
}

/// 清洗模型返回的工具调用 id。
///
/// 空串、批次内重复、或与历史任何 `call_id` 冲突的 id 都替换为本地生成的
/// `call-<ULID>`，保证渲染出的 `tool_call_id` 唯一且非空。
fn sanitize_tool_calls(history: &[Record], calls: &[AssistantToolCall]) -> Vec<AssistantToolCall> {
    let mut used: HashSet<String> = history
        .iter()
        .filter_map(|record| match record {
            Record::ToolCall(call) => Some(call.call_id.clone()),
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

/// 缓存判定：存在非 redacted、名称与指纹都匹配、且有 `code == 0` 配对结果的钩子调用。
///
/// 先一次建表 `call_id → tool_result`，把逐候选的全表扫描降为 `O(n)` 查询。
fn prompt_hook_cached(records: &[Record], name: &str, fingerprint: &str) -> bool {
    let mut results: HashMap<&str, &yaa_msg::ToolResult> = HashMap::new();
    for record in records {
        if let Record::ToolResult(result) = record {
            results.entry(result.call_id.as_str()).or_insert(result);
        }
    }
    records.iter().any(|record| {
        let Record::ToolCall(call) = record else {
            return false;
        };
        if call.origin != ToolOrigin::Hook || call.redacted {
            return false;
        }
        let Some(hook) = &call.hook else {
            return false;
        };
        hook.name == name
            && hook.fingerprint == fingerprint
            && results
                .get(call.call_id.as_str())
                .is_some_and(|result| result.code == 0)
    })
}

/// 模型参数持久化前的清洗：仅当能解析为 JSON 对象时原样保存，否则存 `{}`。
///
/// 非法文本若原样回灌，后续每次请求都会携带非法 `function.arguments`，严格供应商会
/// 直接拒绝；失败原因已由配对的 `tool_result` 告知模型。
fn persisted_arguments(arguments: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(value) if value.is_object() => arguments.to_string(),
        _ => "{}".to_string(),
    }
}

/// 把 Shell 输出转成 `tool_result` 记录，`output_files` 相对数据根。
fn shell_output_to_record(output: &yaa_exec::ShellOutput, data_root: &Path) -> Record {
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

/// 把绝对路径相对化到数据根；不在数据根下时退化为绝对路径字符串。
///
/// `yaa_exec` 返回的是 `canonicalize` 后的路径，而 `data_root` 可能是未规范化的
/// 符号链接目录，因此先按原值匹配，再退回规范化后的数据根（`canonicalize` 失败则
/// 忽略）。两者都不匹配时（如 `--session` 指向数据根之外）保留绝对路径，绝不返回
/// `None`——否则 `truncated` 为真时模型会被误导去一个不存在的相对文件找完整输出。
fn relativize(path: &Path, data_root: &Path) -> String {
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
    use crate::events::NullSink;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// `llm` 钩子：从配置文件读取 TOML，命令本身不含密钥（密钥只出现在 stdout）。
    const LLM_HOOK: &str = "cat $GLB_CFG/llm.toml";
    /// `llm` 钩子输出的 TOML，含用于验证「绝不落盘」的密钥。
    const LLM_TOML: &str =
        "base_url = \"http://localhost\"\napi_key = \"sk-secret\"\nmodel = \"fake-model\"\n";

    /// 按脚本返回回复、并记录每次请求消息的测试模型。
    ///
    /// 内部状态经 `Arc` 共享，克隆体与调用方保留的句柄看到同一份脚本与请求记录。
    #[derive(Clone)]
    struct FakeModel {
        responses: Arc<Mutex<VecDeque<ChatResponse>>>,
        seen: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    }

    impl FakeModel {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into())),
                seen: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn seen(&self) -> Vec<Vec<ChatMessage>> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Model for FakeModel {
        fn chat<'a>(
            &'a self,
            messages: &'a [ChatMessage],
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<ChatResponse>> + Send + 'a>> {
            self.seen.lock().unwrap().push(messages.to_vec());
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("测试模型脚本已耗尽");
            Box::pin(async move { Ok(response) })
        }
    }

    fn text_response(content: &str) -> ChatResponse {
        ChatResponse {
            content: Some(content.to_string()),
            reasoning: None,
            tool_calls: Vec::new(),
            finish_reason: Some("stop".to_string()),
        }
    }

    fn call_response(call_id: &str, arguments: &str) -> ChatResponse {
        ChatResponse {
            content: None,
            reasoning: None,
            tool_calls: vec![AssistantToolCall {
                id: call_id.to_string(),
                name: "shell".to_string(),
                arguments: arguments.to_string(),
            }],
            finish_reason: Some("tool_calls".to_string()),
        }
    }

    fn calls_response(calls: &[(&str, &str)]) -> ChatResponse {
        ChatResponse {
            content: None,
            reasoning: None,
            tool_calls: calls
                .iter()
                .map(|(id, arguments)| AssistantToolCall {
                    id: (*id).to_string(),
                    name: "shell".to_string(),
                    arguments: (*arguments).to_string(),
                })
                .collect(),
            finish_reason: Some("tool_calls".to_string()),
        }
    }

    fn test_config(system_prompt: &str) -> Config {
        let mut config = Config::default();
        config.hooks.system_prompt = system_prompt.to_string();
        config.hooks.user_prompt = "echo USR".to_string();
        config.hooks.llm = LLM_HOOK.to_string();
        config
    }

    struct Harness {
        _dir: tempfile::TempDir,
        config: Config,
        config_root: PathBuf,
        data_root: PathBuf,
        cwd: PathBuf,
        session: yaa_store::Session,
    }

    impl Harness {
        fn new(config: Config) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let config_root = dir.path().join("config");
            let data_root = dir.path().join("data");
            let cwd = dir.path().join("work");
            std::fs::create_dir_all(&config_root).unwrap();
            std::fs::create_dir_all(&data_root).unwrap();
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::write(config_root.join("llm.toml"), LLM_TOML).unwrap();
            let session = session::resolve(&cwd, None, &data_root).unwrap().session;
            Self {
                _dir: dir,
                config,
                config_root,
                data_root,
                cwd,
                session,
            }
        }

        fn runner<'a>(&'a self, sink: &'a dyn EventSink) -> Runner<'a> {
            Runner {
                config: &self.config,
                config_root: &self.config_root,
                data_root: &self.data_root,
                cwd: &self.cwd,
                session: self.session.clone(),
                cwd_warned: false,
                sink,
            }
        }

        fn records(&self) -> Vec<Record> {
            yaa_store::load(&self.session).unwrap()
        }

        fn jsonl(&self) -> String {
            std::fs::read_to_string(&self.session.file).unwrap_or_default()
        }
    }

    fn record_label(record: &Record) -> &'static str {
        match record {
            Record::User(_) => "user",
            Record::Ai(_) => "ai",
            Record::ToolCall(_) => "tool_call",
            Record::ToolResult(_) => "tool_result",
            Record::System(_) => "system",
        }
    }

    fn count_hook_calls(records: &[Record], name: &str) -> usize {
        records
            .iter()
            .filter(|record| {
                matches!(record, Record::ToolCall(call) if call.hook.as_ref().is_some_and(|hook| hook.name == name))
            })
            .count()
    }

    #[test]
    fn exit_code_mapping() {
        assert_eq!(ExitCode::Success.code(), 0);
        assert_eq!(ExitCode::Error.code(), 1);
        assert_eq!(ExitCode::Usage.code(), 2);
        assert_eq!(ExitCode::Config.code(), 3);
        assert_eq!(ExitCode::Model.code(), 4);
        assert_eq!(ExitCode::MaxSteps.code(), 5);
    }

    #[tokio::test]
    async fn one_shell_tool_call_runs_and_records() {
        let harness = Harness::new(test_config("echo SYS"));
        let sink = NullSink;
        let model = FakeModel::new(vec![
            call_response("call_1", r#"{"command":"echo hi"}"#),
            text_response("完成"),
        ]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success);

        let records = harness.records();
        let labels: Vec<&str> = records.iter().map(record_label).collect();
        assert_eq!(
            labels,
            vec![
                "user",
                "tool_call",
                "tool_result",
                "tool_call",
                "tool_result",
                "tool_call",
                "ai",
                "tool_call",
                "tool_result",
                "ai",
            ]
        );

        let ai_call = records
            .iter()
            .find_map(|record| match record {
                Record::ToolCall(call) if call.origin == ToolOrigin::Ai => Some(call),
                _ => None,
            })
            .expect("应有一个 ai 工具调用");
        assert_eq!(ai_call.command, r#"{"command":"echo hi"}"#);

        let result = records
            .iter()
            .find_map(|record| match record {
                Record::ToolResult(result) if result.call_id == ai_call.call_id => Some(result),
                _ => None,
            })
            .expect("应有配对的工具结果");
        assert_eq!(result.stdout, "hi\n");
        assert_eq!(result.code, 0);
        assert!(!result.timed_out);
    }

    #[tokio::test]
    async fn second_run_reuses_prompt_hooks_and_sees_history() {
        let harness = Harness::new(test_config("echo SYS"));
        let sink = NullSink;

        let first = FakeModel::new(vec![text_response("第一次")]);
        harness
            .runner(&sink)
            .run("你好", |_config| first.clone())
            .await
            .unwrap();

        let second = FakeModel::new(vec![text_response("第二次")]);
        harness
            .runner(&sink)
            .run("继续", |_config| second.clone())
            .await
            .unwrap();

        let records = harness.records();
        assert_eq!(count_hook_calls(&records, "system_prompt"), 1);
        assert_eq!(count_hook_calls(&records, "user_prompt"), 1);

        let seen = second.seen();
        assert!(!seen.is_empty(), "第二次运行应调用模型");
        let request = &seen[0];
        assert!(request.iter().any(|message| {
            message.role == "user" && message.content.as_deref() == Some("你好")
        }));
        assert!(request.iter().any(|message| message.role == "assistant"));
    }

    #[tokio::test]
    async fn changing_system_prompt_hook_command_invalidates_cache() {
        let mut harness = Harness::new(test_config("echo SYS"));
        let sink = NullSink;

        let first = FakeModel::new(vec![text_response("a")]);
        harness
            .runner(&sink)
            .run("你好", |_config| first.clone())
            .await
            .unwrap();
        assert_eq!(count_hook_calls(&harness.records(), "system_prompt"), 1);

        harness.config.hooks.system_prompt = "echo SYS2".to_string();
        let second = FakeModel::new(vec![text_response("b")]);
        harness
            .runner(&sink)
            .run("你好", |_config| second.clone())
            .await
            .unwrap();
        assert_eq!(count_hook_calls(&harness.records(), "system_prompt"), 2);
    }

    #[tokio::test]
    async fn reply_without_tool_calls_returns_success() {
        let harness = Harness::new(test_config("echo SYS"));
        let sink = NullSink;
        let model = FakeModel::new(vec![text_response("你好")]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success);
    }

    /// 记录事件的测试接收器。
    struct RecordingSink {
        events: Mutex<Vec<Event>>,
    }

    impl EventSink for RecordingSink {
        fn emit(&self, event: Event) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[tokio::test]
    async fn empty_reply_emits_notice_and_renders_no_assistant() {
        let harness = Harness::new(test_config("echo SYS"));
        let sink = RecordingSink {
            events: Mutex::new(Vec::new()),
        };
        let model = FakeModel::new(vec![ChatResponse {
            content: None,
            reasoning: None,
            tool_calls: Vec::new(),
            finish_reason: Some("stop".to_string()),
        }]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success, "空回复仍算成功");

        let noticed =
            sink.events.lock().unwrap().iter().any(
                |event| matches!(event, Event::Notice { content } if content.contains("空回复")),
            );
        assert!(noticed, "空回复应发出 Notice");

        let rendered = render::render(&harness.records());
        assert!(
            !rendered.iter().any(|message| message.role == "assistant"),
            "空 ai 记录不得渲染为 assistant 消息：{rendered:?}"
        );
    }

    #[tokio::test]
    async fn invalid_arguments_yield_code_minus_one_and_continue() {
        let harness = Harness::new(test_config("echo SYS"));
        let sink = NullSink;
        let model = FakeModel::new(vec![
            call_response("call_bad", "not json"),
            text_response("继续完成"),
        ]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success);

        let records = harness.records();
        let result = records
            .iter()
            .find_map(|record| match record {
                Record::ToolResult(result) if result.call_id == "call_bad" => Some(result),
                _ => None,
            })
            .expect("非法参数应产生 tool_result");
        assert_eq!(result.code, -1);
        assert!(!result.stderr.is_empty());

        // 非法参数不得原样落盘：持久化的是合法 JSON 对象，否则后续请求会被严格供应商拒绝。
        let bad_call = records
            .iter()
            .find_map(|record| match record {
                Record::ToolCall(call) if call.call_id == "call_bad" => Some(call),
                _ => None,
            })
            .expect("非法参数也应有 tool_call 记录");
        assert_eq!(bad_call.command, "{}", "非法参数应清洗为 `{{}}`");
        let persisted: serde_json::Value =
            serde_json::from_str(&bad_call.command).expect("持久化参数必须是合法 JSON");
        assert!(persisted.is_object());

        let ai_count = records
            .iter()
            .filter(|record| matches!(record, Record::Ai(_)))
            .count();
        assert_eq!(ai_count, 2, "循环应继续并收到第二次模型回复");
    }

    #[tokio::test]
    async fn llm_hook_is_redacted_and_secret_never_on_disk() {
        let harness = Harness::new(test_config("echo SYS"));
        let sink = NullSink;
        let model = FakeModel::new(vec![text_response("好")]);

        harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();

        let records = harness.records();
        let llm_calls: Vec<_> = records
            .iter()
            .filter_map(|record| match record {
                Record::ToolCall(call)
                    if call.hook.as_ref().is_some_and(|hook| hook.name == "llm") =>
                {
                    Some(call)
                }
                _ => None,
            })
            .collect();
        assert_eq!(llm_calls.len(), 1);
        assert!(llm_calls[0].redacted);
        assert_eq!(
            llm_calls[0].command, "",
            "llm 审计记录不得保存可能内联密钥的命令"
        );
        assert_eq!(llm_calls[0].hook.as_ref().unwrap().fingerprint.len(), 64);
        assert!(
            !records.iter().any(|record| matches!(
                record,
                Record::ToolResult(result) if result.call_id == llm_calls[0].call_id
            )),
            "llm 钩子不应有 tool_result"
        );

        let jsonl = harness.jsonl();
        assert!(!jsonl.contains("sk-secret"));
        assert!(!jsonl.contains("api_key"));
    }

    #[tokio::test]
    async fn failing_system_prompt_hook_exits_config() {
        let harness = Harness::new(test_config("echo SYS >&2; exit 7"));
        let sink = NullSink;
        let model = FakeModel::new(vec![text_response("不该到达")]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Config);

        // 失败也写入成对记录，但不写成功标记，故下次会重跑。
        let records = harness.records();
        assert_eq!(count_hook_calls(&records, "system_prompt"), 1);
    }

    #[tokio::test]
    async fn max_steps_exhaustion_returns_five() {
        let mut harness = Harness::new(test_config("echo SYS"));
        harness.config.agent.max_steps = 1;
        let sink = NullSink;
        let model = FakeModel::new(vec![
            call_response("call_1", r#"{"command":"echo hi"}"#),
            text_response("不应到达"),
        ]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::MaxSteps);
    }

    #[tokio::test]
    async fn changing_cwd_invalidates_prompt_hook_cache() {
        let mut harness = Harness::new(test_config("echo SYS"));
        let sink = NullSink;

        let first = FakeModel::new(vec![text_response("a")]);
        harness
            .runner(&sink)
            .run("你好", |_config| first.clone())
            .await
            .unwrap();
        assert_eq!(count_hook_calls(&harness.records(), "system_prompt"), 1);
        assert_eq!(count_hook_calls(&harness.records(), "user_prompt"), 1);

        // 换个工作目录续用同一会话：指纹必须变化，提示词钩子重跑。
        let new_cwd = harness._dir.path().join("work2");
        std::fs::create_dir_all(&new_cwd).unwrap();
        harness.cwd = new_cwd;

        let second = FakeModel::new(vec![text_response("b")]);
        harness
            .runner(&sink)
            .run("你好", |_config| second.clone())
            .await
            .unwrap();
        assert_eq!(count_hook_calls(&harness.records(), "system_prompt"), 2);
        assert_eq!(count_hook_calls(&harness.records(), "user_prompt"), 2);
    }

    #[tokio::test]
    async fn failing_prompt_hook_preserves_user_record() {
        // 钩子以退出码 3 失败：用户输入仍应先落盘。
        let harness = Harness::new(test_config("exit 3"));
        let sink = NullSink;
        let model = FakeModel::new(vec![text_response("不该到达")]);

        let exit = harness
            .runner(&sink)
            .run("重要输入", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Config);

        let records = harness.records();
        assert_eq!(
            records.first().map(record_label),
            Some("user"),
            "用户记录应在钩子之前落盘"
        );
        let Record::User(user) = &records[0] else {
            panic!("首条应为用户记录");
        };
        assert_eq!(user.content, "重要输入");
    }

    #[tokio::test]
    async fn hook_execution_failure_returns_config_not_error() {
        // 钩子输出 2 MiB 超过硬上限：属配置/钩子错误（退出码 3），不是 Err（退出码 1）。
        let harness = Harness::new(test_config("head -c 2097152 /dev/zero"));
        let sink = NullSink;
        let model = FakeModel::new(vec![text_response("不该到达")]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Config);
        assert!(
            harness
                .records()
                .iter()
                .any(|record| matches!(record, Record::User(_))),
            "即使钩子执行失败，用户记录也应已落盘"
        );
    }

    #[tokio::test]
    async fn duplicate_and_empty_tool_call_ids_are_sanitized() {
        let harness = Harness::new(test_config("echo SYS"));
        let sink = NullSink;
        let model = FakeModel::new(vec![
            calls_response(&[
                ("dup", r#"{"command":"echo a"}"#),
                ("dup", r#"{"command":"echo b"}"#),
                ("", r#"{"command":"echo c"}"#),
            ]),
            text_response("完成"),
        ]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success);

        let records = harness.records();
        let ai_ids: Vec<String> = records
            .iter()
            .filter_map(|record| match record {
                Record::ToolCall(call) if call.origin == ToolOrigin::Ai => {
                    Some(call.call_id.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(ai_ids.len(), 3, "三条模型调用都应落盘");
        assert!(ai_ids.iter().all(|id| !id.is_empty()), "id 不应为空");
        let unique: std::collections::HashSet<&String> = ai_ids.iter().collect();
        assert_eq!(unique.len(), 3, "清洗后 id 应互不相同：{ai_ids:?}");

        // 每个 assistant.tool_calls[].id 都应有配对的 tool.tool_call_id。
        let rendered = render::render(&records);
        let assigned: Vec<String> = rendered
            .iter()
            .filter_map(|message| message.tool_calls.as_ref())
            .flatten()
            .map(|call| call.id.clone())
            .collect();
        assert_eq!(assigned.len(), 3);
        let tool_ids: Vec<String> = rendered
            .iter()
            .filter(|message| message.role == "tool")
            .filter_map(|message| message.tool_call_id.clone())
            .collect();
        assert_eq!(tool_ids.len(), 3);
        for id in &assigned {
            assert!(tool_ids.contains(id), "缺少配对 tool 消息：{id}");
        }
    }

    #[test]
    fn output_files_outside_data_root_keeps_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path().join("data");
        std::fs::create_dir_all(&data_root).unwrap();
        let outside = dir.path().join("elsewhere").join("out.stdout.out");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, "完整内容").unwrap();

        let output = yaa_exec::ShellOutput {
            call_id: "c1".to_string(),
            stdout: "head ... [truncated] ... tail".to_string(),
            stderr: String::new(),
            code: 0,
            timed_out: false,
            truncated: true,
            stdout_file: Some(outside.clone()),
            stderr_file: None,
        };
        let Record::ToolResult(result) = shell_output_to_record(&output, &data_root) else {
            panic!("应生成 tool_result");
        };
        let files = result.output_files.expect("截断应记录输出文件");
        assert_eq!(
            files.stdout.as_deref(),
            Some(outside.to_string_lossy().as_ref()),
            "数据根之外的输出文件必须保留绝对路径"
        );
    }

    #[test]
    fn output_files_inside_data_root_is_relative() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path().join("data");
        let inside = data_root
            .join("sessions")
            .join("sid")
            .join("out.stdout.out");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        std::fs::write(&inside, "完整内容").unwrap();

        let output = yaa_exec::ShellOutput {
            call_id: "c1".to_string(),
            stdout: "head ... [truncated] ... tail".to_string(),
            stderr: String::new(),
            code: 0,
            timed_out: false,
            truncated: true,
            stdout_file: Some(inside),
            stderr_file: None,
        };
        let Record::ToolResult(result) = shell_output_to_record(&output, &data_root) else {
            panic!("应生成 tool_result");
        };
        let files = result.output_files.expect("截断应记录输出文件");
        assert_eq!(files.stdout.as_deref(), Some("sessions/sid/out.stdout.out"));
    }
}
