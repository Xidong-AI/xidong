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

/// 步数上限为 1：用于验证工具执行期注入的 User 会重置步数预算。
const SINGLE_STEP_CONFIG: &str = r#"
[hooks]
init = "true"

[agent]
max_steps = 1
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
        self.session_db_for(&self.work)
    }

    /// 指定工作目录对应的会话数据库。
    fn session_db_for(&self, work: &Path) -> PathBuf {
        let canon = work.canonicalize().unwrap();
        let mut hasher = Sha256::new();
        hasher.update(canon.as_os_str().as_encoded_bytes());
        let digest = format!("{:x}", hasher.finalize());
        let sid = &digest[..32];
        self.data_home.join("sessions").join(sid).join("session.db")
    }

    /// 默认会话的文件 inbox 根目录：`<session.db 父目录>/inbox`。
    fn inbox_root(&self) -> PathBuf {
        self.session_db()
            .parent()
            .expect("session.db 应有父目录")
            .join("inbox")
    }

    /// 读取指定 phase 的 inbox 消息（`scan_inbox`，按文件名升序），转为 JSON 值。
    ///
    /// `--notify` 改为只发布后，断言注入内容应经此读取而非直接读 events。
    fn read_inbox(&self, phase: &str) -> Vec<Value> {
        let phase = match phase {
            "init" => yaa_store::InboxPhase::Init,
            "pending" => yaa_store::InboxPhase::Pending,
            other => panic!("未知 inbox phase：{other}"),
        };
        let session = yaa_store::Session::new(self.session_db());
        yaa_store::scan_inbox(&session, phase)
            .unwrap()
            .iter()
            .map(|message| serde_json::to_value(&message.record).unwrap())
            .collect()
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

/// 读取任意会话数据库指定 phase 的 inbox 消息（跨会话 `--to` 断言用）。
fn read_inbox_at(db: &Path, phase: &str) -> Vec<Value> {
    let phase = match phase {
        "init" => yaa_store::InboxPhase::Init,
        "pending" => yaa_store::InboxPhase::Pending,
        other => panic!("未知 inbox phase：{other}"),
    };
    let session = yaa_store::Session::new(db);
    yaa_store::scan_inbox(&session, phase)
        .unwrap()
        .iter()
        .map(|message| serde_json::to_value(&message.record).unwrap())
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
    for needle in [
        "shell",
        "command",
        "description",
        "YAA_SESSION",
        "YAA_PARENT",
        "YAA_NOTIFY",
    ] {
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
    let records = env.read_inbox("pending");
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
    let records = env.read_inbox("pending");
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
    let records = env.read_inbox("pending");
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

    let records = env.read_inbox("pending");
    assert_eq!(records.len(), 1, "notify 只发布一条 inbox 消息");
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
    let records = env.read_inbox("pending");
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

    let records = env.read_inbox("pending");
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

    let relative = env
        .read_inbox("pending")
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

/// 阶段 5 环境链：shell 工具子进程看到 `YAA_SESSION`/`YAA_NOTIFY` = 本会话库、
/// `YAA_NOTIFY_CALL_ID` = 本次调用 id，且继承的外部 `YAA_SESSION` 记为 `YAA_PARENT`
/// （模拟嵌套子智能体）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_tool_env_chain_injects_notify_and_parent() {
    let env = TestEnv::new(0);
    env.write_config(PLAIN_CONFIG);
    let parent = env._root.path().join("parent").join("session.db");

    let command = r#"printf '%s\n%s\n%s\n%s\n' "$YAA_SESSION" "$YAA_NOTIFY" "${YAA_PARENT:-<unset>}" "$YAA_NOTIFY_CALL_ID""#;
    let (addr, _state) = start_server(vec![
        tool_call_response("call_1", command, None, None),
        text_response("完成"),
    ])
    .await;
    env.write_llm(&llm_toml_with(addr.port(), "openai"));

    let output = env
        .command()
        .env("YAA_SESSION", &parent)
        .env_remove("YAA_NOTIFY")
        .env_remove("YAA_PARENT")
        .arg("你好")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let self_db = env.session_db().to_string_lossy().into_owned();
    let parent_str = parent.to_string_lossy().into_owned();
    let records = read_events(&env.session_db());
    let result = records
        .iter()
        .find(|record| record["type"] == "tool_result")
        .expect("应有工具结果");
    let lines: Vec<&str> = result["stdout"].as_str().unwrap().lines().collect();
    assert_eq!(
        lines,
        vec![
            self_db.as_str(),
            self_db.as_str(),
            parent_str.as_str(),
            "call_1"
        ],
        "工具子进程环境链应为 SESSION=自己、NOTIFY=自己、PARENT=继承会话、CALL_ID=本条调用"
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
    let records = env.read_inbox("pending");
    assert_eq!(records.len(), 1, "notify 只发布一条 inbox 消息");
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
    let records = env.read_inbox("pending");
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
    let records = env.read_inbox("pending");
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

// ===== 阶段 5 刀 2：`--to` 与严格校验 =====

/// 显式 `--to` 指向不存在的路径：退出 1，且绝不创建目标目录或回落到默认会话。
#[test]
fn notify_to_missing_target_is_rejected_without_creation() {
    let env = TestEnv::new(0);
    let missing_dir = env._root.path().join("missing");
    let missing_db = missing_dir.join("session.db");

    let output = env.run(&[
        "--to",
        missing_db.to_str().unwrap(),
        "--notify=x",
        "--",
        "printf",
        "y",
    ]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr：{}",
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(stderr.contains("目标会话不存在"), "stderr：{stderr}");
    assert!(stderr.contains("不会创建新会话"), "stderr：{stderr}");
    assert!(
        !missing_dir.exists(),
        "显式 --to 不得创建目标目录：{}",
        missing_dir.display()
    );
    assert!(
        env.read_inbox("pending").is_empty(),
        "失败的通知不得回落到默认会话"
    );
}

/// 显式 `--to` 指向非 yaa 的 SQLite 库：退出 1，错误原样来自 store 校验；
/// 且目标目录不得新增 `init.lock`/`-wal`/`-shm`（F6：只读校验不创建任何文件）。
#[test]
fn notify_to_non_yaa_database_is_rejected() {
    let env = TestEnv::new(0);
    let target_dir = env._root.path().join("plain");
    std::fs::create_dir_all(&target_dir).unwrap();
    let db = target_dir.join("session.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE other (x INTEGER);")
        .unwrap();
    drop(conn);
    let before = directory_entries(&target_dir);

    let output = env.run(&[
        "--to",
        db.to_str().unwrap(),
        "--notify=x",
        "--",
        "printf",
        "y",
    ]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("不是 yaa 会话数据库"),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(read_inbox_at(&db, "pending").is_empty());
    assert_eq!(
        directory_entries(&target_dir),
        before,
        "显式 --to 校验失败不得创建 init.lock/-wal/-shm"
    );
}

/// 目录下的条目名集合（排序后），用于断言「没有新增文件」。
fn directory_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// 显式 `--to` 指向已归档会话：退出 1，且不写入 inbox。
#[test]
fn notify_to_archived_session_is_rejected() {
    let env = TestEnv::new(0);
    let db = env._root.path().join("target").join("session.db");
    seed(&db, &[yaa_msg::Record::user("既有")]);
    yaa_store::set_archived(&yaa_store::Session::new(db.clone())).unwrap();

    let output = env.run(&[
        "--to",
        db.to_str().unwrap(),
        "--notify=x",
        "--",
        "printf",
        "y",
    ]);
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
    assert!(read_inbox_at(&db, "pending").is_empty());
}

/// 显式 `--to` 指向旧 `.jsonl`：退出 1，并提示 `--clear`。
#[test]
fn notify_to_legacy_jsonl_is_rejected() {
    let env = TestEnv::new(0);
    let legacy = env._root.path().join("old.jsonl");
    std::fs::write(&legacy, b"{}\n").unwrap();

    let output = env.run(&[
        "--to",
        legacy.to_str().unwrap(),
        "--notify=x",
        "--",
        "printf",
        "y",
    ]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr：{}",
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(stderr.contains("旧版 JSONL"), "stderr：{stderr}");
    assert!(stderr.contains("--clear"), "stderr：{stderr}");
}

/// 显式 `--to` 指向缺少 `session.db` 的目录：退出 1，且不创建 `session.db`。
#[test]
fn notify_to_directory_without_session_db_is_rejected() {
    let env = TestEnv::new(0);
    let target = env._root.path().join("empty-target");
    std::fs::create_dir_all(&target).unwrap();
    let db = target.join("session.db");

    let output = env.run(&[
        "--to",
        target.to_str().unwrap(),
        "--notify=x",
        "--",
        "printf",
        "y",
    ]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr：{}",
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(stderr.contains("目标会话不存在"), "stderr：{stderr}");
    assert!(
        stderr.contains(&db.to_string_lossy().to_string()),
        "应指明探测路径：{stderr}"
    );
    assert!(!db.exists(), "不得在目标目录创建 session.db");
}

/// 显式 `--to` 跨会话投递：消息写入目标 `pending/`，默认会话保持干净。
#[test]
fn notify_to_explicit_session_delivers_to_target_inbox() {
    let env = TestEnv::new(0);
    let target_dir = env._root.path().join("target");
    let target_db = target_dir.join("session.db");
    seed(&target_db, &[yaa_msg::Record::user("既有")]);

    let output = env.run(&[
        "--to",
        target_dir.to_str().unwrap(),
        "--notify=跨会话",
        "--",
        "printf",
        "X",
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );

    let pending = read_inbox_at(&target_db, "pending");
    assert_eq!(pending.len(), 1, "目标 inbox：{pending:?}");
    assert_eq!(pending[0]["content"], "<跨会话>\nX\n</跨会话>");
    assert!(
        read_inbox_at(&target_db, "init").is_empty(),
        "跨会话目标不得写 init/"
    );
    assert!(
        env.read_inbox("pending").is_empty(),
        "默认会话不得收到跨会话通知"
    );
    assert_eq!(
        read_events(&target_db).len(),
        1,
        "--notify 只发布，不得直接写 events"
    );
}

/// 显式 `--to` 即使环境标记为 init 阶段，也一律写目标 `pending/`。
#[test]
fn notify_to_explicit_target_ignores_init_phase() {
    let env = TestEnv::new(0);
    let target_db = env._root.path().join("target").join("session.db");
    seed(&target_db, &[yaa_msg::Record::user("既有")]);

    let output = env
        .command()
        .env("YAA_INBOX_PHASE", "init")
        .args([
            "--to",
            target_db.to_str().unwrap(),
            "--notify=初始化",
            "--",
            "printf",
            "i",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    assert_eq!(read_inbox_at(&target_db, "pending").len(), 1);
    assert!(read_inbox_at(&target_db, "init").is_empty());
}

/// 显式 `--to` 与 `--session` 指向不同会话：用法错误（退出 2），两边都不写。
#[test]
fn notify_to_and_session_conflict_is_usage_error() {
    let env = TestEnv::new(0);
    let left_db = env._root.path().join("left").join("session.db");
    seed(&left_db, &[yaa_msg::Record::user("左")]);
    let right_db = env._root.path().join("right").join("session.db");
    seed(&right_db, &[yaa_msg::Record::user("右")]);

    let output = env.run(&[
        "--to",
        left_db.to_str().unwrap(),
        "--session",
        right_db.to_str().unwrap(),
        "--notify=x",
        "--",
        "printf",
        "y",
    ]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr：{}",
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(stderr.contains("--to"), "stderr：{stderr}");
    assert!(stderr.contains("--session"), "stderr：{stderr}");
    assert!(read_inbox_at(&left_db, "pending").is_empty());
    assert!(read_inbox_at(&right_db, "pending").is_empty());
}

/// `--to` 目录与 `--session` 数据库指向同一会话时允许。
#[test]
fn notify_to_and_session_same_target_is_allowed() {
    let env = TestEnv::new(0);
    let target_dir = env._root.path().join("target");
    let target_db = target_dir.join("session.db");
    seed(&target_db, &[yaa_msg::Record::user("既有")]);

    let output = env.run(&[
        "--to",
        target_dir.to_str().unwrap(),
        "--session",
        target_db.to_str().unwrap(),
        "--notify=同目标",
        "--",
        "printf",
        "X",
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    assert_eq!(read_inbox_at(&target_db, "pending").len(), 1);
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

/// 刀 4：已有 runner 持锁时，普通 run 进入**跟随**而非报错；generation 变化后退出 0。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follower_waits_then_exits_on_generation_change() {
    let (addr, state) = start_server(vec![text_response("不应调用")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    seed(&db, &[yaa_msg::Record::user("既有")]);
    let session = yaa_store::Session::new(db.clone());
    let held = yaa_store::FileLock::try_exclusive(&session.runner_lock_path())
        .unwrap()
        .expect("测试进程应能先持锁");

    let mut child = env
        .command()
        .arg("你好")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // 抢锁失败应进入跟随并保持存活，而不是报「会话正在运行」退出。
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        child.try_wait().unwrap().is_none(),
        "跟随者不应退出：{:?}",
        child.try_wait().unwrap()
    );

    // 构造归档：generation 变化后跟随者应退出 0 并提示。
    bump_generation(&db);
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        !stderr_of(&output).contains("会话正在运行"),
        "不应再报抢锁失败：{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("停止跟随"),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        state.requests.lock().unwrap().is_empty(),
        "纯跟随不得调用模型"
    );
    drop(held);
}

/// 刀 3：follower 不补投 spool，但检测到滞留时提示数量，且不删除文件。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follower_notices_spool_without_replaying() {
    let (addr, state) = start_server(vec![text_response("不应调用")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    seed(&db, &[yaa_msg::Record::user("既有")]);
    let session = yaa_store::Session::new(db.clone());
    let held = yaa_store::FileLock::try_exclusive(&session.runner_lock_path())
        .unwrap()
        .expect("测试进程应能先持锁");

    let mut child = env
        .command()
        .arg("跟随")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none(), "跟随者应保持存活");

    let spooled = yaa_msg::Record::system("notify", "<通知>\n滞留\n</通知>", None, None);
    yaa_store::publish_spool(&session, &spooled).unwrap();
    // 给 follower 一个轮询周期检测 spool。
    std::thread::sleep(std::time::Duration::from_millis(400));

    bump_generation(&db);
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(stderr.contains("暂存在 spool"), "stderr：{stderr}");
    assert!(stderr.contains("1 条"), "应提示数量：{stderr}");
    assert_eq!(
        yaa_store::count_spool(&session).unwrap(),
        1,
        "follower 不得补投或删除 spool 文件"
    );
    assert!(
        state.requests.lock().unwrap().is_empty(),
        "纯跟随不得调用模型"
    );
    drop(held);
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

    let records = env.read_inbox("pending");
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
    // 预置记录让会话库存在，`--status` 才有「记录：」摘要可打印（notify 不再直接建库）。
    seed(&env.session_db(), &[yaa_msg::Record::user("既有")]);
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
    assert_eq!(
        count_source(&env.read_inbox("pending"), "notify"),
        2,
        "两条 notify 都应发布到 inbox"
    );
}

/// S1：慢 notify 命令执行期间并发 `--clear` 不应等待命令结束；总量守恒且两边库完整。
///
/// 刀 6 起 clear 遇到 pending 会拒绝，故这里让慢命令在 clear 期间**尚未发布**：
/// clear 归档既有库；命令结束后 notify 用预检捕获的旧 generation 发布，必须被拒绝，
/// 不得把通知写进归档后的新 generation。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_conserves_records_while_slow_notify_command_runs() {
    let env = TestEnv::new(0);
    // 预置记录，确保库已存在、`--clear` 会真正归档（notify 不再直接建库）。
    seed(&env.session_db(), &[yaa_msg::Record::user("既有")]);

    let pidfile = env._root.path().join("slow-notify.pid");
    let command = format!("echo $$ > '{}'; sleep 2", pidfile.display());
    let writer = env
        .command()
        .args(["--notify=慢", "--", "sh", "-c", &command])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
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

    // P0-6：命令期间发生真归档（generation 1 → 2），发布必须被拒绝并返回 1。
    let writer = writer.wait_with_output().unwrap();
    assert_eq!(
        writer.status.code(),
        Some(1),
        "归档后 notify 应失败：{}",
        stderr_of(&writer)
    );
    assert!(
        stderr_of(&writer).contains("已归档") && stderr_of(&writer).contains("通知未写入"),
        "stderr 应说明会话已归档且通知未写入：{}",
        stderr_of(&writer)
    );

    // 归档承载既有事件；迟到的通知不得进入新 generation 的 inbox。
    let archived = PathBuf::from(stdout_of(&cleared).trim());
    assert!(archived.exists(), "归档库应存在：{}", archived.display());
    assert!(
        read_events(&archived)
            .iter()
            .any(|record| record["type"] == "user"),
        "归档应含既有记录"
    );
    assert_eq!(
        count_source(&env.read_inbox("pending"), "notify"),
        0,
        "归档后的迟到 notify 不得写入 inbox"
    );
    assert!(integrity_ok(&archived), "归档库 integrity 应为 ok");
    assert!(integrity_ok(&env.session_db()), "新库 integrity 应为 ok");
}

// ===== 阶段 4 刀 2+3：notify 只发布、注入先落库 =====

/// 刀 2：`--notify` 只发布，绝不启动 agent（不跑 init/llm、不调模型、不写 events）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_does_not_start_agent_and_only_publishes() {
    let (addr, state) = start_server(vec![text_response("不应调用")]).await;
    let env = TestEnv::new(addr.port());
    // 默认 init 会写三条 notify；若 `--notify` 误触发 agent，这些就会落库。
    env.write_agents("代理须知");

    let output = env.run(&["--notify=外部", "--", "printf", "X"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );

    assert!(
        state.requests.lock().unwrap().is_empty(),
        "notify 不得调用模型"
    );
    assert!(
        !env.session_db().exists(),
        "notify 不得创建或写入会话库：{}",
        env.session_db().display()
    );
    let pending = env.read_inbox("pending");
    assert_eq!(pending.len(), 1, "应发布一条 pending 消息：{pending:?}");
    assert!(env.read_inbox("init").is_empty(), "init/ 应为空");
}

/// 刀 2：`YAA_INBOX_PHASE=init` 使通知写入 `inbox/init/`（磁盘布局断言）。
#[test]
fn notify_init_phase_env_publishes_to_init_dir() {
    let env = TestEnv::new(0);
    let output = env
        .command()
        .env("YAA_INBOX_PHASE", "init")
        .args(["--notify=初始化", "--", "printf", "i"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );

    let init = env.read_inbox("init");
    assert_eq!(init.len(), 1, "通知应写入 init/：{init:?}");
    assert!(env.read_inbox("pending").is_empty(), "pending/ 应为空");

    let init_dir = env.inbox_root().join("init");
    assert_eq!(
        std::fs::read_dir(&init_dir).unwrap().count(),
        1,
        "init/ 下应有且仅有一条消息文件"
    );
}

/// 刀 3：先发布的消息按文件名序先落库，且都排在本次 user 之前。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn earlier_published_messages_land_before_later_user() {
    let (addr, _state) = start_server(vec![text_response("答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let first = env.run(&["--notify=早一", "--", "printf", "A"]);
    assert_eq!(
        first.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&first)
    );
    let second = env.run(&["--notify=早二", "--", "printf", "B"]);
    assert_eq!(
        second.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&second)
    );

    let run = env.run(&["晚"]);
    assert!(run.status.success(), "stderr：{}", stderr_of(&run));

    let records = read_events(&env.session_db());
    let labels: Vec<&str> = records
        .iter()
        .map(|record| record["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        ["system", "system", "system", "user", "ai"],
        "记录顺序：{records:?}"
    );
    assert_eq!(records[0]["source"], "init", "首条应为 init 簿记");
    assert_eq!(records[1]["source"], "notify");
    assert_eq!(records[2]["source"], "notify");
    assert_eq!(records[3]["content"], "晚", "own user 应排在本批通知之后");
    assert_eq!(count_source(&records, "notify"), 2, "两条通知都应落库");
    assert!(
        env.read_inbox("pending").is_empty(),
        "消费后 pending/ 应为空"
    );
}

// ===== 阶段 4 刀 4+5：跟随与接管 =====

/// 刀 4：第二个客户端抢锁失败进入 follower；只有 leader 发模型请求，各自打印自己的答案。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_runner_second_client_follows_and_only_leader_requests() {
    // leader 用一个长工具调用（`sleep 2`）持锁，给 follower 足够时间进入跟随。
    let (addr, state) = start_server(vec![
        tool_call_response("call_1", "sleep 2", None, None),
        text_response("领导人答"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let leader = env
        .command()
        .arg("甲问")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // 等 leader 写完 user/ai/tool_call 并进入工具执行（持锁）。
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let follower = env
        .command()
        .arg("乙问")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let leader_out = leader.wait_with_output().unwrap();
    let follower_out = follower.wait_with_output().unwrap();

    assert!(
        leader_out.status.success(),
        "leader 失败：{}",
        stderr_of(&leader_out)
    );
    assert_eq!(
        follower_out.status.code(),
        Some(0),
        "follower 失败：{}",
        stderr_of(&follower_out)
    );
    assert!(stdout_of(&leader_out).contains("领导人答"));
    assert!(
        stdout_of(&follower_out).contains("领导人答"),
        "follower 应打印归属自己的答案：{}",
        stdout_of(&follower_out)
    );
    assert!(
        !stderr_of(&follower_out).contains("会话正在运行"),
        "follower 不应再报抢锁失败：{}",
        stderr_of(&follower_out)
    );
    assert!(
        !stderr_of(&follower_out).contains("甲问"),
        "follower 不应回放历史：{}",
        stderr_of(&follower_out)
    );
    assert_eq!(
        state.requests.lock().unwrap().len(),
        2,
        "只有 leader 发模型请求（follower 不发）"
    );
}

/// 刀 4：follower 从发布自己之前的 `last_seq` 起跟随，绝不回放既有历史。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follower_does_not_replay_seeded_history() {
    let (addr, _state) = start_server(vec![text_response("不应调用")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    let mut history = Vec::new();
    for index in 0..50 {
        history.push(yaa_msg::Record::user(format!("OLD-{index}")));
        history.push(yaa_msg::Record::ai(
            "m",
            "h",
            format!("OLD-ANS-{index}"),
            None,
        ));
    }
    seed(&db, &history);
    let session = yaa_store::Session::new(db.clone());
    let held = yaa_store::FileLock::try_exclusive(&session.runner_lock_path())
        .unwrap()
        .expect("测试进程应能先持锁");

    let child = env
        .command()
        .arg("新问")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    bump_generation(&db);
    let output = child.wait_with_output().unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        !stderr_of(&output).contains("OLD-"),
        "follower 不应回放历史：{}",
        stderr_of(&output)
    );
    assert!(
        !stdout_of(&output).contains("OLD-"),
        "stdout 不应含历史：{}",
        stdout_of(&output)
    );
    drop(held);
}

/// 刀 5：leader 在工具执行中被 SIGKILL，follower 接管、补中断结果并产出答案（不重放）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn follower_takes_over_after_leader_killed() {
    let (addr, _state) = start_server(vec![
        tool_call_response("call_kill", "sleep 30", None, None),
        text_response("接管答"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    let session = yaa_store::Session::new(db.clone());

    let mut leader = env
        .command()
        .arg("甲问")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // 等待 `tool_call` 落盘（工具随后进入执行）。
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
            "等待 leader 的 tool_call 超时"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::thread::sleep(std::time::Duration::from_millis(150));

    let follower = env
        .command()
        .arg("乙问")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // 让 follower 进入跟随（抢锁失败）。
    std::thread::sleep(std::time::Duration::from_millis(250));

    leader.kill().expect("SIGKILL leader");
    leader.wait().expect("回收 leader");

    let follower_out = follower.wait_with_output().unwrap();
    assert_eq!(
        follower_out.status.code(),
        Some(0),
        "follower 接管失败：{}",
        stderr_of(&follower_out)
    );
    assert!(
        stdout_of(&follower_out).contains("接管答"),
        "接管者应产出答案：{}",
        stdout_of(&follower_out)
    );
    assert!(
        !stderr_of(&follower_out).contains("会话正在运行"),
        "{}",
        stderr_of(&follower_out)
    );
    assert!(
        !stderr_of(&follower_out).contains("甲问"),
        "接管不应回放历史：{}",
        stderr_of(&follower_out)
    );

    let records = yaa_store::load(&session).unwrap();
    let interrupted: Vec<_> = records
        .iter()
        .filter(|record| matches!(record, yaa_msg::Record::ToolResult(result) if result.call_id == "call_kill"))
        .collect();
    assert_eq!(interrupted.len(), 1, "应恰好补一条中断结果：{records:?}");
    let yaa_msg::Record::ToolResult(result) = interrupted[0] else {
        unreachable!()
    };
    assert!(result.stderr.contains("interrupted"), "{result:?}");
    assert!(
        records
            .iter()
            .any(|record| matches!(record, yaa_msg::Record::User(user) if user.content == "乙问")),
        "接管应消费 follower 自己的消息：{records:?}"
    );
    let ai_count = records
        .iter()
        .filter(|record| matches!(record, yaa_msg::Record::Ai(_)))
        .count();
    assert_eq!(ai_count, 2, "甲的中途 ai + 接管 ai：{records:?}");
}

/// 刀 5：纯 follower（own 已落库）遇未决审批不接管，提示 `--approve` 并保持跟随。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pure_follower_with_pending_prompts_approve_without_taking_over() {
    // leader 先用 `sleep 1.5` 长工具持锁，随后消费 follower 消息并命中 `echo *` 待批退出 6。
    let (addr, _state) = start_server(vec![
        tool_call_response("c1", "sleep 1.5", None, None),
        tool_call_response("c2", "echo hi", None, None),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(APPROVAL_ASK_CONFIG);

    let leader = env
        .command()
        .arg("甲问")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let mut follower = env
        .command()
        .arg("乙问")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let leader_out = leader.wait_with_output().unwrap();
    assert_eq!(
        leader_out.status.code(),
        Some(6),
        "leader 应以待批退出：{}",
        stderr_of(&leader_out)
    );

    // follower 应为纯跟随：own 已被 leader 消费，遇未决 pending 不接管。
    std::thread::sleep(std::time::Duration::from_millis(700));
    assert!(
        follower.try_wait().unwrap().is_none(),
        "纯 follower 不应接管退出：{:?}",
        follower.try_wait().unwrap()
    );

    follower.kill().expect("结束 follower");
    let follower_out = follower.wait_with_output().unwrap();
    assert!(
        stderr_of(&follower_out).contains("--approve"),
        "应提示运行 --approve：{}",
        stderr_of(&follower_out)
    );
}

// ===== 阶段 4 刀 6：clear/status 待处理消息与 tmp 清理 =====

/// 刀 6：inbox 有消息时 `--clear` 拒绝归档，消息保留且不产生归档。
#[test]
fn clear_refuses_when_inbox_has_pending() {
    let env = TestEnv::new(0);
    let published = env.run(&["--notify=待归档", "--", "printf", "X"]);
    assert_eq!(
        published.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&published)
    );

    let output = env.run(&["--clear"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr：{}",
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(stderr.contains("待处理"), "stderr：{stderr}");
    assert!(stderr.contains("1 条"), "应报告待处理计数：{stderr}");
    assert_eq!(
        env.read_inbox("pending").len(),
        1,
        "拒绝归档时不得移除 inbox 消息"
    );
    assert!(
        !env.data_home.join("history").exists(),
        "拒绝归档不得产生归档"
    );
}

/// 刀 6：`session.db` 不存在但仅有 init/ 待处理消息时同样拒绝归档。
#[test]
fn clear_refuses_when_only_inbox_exists() {
    let env = TestEnv::new(0);
    let published = env
        .command()
        .env("YAA_INBOX_PHASE", "init")
        .args(["--notify=初始化", "--", "printf", "i"])
        .output()
        .unwrap();
    assert_eq!(
        published.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&published)
    );
    assert!(!env.session_db().exists(), "notify 不得创建会话库");

    let output = env.run(&["--clear"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("待处理"),
        "stderr：{}",
        stderr_of(&output)
    );
    assert_eq!(env.read_inbox("init").len(), 1, "init/ 消息应保留");
    assert!(
        !env.data_home.join("history").exists(),
        "拒绝归档不得产生归档"
    );
}

/// 刀 6：inbox 排空后 `--clear` 正常归档既有会话。
#[test]
fn clear_succeeds_after_inbox_drained() {
    let env = TestEnv::new(0);
    let db = env.session_db();
    seed(&db, &[yaa_msg::Record::user("既有")]);
    let published = env.run(&["--notify=已消费", "--", "printf", "X"]);
    assert!(
        published.status.success(),
        "stderr：{}",
        stderr_of(&published)
    );

    // 手工排空 pending（等价于 runner 已消费）。
    let pending = env.inbox_root().join("pending");
    for entry in std::fs::read_dir(&pending).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }

    let output = env.run(&["--clear"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    let archived = PathBuf::from(stdout_of(&output).trim());
    assert!(archived.exists(), "归档库应存在：{}", archived.display());
    assert!(
        read_events(&archived)
            .iter()
            .any(|record| record["type"] == "user"),
        "归档应含既有记录"
    );
}

/// 刀 6：`--status` 报告 pending/init 计数，且不创建会话库或目录。
#[test]
fn status_reports_pending_inbox_count() {
    let env = TestEnv::new(0);
    for (comment, body) in [("一", "A"), ("二", "B")] {
        let output = env.run(&["--notify", comment, "--", "printf", body]);
        assert!(output.status.success(), "stderr：{}", stderr_of(&output));
    }
    let init = env
        .command()
        .env("YAA_INBOX_PHASE", "init")
        .args(["--notify=初始化", "--", "printf", "i"])
        .output()
        .unwrap();
    assert!(init.status.success(), "stderr：{}", stderr_of(&init));

    let output = env.run(&["--status"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("待处理消息：3 条（pending 2 / init 1）"),
        "stdout：{stdout}"
    );
    assert!(
        !env.session_db().exists(),
        "status 不得创建会话库：{}",
        env.session_db().display()
    );
}

/// 刀 6/S2：工具执行期间注入不得打断 `tool_call`/`tool_result` 配对；注入在其后落库。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injection_during_tool_execution_does_not_break_pairing() {
    let (addr, _state) = start_server(vec![
        tool_call_response("call_inject", "sleep 1", None, None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    let session = yaa_store::Session::new(db.clone());

    let run = env
        .command()
        .arg("开始")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // 等 `tool_call` 落盘（工具随后进入 `sleep 1`）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let has_call = yaa_store::load(&session).is_ok_and(|records| {
            records.iter().any(|record| {
                matches!(record, yaa_msg::Record::ToolCall(call) if call.call_id == "call_inject")
            })
        });
        if has_call {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待 tool_call 落盘超时"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let notify = env.run(&["--notify=工具期间", "--", "printf", "INJECT"]);
    assert_eq!(
        notify.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&notify)
    );

    let output = run.wait_with_output().unwrap();
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = yaa_store::load(&session).unwrap();
    let call_index = records
        .iter()
        .position(|record| {
            matches!(record, yaa_msg::Record::ToolCall(call) if call.call_id == "call_inject")
        })
        .expect("应有 tool_call");
    let result_index = records
        .iter()
        .position(|record| {
            matches!(record, yaa_msg::Record::ToolResult(result) if result.call_id == "call_inject")
        })
        .expect("应有配对的 tool_result");
    assert!(call_index < result_index, "配对顺序错误：{records:?}");
    for record in &records[call_index + 1..result_index] {
        let injects = match record {
            yaa_msg::Record::User(_) => true,
            yaa_msg::Record::System(system) => system.source == "notify",
            _ => false,
        };
        assert!(!injects, "注入不得落在工具批次内部：{records:?}");
    }

    let notify_index = records
        .iter()
        .position(|record| {
            matches!(
                record,
                yaa_msg::Record::System(system)
                    if system.source == "notify" && system.content.contains("INJECT")
            )
        })
        .expect("注入记录应落库");
    assert!(
        notify_index > result_index,
        "注入应排在 tool_result 之后：{records:?}"
    );
}

/// 刀 6：`--notify` 尽力清理 1 小时前的 inbox tmp 残留。
#[test]
fn notify_cleans_stale_inbox_tmp() {
    let env = TestEnv::new(0);
    let tmp_dir = env.inbox_root().join("tmp");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let stale = tmp_dir.join("stale.json");
    std::fs::write(&stale, b"{}").unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&stale)
        .unwrap();
    file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600))
        .unwrap();
    drop(file);

    let output = env.run(&["--notify=清理", "--", "printf", "x"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(!stale.exists(), "陈旧 tmp 文件应被清理");
    assert_eq!(
        std::fs::read_dir(&tmp_dir).unwrap().count(),
        0,
        "tmp/ 不应残留文件"
    );
}

// ===== 阶段 4 终审修复：follower 基线、竞态、排空与接管策略 =====

/// C1：全新会话、两个 `run` 真正同时启动：不得把首次建库误报为归档，双方都得到答案。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_runs_do_not_report_archived() {
    let (addr, state) = start_server(vec![
        text_response("并发答"),
        text_response("并发答"),
        text_response("并发答"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    // 让首个模型请求慢一点，给另一个客户端进入 follow 的机会。
    let mut delay = Command::new("sleep");
    delay.arg("0.4");
    *state.before_response.lock().unwrap() = Some(delay);

    let first = env
        .command()
        .arg("甲问")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let second = env
        .command()
        .arg("乙问")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let first_out = first.wait_with_output().unwrap();
    let second_out = second.wait_with_output().unwrap();
    for (name, output) in [("甲", &first_out), ("乙", &second_out)] {
        assert_eq!(
            output.status.code(),
            Some(0),
            "{name} 退出码 {:?}，stderr：{}",
            output.status.code(),
            stderr_of(output)
        );
        assert!(
            stdout_of(output).contains("并发答"),
            "{name} 应打印答案：{}",
            stdout_of(output)
        );
        assert!(
            !stderr_of(output).contains("已归档"),
            "{name} 不得把建库误报为归档：{}",
            stderr_of(output)
        );
        assert!(
            !stderr_of(output).contains("会话正在运行"),
            "{name} 不应报抢锁失败：{}",
            stderr_of(output)
        );
    }
    assert!(
        !state.requests.lock().unwrap().is_empty(),
        "至少应发起一次模型请求"
    );
}

/// C2：generation 0→1 建库期间 follower 不退出；真归档才退出。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follower_survives_first_database_creation() {
    let (addr, _state) = start_server(vec![text_response("不应调用")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    assert!(!db.exists(), "前置条件：会话库尚不存在");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let session = yaa_store::Session::new(db.clone());
    let held = yaa_store::FileLock::try_exclusive(&session.runner_lock_path())
        .unwrap()
        .expect("测试进程应能先持锁");

    let mut child = env
        .command()
        .arg("你好")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_inbox(&env, yaa_store::InboxPhase::Pending, 1);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        child.try_wait().unwrap().is_none(),
        "建库前 follower 不应退出：{:?}",
        child.try_wait().unwrap()
    );

    // 首次建库：generation 0 → 1。旧实现会在此误判为「会话已归档」。
    seed(&db, &[yaa_msg::Record::system("init", "", None, None)]);
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "建库后 follower 不应退出：{:?}",
        child.try_wait().unwrap()
    );

    // 真正的归档（generation 1 → 2）才退出。
    bump_generation(&db);
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("停止跟随"),
        "真归档应提示停止跟随：{}",
        stderr_of(&output)
    );
    drop(held);
}

/// C3（烟雾）：`--clear` 与 run 发布竞态。
///
/// 主证据在 store 单测：`captured_generation_archived_before_publish_is_rejected`
/// （捕获 generation → 归档 → 发布被拒且不落盘）、
/// `publish_with_generation_waits_for_clear_exclusive_lock`（SH/EX 互斥），
/// 以及 e2e `clear_conserves_records_while_slow_notify_command_runs`（长窗口真归档）。
/// run 的 SH 临界区极短，e2e 无法稳定卡在中途，故这里只验证两种线性化结果自洽：
/// clear 成功 ⇒ run 被拒绝且 own 不滞留；clear 拒绝 ⇒ run 正常落库并回答。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clear_and_publish_race_leaves_no_stranded_own() {
    let (addr, _state) = start_server(vec![
        text_response("竞态答"),
        text_response("竞态答"),
        text_response("竞态答"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    seed(
        &db,
        &[
            yaa_msg::Record::user("既有"),
            yaa_msg::Record::ai("m", "h", "旧答", None),
        ],
    );
    let session = yaa_store::Session::new(db.clone());

    // 先冻结发布临界区：run 会阻塞在 clear.lock SH；clear 随后拿到 runner.lock 并阻塞在 EX。
    let held_clear = yaa_store::FileLock::exclusive(&session.clear_lock_path()).unwrap();
    let run = env
        .command()
        .arg("竞态问")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(250));
    let clear = env
        .command()
        .arg("--clear")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(250));
    drop(held_clear);

    let run_out = run.wait_with_output().unwrap();
    let clear_out = clear.wait_with_output().unwrap();
    let clear_code = clear_out.status.code();
    assert!(
        matches!(clear_code, Some(0 | 1)),
        "clear 只能成功或拒绝：{:?} / {}",
        clear_code,
        stderr_of(&clear_out)
    );

    let current_has_own = yaa_store::load(&session)
        .unwrap()
        .iter()
        .any(|record| matches!(record, yaa_msg::Record::User(user) if user.content == "竞态问"));
    match clear_code {
        // clear 抢在发布前归档：run 的发布被 generation 守卫拒绝，不得误判为成功。
        Some(0) => {
            assert_eq!(
                run_out.status.code(),
                Some(1),
                "归档后 run 应失败：stdout={} stderr={}",
                stdout_of(&run_out),
                stderr_of(&run_out)
            );
            assert!(
                stderr_of(&run_out).contains("已归档"),
                "run 应报告会话已归档：{}",
                stderr_of(&run_out)
            );
            assert!(!current_has_own, "被拒绝的 own 不得出现在新库");
        }
        // clear 看到 own 已发布而拒绝：run 之后接管并完成本轮。
        Some(1) => {
            assert_eq!(
                run_out.status.code(),
                Some(0),
                "clear 拒绝后 run 应成功：stderr={}",
                stderr_of(&run_out)
            );
            assert!(
                stdout_of(&run_out).contains("竞态答"),
                "run 应得到答案：{}",
                stdout_of(&run_out)
            );
            assert!(
                !stderr_of(&run_out).contains("已归档"),
                "run 不得在发布竞态中误报归档：{}",
                stderr_of(&run_out)
            );
            assert!(current_has_own, "own 必须落库");
        }
        _ => unreachable!(),
    }
    assert_eq!(
        yaa_store::count_inbox(&session, yaa_store::InboxPhase::Pending).unwrap(),
        0,
        "own 不得滞留在 inbox（两种线性化结果皆然）"
    );
}

/// C4：最终模型调用期间到达的 notify 必须在本次 run 内消费（纯文本响应也要续跑）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_during_final_model_call_is_consumed_in_same_run() {
    let (addr, state) = start_server(vec![text_response("初答"), text_response("通知后答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let mut notify = env.command();
    notify
        .args(["--notify=模型期间", "--", "printf", "INJECT"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    *state.before_response.lock().unwrap() = Some(notify);

    let output = env.run(&["问题"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );

    let records = read_events(&env.session_db());
    assert!(
        records.iter().any(|record| {
            record["type"] == "system"
                && record["source"] == "notify"
                && record["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("INJECT"))
        }),
        "notify 应在本 run 内落库：{records:?}"
    );
    assert_eq!(
        state.requests.lock().unwrap().len(),
        2,
        "最终回复后应因新 notify 再调用一次模型"
    );
    let requests = state.requests.lock().unwrap();
    assert!(
        requests[1]["messages"]
            .as_array()
            .is_some_and(|messages| messages.iter().any(|message| message["content"]
                .as_str()
                .is_some_and(|content| content.contains("INJECT")))),
        "第二次请求应包含注入正文：{}",
        requests[1]
    );
    assert_eq!(
        yaa_store::count_inbox(
            &yaa_store::Session::new(env.session_db()),
            yaa_store::InboxPhase::Pending
        )
        .unwrap(),
        0,
        "notify 应已消费"
    );
}

/// C5：纯 follower（own 已落库）接管时只有 notify：不作废旧待批，提示 `--approve` 并继续等待。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_only_takeover_keeps_pending_approval() {
    let (addr, _state) = start_server(vec![text_response("不应调用")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    seed(
        &db,
        &[
            yaa_msg::Record::user("甲问"),
            yaa_msg::Record::ai("m", "h", "想跑命令", None),
            yaa_msg::Record::tool_call(yaa_msg::ToolOrigin::Ai, "c1", "echo hi", None),
            yaa_msg::Record::approval_pending("c1", "echo hi", "echo *"),
        ],
    );
    let session = yaa_store::Session::new(db.clone());
    let held = yaa_store::FileLock::try_exclusive(&session.runner_lock_path())
        .unwrap()
        .expect("测试进程应能先持锁");

    let mut follower = env
        .command()
        .arg("乙问")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_inbox(&env, yaa_store::InboxPhase::Pending, 1);
    // 模拟 leader 已消费 follower 的 own（落库并删除 inbox 文件）。
    consume_pending_user(&db);

    let notify = env.run(&["--notify=提醒", "--", "printf", "R"]);
    assert!(
        notify.status.success(),
        "notify 失败：{}",
        stderr_of(&notify)
    );
    drop(held);

    std::thread::sleep(std::time::Duration::from_millis(900));
    assert!(
        follower.try_wait().unwrap().is_none(),
        "纯 follower 不应接管退出：{:?}",
        follower.try_wait().unwrap()
    );

    follower.kill().expect("结束 follower");
    let output = follower.wait_with_output().unwrap();
    assert!(
        stderr_of(&output).contains("--approve"),
        "应提示运行 --approve：{}",
        stderr_of(&output)
    );

    let records = yaa_store::load(&session).unwrap();
    assert!(
        !records.iter().any(|record| matches!(
            record,
            yaa_msg::Record::ApprovalResolved(resolved) if resolved.call_id == "c1"
        )),
        "不得替用户作废旧待批：{records:?}"
    );
    assert!(
        yaa_store::count_inbox(&session, yaa_store::InboxPhase::Pending).unwrap() >= 1,
        "notify 应留在 inbox（未接管）"
    );
}

/// C6：follower 从 `tool_call` 之后启动，前缀回填命令表后仍能展示 `approval_resolved` 的命令。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follower_backfills_command_for_later_approval_resolution() {
    let (addr, _state) = start_server(vec![text_response("不应调用")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    seed(
        &db,
        &[
            yaa_msg::Record::user("甲问"),
            yaa_msg::Record::ai("m", "h", "想跑命令", None),
            yaa_msg::Record::tool_call(yaa_msg::ToolOrigin::Ai, "c1", "echo hi", None),
        ],
    );
    let session = yaa_store::Session::new(db.clone());
    let held = yaa_store::FileLock::try_exclusive(&session.runner_lock_path())
        .unwrap()
        .expect("测试进程应能先持锁");

    let child = env
        .command()
        .arg("乙问")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(350));

    // 在 follower 启动水位之后落一条审批决策；命令来自更早的 tool_call。
    yaa_store::append(
        &session,
        &[yaa_msg::Record::approval_resolved(
            "c1",
            yaa_msg::ApprovalDecision::Approve,
        )],
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(600));

    bump_generation(&db);
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(stderr.contains("已批准"), "stderr：{stderr}");
    assert!(
        stderr.contains("% echo hi"),
        "审批决策应带回前缀中的命令：{stderr}"
    );
    drop(held);
}

/// C7：`max_steps=1` 时工具执行期注入 User → 新 User 被处理，不提前 MaxSteps。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn user_injected_during_tool_execution_resets_max_steps() {
    let (addr, _state) = start_server(vec![
        tool_call_response("call_slow", "sleep 1", None, None),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(SINGLE_STEP_CONFIG);
    let db = env.session_db();
    let session = yaa_store::Session::new(db.clone());

    let leader = env
        .command()
        .arg("甲问")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until(
        || {
            yaa_store::load(&session).is_ok_and(|records| {
                records.iter().any(|record| {
                    matches!(record, yaa_msg::Record::ToolCall(call) if call.call_id == "call_slow")
                })
            })
        },
        "leader 的 tool_call 落盘",
    );
    let follower = env
        .command()
        .arg("乙问")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let leader_out = leader.wait_with_output().unwrap();
    let follower_out = follower.wait_with_output().unwrap();
    assert_eq!(
        leader_out.status.code(),
        Some(0),
        "工具期间注入的 User 应重置步数预算，而不是 MaxSteps：{}",
        stderr_of(&leader_out)
    );
    assert!(
        !stderr_of(&leader_out).contains("达到最大步数"),
        "stderr：{}",
        stderr_of(&leader_out)
    );
    assert!(
        stdout_of(&leader_out).contains("完成"),
        "leader 应产出答案：{}",
        stdout_of(&leader_out)
    );
    assert_eq!(
        follower_out.status.code(),
        Some(0),
        "follower 失败：{}",
        stderr_of(&follower_out)
    );

    let records = yaa_store::load(&session).unwrap();
    assert!(
        records
            .iter()
            .any(|record| matches!(record, yaa_msg::Record::User(user) if user.content == "乙问")),
        "注入的 User 应落库：{records:?}"
    );
}

/// C8：`a.db` 与 `a.sqlite` 的 runner.lock 互不阻塞。
#[test]
fn explicit_db_and_sqlite_runner_locks_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let first = yaa_store::Session::new(dir.path().join("a.db"));
    let second = yaa_store::Session::new(dir.path().join("a.sqlite"));
    assert_ne!(first.runner_lock_path(), second.runner_lock_path());

    let held = yaa_store::FileLock::try_exclusive(&first.runner_lock_path())
        .unwrap()
        .expect("应能先持有 a.db 的 runner.lock");
    let other = yaa_store::FileLock::try_exclusive(&second.runner_lock_path()).unwrap();
    assert!(other.is_some(), "a.sqlite 的 runner.lock 不应被 a.db 阻塞");
    drop(held);
}

/// C9：`--status` 只读计数，malformed inbox 文件不得被移入 rejected/。
#[test]
fn status_does_not_reject_malformed_inbox_file() {
    let env = TestEnv::new(0);
    let pending = env.inbox_root().join("pending");
    std::fs::create_dir_all(&pending).unwrap();
    let malformed = pending.join("broken.json");
    std::fs::write(&malformed, b"{ not json").unwrap();

    let output = env.run(&["--status"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        stdout_of(&output).contains("待处理消息：1 条"),
        "stdout：{}",
        stdout_of(&output)
    );
    assert!(malformed.exists(), "status 不得移动 malformed 文件");
    assert!(
        !env.inbox_root().join("rejected").exists(),
        "status 不得创建 rejected/"
    );
}

/// C9b：`--clear` 遇 malformed inbox 文件也必须拒绝归档，且不移动文件、不创建 rejected。
#[test]
fn clear_refuses_malformed_inbox_file_without_side_effects() {
    let env = TestEnv::new(0);
    // 先让 session.db 存在，否则会走「库不存在」分支而非持锁后的检查。
    seed(&env.session_db(), &[yaa_msg::Record::user("已有记录")]);
    let pending = env.inbox_root().join("pending");
    std::fs::create_dir_all(&pending).unwrap();
    let malformed = pending.join("broken.json");
    std::fs::write(&malformed, b"{ not json").unwrap();

    let output = env.run(&["--clear"]);
    assert_ne!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("待处理消息"),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(malformed.exists(), "clear 不得移动 malformed 文件");
    assert!(
        !env.inbox_root().join("rejected").exists(),
        "clear 不得创建 rejected/"
    );
    let history = env.data_home.join("history");
    assert!(
        !history.exists() || std::fs::read_dir(&history).unwrap().next().is_none(),
        "拒绝归档时不得产生 history/"
    );
}

/// 等待 `inbox` 指定 phase 的 `.json` 文件数达到 `expected`。
fn wait_for_inbox(env: &TestEnv, phase: yaa_store::InboxPhase, expected: usize) {
    let session = yaa_store::Session::new(env.session_db());
    wait_until(
        || yaa_store::count_inbox(&session, phase).is_ok_and(|count| count >= expected),
        "inbox 消息发布",
    );
}

/// 把 inbox 中唯一一条 `User` 记录落库并删除文件（模拟 leader 已消费 own）。
fn consume_pending_user(db: &Path) {
    let session = yaa_store::Session::new(db);
    let messages = yaa_store::scan_inbox(&session, yaa_store::InboxPhase::Pending).unwrap();
    let own = messages
        .into_iter()
        .find(|message| matches!(message.record, yaa_msg::Record::User(_)))
        .expect("pending 中应有 own user");
    yaa_store::append(&session, std::slice::from_ref(&own.record)).unwrap();
    yaa_store::remove_inbox(&own).unwrap();
}

/// 轮询等待条件成立，超时则失败。
fn wait_until(mut condition: impl FnMut() -> bool, description: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "等待超时：{description}"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// 把默认会话的 `generation` 加一（构造归档，让 follower 检测到并退出）。
fn bump_generation(db: &Path) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "UPDATE session SET generation = generation + 1 WHERE id = 1",
        [],
    )
    .unwrap();
}

// ===== 阶段 5 刀 3：归因字段与 spool 保底 =====

/// 刀 3：`--to` 子→父通知落库后带发布者 `origin` 与合法 `call_id`；`--` 与 stdin
/// 两种形态都填。
#[test]
fn notify_to_parent_records_origin_and_call_id() {
    let env = TestEnv::new(0);
    let parent = env.work.join("parent").join("session.db");
    seed(&parent, &[yaa_msg::Record::user("父既有")]);
    let child = env.work.join("child").join("session.db");
    seed(&child, &[yaa_msg::Record::user("子既有")]);

    let output = env
        .command()
        .env("YAA_SESSION", child.to_str().unwrap())
        .env("YAA_NOTIFY_CALL_ID", "call_abc-123")
        .args([
            "--notify=子完成",
            "--to",
            parent.to_str().unwrap(),
            "--",
            "printf",
            "X",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );

    let records = read_inbox_at(&parent, "pending");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["source"], "notify");
    let child_origin = std::path::absolute(&child)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        records[0]["origin"].as_str(),
        Some(child_origin.as_str()),
        "origin 应为发布者会话库绝对路径：{}",
        records[0]
    );
    assert_eq!(records[0]["call_id"], "call_abc-123");

    // stdin 形态同样带归因。
    let stdin = env
        .command()
        .env("YAA_SESSION", child.to_str().unwrap())
        .env("YAA_NOTIFY_CALL_ID", "call_stdin")
        .args(["--notify=stdin", "--to", parent.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = stdin;
    stdin.stdin.take().unwrap().write_all(b"payload").unwrap();
    let stdin = stdin.wait_with_output().unwrap();
    assert_eq!(
        stdin.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&stdin)
    );

    let records = read_inbox_at(&parent, "pending");
    assert_eq!(records.len(), 2);
    let stdin_record = records
        .iter()
        .find(|record| record["content"].as_str().unwrap().contains("payload"))
        .expect("应找到 stdin 通知");
    assert_eq!(stdin_record["call_id"], "call_stdin");
    assert_eq!(stdin_record["origin"].as_str(), Some(child_origin.as_str()));
}

/// 刀 3：非法 `YAA_NOTIFY_CALL_ID`（空、超长、含非法字符）静默忽略，不写字段；
/// `origin` 仍然写。
#[test]
fn notify_ignores_illegal_call_id() {
    let too_long = "x".repeat(129);
    for illegal in ["", "has space", too_long.as_str(), "冒号:"] {
        let env = TestEnv::new(0);
        let output = env
            .command()
            .env("YAA_NOTIFY_CALL_ID", illegal)
            .args(["--notify=非法归因", "--", "true"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "非法 call_id 不得报错：{}",
            stderr_of(&output)
        );

        let records = env.read_inbox("pending");
        assert_eq!(records.len(), 1);
        assert!(
            records[0].get("call_id").is_none(),
            "非法 call_id 不得写入：{}",
            records[0]
        );
        assert!(
            records[0]["origin"]
                .as_str()
                .is_some_and(|path| path.ends_with("session.db")),
            "origin 应总是写入：{}",
            records[0]
        );
    }
}

/// 刀 3：旧 `system` JSON（无 `origin`/`call_id`）仍可被 `--status` 解析。
#[test]
fn status_accepts_system_without_attribution() {
    let env = TestEnv::new(0);
    let db = env.work.join("old.db");
    seed(&db, &[yaa_msg::Record::user("既有")]);

    let payload = r#"{"type":"system","id":"01JOLDSYSTEM000000000000000","ts":5,"source":"notify","command":"echo hi","content":"<注释>\n旧记录\n</注释>"}"#;
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO events (id, payload) VALUES (?1, ?2)",
        rusqlite::params!["01JOLDSYSTEM000000000000000", payload],
    )
    .unwrap();

    let output = env.run(&["--status", "--session", db.to_str().unwrap()]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "缺归因字段的旧 system 应可解析：{}",
        stderr_of(&output)
    );
    assert!(
        stdout_of(&output).contains("记录：2 条"),
        "{}",
        stdout_of(&output)
    );
}

/// 刀 3：发布 inbox 因 IO 失败（`inbox/tmp` 是普通文件）时降级写 spool，命令仍退出 0；
/// 下次 run 补投且重复 run 不重复（events 恰一条）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_falls_back_to_spool_and_next_run_replays_once() {
    let (addr, _state) = start_server(vec![text_response("答一"), text_response("答二")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    // 让 inbox 发布必然失败：`inbox/tmp` 被一个普通文件占位。
    std::fs::create_dir_all(env.inbox_root()).unwrap();
    std::fs::write(env.inbox_root().join("tmp"), b"block").unwrap();

    let notify = env.run(&["--notify=保底", "--", "printf", "X"]);
    assert_eq!(
        notify.status.code(),
        Some(0),
        "降级 spool 后退出码应保持 0：{}",
        stderr_of(&notify)
    );
    assert!(
        stderr_of(&notify).contains("spool"),
        "应提示已写入 spool：{}",
        stderr_of(&notify)
    );

    let session = yaa_store::Session::new(env.session_db());
    let spooled = yaa_store::scan_spool(&session).unwrap();
    assert_eq!(spooled.len(), 1, "应有一条 spool 保底记录");
    let spooled_id = spooled[0].id().to_owned();
    assert!(
        matches!(&spooled[0].record, yaa_msg::Record::System(system) if system.source == "notify")
    );
    assert_eq!(
        yaa_store::count_inbox(&session, yaa_store::InboxPhase::Pending).unwrap(),
        0,
        "inbox 发布失败时不得留下半成品"
    );

    // 解除阻塞后正常 run：spool 在 init 前补投。
    std::fs::remove_file(env.inbox_root().join("tmp")).unwrap();
    let first = env.run(&["任务"]);
    assert!(first.status.success(), "stderr：{}", stderr_of(&first));

    let records = read_events(&env.session_db());
    let delivered: Vec<_> = records
        .iter()
        .filter(|record| record["id"] == spooled_id.as_str())
        .collect();
    assert_eq!(delivered.len(), 1, "spool 记录应恰好落库一次：{records:?}");
    assert_eq!(delivered[0]["source"], "notify");
    assert!(env.read_inbox("pending").is_empty());
    assert!(yaa_store::scan_spool(&session).unwrap().is_empty());

    // 再 run 一次：不得重复补投。
    let second = env.run(&["继续"]);
    assert!(second.status.success(), "stderr：{}", stderr_of(&second));
    let records = read_events(&env.session_db());
    assert_eq!(
        records
            .iter()
            .filter(|record| record["id"] == spooled_id.as_str())
            .count(),
        1,
        "重复 run 不得重复落库：{records:?}"
    );
}

/// 刀 3：同一条记录同时出现在 inbox 与 spool 时，补投按 id 幂等，events 恰一条。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spool_and_inbox_duplicate_is_idempotent() {
    let (addr, _state) = start_server(vec![text_response("答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    let session = yaa_store::Session::new(db.clone());

    let notify = yaa_msg::Record::system("notify", "<注释>\n双投\n</注释>", None, None);
    let id = notify.id().to_owned();
    yaa_store::publish_inbox(&session, yaa_store::InboxPhase::Pending, &notify).unwrap();
    yaa_store::publish_spool(&session, &notify).unwrap();

    let output = env.run(&["任务"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_events(&db);
    assert_eq!(
        records
            .iter()
            .filter(|record| record["id"] == id.as_str())
            .count(),
        1,
        "双投应幂等：{records:?}"
    );
    assert!(yaa_store::scan_spool(&session).unwrap().is_empty());
}

/// 刀 3：非空 spool 阻止 `--clear` 归档；`--status` 显示 spool 计数。
#[test]
fn clear_refuses_when_spool_nonempty_and_status_counts_it() {
    let env = TestEnv::new(0);
    let db = env.session_db();
    seed(&db, &[yaa_msg::Record::user("既有")]);
    let session = yaa_store::Session::new(db.clone());
    yaa_store::publish_spool(
        &session,
        &yaa_msg::Record::system("notify", "<注释>\n滞留\n</注释>", None, None),
    )
    .unwrap();

    let status = env.run(&["--status"]);
    assert_eq!(status.status.code(), Some(0));
    assert!(
        stdout_of(&status).contains("滞留 spool：1 条"),
        "stdout：{}",
        stdout_of(&status)
    );

    let cleared = env.run(&["--clear"]);
    assert_eq!(
        cleared.status.code(),
        Some(1),
        "非空 spool 必须拒绝归档：{}",
        stderr_of(&cleared)
    );
    assert!(
        stderr_of(&cleared).contains("待处理"),
        "stderr：{}",
        stderr_of(&cleared)
    );
    assert_eq!(
        yaa_store::count_spool(&session).unwrap(),
        1,
        "不得删除 spool"
    );
    assert!(
        !env.data_home.join("history").exists(),
        "拒绝归档不得产生历史"
    );
}

// ===== 阶段 5 刀 4：后台回注范式与时序 =====

/// I5：目标会话存在但无 runner 时，外部 `--to` 通知只滞留 `pending/`，
/// `--status` 计数随之 +1，且绝不触发任何模型请求。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_to_session_without_runner_stays_pending_and_makes_no_requests() {
    // 给一个可用响应只为让意外请求留下计数，而不是把服务端脚本耗尽。
    let (addr, state) = start_server(vec![text_response("不应被请求")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let target_db = env._root.path().join("target").join("session.db");
    seed(&target_db, &[yaa_msg::Record::user("既有历史")]);
    let target = target_db.to_str().unwrap();

    let before = env.run(&["--status", "--session", target]);
    assert_eq!(
        before.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&before)
    );
    assert!(
        stdout_of(&before).contains("待处理消息：0 条（pending 0 / init 0）"),
        "stdout：{}",
        stdout_of(&before)
    );

    let notify = env.run(&["--to", target, "--notify=无 runner", "--", "printf", "X"]);
    assert_eq!(
        notify.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&notify)
    );

    let pending = read_inbox_at(&target_db, "pending");
    assert_eq!(pending.len(), 1, "目标 pending：{pending:?}");
    assert_eq!(pending[0]["source"], "notify");

    let after = env.run(&["--status", "--session", target]);
    assert_eq!(
        after.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&after)
    );
    assert!(
        stdout_of(&after).contains("待处理消息：1 条（pending 1 / init 0）"),
        "status 计数应 +1：{}",
        stdout_of(&after)
    );

    assert_eq!(
        read_events(&target_db).len(),
        1,
        "通知只滞留 inbox，不得直接写 events"
    );
    assert!(
        state.requests.lock().unwrap().is_empty(),
        "无 runner 的会话不得产生任何模型请求：{:?}",
        state.requests.lock().unwrap()
    );
}

/// I10：模型请求在途（服务端阻塞）时外部 `--to` 注入；放开后本 run 先执行工具，
/// 再在安全点消费通知并继续请求，通知恰落库一次、恰展示一次，最终回复正常。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_injected_while_request_in_flight_is_consumed_once() {
    let env = TestEnv::new(0);
    env.write_config(PLAIN_CONFIG);

    // 服务端收到请求后先 touch 标记，再轮询等待 release 文件；避免固定长 sleep 赌时序。
    let marker = env._root.path().join("request-seen");
    let release = env._root.path().join("release-request");
    let mut blocker = Command::new("sh");
    blocker
        .arg("-c")
        .arg(format!(
            "touch '{}'; while [ ! -e '{}' ]; do sleep 0.02; done",
            marker.display(),
            release.display()
        ))
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let (addr, state) = start_server_with_side_effect(
        vec![
            tool_call_response("call_wait", "true", None, None),
            text_response("通知后答"),
        ],
        blocker,
    )
    .await;
    env.write_llm(&llm_toml_with(addr.port(), "openai"));

    let run = env
        .command()
        .arg("问题")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // 第一个模型请求已到达服务端且响应被阻塞。
    wait_until(|| marker.exists(), "模型请求到达");
    let db = env.session_db();
    let notify = env.run(&[
        "--to",
        db.to_str().unwrap(),
        "--notify=请求期间",
        "--",
        "printf",
        "INJECT",
    ]);
    assert_eq!(
        notify.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&notify)
    );
    let pending = read_inbox_at(&db, "pending");
    assert_eq!(pending.len(), 1, "注入应滞留 pending：{pending:?}");
    let injected_id = pending[0]["id"].as_str().unwrap().to_string();

    std::fs::write(&release, b"go").unwrap();
    let output = run.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );

    let records = read_events(&db);
    let delivered: Vec<_> = records
        .iter()
        .filter(|record| record["id"] == injected_id.as_str())
        .collect();
    assert_eq!(delivered.len(), 1, "通知应恰落库一次：{records:?}");
    assert_eq!(delivered[0]["type"], "system");
    assert_eq!(delivered[0]["source"], "notify");
    assert_eq!(
        stderr_of(&output).matches("# 请求期间").count(),
        1,
        "通知应恰展示一次：{}",
        stderr_of(&output)
    );
    assert!(
        stdout_of(&output).contains("通知后答"),
        "最终回复应来自注入后的请求：{}",
        stdout_of(&output)
    );
    assert_eq!(
        state.requests.lock().unwrap().len(),
        2,
        "注入后应再请求一次模型"
    );
    assert_eq!(
        yaa_store::count_inbox(&yaa_store::Session::new(db), yaa_store::InboxPhase::Pending)
            .unwrap(),
        0,
        "通知应已消费"
    );
}

/// 刀 4（可选）：父的 `tool_call` → 工具内嵌 `yaa --notify --to "$YAA_SESSION"`
/// → 落库 `call_id` 等于父那次调用 id（不手工设置 `YAA_NOTIFY_CALL_ID`）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_embedded_notify_inherits_tool_call_id() {
    let env = TestEnv::new(0);
    env.write_config(PLAIN_CONFIG);
    let command = format!(
        "'{}' --notify=\"链路\" --to \"$YAA_SESSION\" -- printf CHAIN",
        env!("CARGO_BIN_EXE_yaa")
    );
    let (addr, _state) = start_server(vec![
        tool_call_response("call_chain", &command, None, None),
        text_response("完成"),
    ])
    .await;
    env.write_llm(&llm_toml_with(addr.port(), "openai"));

    let output = env.run(&["任务"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let db = env.session_db();
    let records = read_events(&db);
    let notify = records
        .iter()
        .find(|record| {
            record["type"] == "system"
                && record["source"] == "notify"
                && record["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("CHAIN"))
        })
        .unwrap_or_else(|| panic!("应落库一条通知：{records:?}"));
    assert_eq!(
        notify["call_id"], "call_chain",
        "call_id 应继承本次工具调用：{notify}"
    );
    let self_origin = std::path::absolute(&db)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        notify["origin"].as_str(),
        Some(self_origin.as_str()),
        "origin 应为运行中会话：{notify}"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record["id"] == notify["id"])
            .count(),
        1,
        "通知应恰落库一次：{records:?}"
    );
}

// ===== 阶段 5 终审 F-A：core 域缺陷回归 =====

/// F1：父 shell 工具孵化子会话时，子会话 init 钩子的通知必须落子 `inbox/init/`，
/// 不得继承父会话的 `YAA_NOTIFY` 投进父 `pending/`。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_child_init_notify_stays_in_child_session() {
    let env = TestEnv::new(0);
    let config = format!(
        r#"
[hooks]
init = '''
"{}" --notify="系统提示词" -- printf 'SYS:%s' "$YAA_SESSION"
'''

[agent]
max_steps = 10
"#,
        env!("CARGO_BIN_EXE_yaa")
    );
    env.write_config(&config);

    let child_work = env._root.path().join("child-work");
    std::fs::create_dir_all(&child_work).unwrap();
    let command = format!(
        "'{}' --cwd '{}' -- '子任务'",
        env!("CARGO_BIN_EXE_yaa"),
        child_work.display()
    );
    let (addr, _state) = start_server(vec![
        tool_call_response("call_child", &command, None, None),
        text_response("子答"),
        text_response("父答"),
    ])
    .await;
    env.write_llm(&llm_toml_with(addr.port(), "openai"));

    let output = env.run(&["父任务"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let parent_db = env.session_db();
    let child_db = env.session_db_for(&child_work);
    let child_db_abs = std::path::absolute(&child_db)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let child_records = read_events(&child_db);
    let init_notify = child_records
        .iter()
        .find(|record| {
            record["type"] == "system"
                && record["source"] == "notify"
                && record["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("系统提示词"))
        })
        .unwrap_or_else(|| panic!("子会话应落库 init 通知：{child_records:?}"));
    assert!(
        init_notify["content"]
            .as_str()
            .unwrap()
            .contains(&child_db_abs),
        "init 通知应来自子会话自身：{init_notify}"
    );
    assert!(
        read_inbox_at(&child_db, "init").is_empty(),
        "子 init/ 应在 run 内被消费"
    );
    assert!(
        read_inbox_at(&parent_db, "pending").is_empty(),
        "父 pending 不得收到子 init 通知：{:?}",
        read_inbox_at(&parent_db, "pending")
    );
}

/// F2：进程环境继承父工具调用的 `YAA_NOTIFY_CALL_ID` 时，init 钩子发布的通知
/// 必须清空归因，不得把父 call_id 写进子会话记录。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_hook_clears_inherited_call_id() {
    let (addr, _state) = start_server(vec![text_response("好")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(CUSTOM_INIT_CONFIG);

    let output = env
        .command()
        .env("YAA_NOTIFY_CALL_ID", "call_parent")
        .arg("任务")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_events(&env.session_db());
    let notifies: Vec<_> = records
        .iter()
        .filter(|record| record["type"] == "system" && record["source"] == "notify")
        .collect();
    assert_eq!(notifies.len(), 3, "init 钩子应发布三条通知：{records:?}");
    for notify in notifies {
        assert!(
            notify.get("call_id").is_none(),
            "init 通知不得继承父 call_id：{notify}"
        );
    }
}

/// F5 + F8：`--to` 用符号链接或 `..` 表达时与 `--session` 判等通过，且通知落到
/// 真实会话根（不是链接所在目录）。
#[cfg(unix)]
#[test]
fn notify_to_symlink_and_dotdot_forms_are_same_session() {
    let env = TestEnv::new(0);
    let real_dir = env._root.path().join("real");
    std::fs::create_dir_all(&real_dir).unwrap();
    let real_db = real_dir.join("session.db");
    seed(&real_db, &[yaa_msg::Record::user("既有")]);

    let link_db = env._root.path().join("link.db");
    std::os::unix::fs::symlink(&real_db, &link_db).unwrap();

    let output = env.run(&[
        "--to",
        link_db.to_str().unwrap(),
        "--session",
        real_db.to_str().unwrap(),
        "--notify=符号链接",
        "--",
        "printf",
        "L",
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "符号链接与真实路径应判等：{}",
        stderr_of(&output)
    );
    assert_eq!(
        read_inbox_at(&real_db, "pending").len(),
        1,
        "通知应落真实会话 inbox"
    );
    assert!(
        !env._root.path().join("inbox").exists(),
        "不得写到链接所在目录"
    );

    let sub = real_dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let dotdot = sub.join("..").join("session.db");
    let output = env.run(&[
        "--to",
        dotdot.to_str().unwrap(),
        "--session",
        real_db.to_str().unwrap(),
        "--notify=点点",
        "--",
        "printf",
        "D",
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "`..` 形式应判等：{}",
        stderr_of(&output)
    );
    assert_eq!(read_inbox_at(&real_db, "pending").len(), 2);
}

/// F7：`--to` 指向 FIFO 时必须在 1 秒内退出 1，绝不因 `File::open` 阻塞而挂起。
#[cfg(unix)]
#[test]
fn notify_to_fifo_is_rejected_without_hanging() {
    let env = TestEnv::new(0);
    let fifo = env._root.path().join("pipe");
    let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(status.success(), "mkfifo 失败");

    let started = std::time::Instant::now();
    let mut child = env
        .command()
        .args([
            "--to",
            fifo.to_str().unwrap(),
            "--notify=x",
            "--",
            "printf",
            "y",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("FIFO 目标不得挂起（1s 内应退出）");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "应在 1s 内退出"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("不是 yaa 会话数据库"),
        "stderr：{}",
        stderr_of(&output)
    );
}

/// 构造一条带显式 id 的 notify 记录（用于精确控制 spool/inbox 的排序）。
fn notify_with_id(id: &str, content: &str) -> yaa_msg::Record {
    yaa_msg::Record::System(yaa_msg::System {
        id: id.to_owned(),
        ts: 1,
        source: "notify".to_owned(),
        origin: None,
        call_id: None,
        command: None,
        output_files: None,
        content: content.to_owned(),
    })
}

/// F11：spool 补投只消费 spool 自身，不得把遗留的 `init/` 提前合并消费。
///
/// 构造：上一轮遗留的 `init/` 消息 + 一条 id 更大的 spool 消息。修复后 spool 先落库，
/// `init/` 仍在 init 钩子之后按既有 id 顺序消费；旧实现会按 id 把 init 排到 spool 前。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spool_replay_does_not_consume_init_inbox() {
    let (addr, _state) = start_server(vec![text_response("答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let session = yaa_store::Session::new(env.session_db());

    let init_a = notify_with_id("01H000000000000000000000A", "<注释>\nINIT-A\n</注释>");
    let init_b = notify_with_id("01H000000000000000000000B", "<注释>\nINIT-B\n</注释>");
    let spooled = notify_with_id("01H000000000000000000000Z", "<注释>\nSPOOL\n</注释>");
    yaa_store::publish_inbox(&session, yaa_store::InboxPhase::Init, &init_a).unwrap();
    yaa_store::publish_inbox(&session, yaa_store::InboxPhase::Init, &init_b).unwrap();
    yaa_store::publish_spool(&session, &spooled).unwrap();

    let output = env.run(&["任务"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_events(&env.session_db());
    let position = |id: &str| {
        records
            .iter()
            .position(|record| record["id"] == id)
            .unwrap_or_else(|| panic!("缺少记录 {id}：{records:?}"))
    };
    let spool_pos = position(spooled.id());
    let init_a_pos = position(init_a.id());
    let init_b_pos = position(init_b.id());
    assert!(
        spool_pos < init_a_pos && spool_pos < init_b_pos,
        "spool 应在 init 消息之前落库：spool={spool_pos} initA={init_a_pos} initB={init_b_pos}"
    );
    assert!(
        init_a_pos < init_b_pos,
        "init/ 内部顺序不得被打乱：{records:?}"
    );
    assert!(read_inbox_at(&env.session_db(), "init").is_empty());
    assert!(yaa_store::scan_spool(&session).unwrap().is_empty());
}

/// F12：删除 spool 文件失败只提示，不阻断本轮 run；文件留待下一轮幂等消费。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spool_remove_failure_does_not_abort_run() {
    use std::os::unix::fs::PermissionsExt;

    let (addr, _state) = start_server(vec![text_response("答一"), text_response("答二")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    let session = yaa_store::Session::new(db.clone());
    let spooled = notify_with_id("01H000000000000000000000C", "<注释>\n滞留\n</注释>");
    let spool_path = yaa_store::publish_spool(&session, &spooled).unwrap();
    let id = spooled.id().to_owned();

    let spool_dir = session.spool_dir();
    std::fs::set_permissions(&spool_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let first = env.run(&["任务"]);
    // 立即恢复权限，保证临时目录可清理。
    std::fs::set_permissions(&spool_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        first.status.success(),
        "删除 spool 失败不得阻断 run：{}",
        stderr_of(&first)
    );
    assert!(
        stderr_of(&first).contains("删除 spool 文件失败"),
        "应发出 Notice：{}",
        stderr_of(&first)
    );
    assert!(spool_path.exists(), "删除失败时文件应留待下一轮");

    let second = env.run(&["继续"]);
    assert!(second.status.success(), "stderr：{}", stderr_of(&second));
    assert!(!spool_path.exists(), "下一轮应成功删除");
    let records = read_events(&db);
    assert_eq!(
        records
            .iter()
            .filter(|record| record["id"] == id.as_str())
            .count(),
        1,
        "重放应幂等：{records:?}"
    );
}

// ===== 阶段 5 终审 F-B：CLI 域缺陷回归 =====

/// F4：`origin` 是可选归因；cwd 默认会话存在旧 `default.jsonl` 导致发布者解析失败时，
/// 显式 `--to` 的合法目标仍应成功发布，只是省略 `origin`。
#[test]
fn notify_to_explicit_target_succeeds_when_origin_resolution_fails() {
    let env = TestEnv::new(0);
    let target_db = env._root.path().join("target").join("session.db");
    seed(&target_db, &[yaa_msg::Record::user("既有")]);

    // 让 cwd 默认会话目录存在旧 `default.jsonl` 且没有 `session.db`：
    // `resolve_session_target` 会拒绝解析，但 `--to` 的目标本身合法。
    let default_dir = env.session_db().parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&default_dir).unwrap();
    std::fs::write(default_dir.join("default.jsonl"), b"{}\n").unwrap();

    let output = env.run(&[
        "--to",
        target_db.to_str().unwrap(),
        "--notify=无归因",
        "--",
        "printf",
        "X",
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "origin 解析失败不得阻断显式 --to：{}",
        stderr_of(&output)
    );

    let pending = read_inbox_at(&target_db, "pending");
    assert_eq!(pending.len(), 1, "通知应落目标 inbox：{pending:?}");
    assert!(
        pending[0].get("origin").is_none(),
        "origin 解析失败时应省略该可选字段：{}",
        pending[0]
    );
}

/// F9：目标库被长事务阻塞时，notify 预检 BUSY 降级写 spool，命令退出码不变；
/// 下次 run 恰好补投一次。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_precheck_busy_falls_back_to_spool_and_replays_once() {
    let (addr, _state) = start_server(vec![text_response("答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);

    let target_db = env._root.path().join("target").join("session.db");
    seed(&target_db, &[yaa_msg::Record::user("既有")]);

    // 用独立线程持 EXCLUSIVE 事务。切到 rollback journal 模式，使读路径也会被阻塞。
    // 命令先 touch 标记：标记出现说明预检已 BUSY 失败、命令已开始；此时再释放锁，
    // 让随后的 spool 写入能读到 generation，避免用固定 sleep 赌时序。
    let marker = env._root.path().join("precheck-failed");
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let locked_db = target_db.clone();
    let locker = std::thread::spawn(move || {
        let conn = rusqlite::Connection::open(&locked_db).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        conn.execute_batch("BEGIN EXCLUSIVE").unwrap();
        release_rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .expect("测试应在预检失败后释放锁");
        conn.execute_batch("ROLLBACK").unwrap();
    });

    let command = format!("touch '{}'; printf X", marker.display());
    let child = env
        .command()
        .args([
            "--session",
            target_db.to_str().unwrap(),
            "--notify=忙碌",
            "--",
            "sh",
            "-c",
            &command,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "预检未在预期时间内因 BUSY 失败"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    release_tx.send(()).unwrap();

    let notify = child.wait_with_output().unwrap();
    locker.join().unwrap();
    assert_eq!(
        notify.status.code(),
        Some(0),
        "预检 BUSY 不得改变命令退出码：{}",
        stderr_of(&notify)
    );
    assert!(
        stderr_of(&notify).contains("spool"),
        "应提示已写入 spool：{}",
        stderr_of(&notify)
    );

    let session = yaa_store::Session::new(target_db.clone());
    let spooled = yaa_store::scan_spool(&session).unwrap();
    assert_eq!(spooled.len(), 1, "预检 BUSY 应降级写 spool");
    let spooled_id = spooled[0].id().to_owned();

    let replay = env.run(&["--session", target_db.to_str().unwrap(), "任务"]);
    assert!(replay.status.success(), "stderr：{}", stderr_of(&replay));
    let records = read_events(&target_db);
    assert_eq!(
        records
            .iter()
            .filter(|record| record["id"] == spooled_id.as_str())
            .count(),
        1,
        "spool 应恰好补投一次：{records:?}"
    );
    assert!(yaa_store::scan_spool(&session).unwrap().is_empty());
}

/// F13/F14/F15：`--notify` 与 `--clear`/`--status`/`--get-prompt`/位置提示词组合
/// 一律退出 2，并给出明确文案，不得静默忽略后者。
#[test]
fn conflicting_notify_combinations_exit_two() {
    let env = TestEnv::new(0);
    for args in [
        vec!["--notify=x", "--clear"],
        vec!["--notify=x", "--status"],
        vec!["--notify=x", "--get-prompt", "system"],
        vec!["--notify=x", "提示词"],
    ] {
        let output = env.run(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?} 应为用法错误：{}",
            stderr_of(&output)
        );
        let stderr = stderr_of(&output);
        assert!(!stderr.trim().is_empty(), "{args:?} 应有明确文案");
        assert!(
            stderr.contains("--notify"),
            "{args:?} 文案应指明 --notify：{stderr}"
        );
    }
}

// ===== 阶段 5 终审 F-C：提示词后台范式回归 =====

/// F3：提示词推荐的后台范式必须在 shell 工具内可用。
///
/// 整条后台作业重定向 stdout/stderr 后，工具应立即返回（<1s）且无读取超时占位；
/// 后台 `yaa --notify` 约 2s 后把通知投到目标 inbox。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_notify_paradigm_returns_fast_and_delivers() {
    let env = TestEnv::new(0);
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    let command = format!(
        "{{ sleep 2; '{}' --notify=\"后台到达\" --to \"$YAA_SESSION\" -- printf 'BG'; }} >/dev/null 2>&1 &",
        env!("CARGO_BIN_EXE_yaa")
    );
    let (addr, _state) = start_server(vec![
        tool_call_response("call_bg", &command, None, None),
        text_response("完成"),
    ])
    .await;
    env.write_llm(&llm_toml_with(addr.port(), "openai"));

    let started = std::time::Instant::now();
    let child = env
        .command()
        .arg("任务")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // 轮询 events 直到本次工具结果落库：1s 内必须返回。
    let session = yaa_store::Session::new(db.clone());
    let deadline = started + std::time::Duration::from_secs(1);
    let mut tool_result: Option<yaa_msg::ToolResult> = None;
    while std::time::Instant::now() < deadline {
        if let Ok(records) = yaa_store::load(&session)
            && let Some(result) = records.into_iter().find_map(|record| match record {
                yaa_msg::Record::ToolResult(result) if result.call_id == "call_bg" => Some(result),
                _ => None,
            })
        {
            tool_result = Some(result);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let tool_result = tool_result.expect("shell 工具应在 1s 内返回");
    assert_eq!(tool_result.code, 0, "后台作业的 shell 应以 0 返回");
    assert!(
        !tool_result.stdout.contains("读取超时") && !tool_result.stderr.contains("读取超时"),
        "不得出现读取超时占位：stdout={:?} stderr={:?}",
        tool_result.stdout,
        tool_result.stderr
    );

    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    // 后台通知约 2s 后到达；轮询 pending 与 events，上限 4s。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    let mut pending = Vec::new();
    while std::time::Instant::now() < deadline {
        pending = read_inbox_at(&db, "pending");
        if !pending.is_empty() || delivered_notify(&db) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        !pending.is_empty() || delivered_notify(&db),
        "后台通知应在 ~3s 内到达 inbox 或落库：pending={pending:?}"
    );
}

/// events 中是否已有「后台到达」通知（runner 可能已消费）。
fn delivered_notify(db: &Path) -> bool {
    read_events(db).iter().any(|record| {
        record["type"] == "system"
            && record["source"] == "notify"
            && record["content"]
                .as_str()
                .is_some_and(|content| content.contains("后台到达"))
    })
}
