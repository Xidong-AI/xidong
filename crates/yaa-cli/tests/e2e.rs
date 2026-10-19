//! `yaa` 二进制的端到端测试，覆盖 `docs/plan.md` 的「阶段 1 验收」。
//!
//! 每个需要模型的测试起一个只监听 `127.0.0.1:0` 的假 OpenAI 兼容服务，并用相互
//! 独立的临时 `YAA_CONFIG_HOME`/`YAA_DATA_HOME` 运行**真实二进制**
//! （`CARGO_BIN_EXE_yaa`）。绝不读写真实 `$HOME`、绝不访问网络。

use std::collections::{HashSet, VecDeque};
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

/// 命中 `echo *` 即询问的审批配置：用于 ask 拦截与 `--approve` 恢复。
const APPROVAL_ASK_CONFIG: &str = r#"
[hooks]
init = "true"

[approval]
rules = [{ pattern = "echo *", action = "ask" }]

[agent]
max_steps = 10
"#;

/// 命中 `rm *` 即拒绝、其余（含 `echo *`）询问的审批配置：用于 `--auto` 与 deny 用例。
const APPROVAL_AUTO_DENY_CONFIG: &str = r#"
[hooks]
init = "true"

[approval]
rules = [
  { pattern = "echo *", action = "ask" },
  { pattern = "rm *", action = "deny" },
]

[agent]
max_steps = 10
"#;

/// 假服务端共享状态：按序返回脚本化响应，并记录每个请求体与 `Authorization` 头。
#[derive(Clone)]
struct ServerState {
    requests: Arc<Mutex<Vec<Value>>>,
    auth: Arc<Mutex<Vec<Option<String>>>>,
    responses: Arc<Mutex<VecDeque<Value>>>,
    /// 首个请求返回前执行一次的外部副作用（如外部 `yaa --notify`），用于覆盖
    /// 「模型请求期间有并发写入」的边界。
    before_response: Arc<Mutex<Option<Command>>>,
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
    // 在返回响应之前执行一次外部写入，模拟模型请求期间落盘的并发记录。
    if let Some(mut command) = state.before_response.lock().unwrap().take() {
        command.status().unwrap();
    }
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
    start_server_with(responses, None).await
}

/// 启动假服务端，并在首个请求返回前执行一次 `side_effect`（阻塞至其结束）。
async fn start_server_with_side_effect(
    responses: Vec<Value>,
    side_effect: Command,
) -> (SocketAddr, ServerState) {
    start_server_with(responses, Some(side_effect)).await
}

async fn start_server_with(
    responses: Vec<Value>,
    side_effect: Option<Command>,
) -> (SocketAddr, ServerState) {
    let state = ServerState {
        requests: Arc::new(Mutex::new(Vec::new())),
        auth: Arc::new(Mutex::new(Vec::new())),
        responses: Arc::new(Mutex::new(responses.into())),
        before_response: Arc::new(Mutex::new(side_effect)),
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

/// 带中间正文的工具调用响应：`content` 非空且同时带 `tool_calls`（用于 A5）。
fn tool_call_with_content_response(call_id: &str, content: &str, command: &str) -> Value {
    let arguments = json!({ "command": command }).to_string();
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": content,
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": { "name": "shell", "arguments": arguments }
                }]
            },
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
        self.run_with_stdin_bytes(args, input.as_bytes())
    }

    /// 带任意字节 stdin 的运行：用于覆盖非 UTF-8 输入。
    fn run_with_stdin_bytes(&self, args: &[&str], input: &[u8]) -> Output {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    /// 默认会话数据库 `<data>/sessions/<sid>/session.db`。
    ///
    /// `sid` 由规范化工作目录的 sha256 前 32 位十六进制决定，与 core 算法一致。
    fn session_db(&self) -> PathBuf {
        let canon = self.work.canonicalize().unwrap();
        let mut hasher = Sha256::new();
        hasher.update(canon.as_os_str().as_encoded_bytes());
        let digest = format!("{:x}", hasher.finalize());
        let sid = &digest[..32];
        self.data_home.join("sessions").join(sid).join("session.db")
    }
}

/// 读取会话数据库的全部记录，转为 JSON 值列表（断言结构保持与旧 JSONL 一致）。
fn read_events(db: &Path) -> Vec<Value> {
    let session = yaa_store::Session::new(db);
    yaa_store::load(&session)
        .unwrap()
        .iter()
        .map(|record| serde_json::to_value(record).unwrap())
        .collect()
}

/// 用 store API 预置历史记录。
fn seed(db: &Path, records: &[yaa_msg::Record]) {
    let session = yaa_store::Session::new(db);
    yaa_store::append(&session, records).unwrap();
}

/// 数据库 `PRAGMA integrity_check` 是否为 `ok`；库不存在返回 false。
fn integrity_ok(db: &Path) -> bool {
    let Ok(conn) = rusqlite::Connection::open(db) else {
        return false;
    };
    conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        .is_ok_and(|result| result == "ok")
}

/// 会话数据库的全部 payload 文本（用于敏感信息/原始内容断言）。
fn payload_text(db: &Path) -> String {
    read_events(db)
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
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

    let session = env.session_db();
    let records = read_events(&session);
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
        !payload_text(&session).contains("sk-secret"),
        "会话记录不得含 api_key"
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

    let records = read_events(&session);
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

    let records = read_events(&env.session_db());
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

/// 验收 5：`--clear` 归档数据库到 `history/<sid>/sessions-<ts>.db`，清空主库并递增
/// generation；`outputs/` 留在原地，归档记录中的相对路径仍有效。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_archives_database_and_keeps_outputs_resolvable() {
    let (addr, _state) = start_server(vec![
        tool_call_response("call_big", "seq 1 200", None, None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(TRUNCATION_CONFIG);

    let output = env.run(&["你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let session = env.session_db();
    let session_handle = yaa_store::Session::new(session.clone());
    let records = read_events(&session);
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
    assert_eq!(yaa_store::generation(&session_handle).unwrap(), 1);

    let cleared = env.run(&["--clear"]);
    assert!(cleared.status.success(), "stderr：{}", stderr_of(&cleared));
    assert!(session.exists(), "主库不删除，只清空事件");
    assert!(read_events(&session).is_empty(), "主库 events 应清空");
    assert_eq!(yaa_store::generation(&session_handle).unwrap(), 2);
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
    let archived = archived.expect("history 下应有 sessions-<ts>.db");
    assert_eq!(
        archived.extension().and_then(|ext| ext.to_str()),
        Some("db")
    );
    let archived_session = yaa_store::Session::new(archived.clone());
    let archived_records = yaa_store::load(&archived_session).unwrap();
    assert_eq!(archived_records.len(), records.len(), "归档应保留全部记录");
    assert!(yaa_store::is_archived(&archived_session).unwrap());
    assert!(
        stdout_of(&cleared).contains(&archived.to_string_lossy().to_string()),
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
            !payload_text(&env.session_db()).contains("sk-secret"),
            "会话记录不得含 api_key（{args:?}）"
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

/// 验收 8：历史里 `ai` + `tool_call(origin=ai)` 无配对结果时，启动恢复补写中断
/// `tool_result`，请求体合法。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_tool_call_gets_synthetic_tool_message() {
    let (addr, state) = start_server(vec![text_response("继续完成")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let session = env.session_db();
    seed(
        &session,
        &[
            yaa_msg::Record::user("第一问"),
            yaa_msg::Record::ai("fake-model", "h", "", None),
            yaa_msg::Record::tool_call(yaa_msg::ToolOrigin::Ai, "orphan", "sleep 1", None),
        ],
    );

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
        "中断结果应带 interrupted 标记：{following}"
    );
    assert!(
        read_events(&session)
            .iter()
            .any(|record| record["type"] == "tool_result"
                && record["call_id"] == "orphan"
                && record["code"] == -1),
        "中断结果应已持久化"
    );
}

/// D-S10：真实进程被 SIGKILL 后，下次启动补写中断 `tool_result` 并继续对话。
///
/// 第一次运行在 `sleep 60` 工具执行中被 SIGKILL：`ai` + `tool_call` 已落盘、无结果。
/// 第二次运行启动恢复应补一条 `code=-1`、`stderr` 含 `interrupted` 的结果，且只补一次。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_after_sigkill_recovers_interrupted_tool_call() {
    let (addr, state) = start_server(vec![
        tool_call_response("call_kill", "sleep 60", None, None),
        text_response("已恢复"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let mut child = env
        .command()
        .arg("开始")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // 轮询等待 `ai` + `tool_call` 落盘（工具随后进入执行）。
    let session = yaa_store::Session::new(env.session_db());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let has_call = yaa_store::load(&session).is_ok_and(|records| {
            records
                .iter()
                .any(|record| matches!(record, yaa_msg::Record::ToolCall(call) if call.call_id == "call_kill"))
        });
        if has_call {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待 tool_call 落盘超时：{}",
            session.db.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // 进入工具执行后再强杀，模拟运行中崩溃。
    std::thread::sleep(std::time::Duration::from_millis(200));

    child.kill().expect("发送 SIGKILL");
    child.wait().expect("回收子进程");

    let records = yaa_store::load(&session).unwrap();
    assert!(
        records
            .iter()
            .any(|record| matches!(record, yaa_msg::Record::ToolCall(call) if call.call_id == "call_kill")),
        "SIGKILL 前 tool_call 应已落盘：{records:?}"
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, yaa_msg::Record::ToolResult(result) if result.call_id == "call_kill")),
        "SIGKILL 时不应存在 tool_result：{records:?}"
    );

    let second = env.run(&["继续"]);
    assert!(
        second.status.success(),
        "恢复运行应成功，退出码 {:?}，stderr：{}",
        second.status.code(),
        stderr_of(&second)
    );
    assert!(
        stdout_of(&second).contains("已恢复"),
        "stdout：{}",
        stdout_of(&second)
    );

    // 恢复结果恰好一条，内容为中断标记。
    let records = yaa_store::load(&session).unwrap();
    let results: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            yaa_msg::Record::ToolResult(result) if result.call_id == "call_kill" => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "恢复应恰好补一条结果：{records:?}");
    assert_eq!(results[0].code, -1);
    assert!(
        results[0].stderr.contains("interrupted"),
        "中断结果应带 interrupted 标记：{:?}",
        results[0]
    );

    // 第二跳请求体合法，且包含该调用的 tool 消息（模型能看到中断）。
    let requests = state.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "应恰好两次模型请求：{requests:?}");
    let messages = requests[1]["messages"].as_array().unwrap();
    assert_valid_openai_sequence(messages);
    let tool_message = messages
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_kill")
        .expect("第二跳应包含中断 tool 消息");
    assert!(
        tool_message["content"]
            .as_str()
            .unwrap_or_default()
            .contains("interrupted"),
        "第二跳 tool 消息应含中断标记：{tool_message:?}"
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

    let records = read_events(&env.session_db());
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

        let raw = payload_text(&env.session_db());
        assert!(
            !raw.contains("llm"),
            "会话记录不应含 llm 记录（{args:?}）：{raw}"
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
    let records = read_events(&env.session_db());
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
    let records = read_events(&env.session_db());
    assert_eq!(records[0]["content"], "<注释>\nhi\n</注释>");
    assert!(
        records[0].get("command").is_none(),
        "stdin 形态不应记 command：{}",
        records[0]
    );
    assert!(
        records[0].get("output_files").is_none(),
        "短输入的 stdin 形态不应落盘完整输出：{}",
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
    let records = read_events(&env.session_db());
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

    let records = read_events(&env.session_db());
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
    let records = read_events(&env.session_db());
    assert_eq!(records[0]["command"], "sleep 30");
}

/// stdin 形态复用 `yaa-exec`：超限时截断、落盘完整输入并记入 `output_files`。
#[test]
fn notify_stdin_truncates_and_records_output_files() {
    let env = TestEnv::new(0);
    let mut command = env.command();
    command
        .env("YAA_NOTIFY_MAX_STREAM_CHARS", "8")
        .env("YAA_NOTIFY_HEAD_CHARS", "4")
        .env("YAA_NOTIFY_TAIL_CHARS", "2")
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

    let records = read_events(&env.session_db());
    assert!(
        records[0].get("command").is_none(),
        "stdin 形态不记 command"
    );

    let relative = records[0]["output_files"]["stdout"]
        .as_str()
        .expect("stdin 截断应记录完整输出文件")
        .to_string();
    assert!(
        !relative.starts_with('/'),
        "数据根内应为相对路径：{relative}"
    );
    let full = env.data_home.join(&relative);
    assert!(full.exists(), "完整输出文件应存在：{}", full.display());
    assert_eq!(
        std::fs::read_to_string(&full).unwrap(),
        "0123456789ABC",
        "完整输出文件应保留全部原始输入"
    );

    let content = records[0]["content"].as_str().unwrap();
    assert!(
        content.contains("... [truncated] ..."),
        "正文应体现截断：{content}"
    );
    assert!(
        content.starts_with("<管道>\n0123"),
        "正文应保留头部：{content}"
    );
    assert!(content.contains("BC\n</管道>"), "正文应保留尾部：{content}");
}

/// stdin 形态超限的通知在第二跳请求体中体现截断提示与相对路径。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_stdin_truncation_is_visible_to_model_with_path() {
    let (addr, state) = start_server(vec![text_response("第一答"), text_response("第二答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(TRUNCATION_CONFIG);

    let first = env.run(&["你好"]);
    assert!(first.status.success(), "stderr：{}", stderr_of(&first));

    let long_input = "abcdefghij".repeat(20);
    let notify = env.run_with_stdin(&["--notify=长输入"], &long_input);
    assert_eq!(
        notify.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&notify)
    );

    let relative = read_events(&env.session_db())
        .iter()
        .find(|record| record["type"] == "system" && record["source"] == "notify")
        .and_then(|record| record["output_files"]["stdout"].as_str())
        .expect("超限通知应记录完整输出文件")
        .to_string();

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
        joined.contains("[truncated] 仅保留头尾输出，完整内容见输出文件"),
        "第二跳请求应含截断提示：{joined}"
    );
    assert!(
        joined.contains(&relative),
        "第二跳请求应含完整输出相对路径 {relative}：{joined}"
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
    let session = env.work.join("s.db");
    seed(
        &session,
        &[yaa_msg::Record::system(
            "notify",
            "<x>\nhi\n</x>",
            Some("echo hi".to_string()),
            None,
        )],
    );

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

    let session_path = env.data_home.parent().unwrap().join("custom").join("s.db");
    let output = env
        .command()
        .arg("--session")
        .arg(&session_path)
        .arg("你好")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_events(&session_path);
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

/// 验收 15：旧 JSONL 会话由 `--status` 报明确错误并提示 `--clear`，退出码非零。
#[test]
fn status_reports_old_jsonl_with_clear_hint() {
    let env = TestEnv::new(0);
    let session_path = env.work.join("old.jsonl");
    std::fs::write(
        &session_path,
        "{\"type\":\"tool_call\",\"id\":\"x\",\"ts\":1,\"origin\":\"hook\",\"call_id\":\"c\",\"command\":\"echo\"}\n",
    )
    .unwrap();

    let output = env.run(&["--status", "--session", session_path.to_str().unwrap()]);
    assert_ne!(output.status.code(), Some(0), "旧格式应报错");
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("旧版 JSONL"),
        "错误应说明旧版 JSONL：{stderr}"
    );
    assert!(stderr.contains("--clear"), "错误应提示 --clear：{stderr}");
    assert!(
        !stdout_of(&output).contains("记录："),
        "解析失败不应打印摘要"
    );
}

/// M2：默认会话目录里残留旧 `default.jsonl` 且无 `session.db` 时，默认路径明确报错，
/// 不得静默创建空库绕过旧历史。
#[test]
fn default_session_rejects_legacy_default_jsonl() {
    let env = TestEnv::new(0);
    let db = env.session_db();
    let session_dir = db.parent().unwrap();
    std::fs::create_dir_all(session_dir).unwrap();
    let legacy = session_dir.join("default.jsonl");
    std::fs::write(
        &legacy,
        b"{\"type\":\"user\",\"id\":\"01J\",\"ts\":1,\"content\":\"x\"}\n",
    )
    .unwrap();

    let output = env.run(&["--status"]);
    assert_ne!(output.status.code(), Some(0), "默认旧历史应报错");
    let stderr = stderr_of(&output);
    assert!(stderr.contains("旧版 JSONL"), "stderr：{stderr}");
    assert!(stderr.contains("--clear"), "stderr：{stderr}");
    assert!(
        stderr.contains(&legacy.to_string_lossy().to_string()),
        "文案应给出具体 default.jsonl 路径：{stderr}"
    );
    assert!(!db.exists(), "报错时不得创建空 session.db");
}

/// M2：显式目录里残留旧 `default.jsonl` 且无 `session.db` 时，`--status` 同样报错。
#[test]
fn explicit_directory_status_rejects_legacy_default_jsonl() {
    let env = TestEnv::new(0);
    let dir = env.work.join("custom");
    std::fs::create_dir_all(&dir).unwrap();
    let legacy = dir.join("default.jsonl");
    std::fs::write(
        &legacy,
        b"{\"type\":\"user\",\"id\":\"01J\",\"ts\":1,\"content\":\"x\"}\n",
    )
    .unwrap();

    let output = env.run(&["--status", "--session", dir.to_str().unwrap()]);
    assert_ne!(output.status.code(), Some(0), "显式目录旧历史应报错");
    let stderr = stderr_of(&output);
    assert!(stderr.contains("旧版 JSONL"), "stderr：{stderr}");
    assert!(stderr.contains("--clear"), "stderr：{stderr}");
    assert!(
        stderr.contains(&legacy.to_string_lossy().to_string()),
        "文案应给出具体 default.jsonl 路径：{stderr}"
    );
    assert!(
        !dir.join("session.db").exists(),
        "报错时不得创建空 session.db"
    );
}

/// M2：显式目录里残留旧 `default.jsonl` 时普通 run 也应拒绝，且不创建新库。
#[test]
fn explicit_directory_run_rejects_legacy_default_jsonl() {
    let env = TestEnv::new(0);
    let dir = env.work.join("custom");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("default.jsonl"),
        b"{\"type\":\"user\",\"id\":\"01J\",\"ts\":1,\"content\":\"x\"}\n",
    )
    .unwrap();

    let output = env.run(&["--session", dir.to_str().unwrap(), "你好"]);
    assert_ne!(output.status.code(), Some(0), "显式目录旧历史应报错");
    let stderr = stderr_of(&output);
    assert!(stderr.contains("旧版 JSONL"), "stderr：{stderr}");
    assert!(stderr.contains("--clear"), "stderr：{stderr}");
    assert!(
        !dir.join("session.db").exists(),
        "报错时不得创建空 session.db"
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

    let records = read_events(&env.session_db());
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

    let session = env.session_db();
    seed(
        &session,
        &[
            yaa_msg::Record::user("旧问"),
            yaa_msg::Record::ai("fake-model", "h", "旧答案", None),
        ],
    );

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

    let session_path = env.data_home.parent().unwrap().join("custom").join("s3.db");
    let output = env
        .command()
        .arg("--session")
        .arg(&session_path)
        .arg("你好")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_events(&session_path);
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

    let records = read_events(&env.session_db());
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

    let records = read_events(&env.session_db());
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
    let records = read_events(&env.session_db());
    assert_eq!(count_source(&records, "init"), 0, "失败不写簿记");
    assert_eq!(
        records.iter().filter(|r| r["type"] == "user").count(),
        1,
        "init 失败仍须保留用户输入"
    );

    env.write_config(PLAIN_CONFIG);
    let second = env.run(&["二"]);
    assert!(second.status.success(), "stderr：{}", stderr_of(&second));
    let records = read_events(&env.session_db());
    assert_eq!(count_source(&records, "init"), 1, "修好后重试应成功 init");
}

/// F9：旧 `.jsonl` 的 `--clear --session <裸文件名>` 走文件级归档（绝对化后 sid 取父目录名）。
#[test]
fn clear_bare_filename_session_archives() {
    let env = TestEnv::new(0);
    let original = b"{\"type\":\"user\",\"id\":\"01J\",\"ts\":1,\"content\":\"x\"}\n";
    std::fs::write(env.work.join("s.jsonl"), original).unwrap();

    let output = env.run(&["--clear", "--session", "s.jsonl"]);
    assert!(
        output.status.success(),
        "裸文件名归档应成功：{}",
        stderr_of(&output)
    );
    assert!(!env.work.join("s.jsonl").exists(), "原文件应已归档");
    let printed = stdout_of(&output).trim().to_string();
    assert!(printed.contains("history"), "应打印归档路径：{printed}");

    let archived = PathBuf::from(&printed);
    assert_eq!(
        archived.extension().and_then(|ext| ext.to_str()),
        Some("jsonl")
    );
    assert_eq!(
        std::fs::read(&archived).unwrap(),
        original,
        "内容应原样保留"
    );
    let sid = archived
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap();
    assert_eq!(
        sid,
        env.work.file_name().unwrap().to_str().unwrap(),
        "sid 应取 JSONL 父目录名"
    );
}

/// A1 回归：单次运行内，工具调用的描述、命令与中间正文各自只展示一次。
///
/// 旧实现每轮 `emit_new` 会把上一轮已 `emit_items` 过的批次重新投影，导致重复。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_step_is_displayed_exactly_once() {
    let (addr, _state) = start_server(vec![
        json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "我先调用工具",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "shell",
                            "arguments": json!({
                                "command": "echo hello-tool",
                                "description": "打招呼"
                            })
                            .to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }),
        text_response("最终回复"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let output = env.run(&["你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let stderr = stderr_of(&output);
    assert_eq!(
        stderr.matches("# 打招呼").count(),
        1,
        "工具描述应恰好展示一次：\n{stderr}"
    );
    assert_eq!(
        stderr.matches("% echo hello-tool").count(),
        1,
        "工具命令应恰好展示一次：\n{stderr}"
    );
    assert_eq!(
        stderr.matches("我先调用工具").count(),
        1,
        "中间正文应恰好展示一次：\n{stderr}"
    );

    let stdout = stdout_of(&output);
    assert!(stdout.contains("最终回复"), "stdout：{stdout}");
    assert!(
        !stdout.contains("我先调用工具"),
        "stdout 只含最终回复：{stdout}"
    );
}

/// A1 回归：模型请求期间的外部 `yaa --notify` 既不丢失也不重复展示。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_notify_during_model_request_is_displayed_exactly_once() {
    let env = TestEnv::new(0);
    let mut side_effect = env.command();
    side_effect
        .arg("--session")
        .arg(env.session_db())
        .arg("--notify=外部")
        .arg("--")
        .arg("printf")
        .arg("NOTIFY")
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let (addr, _state) = start_server_with_side_effect(
        vec![
            tool_call_response("call_1", "echo hello-tool", Some("打招呼"), None),
            text_response("最终回复"),
        ],
        side_effect,
    )
    .await;
    env.write_config(PLAIN_CONFIG);
    env.write_llm(&llm_toml_with(addr.port(), "openai"));

    let output = env.run(&["你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let stderr = stderr_of(&output);
    assert_eq!(
        stderr.matches("# 外部").count(),
        1,
        "外部通知应恰好展示一次：\n{stderr}"
    );
    assert_eq!(
        stderr.matches("% echo hello-tool").count(),
        1,
        "工具命令应恰好展示一次：\n{stderr}"
    );

    let records = read_events(&env.session_db());
    assert_eq!(
        count_source(&records, "notify"),
        1,
        "外部通知应恰好落盘一次：{records:?}"
    );
}

/// 轮询等待 pid 文件出现并返回其中的 pid。
fn wait_for_pid(path: &Path, timeout: std::time::Duration) -> Option<u32> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            return Some(pid);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    None
}

/// 轮询等待进程消失（`kill -0` 返回非零）。
fn wait_for_process_gone(pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !alive {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

/// A3 回归：向 yaa 发 SIGINT 时，应先转发给当前工具进程组并清理，再以 130 退出。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_c_kills_tool_process_group_and_exits_130() {
    let env = TestEnv::new(0);
    env.write_config(PLAIN_CONFIG);
    let pidfile = env._root.path().join("tool.pid");
    let command = format!("echo $$ > '{}'; sleep 30", pidfile.display());

    let (addr, _state) =
        start_server(vec![tool_call_response("call_1", &command, None, None)]).await;
    env.write_llm(&llm_toml_with(addr.port(), "openai"));

    let child = env
        .command()
        .arg("你好")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let yaa_pid = child.id();

    let tool_pid = wait_for_pid(&pidfile, std::time::Duration::from_secs(5))
        .unwrap_or_else(|| panic!("工具进程未写出 pid 文件：{}", pidfile.display()));

    let sent = Command::new("kill")
        .args(["-INT", &yaa_pid.to_string()])
        .status()
        .unwrap();
    assert!(sent.success(), "发送 SIGINT 失败");

    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(130),
        "yaa 应以 130 退出：{:?}\nstderr：{}",
        output.status,
        stderr_of(&output)
    );
    assert!(
        wait_for_process_gone(tool_pid, std::time::Duration::from_secs(5)),
        "工具进程 {tool_pid} 应已被清理"
    );
}

/// 阶段 2 验收 2：`ask` 拦截写 `approval_pending`、stderr 打印待批清单、以 6 退出、
/// **不写** `tool_result`。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_interception_writes_pending_and_exits_six() {
    let (addr, _state) =
        start_server(vec![tool_call_response("call_1", "echo hi", None, None)]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_ASK_CONFIG);

    let output = env.run(&["你好"]);
    assert_eq!(
        output.status.code(),
        Some(6),
        "ask 应以退出码 6 结束：{}",
        stderr_of(&output)
    );
    assert!(stdout_of(&output).is_empty(), "待批回合 stdout 必须干净");

    let stderr = stderr_of(&output);
    assert!(stderr.contains("# 待批准"), "应打印待批清单：{stderr}");
    assert!(stderr.contains("call_1"), "清单应含 call_id：{stderr}");
    assert!(stderr.contains("echo *"), "清单应含命中规则：{stderr}");
    assert!(stderr.contains("% echo hi"), "清单应含可重放命令：{stderr}");

    let records = read_events(&env.session_db());
    let pending = records
        .iter()
        .find(|record| record["type"] == "approval_pending")
        .expect("应落盘 approval_pending");
    assert_eq!(pending["call_id"], "call_1");
    assert_eq!(pending["command"], "echo hi");
    assert_eq!(pending["matched_rule"], "echo *");
    assert!(
        !records.iter().any(|record| record["type"] == "tool_result"),
        "ask 拦截不得写 tool_result：{records:?}"
    );
}

/// 阶段 2 验收 3：`--approve` 执行最旧未决 pending，写 `approval_resolved: approve`
/// 与 `tool_result`，随后继续 loop 至最终回复；不写 `user`、不重跑 `init`。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_executes_oldest_pending_and_continues() {
    let (addr, _state) = start_server(vec![
        tool_call_response("call_1", "echo hi", None, None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_ASK_CONFIG);

    let first = env.run(&["你好"]);
    assert_eq!(
        first.status.code(),
        Some(6),
        "首次运行应待批退出：{}",
        stderr_of(&first)
    );

    let second = env.run(&["--approve"]);
    assert!(second.status.success(), "stderr：{}", stderr_of(&second));
    assert!(
        stdout_of(&second).contains("完成"),
        "--approve 后应继续 loop 并打印最终回复：{}",
        stdout_of(&second)
    );
    let stderr = stderr_of(&second);
    assert!(stderr.contains("# 待批准"), "应展示待批条目：{stderr}");
    assert!(stderr.contains("# 已批准"), "应展示批准条目：{stderr}");

    let records = read_events(&env.session_db());
    let labels: Vec<&str> = records
        .iter()
        .map(|record| record["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        [
            "system",
            "user",
            "ai",
            "tool_call",
            "approval_pending",
            "tool_result",
            "approval_resolved",
            "ai"
        ],
        "记录顺序不符：{records:?}"
    );
    assert_eq!(records[5]["call_id"], "call_1");
    assert_eq!(records[5]["stdout"], "hi\n");
    assert_eq!(records[6]["call_id"], "call_1");
    assert_eq!(records[6]["decision"], "approve");
    assert_eq!(records[7]["content"], "完成");
    assert_eq!(count_source(&records, "init"), 1, "不得重跑 init");
    assert_eq!(
        records.iter().filter(|r| r["type"] == "user").count(),
        1,
        "不得写 user"
    );
}

/// 阶段 2 验收 3：无未决 pending 时 `--approve` 提示并不报错。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_without_pending_is_success_with_notice() {
    let (addr, _state) = start_server(vec![]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_ASK_CONFIG);

    let output = env.run(&["--approve"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));
    assert!(
        stderr_of(&output).contains("没有待批准"),
        "应提示没有待批：{}",
        stderr_of(&output)
    );
    assert!(stdout_of(&output).is_empty());
}

/// 阶段 2 验收 4：`--auto` 自动放行命中 `ask` 的调用；显式 `deny` 仍强制。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_allows_ask_calls_and_completes_loop() {
    let (addr, _state) = start_server(vec![
        tool_call_response("call_1", "echo hi", None, None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_AUTO_DENY_CONFIG);

    let output = env.run(&["--auto", "你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));
    assert!(stdout_of(&output).contains("完成"));

    let records = read_events(&env.session_db());
    let labels: Vec<&str> = records
        .iter()
        .map(|record| record["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        [
            "system",
            "user",
            "ai",
            "tool_call",
            "approval_pending",
            "approval_resolved",
            "tool_result",
            "ai"
        ],
        "--auto 应写 pending + resolved(approve) 后执行：{records:?}"
    );
    assert_eq!(records[5]["decision"], "approve");
    assert_eq!(records[6]["stdout"], "hi\n");
}

/// 阶段 2 验收 4/5：`--auto` 下显式 `deny` 仍强制拒绝，进程不退出，模型下一跳可见。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_still_enforces_explicit_deny() {
    let (addr, state) = start_server(vec![
        tool_call_response("call_1", "rm -rf /tmp/yaa-nope", None, None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_AUTO_DENY_CONFIG);

    let output = env.run(&["--auto", "你好"]);
    assert!(
        output.status.success(),
        "deny 不应导致退出：{}",
        stderr_of(&output)
    );

    let records = read_events(&env.session_db());
    let resolved = records
        .iter()
        .find(|record| record["type"] == "approval_resolved")
        .expect("应写 approval_resolved");
    assert_eq!(resolved["decision"], "deny");
    let result = records
        .iter()
        .find(|record| record["type"] == "tool_result")
        .expect("应写失败 tool_result");
    assert_eq!(result["code"], -1);

    let requests = state.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "deny 后应继续 loop 到下一跳");
    let tool_message = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("第二跳应见 tool 消息");
    assert!(
        tool_message["content"]
            .as_str()
            .unwrap_or_default()
            .contains("拒绝"),
        "被拒结果应对模型可见：{tool_message:?}"
    );
}

/// 阶段 2 验收 6：普通 run 启动时先把旧未决 pending 作废（补 resolved deny + 失败 result）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_prompt_abandons_old_pending() {
    let (addr, _state) = start_server(vec![
        tool_call_response("call_1", "echo hi", None, None),
        text_response("新答"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_ASK_CONFIG);

    let first = env.run(&["第一次"]);
    assert_eq!(
        first.status.code(),
        Some(6),
        "首次运行应待批退出：{}",
        stderr_of(&first)
    );

    let second = env.run(&["新提示词"]);
    assert!(second.status.success(), "stderr：{}", stderr_of(&second));
    assert!(stdout_of(&second).contains("新答"));

    let records = read_events(&env.session_db());
    let deny_index = records
        .iter()
        .position(|record| record["type"] == "approval_resolved" && record["decision"] == "deny")
        .expect("应补 approval_resolved(deny)");
    let result = &records[deny_index + 1];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["call_id"], "call_1");
    assert_eq!(result["code"], -1);
    let user_index = records
        .iter()
        .position(|record| record["type"] == "user" && record["content"] == "新提示词")
        .expect("应有新 user 记录");
    assert!(
        deny_index < user_index,
        "旧待批作废应发生在新 user 落盘之前：{records:?}"
    );
}

/// 阶段 2 验收 11：`--approve` / `--auto` 与短路路径组合是用法错误（退出码 2）。
#[test]
fn approve_and_auto_combination_usage_errors() {
    let env = TestEnv::new(0);
    for args in [
        vec!["--approve", "--notify=注释"],
        vec!["--approve", "--clear"],
        vec!["--approve", "--status"],
        vec!["--approve", "某提示"],
        vec!["--auto", "--notify=注释"],
    ] {
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

/// 阶段 2 验收 9（A5）：末步为空回复时 stdout 不打印本轮非末步正文。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intermediate_text_not_printed_when_final_reply_is_empty() {
    let (addr, _state) = start_server(vec![
        tool_call_with_content_response("call_1", "中间正文", "echo hi"),
        text_response(""),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let output = env.run(&["你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));
    assert!(
        !stdout_of(&output).contains("中间正文"),
        "空回复不得把中间正文当结果：{:?}",
        stdout_of(&output)
    );
}

/// 阶段 2 验收 10（A6）：`--config` 指定的 `[shell]` 限额沿 `init` 钩子内的
/// `yaa --notify` 一致生效（否则截断不会发生）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_shell_limits_flow_into_init_notify() {
    let (addr, _state) = start_server(vec![text_response("完成")]).await;
    let env = TestEnv::new(addr.port());

    // 配置写到非默认路径，只有 `--config` 才可见；若限额未沿钩子链传递，
    // 钩子内的 notify 会退回默认的大上限而不截断。
    let custom_config = env.config_home.join("custom.toml");
    std::fs::write(
        &custom_config,
        r#"
[hooks]
init = '"$YAA_BIN" --notify="截断" -- seq 1 200'

[shell]
timeout_secs = 30
max_stream_chars = 64
head_chars = 32
tail_chars = 16

[agent]
max_steps = 10
"#,
    )
    .unwrap();

    let output = env
        .command()
        .arg("--config")
        .arg(&custom_config)
        .arg("你好")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_events(&env.session_db());
    let notify = records
        .iter()
        .find(|record| record["type"] == "system" && record["source"] == "notify")
        .expect("init 应写入 notify 记录");
    assert!(
        notify["output_files"]["stdout"].as_str().is_some(),
        "自定义限额应让 notify 截断并落盘：{notify:?}"
    );
    assert!(
        notify["content"]
            .as_str()
            .unwrap_or_default()
            .contains("[truncated]"),
        "正文应含截断标记：{notify:?}"
    );
}

/// C5：`--notify 注释 -- cmd`（未用 `=`）仍把注释写入信封，不得静默丢弃。
#[test]
fn notify_space_separated_comment_is_written() {
    let env = TestEnv::new(0);
    let output = env.run(&["--notify", "空格注释", "--", "echo", "hi"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    let records = read_events(&env.session_db());
    assert_eq!(records.len(), 1, "notify 短路路径只写一条记录");
    assert_eq!(records[0]["command"], "echo hi");
    assert_eq!(records[0]["content"], "<空格注释>\nhi\n</空格注释>");
}

/// C4：stdin 含非 UTF-8 字节时不得丢弃输入或失败，非法字节以替换字符呈现。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_utf8_stdin_is_lossy_not_fatal() {
    let (addr, _state) = start_server(vec![text_response("完成")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let output = env.run_with_stdin_bytes(&[], b"\xff\xfe Hello");
    assert!(
        output.status.success(),
        "非 UTF-8 stdin 不得失败：{}",
        stderr_of(&output)
    );
    assert!(stdout_of(&output).contains("完成"));

    let records = read_events(&env.session_db());
    let user = records
        .iter()
        .find(|record| record["type"] == "user")
        .expect("应写 user 记录");
    let content = user["content"].as_str().unwrap();
    assert!(content.contains("Hello"), "已读内容不得丢失：{content}");
    assert!(
        content.contains('\u{FFFD}'),
        "非法字节应呈现为替换字符：{content}"
    );
}

/// C7：注释含换行时信封首行被压成单行，展示标题不退化。
#[test]
fn notify_comment_newlines_are_flattened() {
    let env = TestEnv::new(0);
    let output = env.run(&["--notify=a\nb", "--", "echo", "hi"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    let records = read_events(&env.session_db());
    assert_eq!(records[0]["content"], "<a b>\nhi\n</a b>");
}

/// C6：`--quiet` 仍须保留运维警告（配置不存在）；只抑制过程输出。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiet_keeps_config_missing_warning() {
    let (addr, _state) = start_server(vec![text_response("完成")]).await;
    let env = TestEnv::new(addr.port());
    let missing = env.config_home.join("does-not-exist.toml");

    let output = env
        .command()
        .arg("--quiet")
        .arg("--config")
        .arg(&missing)
        .arg("你好")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));
    assert!(stdout_of(&output).contains("完成"));

    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("配置不存在"),
        "quiet 下配置警告仍须可见：{stderr}"
    );
    assert!(!stderr.contains("---"), "过程块分隔仍应被抑制：{stderr}");
}

// ===== 阶段 2 补充：审批恢复路径与集成边界（独立审查发现的盲区 T1–T5）=====

/// 一次响应携带多个 shell 工具调用（用于「同批多 ask」与「最旧优先」）。
fn multi_tool_call_response(calls: &[(&str, &str)]) -> Value {
    let tool_calls: Vec<Value> = calls
        .iter()
        .map(|(call_id, command)| {
            json!({
                "id": call_id,
                "type": "function",
                "function": {
                    "name": "shell",
                    "arguments": json!({ "command": command }).to_string()
                }
            })
        })
        .collect();
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": tool_calls
            },
            "finish_reason": "tool_calls"
        }]
    })
}

/// 指定原始 `arguments` 文本的 shell 调用（覆盖参数非法 / 非字符串字段）。
fn raw_tool_call_response(call_id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    })
}

/// T1 + T2 + 验收 7：同批多条 `ask` 全部落 pending，`--approve` 每次只放行最旧一条，
/// 未决调用在请求体中渲染为「待批准」而非「中断」，且审批记录不进模型消息投影。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_releases_pending_oldest_first_and_keeps_projection_clean() {
    let (addr, state) = start_server(vec![
        multi_tool_call_response(&[("call_1", "echo one"), ("call_2", "echo two")]),
        text_response("第一答"),
        text_response("第二答"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_ASK_CONFIG);

    let first = env.run(&["你好"]);
    assert_eq!(
        first.status.code(),
        Some(6),
        "同批 ask 应以 6 退出：{}",
        stderr_of(&first)
    );
    let records = read_events(&env.session_db());
    let pendings: Vec<&Value> = records
        .iter()
        .filter(|record| record["type"] == "approval_pending")
        .collect();
    assert_eq!(
        pendings.len(),
        2,
        "同批两条 ask 都要落 pending（不得只落第一条）：{records:?}"
    );
    assert_eq!(pendings[0]["call_id"], "call_1");
    assert_eq!(pendings[1]["call_id"], "call_2");

    // 第一次放行：只执行最旧的 call_1，call_2 仍待批。
    let second = env.run(&["--approve"]);
    assert!(second.status.success(), "stderr：{}", stderr_of(&second));
    assert!(stdout_of(&second).contains("第一答"));
    let records = read_events(&env.session_db());
    let results: Vec<&Value> = records
        .iter()
        .filter(|record| record["type"] == "tool_result")
        .collect();
    assert_eq!(results.len(), 1, "一次只放行最旧一条：{records:?}");
    assert_eq!(results[0]["call_id"], "call_1");

    // 第二跳请求：未决的 call_2 投影为「待批准」，序列仍合法，且不含审批记录。
    let requests = state.requests.lock().unwrap().clone();
    let last = requests.last().expect("应有请求").clone();
    let messages = last["messages"].as_array().unwrap();
    assert_valid_openai_sequence(messages);
    let call_b = messages
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_2")
        .expect("应有 call_2 的 tool 消息");
    let content = call_b["content"].as_str().unwrap_or_default();
    assert!(
        content.contains("待批准"),
        "未决 pending 应渲染为待批准：{call_b:?}"
    );
    assert!(
        !content.contains("interrupted"),
        "不得伪装成中断：{call_b:?}"
    );
    let body = last.to_string();
    assert!(
        !body.contains("approval_pending") && !body.contains("approval_resolved"),
        "审批记录不得进入请求体：{body}"
    );

    // 第二次放行：执行 call_2。
    let third = env.run(&["--approve"]);
    assert!(third.status.success(), "stderr：{}", stderr_of(&third));
    assert!(stdout_of(&third).contains("第二答"));
    let records = read_events(&env.session_db());
    assert_eq!(
        records
            .iter()
            .filter(|record| record["type"] == "tool_result")
            .count(),
        2,
        "第二次应执行 call_2：{records:?}"
    );

    // 第三次：已无待批。
    let fourth = env.run(&["--approve"]);
    assert!(fourth.status.success(), "stderr：{}", stderr_of(&fourth));
    assert!(
        stderr_of(&fourth).contains("没有待批准"),
        "应提示无待批：{}",
        stderr_of(&fourth)
    );
}

/// T3（A4）：`tool_call.arguments` 非字符串应报模型错误（退出码 4），且不落盘半截记录。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_string_tool_arguments_exit_model_error() {
    let bad = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "shell", "arguments": { "command": "echo hi" } }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    let (addr, _state) = start_server(vec![bad]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let output = env.run(&["你好"]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "非字符串 arguments 应为模型错误：{}",
        stderr_of(&output)
    );
    let records = read_events(&env.session_db());
    assert!(
        !records
            .iter()
            .any(|record| record["type"] == "ai" || record["type"] == "tool_call"),
        "非法调用不得落盘：{records:?}"
    );
}

/// T4a：未知工具名（命中 `ask` 规则）不得进入待批，而应落失败结果并继续循环。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_tool_fails_without_pending() {
    let (addr, _state) = start_server(vec![
        tool_call_response_named("call_1", "other", "touch pwn", None, None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_ASK_CONFIG);

    let output = env.run(&["你好"]);
    assert!(
        output.status.success(),
        "未知工具应落失败结果而非待批退出：{}",
        stderr_of(&output)
    );
    assert!(stdout_of(&output).contains("完成"));

    let records = read_events(&env.session_db());
    assert!(
        !records
            .iter()
            .any(|record| record["type"] == "approval_pending"),
        "未知工具不得产生可放行的 pending：{records:?}"
    );
    let result = records
        .iter()
        .find(|record| record["type"] == "tool_result")
        .expect("应落失败 tool_result");
    assert_eq!(result["code"], -1, "未知工具应为失败结果：{result:?}");
    assert!(
        result["stderr"]
            .as_str()
            .unwrap_or_default()
            .contains("不支持的工具"),
        "失败原因应说明工具不受支持：{result:?}"
    );
    assert!(
        !env.work.join("pwn").exists(),
        "未知工具的命令绝不能被 `--approve` 或拦截路径执行"
    );
}

/// T4b：`arguments` 是合法 JSON 但 `command` 非字符串（解析失败）不得进入待批。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_command_field_fails_without_pending() {
    let (addr, _state) = start_server(vec![
        raw_tool_call_response("call_1", "shell", "{\"command\": 123}"),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_ASK_CONFIG);

    let output = env.run(&["你好"]);
    assert!(
        output.status.success(),
        "参数非法应落失败结果而非待批退出：{}",
        stderr_of(&output)
    );
    let records = read_events(&env.session_db());
    assert!(
        !records
            .iter()
            .any(|record| record["type"] == "approval_pending"),
        "参数非法不得产生 pending：{records:?}"
    );
    let result = records
        .iter()
        .find(|record| record["type"] == "tool_result")
        .expect("应落失败 tool_result");
    assert_eq!(result["code"], -1);
}

/// T4c（验收 C1）：`--approve` 认 `YAA_SESSION`，作用于该会话而非默认会话。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_honors_env_session_target() {
    let (addr, _state) = start_server(vec![
        tool_call_response("call_1", "echo hi", None, None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_ASK_CONFIG);
    let target = env.work.join("explicit.db");

    let first = env.run(&["--session", target.to_str().unwrap(), "你好"]);
    assert_eq!(
        first.status.code(),
        Some(6),
        "--session 目标应待批退出：{}",
        stderr_of(&first)
    );
    assert!(
        read_events(&target)
            .iter()
            .any(|record| record["type"] == "approval_pending"),
        "pending 应落在 --session 指定的会话"
    );

    // 只设 YAA_SESSION（不传 --session）：--approve 必须作用于该会话。
    let approved = env
        .command()
        .env("YAA_SESSION", &target)
        .arg("--approve")
        .output()
        .unwrap();
    assert!(
        approved.status.success(),
        "YAA_SESSION 指向的会话应被放行：{}",
        stderr_of(&approved)
    );
    assert!(stdout_of(&approved).contains("完成"));
    let resolved = read_events(&target)
        .into_iter()
        .find(|record| record["type"] == "approval_resolved")
        .expect("应写 approval_resolved");
    assert_eq!(resolved["decision"], "approve");
    assert!(
        !env.session_db().exists(),
        "不得越界操作默认会话：{:?}",
        env.session_db()
    );
}

/// T5：`deny` 路径的 `approval_pending` 必须记录命中的规则文本。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deny_records_matched_rule() {
    let (addr, _state) = start_server(vec![
        tool_call_response("call_1", "rm -rf /tmp/yaa-nope", None, None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_AUTO_DENY_CONFIG);

    let output = env.run(&["你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));
    let records = read_events(&env.session_db());
    let pending = records
        .iter()
        .find(|record| record["type"] == "approval_pending")
        .expect("deny 应写 approval_pending");
    assert_eq!(
        pending["matched_rule"], "rm *",
        "deny 的 pending 应记录命中规则：{pending:?}"
    );
    let resolved = records
        .iter()
        .find(|record| record["type"] == "approval_resolved")
        .expect("deny 应写 approval_resolved");
    assert_eq!(resolved["decision"], "deny");
}

/// T4（回归）：单个 `&` 拆段使 `deny` 不被整段前缀绕过。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deny_not_bypassed_by_single_ampersand() {
    let (addr, _state) = start_server(vec![
        tool_call_response("call_1", "true & rm -rf marker", None, None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_AUTO_DENY_CONFIG);
    std::fs::write(env.work.join("marker"), "keep").unwrap();

    let output = env.run(&["你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));
    let records = read_events(&env.session_db());
    let resolved = records
        .iter()
        .find(|record| record["type"] == "approval_resolved")
        .expect("应写 approval_resolved");
    assert_eq!(
        resolved["decision"], "deny",
        "`&` 之后的 rm 必须被 deny 拦截：{records:?}"
    );
    assert!(
        env.work.join("marker").exists(),
        "被拒命令不得执行（marker 应保留）"
    );
}

/// T4（回归）：带引号的 env 前缀不绕过审批（`FOO="a b" echo x` 仍命中 `echo *`）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quoted_env_prefix_does_not_bypass_approval() {
    let (addr, _state) = start_server(vec![
        tool_call_response("call_1", "FOO=\"a b\" echo bypass", None, None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_ASK_CONFIG);

    let output = env.run(&["你好"]);
    assert_eq!(
        output.status.code(),
        Some(6),
        "引号 env 前缀应仍命中 ask：{}",
        stderr_of(&output)
    );
    assert!(
        read_events(&env.session_db())
            .iter()
            .any(|record| record["type"] == "approval_pending"),
        "应落 approval_pending"
    );
}

/// 阶段 3 验收 5–7、9：历史超阈值时压缩为侧车摘要，投影被替换、既有会话记录不被
/// 改写，并在 stderr 展示一条压缩条目。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_summarizes_history_into_sidecar_and_keeps_records() {
    // 第一次运行：压缩未启用，制造两轮大工具输出作为历史。
    let (first_addr, _first_state) = start_server(vec![
        tool_call_response("c1", "seq 1 4000", None, None),
        tool_call_response("c2", "seq 9000 13000", None, None),
        text_response("第一轮完成"),
    ])
    .await;
    let env = TestEnv::new(first_addr.port());
    env.write_config(
        r#"
[hooks]
init = "true"

[agent]
max_steps = 10
"#,
    );
    let first = env.run(&["第一问"]);
    assert!(
        first.status.success(),
        "首次运行失败：{}",
        stderr_of(&first)
    );

    // 第二次运行：启用压缩（pruner 预算调大以免干扰本用例），既有历史立即超阈值。
    let (second_addr, second_state) = start_server(vec![
        text_response("## Primary Request and Intent\n- 回顾早前目标"),
        text_response("最终答复"),
    ])
    .await;
    // 窗口随 llm 供应配置提供；config.toml 只留压缩策略。
    env.write_llm(&format!(
        "{}context_window = 800\n",
        llm_toml_with(second_addr.port(), "openai")
    ));
    env.write_config(
        r#"
[hooks]
init = "true"

[compaction]
threshold_ratio = 0.5
retain_ratio = 0.1
chars_per_token = 4
prune_threshold_chars = 1000000
prune_head_chars = 1000
prune_tail_chars = 500

[agent]
max_steps = 10
"#,
    );

    let session = env.session_db();
    let before = yaa_store::load(&yaa_store::Session::new(session.clone())).unwrap();
    let output = env.run(&["第二问"]);
    assert!(
        output.status.success(),
        "压缩运行失败：{}",
        stderr_of(&output)
    );

    // 恰好一次摘要 + 一次主调用，顺序为「摘要 → 主」。
    let requests = second_state.requests.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        2,
        "应恰好一次摘要 + 一次主调用：{requests:?}"
    );
    let instruction = requests[0]["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap_or_default();
    assert!(
        instruction.contains("compaction engine"),
        "首个请求末条应为压缩指令：{instruction}"
    );
    let main_body = serde_json::to_string(&requests[1]["messages"]).unwrap();
    assert!(
        main_body.contains("<compacted-summary>"),
        "主请求应携带摘要检查点"
    );
    // 第一轮（`seq 1 4000`）被遮蔽，第二轮（`seq 9000 13000`）逐字保留。
    assert!(
        main_body.contains("9000") && !main_body.contains("4000"),
        "被遮蔽区间应替换为摘要、保留区应逐字保留；roles={:?}",
        requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| {
                let content = message["content"].as_str().unwrap_or("");
                (
                    message["role"].as_str().unwrap_or("").to_string(),
                    content.len(),
                    content.contains("4000"),
                    content.contains("9000"),
                )
            })
            .collect::<Vec<_>>()
    );

    // 侧车摘要落盘；既有记录逐条保留（只追加本次记录）。
    let summaries = session.parent().unwrap().join("summaries");
    let files: Vec<_> = std::fs::read_dir(&summaries)
        .unwrap()
        .filter_map(|item| item.ok())
        .map(|item| item.path())
        .collect();
    assert_eq!(files.len(), 1, "应生成 1 条侧车摘要：{files:?}");
    let after = yaa_store::load(&yaa_store::Session::new(session.clone())).unwrap();
    assert!(
        after.len() > before.len(),
        "压缩只追加记录，不删改历史：{} → {}",
        before.len(),
        after.len()
    );
    assert_eq!(
        after[..before.len()],
        before[..],
        "压缩前的记录必须逐条保留"
    );

    // 展示：stderr 出现一条压缩条目。
    assert!(
        stderr_of(&output).contains("已压缩早期历史"),
        "stderr 应展示压缩条目：{}",
        stderr_of(&output)
    );
    assert_eq!(stdout_of(&output).trim(), "最终答复");
}

/// 阶段 3 验收 10：`--json` 把展示事件输出为带 `type` 的 JSONL，最终回复也在其中。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_outputs_jsonl_event_stream() {
    let (addr, _state) = start_server(vec![text_response("结构化答复")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let output = env.run(&["--json", "你好"]);
    assert!(output.status.success(), "stderr={}", stderr_of(&output));

    let stdout = stdout_of(&output);
    let events: Vec<Value> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("每行必须是合法 JSON"))
        .collect();
    assert!(!events.is_empty(), "JSONL 不应为空：{stdout:?}");
    assert!(
        events.iter().all(|event| event.get("type").is_some()),
        "每个事件都应带 type 标签：{events:?}"
    );
    assert!(
        events.iter().any(|event| {
            event["type"] == "text"
                && event["final_step"] == true
                && event["content"] == "结构化答复"
        }),
        "应含最终回复的 text 事件：{events:?}"
    );
    assert!(
        stderr_of(&output).trim().is_empty(),
        "`--json` 下诊断也走 stdout，stderr 应为空：{}",
        stderr_of(&output)
    );
}

/// 阶段 3 验收 11：`--no-store` 用临时会话运行，不读写既有会话，退出即淘。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_store_uses_temporary_session_without_touching_existing() {
    let (addr, state) = start_server(vec![text_response("首次答复")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    // 先用普通运行创建一个既有会话。
    let first = env.run(&["首次提问"]);
    assert!(first.status.success(), "stderr={}", stderr_of(&first));
    let session = env.session_db();
    assert!(session.exists(), "普通运行应创建会话数据库");
    let before = read_events(&session);

    // 复用同一服务端，追加一个响应给临时运行。
    state
        .responses
        .lock()
        .unwrap()
        .push_back(text_response("临时答复"));

    let output = env.run(&["--no-store", "临时提问"]);
    assert!(output.status.success(), "stderr={}", stderr_of(&output));
    assert_eq!(stdout_of(&output).trim(), "临时答复");

    // 既有会话记录未变；数据根只保留普通运行创建的会话目录。
    assert_eq!(
        read_events(&session),
        before,
        "`--no-store` 不得改动既有会话"
    );
    let session_dirs: Vec<_> = std::fs::read_dir(env.data_home.join("sessions"))
        .unwrap()
        .filter_map(|item| item.ok())
        .map(|item| item.file_name())
        .collect();
    assert_eq!(
        session_dirs.len(),
        1,
        "只应有普通运行创建的会话目录：{session_dirs:?}"
    );
    let session_dir = session.parent().unwrap();
    assert!(
        !session_dir.join("summaries").exists() && !session_dir.join("outputs").exists(),
        "临时运行不得在既有会话目录留下 summaries/outputs"
    );
}

// ===== 存储迁移：旧格式、非 yaa 库、锁与并发 =====

/// 旧 `.jsonl` 会话在普通 run 前被拒绝，并提示 `--clear`。
#[test]
fn run_rejects_legacy_jsonl_session() {
    let env = TestEnv::new(0);
    std::fs::write(
        env.work.join("legacy.jsonl"),
        b"{\"type\":\"user\",\"id\":\"01J\",\"ts\":1,\"content\":\"x\"}\n",
    )
    .unwrap();

    let output = env.run(&["--session", "legacy.jsonl", "你好"]);
    assert_ne!(output.status.code(), Some(0), "旧格式应报错");
    let stderr = stderr_of(&output);
    assert!(stderr.contains("旧版 JSONL"), "应说明旧版 JSONL：{stderr}");
    assert!(stderr.contains("--clear"), "应提示 --clear：{stderr}");
}

/// 非 yaa 的普通 SQLite 库绝不能被当作会话读取或写入。
#[test]
fn status_rejects_non_yaa_database() {
    let env = TestEnv::new(0);
    let db = env.work.join("plain.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE other (x INTEGER);")
        .unwrap();
    drop(conn);

    let output = env.run(&["--status", "--session", db.to_str().unwrap()]);
    assert_ne!(output.status.code(), Some(0), "非 yaa 库应报错");
    assert!(
        stderr_of(&output).contains("不是 yaa 会话数据库"),
        "stderr：{}",
        stderr_of(&output)
    );
}

/// `--notify` 目标不存在时先告警再创建。
#[test]
fn notify_warns_and_creates_missing_session() {
    let env = TestEnv::new(0);
    assert!(!env.session_db().exists());

    let output = env.run(&["--notify=新", "--", "printf", "hi"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("目标会话不存在"),
        "应告警目标不存在：{}",
        stderr_of(&output)
    );
    let records = read_events(&env.session_db());
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["content"], "<新>\nhi\n</新>");
}

/// 已归档的数据库拒绝写入，通知不得静默丢失。
#[test]
fn notify_rejects_archived_session() {
    let env = TestEnv::new(0);
    let db = env.session_db();
    seed(&db, &[yaa_msg::Record::user("既有")]);
    let session = yaa_store::Session::new(db.clone());
    yaa_store::set_archived(&session).unwrap();

    let output = env.run(&["--notify=x", "--", "printf", "y"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("已归档"),
        "stderr：{}",
        stderr_of(&output)
    );
    assert_eq!(read_events(&db).len(), 1, "归档库不得新增记录");
}

/// 归档当前（默认）会话并返回归档数据库路径。
fn archive_current_session(env: &TestEnv) -> PathBuf {
    let cleared = env.run(&["--clear"]);
    assert!(
        cleared.status.success(),
        "归档失败，stderr：{}",
        stderr_of(&cleared)
    );
    let printed = stdout_of(&cleared).trim().to_string();
    assert!(!printed.is_empty(), "归档应打印路径");
    PathBuf::from(printed)
}

/// S3：归档库上的 run 在写 user / init / 执行前拒绝，退出 1 且归档库不变。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_on_archived_session_is_rejected() {
    let (addr, _state) = start_server(vec![text_response("不应调用")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    seed(&db, &[yaa_msg::Record::user("既有")]);
    let archived = archive_current_session(&env);
    let before = read_events(&archived);

    let output = env.run(&["--session", archived.to_str().unwrap(), "你好"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("已归档"),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(stdout_of(&output).is_empty(), "归档库不得产生最终回复");
    assert_eq!(read_events(&archived), before, "归档库不得被修改");
}

/// S3：归档库上的 `--approve` 不执行 pending 命令、不写记录。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_on_archived_session_is_rejected() {
    let (addr, _state) = start_server(vec![]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_ASK_CONFIG);
    let db = env.session_db();
    let marker = env.work.join("approve-marker");
    seed(
        &db,
        &[
            yaa_msg::Record::tool_call(
                yaa_msg::ToolOrigin::Ai,
                "c1",
                format!("touch {}", marker.display()),
                None,
            ),
            yaa_msg::Record::approval_pending(
                "c1",
                format!("touch {}", marker.display()),
                "touch *",
            ),
        ],
    );
    let archived = archive_current_session(&env);
    let before = read_events(&archived);

    let output = env.run(&["--approve", "--session", archived.to_str().unwrap()]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("已归档"),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(!marker.exists(), "归档库不得执行 pending 命令");
    assert_eq!(read_events(&archived), before, "归档库不得被修改");
}

/// S3：`--status` 仍可只读归档库。
#[test]
fn status_reads_archived_session() {
    let env = TestEnv::new(0);
    let db = env.session_db();
    seed(&db, &[yaa_msg::Record::user("既有")]);
    let archived = archive_current_session(&env);

    let output = env.run(&["--status", "--session", archived.to_str().unwrap()]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(stdout.contains("记录："), "stdout：{stdout}");
    assert!(
        stdout.contains(&archived.to_string_lossy().to_string()),
        "stdout：{stdout}"
    );
}

/// 已有 runner 持锁时，普通 run 明确拒绝且不写记录。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_refuses_when_runner_lock_is_held() {
    let (addr, _state) = start_server(vec![text_response("不应调用")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let session = yaa_store::Session::new(env.session_db());
    std::fs::create_dir_all(&session.root).unwrap();
    let _held = yaa_store::FileLock::try_exclusive(&session.runner_lock_path())
        .unwrap()
        .expect("测试进程应能先持锁");

    let output = env.run(&["你好"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("会话正在运行"),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(!session.db.exists(), "持锁失败时不得创建/写入会话");
}

/// runner 持锁时 `--clear` 拒绝归档，且主库记录不变。
#[test]
fn clear_refuses_while_runner_lock_is_held() {
    let env = TestEnv::new(0);
    let db = env.session_db();
    seed(&db, &[yaa_msg::Record::user("保留")]);
    let session = yaa_store::Session::new(db.clone());
    let _held = yaa_store::FileLock::try_exclusive(&session.runner_lock_path())
        .unwrap()
        .expect("测试进程应能先持锁");

    let output = env.run(&["--clear"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("无法归档"),
        "stderr：{}",
        stderr_of(&output)
    );
    assert_eq!(read_events(&db).len(), 1, "拒绝归档时不得清空记录");
    assert!(!env.data_home.join("history").exists(), "不得产生归档");
}

/// 8 个进程并发 `--notify`：记录不丢、不重、无半条。
#[test]
fn concurrent_notify_processes_do_not_lose_records() {
    let env = TestEnv::new(0);
    let mut children = Vec::new();
    for index in 0..8 {
        let mut command = env.command();
        command
            .arg("--notify=并发")
            .arg("--")
            .arg("printf")
            .arg(format!("N{index}"))
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        children.push(command.spawn().unwrap());
    }
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "并发 notify 失败：{}",
            stderr_of(&output)
        );
    }

    let records = read_events(&env.session_db());
    assert_eq!(
        count_source(&records, "notify"),
        8,
        "记录不得丢失：{records:?}"
    );
    let ids: HashSet<&str> = records
        .iter()
        .map(|record| record["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 8, "记录 id 不得重复");
    for index in 0..8 {
        assert!(
            records.iter().any(|record| record["content"]
                .as_str()
                .is_some_and(|content| content.contains(&format!("N{index}")))),
            "缺少 N{index} 的通知：{records:?}"
        );
    }
}

/// 写者活跃（正在执行 notify 命令）时 `--status` 仍能正常返回。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reads_while_writer_is_active() {
    let env = TestEnv::new(0);
    let first = env.run(&["--notify=初始", "--", "printf", "init"]);
    assert!(first.status.success(), "stderr：{}", stderr_of(&first));

    let pidfile = env._root.path().join("writer.pid");
    let command = format!("echo $$ > '{}'; sleep 2", pidfile.display());
    let writer = env
        .command()
        .args(["--notify=慢", "--", "sh", "-c", &command])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_pid(&pidfile, std::time::Duration::from_secs(5))
        .unwrap_or_else(|| panic!("写者未启动：{}", pidfile.display()));

    let started = std::time::Instant::now();
    let status = env.run(&["--status"]);
    assert!(status.status.success(), "stderr：{}", stderr_of(&status));
    assert!(stdout_of(&status).contains("记录："));
    assert!(
        started.elapsed() < std::time::Duration::from_millis(1500),
        "status 不应等待写者：{:?}",
        started.elapsed()
    );

    let writer = writer.wait_with_output().unwrap();
    assert!(writer.status.success(), "写者应正常结束");
    assert_eq!(count_source(&read_events(&env.session_db()), "notify"), 2);
}

/// S1：慢 notify 命令执行期间并发 `--clear` 不应等待命令结束；总量守恒且两边库完整。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_conserves_records_while_slow_notify_command_runs() {
    let env = TestEnv::new(0);
    // 前置一条，确保库已存在、`--clear` 会真正归档。
    let first = env.run(&["--notify=前置", "--", "printf", "pre"]);
    assert!(first.status.success(), "stderr：{}", stderr_of(&first));

    let pidfile = env._root.path().join("slow-notify.pid");
    let command = format!("echo $$ > '{}'; sleep 2", pidfile.display());
    let writer = env
        .command()
        .args(["--notify=慢", "--", "sh", "-c", &command])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_pid(&pidfile, std::time::Duration::from_secs(5))
        .unwrap_or_else(|| panic!("慢通知未启动：{}", pidfile.display()));

    // notify 命令执行阶段不持 clear.lock，clear 应迅速完成而非等待 2 秒命令。
    let started = std::time::Instant::now();
    let cleared = env.run(&["--clear"]);
    assert!(cleared.status.success(), "stderr：{}", stderr_of(&cleared));
    assert!(
        started.elapsed() < std::time::Duration::from_millis(1500),
        "clear 不应等待 notify 命令结束：{:?}",
        started.elapsed()
    );

    let writer = writer.wait_with_output().unwrap();
    assert!(
        writer.status.success(),
        "慢通知应成功：{}",
        stderr_of(&writer)
    );

    // 总量守恒：两次成功的 notify 要么进归档、要么进新库。
    let archived = PathBuf::from(stdout_of(&cleared).trim());
    assert!(archived.exists(), "归档库应存在：{}", archived.display());
    let archived_count = count_source(&read_events(&archived), "notify");
    let live_count = count_source(&read_events(&env.session_db()), "notify");
    assert_eq!(
        archived_count + live_count,
        2,
        "通知总数应守恒（归档 {archived_count} + 新库 {live_count}）"
    );
    assert!(integrity_ok(&archived), "归档库 integrity 应为 ok");
    assert!(integrity_ok(&env.session_db()), "新库 integrity 应为 ok");
}
