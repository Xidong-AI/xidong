//! 阶段 1 的 agent loop。
//!
//! 流程（见 `docs/plan.md`「Agent loop 与并发形状」）：
//!
//! 1. 记录起点水位（**在任何写入之前**），用于后续只展示增量记录；
//! 2. 空会话先执行 `init` 钩子一次：成功写 `system(source=init)` 簿记，失败则仍写
//!    用户记录后按配置错误退出（不写簿记，下次重试）；
//! 3. 追加用户记录：配置/钩子失败也不丢用户输入；
//! 4. 每次运行执行一次 `llm` 钩子，解析配置构造模型，**不写任何记录**；
//! 5. 循环：重读历史 → 渲染 → 展示增量 → 调用模型 → 原子追加 `ai` 及其
//!    `tool_call` → 执行工具并追加 `tool_result`，直到无工具调用或达到 `max_steps`。
//!
//! 退出码：`0` 成功，`1` 通用错误，`2` 用法错误，`3` 配置/钩子错误，`4` 模型错误，
//! `5` 达到 `max_steps`。`run` 只在非预期 I/O 失败时返回 `Err`。

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use yaa_msg::{Record, ToolOrigin};

use crate::config::Config;
use crate::display::{self, DisplayItem, Sink};
use crate::llm::{self, ChatResponse, LlmConfig};
use crate::message::ChatMessage;
use crate::render;
use crate::session;
use crate::tools;

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
    /// 单次请求硬超时；由 `[agent] request_timeout_secs` 构造。
    pub timeout: std::time::Duration,
}

impl Model for OpenAiModel {
    fn chat<'a>(
        &'a self,
        messages: &'a [ChatMessage],
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ChatResponse>> + Send + 'a>> {
        Box::pin(async move {
            let tools = tools::shell::tools();
            llm::chat(&self.cfg, messages, &tools, self.timeout).await
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
    /// 展示条目接收端。
    pub sink: &'a dyn Sink,
}

impl Runner<'_> {
    /// 运行一个回合。
    ///
    /// `make_model` 在 `llm` 钩子解析成功后恰好被调用一次，模型由本次循环独占，
    /// 从而保证 `llm` 钩子每次 `run` 只执行一次。
    pub async fn run<M, F>(&self, prompt_text: &str, make_model: F) -> anyhow::Result<ExitCode>
    where
        F: FnOnce(LlmConfig) -> M,
        M: Model,
    {
        if self.cwd_warned {
            self.sink.emit(DisplayItem::Notice(
                "警告：工作目录无法规范化，已回退到词法绝对路径计算会话 id。".to_string(),
            ));
        }

        // 1. 写入前的水位：之后只展示水位之后的新记录。是否初始化改由「簿记」判定：
        //    只要历史里没有 `system(source=init)` 记录（含 `--notify` 先行写入的场景），
        //    就执行一次 `init`；因此上轮 init 失败的会话下次运行会自动重试。
        let existing = yaa_store::load(&self.session)?;
        let mut watermark = existing.len();
        let initialized = existing
            .iter()
            .any(|record| matches!(record, Record::System(system) if system.source == "init"));

        // 2. 无初始化簿记才执行 `init`；成功后写簿记，失败仍写 user 并按配置错误退出。
        if !initialized {
            match self.run_init_hook().await {
                Ok(()) => {
                    yaa_store::append(&self.session, &[Record::system("init", "", None, None)])?;
                }
                Err(exit) => {
                    yaa_store::append(&self.session, &[Record::user(prompt_text)])?;
                    return Ok(exit);
                }
            }
        }

        // 3. 用户记录落盘：配置/钩子失败也不能丢失用户输入。
        yaa_store::append(&self.session, &[Record::user(prompt_text)])?;

        // 4. llm 钩子（每次运行一次，不写记录），并用解析出的配置构造模型。
        let llm_config = match self.resolve_llm_hook().await {
            Ok(config) => config,
            Err(exit) => return Ok(exit),
        };
        let model = make_model(llm_config);

        let schema_hash = tools::shell::schema_hash();
        let limits = self.config.shell.to_limits();
        let outputs_dir = session::outputs_dir(&self.session);
        // 工具子进程注入本会话路径，使 `cmd | yaa --notify` 写回本会话（D25）。
        let extra_env = [("YAA_SESSION".to_string(), session_path(&self.session))];

        // 5. 循环。
        for _ in 0..self.config.agent.max_steps {
            let records = yaa_store::load(&self.session)?;
            let messages = render::render(&records);

            // 先展示自上次以来新增的记录（注入、用户、工具批次），再推进水位。
            self.emit_new(&records, &mut watermark);

            let response = match model.chat(&messages).await {
                Ok(response) => response,
                Err(err) => {
                    self.sink
                        .emit(DisplayItem::Error(format!("模型调用失败：{err:#}")));
                    return Ok(ExitCode::Model);
                }
            };

            // 清洗模型返回的 call_id：空串、批次内重复、或与历史冲突者一律重生成，
            // 否则 OpenAI 会因重复/空 `tool_call_id` 拒绝请求。
            let tool_calls = tools::shell::sanitize_tool_calls(&records, &response.tool_calls);
            let parsed_calls: Vec<_> = tool_calls
                .iter()
                .map(|call| tools::shell::parse(&call.arguments))
                .collect();

            // `ai` 与其 `tool_call` 以单批原子追加；`tool_call.command` 取解析后的
            // 展示用命令，**不是** arguments JSON。
            let mut batch = Vec::with_capacity(1 + tool_calls.len());
            batch.push(Record::ai(
                model.name(),
                schema_hash.clone(),
                response.content.clone().unwrap_or_default(),
                response.reasoning.clone(),
            ));
            for (call, parsed) in tool_calls.iter().zip(&parsed_calls) {
                batch.push(Record::tool_call(
                    ToolOrigin::Ai,
                    call.id.clone(),
                    parsed.command.clone(),
                    parsed.description.clone(),
                ));
            }
            yaa_store::append(&self.session, &batch)?;
            // append 后重新 load 再按水位增量展示：不能简单 `watermark += batch.len()`，
            // 否则会跳过模型请求期间外部 `yaa --notify` 追加在水位与批次之间的记录，
            // 使其永久不被展示。重新 load 覆盖「批次之前有外部写入」的边界。
            let after = yaa_store::load(&self.session)?;
            self.emit_new(&after, &mut watermark);

            if tool_calls.is_empty() {
                // 既无文本也无工具调用：不算错误（退出码仍为 0），但要明确告知用户，
                // 否则 CLI 会静默结束，用户误以为卡死。
                if response.content.as_deref().is_none_or(str::is_empty) {
                    self.sink.emit(DisplayItem::Notice(
                        "模型返回了空回复（既无文本也无工具调用）。".to_string(),
                    ));
                }
                return Ok(ExitCode::Success);
            }

            for (call, parsed) in tool_calls.iter().zip(&parsed_calls) {
                let result = tools::shell::execute(
                    &call.name,
                    &call.id,
                    parsed,
                    &limits,
                    &outputs_dir,
                    self.data_root,
                    &extra_env,
                )
                .await;
                yaa_store::append(&self.session, std::slice::from_ref(&result))?;
                // 同样以「重新 load + 增量水位」展示，保证每条结果恰好一次。
                let after = yaa_store::load(&self.session)?;
                self.emit_new(&after, &mut watermark);
            }
        }

        // 达到步数上限不在此发事件：`ExitCode::MaxSteps` 已是明确信号，具体警告与
        // 「是否仍打印部分回复」的显示策略完全由 CLI 决定。
        Ok(ExitCode::MaxSteps)
    }

    /// 执行 `init` 钩子一次。stdout 忽略（写入上下文只经 `yaa --notify`）。
    ///
    /// 失败（spawn/IO/超限/非零退出码）返回 `ExitCode::Config`。
    async fn run_init_hook(&self) -> Result<(), ExitCode> {
        let env = self.hook_env();
        let command = self.config.hooks.init.as_str();
        match crate::hook::run(command, self.cwd, &env, self.config.hooks.timeout_secs).await {
            Ok(outcome) => {
                // 钩子内的 `yaa --notify` 的 stderr 透传给用户（D27）；stdout 忽略，
                // 内容只经 `yaa --notify` 写入上下文。
                self.emit_hook_stderr(&outcome.stderr);
                if outcome.code == 0 {
                    return Ok(());
                }
                self.sink.emit(DisplayItem::Error(format!(
                    "初始化钩子以退出码 {} 失败。",
                    outcome.code
                )));
                Err(ExitCode::Config)
            }
            Err(err) => {
                self.sink
                    .emit(DisplayItem::Error(format!("执行初始化钩子失败：{err}")));
                Err(ExitCode::Config)
            }
        }
    }

    /// 把钩子 stderr 原样上报为 `Notice`；空内容不产生条目。
    fn emit_hook_stderr(&self, stderr: &str) {
        let text = stderr.trim_end();
        if !text.is_empty() {
            self.sink.emit(DisplayItem::Notice(text.to_string()));
        }
    }

    /// 执行 `llm` 钩子：解析配置供进程使用，**不写任何记录、不展示**。
    ///
    /// 钩子失败、退出码非零或 TOML 非法时返回 `ExitCode::Config`。
    async fn resolve_llm_hook(&self) -> Result<LlmConfig, ExitCode> {
        let env = self.hook_env();
        let command = self.config.hooks.llm.as_str();
        let outcome =
            match crate::hook::run(command, self.cwd, &env, self.config.hooks.timeout_secs).await {
                Ok(outcome) => outcome,
                Err(err) => {
                    self.sink
                        .emit(DisplayItem::Error(format!("执行 llm 钩子失败：{err}")));
                    return Err(ExitCode::Config);
                }
            };
        if outcome.code != 0 {
            self.emit_hook_stderr(&outcome.stderr);
            self.sink.emit(DisplayItem::Error(format!(
                "llm 钩子以退出码 {} 失败。",
                outcome.code
            )));
            return Err(ExitCode::Config);
        }
        match LlmConfig::from_toml(&outcome.stdout) {
            Ok(config) => Ok(config),
            Err(err) => {
                self.emit_hook_stderr(&outcome.stderr);
                self.sink.emit(DisplayItem::Error(format!(
                    "解析 llm 钩子输出失败：{err:#}"
                )));
                Err(ExitCode::Config)
            }
        }
    }

    /// 展示历史中水位之后的记录，并推进水位到当前长度。
    fn emit_new(&self, records: &[Record], watermark: &mut usize) {
        let start = (*watermark).min(records.len());
        self.emit_items(&records[start..]);
        *watermark = records.len();
    }

    /// 把记录投影为展示条目并逐条上报。
    fn emit_items(&self, records: &[Record]) {
        for item in display::items(records) {
            self.sink.emit(item);
        }
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
        // `init` 钩子内的 `yaa --notify` 子进程默认写回本会话（D25）。
        env.insert("YAA_SESSION".to_string(), session_path(&self.session));
        // 当前可执行文件路径，供默认钩子不依赖 PATH 地调用自身；取不到则省略。
        if let Ok(exe) = std::env::current_exe() {
            env.insert("YAA_BIN".to_string(), exe.to_string_lossy().into_owned());
        }
        env
    }
}

/// 会话 JSONL 的绝对路径；`absolute` 失败时退回原路径。
fn session_path(session: &yaa_store::Session) -> String {
    std::path::absolute(&session.file)
        .unwrap_or_else(|_| session.file.clone())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::NullSink;
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

        fn name(&self) -> &str {
            "fake-model"
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
            tool_calls: vec![crate::message::AssistantToolCall {
                id: call_id.to_string(),
                name: tools::shell::NAME.to_string(),
                arguments: arguments.to_string(),
            }],
            finish_reason: Some("tool_calls".to_string()),
        }
    }

    /// 记录展示条目的测试接收端。
    #[derive(Default)]
    struct RecordingSink {
        items: Mutex<Vec<DisplayItem>>,
    }

    impl RecordingSink {
        fn items(&self) -> Vec<DisplayItem> {
            self.items.lock().unwrap().clone()
        }
    }

    impl Sink for RecordingSink {
        fn emit(&self, item: DisplayItem) {
            self.items.lock().unwrap().push(item);
        }
    }

    fn test_config(init: &str) -> Config {
        let mut config = Config::default();
        config.hooks.init = init.to_string();
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

        fn runner<'a>(&'a self, sink: &'a dyn Sink) -> Runner<'a> {
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

    fn system_sources(records: &[Record]) -> Vec<&str> {
        records
            .iter()
            .filter_map(|record| match record {
                Record::System(system) => Some(system.source.as_str()),
                _ => None,
            })
            .collect()
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
    async fn empty_session_runs_init_and_writes_bookkeeping() {
        let harness = Harness::new(test_config("true"));
        let sink = NullSink;
        let model = FakeModel::new(vec![text_response("你好")]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success);

        let records = harness.records();
        let labels: Vec<&str> = records.iter().map(record_label).collect();
        assert_eq!(labels, vec!["system", "user", "ai"]);
        let Record::System(bookkeeping) = &records[0] else {
            panic!("首条应为 init 簿记");
        };
        assert_eq!(bookkeeping.source, "init");
        assert_eq!(bookkeeping.content, "");
    }

    #[tokio::test]
    async fn non_empty_session_does_not_rerun_init() {
        let harness = Harness::new(test_config("true"));
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
        assert_eq!(
            system_sources(&records)
                .iter()
                .filter(|s| **s == "init")
                .count(),
            1,
            "init 簿记只应有一条"
        );
        assert!(
            second
                .seen()
                .first()
                .is_some_and(|request| request.iter().any(|message| message.role == "assistant")),
            "第二次运行应读到上一轮历史"
        );
    }

    #[tokio::test]
    async fn failing_init_still_writes_user_and_exits_config() {
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
        let labels: Vec<&str> = records.iter().map(record_label).collect();
        assert_eq!(
            labels,
            vec!["user"],
            "init 失败时不写簿记，但 user 必须落盘"
        );
        let Record::User(user) = &records[0] else {
            panic!("首条应为用户记录");
        };
        assert_eq!(user.content, "重要输入");
        assert!(model.seen().is_empty(), "init 失败不应调用模型");
    }

    #[tokio::test]
    async fn failing_llm_hook_exits_config_without_records() {
        let mut config = test_config("true");
        config.hooks.llm = "exit 3".to_string();
        let harness = Harness::new(config);
        let sink = NullSink;
        let model = FakeModel::new(vec![]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Config);

        let records = harness.records();
        assert_eq!(
            records.iter().map(record_label).collect::<Vec<_>>(),
            vec!["system", "user"],
            "llm 钩子不写任何记录"
        );
        assert!(model.seen().is_empty(), "llm 失败不应调用模型");
    }

    #[tokio::test]
    async fn malformed_llm_toml_exits_config_without_records() {
        let mut config = test_config("true");
        config.hooks.llm = "printf 'not = = toml'".to_string();
        let harness = Harness::new(config);
        let sink = NullSink;
        let model = FakeModel::new(vec![]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Config);
        assert_eq!(
            harness
                .records()
                .iter()
                .map(record_label)
                .collect::<Vec<_>>(),
            vec!["system", "user"]
        );
    }

    #[tokio::test]
    async fn shell_tool_call_replays_and_loops_to_success() {
        let harness = Harness::new(test_config("true"));
        let sink = RecordingSink::default();
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
            vec!["system", "user", "ai", "tool_call", "tool_result", "ai"]
        );

        let ai_call = records
            .iter()
            .find_map(|record| match record {
                Record::ToolCall(call) if call.origin == ToolOrigin::Ai => Some(call),
                _ => None,
            })
            .expect("应有一个 ai 工具调用");
        assert_eq!(ai_call.command, "echo hi", "command 应为解析后的展示命令");
        assert_eq!(ai_call.description, None);
        assert_eq!(ai_call.call_id, "call_1");

        let result = records
            .iter()
            .find_map(|record| match record {
                Record::ToolResult(result) if result.call_id == "call_1" => Some(result),
                _ => None,
            })
            .expect("应有配对的工具结果");
        assert_eq!(result.stdout, "hi\n");
        assert_eq!(result.code, 0);

        // 展示顺序：工具调用块先于结果块。
        let items = sink.items();
        let call_pos = items
            .iter()
            .position(|item| matches!(item, DisplayItem::ToolCall { .. }))
            .expect("应展示工具调用");
        let output_pos = items
            .iter()
            .position(|item| matches!(item, DisplayItem::ToolOutput { .. }))
            .expect("应展示工具输出");
        assert!(call_pos < output_pos, "工具调用块应先于结果出现");
    }

    #[tokio::test]
    async fn tool_call_description_is_persisted() {
        let harness = Harness::new(test_config("true"));
        let sink = NullSink;
        let model = FakeModel::new(vec![
            call_response("call_1", r#"{"command":"echo hi","description":"打招呼"}"#),
            text_response("完成"),
        ]);

        harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();

        let ai_call = harness
            .records()
            .into_iter()
            .find_map(|record| match record {
                Record::ToolCall(call) if call.origin == ToolOrigin::Ai => Some(call),
                _ => None,
            })
            .expect("应有 ai 工具调用");
        assert_eq!(ai_call.command, "echo hi");
        assert_eq!(ai_call.description.as_deref(), Some("打招呼"));
    }

    #[tokio::test]
    async fn empty_reply_exits_success_with_notice() {
        let harness = Harness::new(test_config("true"));
        let sink = RecordingSink::default();
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

        assert!(
            sink.items().iter().any(|item| matches!(
                item,
                DisplayItem::Notice(content) if content.contains("空回复")
            )),
            "空回复应发出 Notice"
        );

        // 空 `ai` 记录仍落盘（不投影消息由渲染器负责）。
        let ai = harness
            .records()
            .into_iter()
            .filter(|record| matches!(record, Record::Ai(_)))
            .count();
        assert_eq!(ai, 1);
    }

    #[tokio::test]
    async fn max_steps_exhaustion_returns_five() {
        let mut config = test_config("true");
        config.agent.max_steps = 1;
        let harness = Harness::new(config);
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
    async fn init_hook_receives_session_path_env() {
        let harness = Harness::new(test_config(
            r#"printf '%s' "$YAA_SESSION" > "$GLB_CFG/session.txt""#,
        ));
        let sink = NullSink;
        let model = FakeModel::new(vec![text_response("好")]);

        harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();

        let written = std::fs::read_to_string(harness.config_root.join("session.txt")).unwrap();
        let expected = std::path::absolute(&harness.session.file)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(written, expected, "init 钩子应收到 YAA_SESSION 绝对路径");
    }

    #[tokio::test]
    async fn notify_record_is_displayed_as_injection() {
        // 让 init 钩子经 `$YAA_SESSION` 直接追加一条 notify 记录，验证增量展示。
        // 会话目录此时尚未物化，先建目录（真实路径由 `yaa --notify` 的 store 负责）。
        let harness = Harness::new(test_config(
            r#"mkdir -p "$(dirname "$YAA_SESSION")" && printf '%s\n' '{"type":"system","id":"01JNOTIFY","ts":1,"source":"notify","command":"echo hi","content":"<注释>\n内容\n</注释>"}' >> "$YAA_SESSION""#,
        ));
        let sink = RecordingSink::default();
        let model = FakeModel::new(vec![text_response("好")]);

        harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();

        let items = sink.items();
        let injections: Vec<_> = items
            .iter()
            .filter_map(|item| match item {
                DisplayItem::Injection {
                    comment,
                    command,
                    content,
                } => Some((comment.as_str(), command.as_deref(), content.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            injections,
            vec![("注释", Some("echo hi"), "<注释>\n内容\n</注释>")]
        );
    }
}
