//! `yaa` 二进制的端到端测试。
//!
//! 每个测试起一个只监听 `127.0.0.1:0` 的假 OpenAI 兼容服务，并用相互独立的临时
//! `YAA_CONFIG_HOME`/`YAA_DATA_HOME` 运行**真实二进制**（`CARGO_BIN_EXE_yaa`）。
//! 绝不读写真实 `$HOME`。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tempfile::TempDir;

/// 测试配置：三个钩子都用确定性输出，`llm` 钩子从临时配置根读取含密钥的 TOML。
const CONFIG_TOML: &str = r#"
[hooks]
system_prompt = "echo SYS"
user_prompt = "echo USR"
llm = "cat $YAA_CONFIG_HOME/llm.toml"

[shell]
timeout_secs = 30

[agent]
max_steps = 10
"#;

/// 假服务端共享状态：按序返回脚本化响应，并记录每个收到的请求体。
#[derive(Clone)]
struct ServerState {
    requests: Arc<Mutex<Vec<Value>>>,
    responses: Arc<Mutex<VecDeque<Value>>>,
}

/// 处理一次 Chat Completions 请求。
async fn chat(State(state): State<ServerState>, Json(body): Json<Value>) -> Json<Value> {
    state.requests.lock().unwrap().push(body);
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

/// 首个响应：助手发起一个 `shell` 工具调用。
fn tool_call_response() -> Value {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "shell",
                        "arguments": "{\"command\":\"echo hi\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    })
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

        std::fs::write(config_home.join("config.toml"), CONFIG_TOML).unwrap();
        std::fs::write(
            config_home.join("llm.toml"),
            format!(
                "base_url = \"http://127.0.0.1:{port}\"\n\
                 api_key = \"sk-secret\"\n\
                 model = \"fake-model\"\n"
            ),
        )
        .unwrap();

        Self {
            _root: root,
            work,
            config_home,
            data_home,
        }
    }

    /// 构造指向真实二进制、环境隔离的命令。
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_yaa"));
        command
            .current_dir(&self.work)
            .env("YAA_CONFIG_HOME", &self.config_home)
            .env("YAA_DATA_HOME", &self.data_home)
            // 双重隔离：即便 YAA_* 被意外忽略，HOME 也指向临时目录。
            .env("HOME", &self.work);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    /// 默认会话文件 `<data>/sessions/<sid>/default.jsonl`。
    fn session_file(&self) -> PathBuf {
        let sessions = self.data_home.join("sessions");
        for entry in std::fs::read_dir(&sessions).unwrap() {
            let candidate = entry.unwrap().path().join("default.jsonl");
            if candidate.exists() {
                return candidate;
            }
        }
        panic!("未找到会话 JSONL：{}", sessions.display());
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

/// 统计提示词钩子的 `tool_call` 数。
fn prompt_hook_calls(records: &[Value]) -> usize {
    records
        .iter()
        .filter(|record| {
            record["type"] == "tool_call"
                && (record["hook"]["name"] == "system_prompt"
                    || record["hook"]["name"] == "user_prompt")
        })
        .count()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_tool_call_then_finish() {
    let (addr, state) = start_server(vec![tool_call_response(), text_response("完成")]).await;
    let env = TestEnv::new(addr.port());

    let output = env.run(&["你好"]);
    assert!(
        output.status.success(),
        "退出码非零，stderr：{}",
        stderr_of(&output)
    );
    assert!(
        stdout_of(&output).contains("完成"),
        "stdout：{}",
        stdout_of(&output)
    );

    // 断言 2：JSONL 顺序。
    let session = env.session_file();
    let records = read_jsonl(&session);

    assert_eq!(records[0]["type"], "tool_call");
    assert_eq!(records[0]["origin"], "hook");
    assert_eq!(records[0]["hook"]["name"], "system_prompt");
    assert_eq!(records[1]["type"], "tool_result");

    assert_eq!(records[2]["type"], "tool_call");
    assert_eq!(records[2]["hook"]["name"], "user_prompt");
    assert_eq!(records[3]["type"], "tool_result");

    let user_pos = records
        .iter()
        .position(|record| record["type"] == "user")
        .expect("应有 user 记录");
    let llm_pos = records
        .iter()
        .position(|record| record["type"] == "tool_call" && record["hook"]["name"] == "llm")
        .expect("应有 llm 审计记录");
    assert!(llm_pos < user_pos, "llm 审计记录应在 user 之前");
    assert_eq!(records[llm_pos]["redacted"], true, "llm 记录必须 redacted");
    assert_eq!(records[user_pos]["content"], "你好");

    let ai_pos = records
        .iter()
        .position(|record| record["type"] == "ai")
        .expect("应有首个 ai 记录");
    assert!(ai_pos > user_pos);

    let ai_call_pos = records
        .iter()
        .position(|record| record["type"] == "tool_call" && record["origin"] == "ai")
        .expect("应有 ai 来源的工具调用");
    assert!(ai_call_pos > ai_pos);
    assert_eq!(records[ai_call_pos]["call_id"], "call_1");
    assert!(
        records[ai_call_pos]["command"]
            .as_str()
            .unwrap()
            .contains("echo hi")
    );

    let result_pos = records
        .iter()
        .position(|record| record["type"] == "tool_result" && record["call_id"] == "call_1")
        .expect("应有配对的工具结果");
    assert!(result_pos > ai_call_pos);
    assert_eq!(records[result_pos]["stdout"], "hi\n");

    // 断言 4：JSONL 不含密钥。
    let raw = std::fs::read_to_string(&session).unwrap();
    assert!(!raw.contains("sk-secret"), "JSONL 不得含 api_key");

    // 断言 3：第二跳请求体是合法 OpenAI 序列。
    let requests = state.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "应有两次模型请求");
    let body_text = serde_json::to_string(&requests).unwrap();
    assert!(!body_text.contains("sk-secret"), "请求体不得含 api_key");

    let messages = requests[1]["messages"]
        .as_array()
        .expect("请求体应含 messages");
    let assistant_idx = messages
        .iter()
        .position(|message| message["role"] == "assistant" && message.get("tool_calls").is_some())
        .expect("第二跳应含带工具调用的 assistant 消息");
    let assistant = &messages[assistant_idx];
    let following = &messages[assistant_idx + 1];
    assert_eq!(
        following["role"], "tool",
        "assistant.tool_calls 后必须紧跟 tool"
    );
    assert_eq!(
        following["tool_call_id"], assistant["tool_calls"][0]["id"],
        "tool 消息的 tool_call_id 必须与 assistant 配对"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_prompt_short_circuits_without_session() {
    let env = TestEnv::new(0);

    let output = env.run(&["--get-prompt", "system"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_of(&output);
    assert!(stdout.contains("你是 YAA"), "stdout：{stdout}");
    assert!(
        !env.data_home.join("sessions").exists(),
        "--get-prompt 不应创建会话文件"
    );

    let unknown = env.run(&["--get-prompt", "nope"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(
        stderr_of(&unknown).contains("未知"),
        "stderr：{}",
        stderr_of(&unknown)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_run_reuses_prompt_hook_cache() {
    let (addr, _state) = start_server(vec![
        tool_call_response(),
        text_response("完成"),
        text_response("再次完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());

    let first = env.run(&["你好"]);
    assert!(
        first.status.success(),
        "第一次运行失败：{}",
        stderr_of(&first)
    );

    let second = env.run(&["继续"]);
    assert!(
        second.status.success(),
        "第二次运行失败：{}",
        stderr_of(&second)
    );

    let records = read_jsonl(&env.session_file());
    assert_eq!(
        prompt_hook_calls(&records),
        2,
        "第二次运行应复用提示词钩子缓存"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_archives_default_jsonl() {
    let (addr, _state) = start_server(vec![text_response("完成")]).await;
    let env = TestEnv::new(addr.port());

    let run = env.run(&["你好"]);
    assert!(run.status.success(), "运行失败：{}", stderr_of(&run));
    let session = env.session_file();

    let cleared = env.run(&["--clear"]);
    assert!(
        cleared.status.success(),
        "--clear 失败：{}",
        stderr_of(&cleared)
    );
    assert!(!session.exists(), "default.jsonl 应已归档");

    let history_dir = env.data_home.join("history");
    let stdout = stdout_of(&cleared);
    assert!(
        stdout.contains(&history_dir.to_string_lossy().to_string()),
        "stdout 应打印归档路径：{stdout}"
    );

    let mut archived = None;
    for sid in std::fs::read_dir(&history_dir).unwrap() {
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
}
