//! `yaa` 二进制的端到端测试，覆盖 `docs/plan.md` 的「阶段 1 验收」。
//!
//! 每个需要模型的测试起一个只监听 `127.0.0.1:0` 的假 OpenAI 兼容服务，并用相互
//! 独立的临时 `YAA_CONFIG_HOME`/`YAA_DATA_HOME` 运行**真实二进制**
//! （`CARGO_BIN_EXE_yaa`）。绝不读写真实 `$HOME`、绝不访问网络。

use std::collections::VecDeque;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// 默认配置：只覆盖 agent 限额，`hooks` 走内置默认（`init` 三行 notify、`llm` 读
/// `llm.toml`）。用于验证真实默认初始化路径，包括递归防护。
const DEFAULT_CONFIG: &str = r#"
[agent]
max_steps = 10
"#;

/// 自定义初始化钩子：三条确定性 notify，正文分别为 SYS/USR/ENV。
const CUSTOM_INIT_CONFIG: &str = r#"
[hooks]
init = '''
"$YAA_BIN" --notify="系统提示词" -- printf 'SYS'
"$YAA_BIN" --notify="用户总提示词" -- printf 'USR'
"$YAA_BIN" --notify="当前环境" -- printf 'ENV'
'''

[agent]
max_steps = 10
"#;

/// 小阈值截断配置：头 32 + 尾 16 ≤ 上限 64，保证大输出必被截断。
/// `init = "true"` 使会话只含确定性记录，便于断言归档后的输出路径。
const TRUNCATION_CONFIG: &str = r#"
[hooks]
init = "true"

[shell]
timeout_secs = 30
max_stream_chars = 64
head_chars = 32
tail_chars = 16

[agent]
max_steps = 10
"#;

/// 只做初始化簿记（`init = "true"`）的最小配置：隔离与 init 无关的用例。
const PLAIN_CONFIG: &str = r#"
[hooks]
init = "true"

[agent]
max_steps = 10
"#;

/// 假服务端共享状态：按序返回脚本化响应，并记录每个请求体与 `Authorization` 头。
#[derive(Clone)]
struct ServerState {
    requests: Arc<Mutex<Vec<Value>>>,
    auth: Arc<Mutex<Vec<Option<String>>>>,
    responses: Arc<Mutex<VecDeque<Value>>>,
}

/// 处理一次 Chat Completions 请求。
async fn chat(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    state.requests.lock().unwrap().push(body);
    state.auth.lock().unwrap().push(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    );
    let response = state
        .responses
        .lock()
        .unwrap()
        .pop_front()
        .expect("测试服务脚本已耗尽");
    Json(response)
}

/// 启动假服务端，返回地址与共享状态。
async fn start_server(responses: Vec<Value>) -> (SocketAddr, ServerState) {
    let state = ServerState {
        requests: Arc::new(Mutex::new(Vec::new())),
        auth: Arc::new(Mutex::new(Vec::new())),
        responses: Arc::new(Mutex::new(responses.into())),
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, state)
}

/// 纯文本响应。
fn text_response(content: &str) -> Value {
    json!({
        "choices": [{
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }]
    })
}

/// 工具调用响应：可指定命令用途说明与思考内容。
fn tool_call_response(
    call_id: &str,
    command: &str,
    description: Option<&str>,
    reasoning: Option<&str>,
) -> Value {
    tool_call_response_named(call_id, "shell", command, description, reasoning)
}

/// 工具调用响应：可指定函数名（用于验证未知工具被拒绝）。
fn tool_call_response_named(
    call_id: &str,
    name: &str,
    command: &str,
    description: Option<&str>,
    reasoning: Option<&str>,
) -> Value {
    let mut arguments = serde_json::Map::new();
    arguments.insert("command".to_string(), json!(command));
    if let Some(description) = description {
        arguments.insert("description".to_string(), json!(description));
    }

    let mut message = serde_json::Map::new();
    message.insert("role".to_string(), json!("assistant"));
    message.insert("content".to_string(), Value::Null);
    if let Some(reasoning) = reasoning {
        message.insert("reasoning_content".to_string(), json!(reasoning));
    }
    message.insert(
        "tool_calls".to_string(),
        json!([{
            "id": call_id,
            "type": "function",
            "function": {
                "name": name,
                "arguments": Value::Object(arguments).to_string()
            }
        }]),
    );

    json!({
        "choices": [{
            "message": Value::Object(message),
            "finish_reason": "tool_calls"
        }]
    })
}

/// `llm.toml` 内容：假 base_url、假密钥与指定端点方言。
fn llm_toml_with(port: u16, endpoint_format: &str) -> String {
    format!(
        "endpoint_format = \"{endpoint_format}\"\n\
         base_url = \"http://127.0.0.1:{port}\"\n\
         api_key = \"sk-secret\"\n\
         model = \"fake-model\"\n"
    )
}

/// 隔离的临时运行环境。
struct TestEnv {
    _root: TempDir,
    work: PathBuf,
    config_home: PathBuf,
    data_home: PathBuf,
}

impl TestEnv {
    fn new(port: u16) -> Self {
        let root = TempDir::new().unwrap();
        let work = root.path().join("work");
        let config_home = root.path().join("config");
        let data_home = root.path().join("data");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::create_dir_all(&data_home).unwrap();

        std::fs::write(config_home.join("config.toml"), DEFAULT_CONFIG).unwrap();
        std::fs::write(config_home.join("llm.toml"), llm_toml_with(port, "openai")).unwrap();

        Self {
            _root: root,
            work,
            config_home,
            data_home,
        }
    }

    /// 覆盖 `config.toml`。
    fn write_config(&self, text: &str) {
        std::fs::write(self.config_home.join("config.toml"), text).unwrap();
    }

    /// 覆盖 `llm.toml`。
    fn write_llm(&self, text: &str) {
        std::fs::write(self.config_home.join("llm.toml"), text).unwrap();
    }

    /// 写入默认初始化钩子读取的 `AGENTS.md`。
    fn write_agents(&self, text: &str) {
        std::fs::write(self.config_home.join("AGENTS.md"), text).unwrap();
    }

    /// 构造指向真实二进制、环境隔离的命令。
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_yaa"));
        command
            .current_dir(&self.work)
            .env("YAA_CONFIG_HOME", &self.config_home)
            .env("YAA_DATA_HOME", &self.data_home)
            // 双重隔离：即便 YAA_* 被意外忽略，HOME 也指向临时目录；
            // 清掉外部 YAA_SESSION，避免 `--notify` 写到测试之外。
            .env("HOME", &self.work)
            .env_remove("YAA_SESSION");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    /// 带 stdin 的运行（`--notify` 的 stdin 形式）。
    fn run_with_stdin(&self, args: &[&str], input: &str) -> Output {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// 默认会话文件 `<data>/sessions/<sid>/default.jsonl`。
    ///
    /// `sid` 由规范化工作目录的 sha256 前 32 位十六进制决定，与 core 算法一致。
    fn session_file(&self) -> PathBuf {
        let canon = self.work.canonicalize().unwrap();
        let mut hasher = Sha256::new();
        hasher.update(canon.as_os_str().as_encoded_bytes());
        let digest = format!("{:x}", hasher.finalize());
        let sid = &digest[..32];
        self.data_home
            .join("sessions")
            .join(sid)
            .join("default.jsonl")
    }
}

/// 读取 JSONL 为 JSON 值列表。
fn read_jsonl(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// 统计 `system` 记录中指定 `source` 的数量。
fn count_source(records: &[Value], source: &str) -> usize {
    records
        .iter()
        .filter(|record| record["type"] == "system" && record["source"] == source)
        .count()
}

/// 取请求体里指定角色的全部文本内容。
fn message_contents(body: &Value, role: &str) -> Vec<String> {
    body["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message["role"] == role)
                .filter_map(|message| message["content"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 请求体中第一个带 `tool_calls` 的 assistant 消息。
fn assistant_with_tool_calls(messages: &[Value]) -> &Value {
    messages
        .iter()
        .find(|message| message["role"] == "assistant" && message.get("tool_calls").is_some())
        .expect("应含带工具调用的 assistant 消息")
}

/// 校验 OpenAI 消息序列合法：每个 `assistant.tool_calls` 后紧跟 `tool_call_id` 配对的
/// `tool` 消息，不允许悬空。
fn assert_valid_openai_sequence(messages: &[Value]) {
    assert!(!messages.is_empty(), "消息序列不应为空");
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        if message["role"] == "assistant"
            && let Some(calls) = message.get("tool_calls").and_then(Value::as_array)
        {
            for call in calls {
                index += 1;
                assert!(
                    index < messages.len(),
                    "assistant.tool_calls 后缺少配对的 tool 消息"
                );
                let following = &messages[index];
                assert_eq!(following["role"], "tool", "tool_calls 后必须紧跟 tool");
                assert_eq!(
                    following["tool_call_id"], call["id"],
                    "tool 消息的 tool_call_id 必须配对"
                );
            }
        }
        index += 1;
    }
}

/// 验收 1、2、7、10：空会话跑默认 `init`（内部 `--get-prompt system` 不递归），
/// 留下三条 notify + 一条 init 簿记 + user + ai；第二次不重跑且不再展示注入。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_session_runs_default_init_and_displays_once() {
    let (addr, state) = start_server(vec![text_response("第一答"), text_response("第二答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_agents("代理须知");

    let output = env.run(&["你好"]);
    assert!(
        output.status.success(),
        "退出码 {:?}，stderr：{}",
        output.status.code(),
        stderr_of(&output)
    );
    assert!(
        stdout_of(&output).contains("第一答"),
        "stdout：{}",
        stdout_of(&output)
    );

    let session = env.session_file();
    let records = read_jsonl(&session);
    let labels: Vec<&str> = records
        .iter()
        .map(|r| r["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        ["system", "system", "system", "system", "user", "ai"],
        "记录顺序不符：{records:?}"
    );
    for record in &records[..3] {
        assert_eq!(record["source"], "notify");
    }
    assert_eq!(records[3]["source"], "init");
    assert_eq!(records[3]["content"], "");
    assert_eq!(records[4]["content"], "你好");
    assert_eq!(records[5]["content"], "第一答");

    // 首次展示：三个注入块。
    let stderr = stderr_of(&output);
    for title in ["# 系统提示词", "# 用户总提示词", "# 当前环境"] {
        assert!(stderr.contains(title), "缺少注入块 {title}：{stderr}");
    }
    assert!(stderr.contains("% "), "块应含命令摘要：{stderr}");
    assert!(stderr.contains("---"), "多块间应有分隔线：{stderr}");
    assert!(
        !stderr.contains("以 Shell 为唯一工具的智能体"),
        "默认不打印注入正文：{stderr}"
    );

    // 递归防护 + 首条 system 合并 + 不泄漏密钥（验收 6 的一部分）。
    let requests = state.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "应只发起一次模型请求");
    let first_system = message_contents(&requests[0], "system").join("\n");
    assert!(
        first_system.contains("以 Shell 为唯一工具的智能体"),
        "init 内部 `--get-prompt system` 应正常返回内置提示词：{first_system}"
    );
    assert!(
        !serde_json::to_string(&requests)
            .unwrap()
            .contains("sk-secret"),
        "请求体不得含 api_key"
    );
    assert!(
        !std::fs::read_to_string(&session)
            .unwrap()
            .contains("sk-secret"),
        "JSONL 不得含 api_key"
    );

    // 第二次：不重跑 init、不再展示注入块。
    let second = env.run(&["继续"]);
    assert!(
        second.status.success(),
        "退出码 {:?}，stderr：{}",
        second.status.code(),
        stderr_of(&second)
    );
    assert!(stdout_of(&second).contains("第二答"));

    let records = read_jsonl(&session);
    assert_eq!(count_source(&records, "notify"), 3, "不应新增 notify 记录");
    assert_eq!(count_source(&records, "init"), 1, "init 簿记只应有一条");

    let stderr = stderr_of(&second);
    assert!(
        !stderr.contains("# 系统提示词"),
        "第二次不应再展示注入块：{stderr}"
    );

    // 断言第二跳请求体是合法 OpenAI 序列。
    let requests = state.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let messages = requests[1]["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "system", "首条消息必须是 system");
    assert_valid_openai_sequence(messages);
}

/// 验收 3：首条 `user` 前的通知合并为一条 `system`；首条 `user` 之后的
/// `yaa --notify` 与下一条 `user` 双向合并为一条 `user`（信封保留）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn layered_assembly_merges_prefix_and_adjacent_notify() {
    let (addr, state) = start_server(vec![text_response("第一答"), text_response("第二答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(CUSTOM_INIT_CONFIG);

    let first = env.run(&["你好"]);
    assert!(first.status.success(), "stderr：{}", stderr_of(&first));

    let requests = state.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let systems = message_contents(&requests[0], "system");
    assert_eq!(systems.len(), 1, "三条 notify 应合并为一条 system 消息");
    // `command` 现进入上下文：每条通知的最前拼 `[执行命令] <command>`（T1）。
    assert_eq!(
        systems[0],
        "[执行命令] printf SYS\n<系统提示词>\nSYS\n</系统提示词>\n\n\
         [执行命令] printf USR\n<用户总提示词>\nUSR\n</用户总提示词>\n\n\
         [执行命令] printf ENV\n<当前环境>\nENV\n</当前环境>"
    );

    // 首条 user 之后写入通知。
    let notify = env.run(&["--notify=X", "--", "echo", "hi"]);
    assert!(notify.status.success(), "stderr：{}", stderr_of(&notify));

    let second = env.run(&["接着"]);
    assert!(second.status.success(), "stderr：{}", stderr_of(&second));

    let requests = state.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let messages = requests[1]["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    let merged = messages[3]["content"].as_str().unwrap();
    assert!(merged.contains("<X>\nhi\n</X>"), "信封应保留：{merged}");
    assert!(merged.contains("接着"), "通知应与相邻 user 合并：{merged}");
    assert_valid_openai_sequence(messages);
}

/// 验收 4：多步工具循环，`tool_call`/`tool_result` 落盘并循环到最终回答。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_loop_persists_call_result_and_finishes() {
    let (addr, state) = start_server(vec![
        tool_call_response("call_1", "echo hi", Some("打招呼"), None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(CUSTOM_INIT_CONFIG);

    let output = env.run(&["你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));
    assert!(stdout_of(&output).contains("完成"));

    let records = read_jsonl(&env.session_file());
    let labels: Vec<&str> = records
        .iter()
        .map(|r| r["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        [
            "system",
            "system",
            "system",
            "system",
            "user",
            "ai",
            "tool_call",
            "tool_result",
            "ai"
        ]
    );

    let call = &records[6];
    assert_eq!(call["origin"], "ai");
    assert_eq!(call["call_id"], "call_1");
    assert_eq!(call["command"], "echo hi", "command 应为解析后的展示命令");
    assert_eq!(call["description"], "打招呼");

    let result = &records[7];
    assert_eq!(result["call_id"], "call_1");
    assert_eq!(result["stdout"], "hi\n");

    let requests = state.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let messages = requests[1]["messages"].as_array().unwrap();
    assert_valid_openai_sequence(messages);
    let assistant = assistant_with_tool_calls(messages);
    let index = messages
        .iter()
        .position(|message| std::ptr::eq(message, assistant))
        .unwrap();
    let following = &messages[index + 1];
    assert_eq!(following["tool_call_id"], assistant["tool_calls"][0]["id"]);
    assert!(
        following["content"]
            .as_str()
            .unwrap_or_default()
            .contains("hi\n"),
        "tool 消息应回填 shell stdout"
    );
}

/// 验收 5：`--clear` 归档 JSONL 到 `history/<sid>/sessions-<ts>.jsonl`，`outputs/`
/// 留在原地，归档记录中的相对路径仍有效。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_archives_jsonl_and_keeps_outputs_resolvable() {
    let (addr, _state) = start_server(vec![
        tool_call_response("call_big", "seq 1 200", None, None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(TRUNCATION_CONFIG);

    let output = env.run(&["你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let session = env.session_file();
    let records = read_jsonl(&session);
    let result = records
        .iter()
        .find(|record| record["type"] == "tool_result")
        .expect("应有工具结果");
    assert_eq!(result["truncated"], true);
    let relative = result["output_files"]["stdout"]
        .as_str()
        .expect("截断应记录 stdout 文件")
        .to_string();
    assert!(!relative.starts_with('/'), "数据根内应为相对路径");
    let full = env.data_home.join(&relative);
    assert!(full.exists(), "完整输出文件应存在：{}", full.display());

    let cleared = env.run(&["--clear"]);
    assert!(cleared.status.success(), "stderr：{}", stderr_of(&cleared));
    assert!(!session.exists(), "default.jsonl 应已归档");
    assert!(full.exists(), "outputs/ 不移动，相对路径仍有效");

    let history = env.data_home.join("history");
    let mut archived = None;
    for sid in std::fs::read_dir(&history).unwrap() {
        let sid = sid.unwrap().path();
        for entry in std::fs::read_dir(&sid).unwrap() {
            let path = entry.unwrap().path();
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("sessions-"))
            {
                archived = Some(path);
            }
        }
    }
    assert!(archived.is_some(), "history 下应有 sessions-<ts>.jsonl");
    assert!(
        stdout_of(&cleared).contains(&history.to_string_lossy().to_string()),
        "stdout 应打印归档路径：{}",
        stdout_of(&cleared)
    );
}

/// 验收 6：请求体、JSONL 与 stderr 均不含 `llm.toml` 里的假密钥。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_key_never_leaks_to_requests_jsonl_or_stderr() {
    for args in [vec!["你好"], vec!["--verbose", "你好"]] {
        let (addr, state) = start_server(vec![text_response("完成")]).await;
        let env = TestEnv::new(addr.port());

        let output = env.run(&args);
        assert!(output.status.success(), "stderr：{}", stderr_of(&output));
        assert!(
            !stderr_of(&output).contains("sk-secret"),
            "stderr 不得含 api_key（{args:?}）"
        );
        assert!(
            !std::fs::read_to_string(env.session_file())
                .unwrap()
                .contains("sk-secret"),
            "JSONL 不得含 api_key（{args:?}）"
        );
        let body = serde_json::to_string(&*state.requests.lock().unwrap()).unwrap();
        assert!(
            !body.contains("sk-secret"),
            "请求体不得含 api_key（{args:?}）"
        );
    }
}

/// 验收 12：`--get-prompt user` 为用法错误；`system` 的工具 section 与
/// `tools::shell` schema 的名称/参数一致（防漂移）。
#[test]
fn get_prompt_matches_shell_schema_and_user_is_usage_error() {
    let env = TestEnv::new(0);

    let output = env.run(&["--get-prompt", "system"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_of(&output);
    for needle in ["shell", "command", "description"] {
        assert!(
            stdout.contains(needle),
            "工具 section 缺少 {needle}：{stdout}"
        );
    }
    assert!(
        !env.data_home.join("sessions").exists(),
        "--get-prompt 不应创建会话文件"
    );

    let unknown = env.run(&["--get-prompt", "user"]);
    assert_eq!(unknown.status.code(), Some(2), "user 应为用法错误");
    assert!(
        stderr_of(&unknown).contains("未知"),
        "stderr：{}",
        stderr_of(&unknown)
    );
}

/// 验收 8：历史里 `ai` + `tool_call(origin=ai)` 无配对结果时，渲染器合成中断
/// `tool` 消息，请求体合法。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_tool_call_gets_synthetic_tool_message() {
    let (addr, state) = start_server(vec![text_response("继续完成")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let session = env.session_file();
    std::fs::create_dir_all(session.parent().unwrap()).unwrap();
    let lines = [
        r#"{"type":"user","id":"01JUSER","ts":1,"content":"第一问"}"#,
        r#"{"type":"ai","id":"01JAI","ts":2,"model":"fake-model","tool_schema_hash":"h","content":""}"#,
        r#"{"type":"tool_call","id":"01JCALL","ts":3,"origin":"ai","call_id":"orphan","command":"sleep 1"}"#,
    ]
    .join("\n");
    std::fs::write(&session, format!("{lines}\n")).unwrap();

    let output = env.run(&["继续"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));
    assert!(stdout_of(&output).contains("继续完成"));

    let requests = state.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let messages = requests[0]["messages"].as_array().unwrap();
    assert_valid_openai_sequence(messages);
    let assistant = assistant_with_tool_calls(messages);
    assert_eq!(assistant["tool_calls"][0]["id"], "orphan");
    let index = messages
        .iter()
        .position(|message| std::ptr::eq(message, assistant))
        .unwrap();
    let following = &messages[index + 1];
    assert_eq!(following["tool_call_id"], "orphan");
    assert!(
        following["content"]
            .as_str()
            .unwrap_or_default()
            .contains("interrupted"),
        "中断结果应为合成标记：{following}"
    );
}

/// 验收 9：展示中属于上下文的注入正文与请求体一致。`--verbose` 下断言
/// 「注入正文集合 ⊆ 展示文本集合」且请求体的 system/user 文本无额外内容。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn displayed_injections_match_request_context() {
    let (addr, state) = start_server(vec![text_response("FINAL")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(CUSTOM_INIT_CONFIG);

    let output = env.run(&["--verbose", "你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_jsonl(&env.session_file());
    let notifies: Vec<(String, String)> = records
        .iter()
        .filter(|record| record["type"] == "system" && record["source"] == "notify")
        .map(|record| {
            (
                record["content"].as_str().unwrap().to_string(),
                record["command"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(notifies.len(), 3);

    let requests = state.requests.lock().unwrap().clone();
    let messages = requests[0]["messages"].as_array().unwrap();
    let system = messages.iter().find(|m| m["role"] == "system").unwrap();
    // 投影正文 = `[执行命令] <command>` 前缀 + 注入信封（T1）。
    let expected_system = notifies
        .iter()
        .map(|(content, command)| format!("[执行命令] {command}\n{content}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    assert_eq!(
        system["content"].as_str().unwrap(),
        expected_system,
        "请求体 system 应由注入正文（含命令前缀）拼成"
    );
    let user = messages.iter().find(|m| m["role"] == "user").unwrap();
    assert_eq!(user["content"], "你好", "请求体 user 只能是用户输入");

    let stderr = stderr_of(&output);
    for (content, _) in &notifies {
        assert!(
            stderr.contains(content),
            "verbose 展示应含注入正文：{content}\n{stderr}"
        );
    }
    assert!(!stderr.contains("# 思考"), "无思考内容不应展示思考块");
    assert_valid_openai_sequence(messages);
}

/// 验收 11：`llm` 钩子不写任何记录、默认与 `--verbose` 都不出现在 stderr。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_hook_is_never_persisted_or_displayed() {
    for args in [vec!["你好"], vec!["--verbose", "你好"]] {
        let (addr, _state) = start_server(vec![text_response("完成")]).await;
        let env = TestEnv::new(addr.port());

        let output = env.run(&args);
        assert!(output.status.success(), "stderr：{}", stderr_of(&output));

        let raw = std::fs::read_to_string(env.session_file()).unwrap();
        assert!(
            !raw.contains("llm"),
            "JSONL 不应含 llm 记录（{args:?}）：{raw}"
        );

        let stderr = stderr_of(&output);
        assert!(
            !stderr.contains("llm"),
            "stderr 不应出现 llm（{args:?}）：{stderr}"
        );
        assert!(!stderr.contains("sk-secret"));
    }
}

/// 验收 13：`--notify` 两种形态写入 `system(source=notify)` 与信封正文；`--`
/// 形态记 `command`、stdin 形态不记；注释缺省为「由用户执行」。
#[test]
fn notify_command_and_stdin_forms_write_envelopes() {
    let env = TestEnv::new(0);
    let output = env.run(&["--notify=注释", "--", "echo", "hi"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    let records = read_jsonl(&env.session_file());
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["type"], "system");
    assert_eq!(records[0]["source"], "notify");
    assert_eq!(records[0]["command"], "echo hi");
    assert_eq!(records[0]["content"], "<注释>\nhi\n</注释>");

    let env = TestEnv::new(0);
    let output = env.run_with_stdin(&["--notify=注释"], "hi\n");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    let records = read_jsonl(&env.session_file());
    assert_eq!(records[0]["content"], "<注释>\nhi\n</注释>");
    assert!(
        records[0].get("command").is_none(),
        "stdin 形态不应记 command：{}",
        records[0]
    );

    let env = TestEnv::new(0);
    let output = env.run(&["--notify", "--", "echo", "x"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    let records = read_jsonl(&env.session_file());
    assert_eq!(records[0]["content"], "<由用户执行>\nx\n</由用户执行>");
}

/// T1/T3：通知的 `command` 对模型可见——第二跳请求的注入正文含 `[执行命令]` 前缀。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_command_is_visible_to_model_on_next_request() {
    let (addr, state) = start_server(vec![text_response("第一答"), text_response("第二答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let first = env.run(&["你好"]);
    assert!(first.status.success(), "stderr：{}", stderr_of(&first));

    let notify = env.run(&["--notify=外部", "--", "echo", "hi"]);
    assert_eq!(
        notify.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&notify)
    );

    let second = env.run(&["继续"]);
    assert!(second.status.success(), "stderr：{}", stderr_of(&second));

    let requests = state.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let messages = requests[1]["messages"].as_array().unwrap();
    let joined = messages
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("[执行命令] echo hi"),
        "第二跳请求应含命令前缀：{joined}"
    );
    assert!(
        joined.contains("<外部>\nhi\n</外部>"),
        "第二跳请求应含通知信封：{joined}"
    );
}

/// T3：`--notify --` 超限时复用 `yaa-exec` 截断并落盘完整输出，`output_files` 为相对路径。
#[test]
fn notify_truncates_and_records_output_files() {
    let env = TestEnv::new(0);
    env.write_config(TRUNCATION_CONFIG);

    let output = env.run(&["--notify=大输出", "--", "seq", "1", "200"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );

    let records = read_jsonl(&env.session_file());
    assert_eq!(records.len(), 1, "notify 是短路路径，只写一条记录");
    let record = &records[0];
    assert_eq!(record["source"], "notify");
    assert_eq!(
        record["command"], "seq 1 200",
        "command 应为转义后可重放命令"
    );

    let relative = record["output_files"]["stdout"]
        .as_str()
        .expect("截断应记录完整 stdout 文件")
        .to_string();
    assert!(
        !relative.starts_with('/'),
        "数据根内应为相对路径：{relative}"
    );
    assert!(
        env.data_home.join(&relative).exists(),
        "完整输出文件应存在：{}",
        env.data_home.join(&relative).display()
    );
    assert!(
        record["content"]
            .as_str()
            .unwrap()
            .contains("... [truncated] ..."),
        "正文应体现截断：{}",
        record["content"]
    );
}

/// T3：`--notify-timeout` 覆盖超时，命令被终止且按通用错误码退出。
#[test]
fn notify_timeout_flag_kills_long_command() {
    let env = TestEnv::new(0);
    let output = env.run(&["--notify-timeout", "1", "--notify=T", "--", "sleep", "30"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "超时命令应按 1 退出：{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("超时"),
        "应提示超时：{}",
        stderr_of(&output)
    );
    let records = read_jsonl(&env.session_file());
    assert_eq!(records[0]["command"], "sleep 30");
}

/// T3：stdin 形态按 `YAA_NOTIFY_MAX_STREAM_CHARS` 加上限，超出即视为截断。
#[test]
fn notify_stdin_truncates_at_env_limit() {
    let env = TestEnv::new(0);
    let mut command = env.command();
    command
        .env("YAA_NOTIFY_MAX_STREAM_CHARS", "8")
        .arg("--notify=管道")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"0123456789ABC")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );

    let records = read_jsonl(&env.session_file());
    assert!(
        records[0].get("command").is_none(),
        "stdin 形态不记 command"
    );
    assert!(
        records[0].get("output_files").is_none(),
        "stdin 形态不落盘完整输出"
    );
    let content = records[0]["content"].as_str().unwrap();
    assert!(
        content.contains("输入超过上限 8 字符"),
        "正文应体现截断：{content}"
    );
}

/// T3：`--notify` 的限额解析链——环境变量覆盖配置、`--notify-timeout` 覆盖环境变量。
#[test]
fn notify_timeout_flag_overrides_env_and_config() {
    let env = TestEnv::new(0);
    env.write_config(TRUNCATION_CONFIG);

    // 只给环境变量：1 秒超时足以杀掉 sleep 30。
    let mut command = env.command();
    command
        .env("YAA_NOTIFY_TIMEOUT_SECS", "1")
        .args(["--notify=env", "--", "sleep", "30"]);
    let output = command.output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(stderr_of(&output).contains("超时"));

    // CLI 覆盖为一个足够长的超时：命令应正常结束。
    let env2 = TestEnv::new(0);
    let output = env2.run(&["--notify-timeout", "10", "--notify=flag", "--", "true"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
}

/// T2：旧 `system` 记录缺 `output_files` 字段时 `--status` 仍能解析（兼容性）。
#[test]
fn status_accepts_system_without_output_files() {
    let env = TestEnv::new(0);
    let session = env.work.join("s.jsonl");
    std::fs::write(
        &session,
        b"{\"type\":\"system\",\"id\":\"01J\",\"ts\":1,\"source\":\"notify\",\"command\":\"echo hi\",\"content\":\"<x>\\nhi\\n</x>\"}\n",
    )
    .unwrap();

    let output = env.run(&["--status", "--session", session.to_str().unwrap()]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "缺 output_files 的旧 system 应可解析：{}",
        stderr_of(&output)
    );
    assert!(stdout_of(&output).contains("记录："));
}

/// 验收 14：`deepseek` 端点回传 `reasoning_content`，`--verbose` 展示思考。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deepseek_replays_reasoning_and_verbose_shows_it() {
    let (addr, state) = start_server(vec![
        tool_call_response("call_1", "echo hi", None, Some("THINK")),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_llm(&llm_toml_with(addr.port(), "deepseek"));

    let output = env.run(&["--verbose", "你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("# 思考"),
        "verbose 应展示思考标题：{stderr}"
    );
    assert!(stderr.contains("THINK"), "verbose 应展示思考内容：{stderr}");

    let requests = state.requests.lock().unwrap().clone();
    let messages = requests[1]["messages"].as_array().unwrap();
    let assistant = assistant_with_tool_calls(messages);
    assert_eq!(
        assistant["reasoning_content"], "THINK",
        "deepseek 应回传思考内容"
    );
}

/// 验收 14：`deepseek` 且思考为空时，带 `tool_calls` 的 assistant 仍回传
/// `reasoning_content = ""`。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deepseek_replays_empty_reasoning_string_for_tool_calls() {
    let (addr, state) = start_server(vec![
        tool_call_response("call_1", "echo hi", None, None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_llm(&llm_toml_with(addr.port(), "deepseek"));

    let output = env.run(&["你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let requests = state.requests.lock().unwrap().clone();
    let messages = requests[1]["messages"].as_array().unwrap();
    let assistant = assistant_with_tool_calls(messages);
    assert_eq!(assistant["reasoning_content"], "", "思考为空也须回传空串");
}

/// 验收 14：`openai` 端点不回传 `reasoning_content`。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_omits_reasoning_content() {
    let (addr, state) = start_server(vec![
        tool_call_response("call_1", "echo hi", None, Some("THINK")),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());

    let output = env.run(&["你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let requests = state.requests.lock().unwrap().clone();
    let messages = requests[1]["messages"].as_array().unwrap();
    let assistant = assistant_with_tool_calls(messages);
    assert!(
        assistant.get("reasoning_content").is_none(),
        "openai 端点不应含 reasoning_content：{assistant}"
    );
}

/// 验收 15：`--session <path>` 在空会话触发 `init`，三条 notify 经 `YAA_SESSION`
/// 写入该文件，且不创建默认会话目录。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_flag_triggers_init_into_given_file() {
    let (addr, _state) = start_server(vec![text_response("完成")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(CUSTOM_INIT_CONFIG);

    let session_path = env
        .data_home
        .parent()
        .unwrap()
        .join("custom")
        .join("s.jsonl");
    let output = env
        .command()
        .arg("--session")
        .arg(&session_path)
        .arg("你好")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_jsonl(&session_path);
    let labels: Vec<&str> = records
        .iter()
        .map(|r| r["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        ["system", "system", "system", "system", "user", "ai"]
    );
    assert_eq!(count_source(&records, "notify"), 3);
    assert!(
        !env.data_home.join("sessions").exists(),
        "显式会话不应创建默认会话目录"
    );
}

/// 验收 15：旧 schema 会话由 `--status` 报明确错误并提示 `--clear`，退出码非零。
#[test]
fn status_reports_old_schema_with_clear_hint() {
    let env = TestEnv::new(0);
    let session_path = env.work.join("old.jsonl");
    std::fs::write(
        &session_path,
        "{\"type\":\"tool_call\",\"id\":\"x\",\"ts\":1,\"origin\":\"hook\",\"call_id\":\"c\",\"command\":\"echo\"}\n",
    )
    .unwrap();

    let output = env.run(&["--status", "--session", session_path.to_str().unwrap()]);
    assert_ne!(output.status.code(), Some(0), "旧 schema 应报错");
    let stderr = stderr_of(&output);
    assert!(stderr.contains("--clear"), "错误应提示 --clear：{stderr}");
    assert!(
        !stdout_of(&output).contains("记录："),
        "解析失败不应打印摘要"
    );
}

/// 过程输出约束：`--quiet` 抑制全部 stderr，只留 stdout 最终回复。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiet_suppresses_all_process_output() {
    let (addr, _state) = start_server(vec![
        tool_call_response("call_1", "echo hi", None, None),
        text_response("FINAL"),
    ])
    .await;
    let env = TestEnv::new(addr.port());

    let output = env.run(&["--quiet", "你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));
    assert!(
        stderr_of(&output).is_empty(),
        "quiet 应让 stderr 完全干净：{}",
        stderr_of(&output)
    );
    assert!(stdout_of(&output).contains("FINAL"));
}

/// 多个位置参数以单个空格连接。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_word_prompt_is_joined_with_single_space() {
    let (addr, _state) = start_server(vec![text_response("完成")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(CUSTOM_INIT_CONFIG);

    let output = env.run(&["你好", "世界", "！"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_jsonl(&env.session_file());
    let user = records
        .iter()
        .find(|record| record["type"] == "user")
        .expect("应有用户记录");
    assert_eq!(user["content"], "你好 世界 ！");
}

/// 未知参数与已删除的 `--trace` 都是用法错误（退出码 2）。
#[test]
fn unknown_and_removed_flags_are_usage_errors() {
    let env = TestEnv::new(0);
    for args in [vec!["--definitely-not-a-flag"], vec!["--trace", "你好"]] {
        let output = env.run(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "应为用法错误（{args:?}）：{}",
            stderr_of(&output)
        );
        assert!(
            stdout_of(&output).is_empty(),
            "用法错误不应污染 stdout（{args:?}）"
        );
    }
}

/// F1：预置旧 `ai` 后模型返回空回复，stdout 必须为空（不得重印旧答案）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_reply_does_not_reprint_previous_answer() {
    let (addr, _state) = start_server(vec![text_response("")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let session = env.session_file();
    std::fs::create_dir_all(session.parent().unwrap()).unwrap();
    let seed = [
        r#"{"type":"user","id":"01JOLDQ","ts":1,"content":"旧问"}"#,
        r#"{"type":"ai","id":"01JOLDA","ts":2,"model":"fake-model","tool_schema_hash":"h","content":"旧答案"}"#,
    ]
    .join("\n");
    std::fs::write(&session, format!("{seed}\n")).unwrap();

    let output = env.run(&["新问"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));
    assert!(
        stdout_of(&output).is_empty(),
        "空回复不得重印旧答案，实际 stdout：{:?}",
        stdout_of(&output)
    );
    assert!(
        stderr_of(&output).contains("空回复"),
        "应提示空回复：{}",
        stderr_of(&output)
    );
}

/// F3：`--session` 下工具内嵌 `cmd | yaa --notify` 写回本会话，不污染默认会话。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_embedded_notify_writes_to_explicit_session() {
    let bin = env!("CARGO_BIN_EXE_yaa");
    let command = format!("printf x | '{bin}' --notify=注释");
    let (addr, _state) = start_server(vec![
        tool_call_response("call_1", &command, None, None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(CUSTOM_INIT_CONFIG);

    let session_path = env
        .data_home
        .parent()
        .unwrap()
        .join("custom")
        .join("s3.jsonl");
    let output = env
        .command()
        .arg("--session")
        .arg(&session_path)
        .arg("你好")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_jsonl(&session_path);
    assert!(
        records.iter().any(|record| {
            record["type"] == "system"
                && record["source"] == "notify"
                && record["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("<注释>\nx\n</注释>"))
        }),
        "工具内嵌 notify 应写入显式会话：{records:?}"
    );
    assert!(
        !env.data_home.join("sessions").exists(),
        "显式会话不得创建默认会话目录"
    );
}

/// F8：模型给出非 `shell` 的工具名时返回失败记录且不执行命令。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_tool_name_is_rejected_without_execution() {
    let (addr, _state) = start_server(vec![
        tool_call_response_named(
            "call_x",
            "other",
            "touch marker_should_not_exist",
            None,
            None,
        ),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let output = env.run(&["你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_jsonl(&env.session_file());
    let result = records
        .iter()
        .find(|record| record["type"] == "tool_result" && record["call_id"] == "call_x")
        .expect("应有该工具调用的结果记录");
    assert_eq!(result["code"], -1);
    assert_eq!(
        result["stderr"],
        "不支持的工具：other（阶段 1 仅有 shell）。"
    );
    assert!(
        !env.work.join("marker_should_not_exist").exists(),
        "未知工具名不得执行命令"
    );
}

/// F4：`--quiet` 下致命错误（`llm` 钩子非零退出）仍打印到 stderr，退出码为 3。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiet_still_reports_fatal_llm_error() {
    let (addr, _state) = start_server(vec![]).await;
    let env = TestEnv::new(addr.port());
    env.write_config("[hooks]\ninit = \"true\"\nllm = \"exit 3\"\n\n[agent]\nmax_steps = 10\n");

    let output = env.run(&["--quiet", "你好"]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        !stderr_of(&output).is_empty(),
        "--quiet 下致命错误仍须打印到 stderr"
    );
    assert!(
        stdout_of(&output).is_empty(),
        "--quiet 失败回合不得污染 stdout：{:?}",
        stdout_of(&output)
    );
}

/// F5/F6：已有 `notify` 记录但无 `system(source=init)` 簿记时仍执行 init。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_before_task_still_runs_init() {
    let (addr, state) = start_server(vec![text_response("答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_agents("代理须知");

    let notify = env.run_with_stdin(&["--notify=外部"], "外部内容\n");
    assert_eq!(
        notify.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&notify)
    );

    let output = env.run(&["新任务"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_jsonl(&env.session_file());
    assert_eq!(
        count_source(&records, "init"),
        1,
        "有 notify 但无簿记仍应执行 init"
    );

    let requests = state.requests.lock().unwrap().clone();
    let system = message_contents(&requests[0], "system").join("\n");
    assert!(
        system.contains("以 Shell 为唯一工具的智能体"),
        "init 应注入系统提示词：{system}"
    );
}

/// F5/F6：init 失败不写簿记，修好后下次运行能成功重试。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_init_retries_on_next_run() {
    let (addr, _state) = start_server(vec![text_response("答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config("[hooks]\ninit = \"exit 3\"\n\n[agent]\nmax_steps = 10\n");

    let first = env.run(&["一"]);
    assert_eq!(
        first.status.code(),
        Some(3),
        "init 失败应配置错误退出：{}",
        stderr_of(&first)
    );
    let records = read_jsonl(&env.session_file());
    assert_eq!(count_source(&records, "init"), 0, "失败不写簿记");
    assert_eq!(
        records.iter().filter(|r| r["type"] == "user").count(),
        1,
        "init 失败仍须保留用户输入"
    );

    env.write_config(PLAIN_CONFIG);
    let second = env.run(&["二"]);
    assert!(second.status.success(), "stderr：{}", stderr_of(&second));
    let records = read_jsonl(&env.session_file());
    assert_eq!(count_source(&records, "init"), 1, "修好后重试应成功 init");
}

/// F9：`--clear --session <裸文件名>` 也能成功归档（无目录名时 sid 回退路径哈希）。
#[test]
fn clear_bare_filename_session_archives() {
    let env = TestEnv::new(0);
    std::fs::write(
        env.work.join("s.jsonl"),
        b"{\"type\":\"user\",\"id\":\"01J\",\"ts\":1,\"content\":\"x\"}\n",
    )
    .unwrap();

    let output = env.run(&["--clear", "--session", "s.jsonl"]);
    assert!(
        output.status.success(),
        "裸文件名归档应成功：{}",
        stderr_of(&output)
    );
    assert!(!env.work.join("s.jsonl").exists(), "原文件应已归档");
    let printed = stdout_of(&output);
    assert!(printed.contains("history"), "应打印归档路径：{printed}");
}
