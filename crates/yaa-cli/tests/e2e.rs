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
use axum::http::{HeaderMap, header};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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

/// 递归防护配置：`system_prompt` 钩子调用二进制自身的 `--get-prompt system`。
const RECURSION_CONFIG_TOML: &str = r#"
[hooks]
system_prompt = '"$YAA_BIN" --get-prompt system'
user_prompt = "echo USR"
llm = "cat $YAA_CONFIG_HOME/llm.toml"

[agent]
max_steps = 10
"#;

/// 小阈值截断配置：头 32 + 尾 16 ≤ 上限 64，保证大输出必被截断。
const TRUNCATION_CONFIG_TOML: &str = r#"
[hooks]
system_prompt = "echo SYS"
user_prompt = "echo USR"
llm = "cat $YAA_CONFIG_HOME/llm.toml"

[shell]
timeout_secs = 30
max_stream_chars = 64
head_chars = 32
tail_chars = 16

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

/// 首个响应：助手发起一个 `shell` 工具调用。
fn tool_call_response() -> Value {
    tool_call_response_with("call_1", "echo hi")
}

/// 指定 `call_id` 与命令的工具调用响应。
fn tool_call_response_with(call_id: &str, command: &str) -> Value {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": "shell",
                        "arguments": json!({"command": command}).to_string()
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

    /// 覆盖 `config.toml`。
    fn write_config(&self, text: &str) {
        std::fs::write(self.config_home.join("config.toml"), text).unwrap();
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
    ///
    /// `sid` 由规范化工作目录的 sha256 前 32 位十六进制决定，与 core 的算法一致；
    /// 直接从 sid 拼路径，避免扫描 `sessions/` 带来的歧义。
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

/// 从记录中找指定 `call_id` 的 `tool_result`。
fn tool_result<'a>(records: &'a [Value], call_id: &str) -> &'a Value {
    records
        .iter()
        .find(|record| record["type"] == "tool_result" && record["call_id"] == call_id)
        .unwrap_or_else(|| panic!("应有 call_id = {call_id} 的工具结果"))
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

    // 断言 2：JSONL 顺序。用户记录先落盘（配置/钩子失败也不丢用户输入），
    // 随后才是提示词钩子与 llm 审计记录。
    let session = env.session_file();
    let records = read_jsonl(&session);

    assert_eq!(records[0]["type"], "user");
    assert_eq!(records[0]["content"], "你好");

    assert_eq!(records[1]["type"], "tool_call");
    assert_eq!(records[1]["origin"], "hook");
    assert_eq!(records[1]["hook"]["name"], "system_prompt");
    assert_eq!(records[2]["type"], "tool_result");

    assert_eq!(records[3]["type"], "tool_call");
    assert_eq!(records[3]["hook"]["name"], "user_prompt");
    assert_eq!(records[4]["type"], "tool_result");

    let user_pos = 0usize;
    let llm_pos = records
        .iter()
        .position(|record| record["type"] == "tool_call" && record["hook"]["name"] == "llm")
        .expect("应有 llm 审计记录");
    assert!(llm_pos > user_pos, "llm 审计记录应在 user 之后");
    assert_eq!(records[llm_pos]["redacted"], true, "llm 记录必须 redacted");
    assert_eq!(
        records[llm_pos]["command"], "",
        "llm 审计记录不得保存可能内联密钥的命令"
    );

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

    // 断言 5a：首个请求的 system 消息含两个提示词钩子的输出。
    let first_system: String = requests[0]["messages"]
        .as_array()
        .expect("首个请求应含 messages")
        .iter()
        .filter(|message| message["role"] == "system")
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        first_system.contains("SYS"),
        "首个请求应含系统提示词：{first_system}"
    );
    assert!(
        first_system.contains("USR"),
        "首个请求应含用户提示词：{first_system}"
    );

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
    // 断言 5b：第二跳的 tool 消息回填了 Shell stdout。
    let tool_content = following["content"].as_str().unwrap_or_default();
    assert!(
        tool_content.contains("hi\n"),
        "tool 消息应含 shell stdout：{tool_content}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authorization_header_is_sent_on_every_request() {
    let (addr, state) = start_server(vec![tool_call_response(), text_response("完成")]).await;
    let env = TestEnv::new(addr.port());

    let output = env.run(&["你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let auth = state.auth.lock().unwrap().clone();
    assert_eq!(auth.len(), 2, "应有两次请求");
    assert!(
        auth.iter()
            .all(|value| value.as_deref() == Some("Bearer sk-secret")),
        "每个请求都应携带 `Bearer sk-secret`（证明 llm 钩子输出被消费）：{auth:?}"
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
async fn system_prompt_hook_calling_get_prompt_does_not_recurse() {
    // 验收标准 6：默认钩子通过 `yaa --get-prompt system` 拼内置模板，必须能正常返回。
    let (addr, state) = start_server(vec![text_response("完成")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(RECURSION_CONFIG_TOML);

    let output = env.run(&["你好"]);
    assert!(
        output.status.success(),
        "递归防护失败，退出码 {:?}，stderr：{}",
        output.status.code(),
        stderr_of(&output)
    );

    let requests = state.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "应完成一次模型请求");
    let has_builtin = requests[0]["messages"]
        .as_array()
        .expect("请求体应含 messages")
        .iter()
        .filter(|message| message["role"] == "system")
        .filter_map(|message| message["content"].as_str())
        .any(|content| content.contains("你是 YAA"));
    assert!(has_builtin, "首个请求应含内置系统模板文本");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_output_is_truncated_and_full_file_persisted() {
    let (addr, _state) = start_server(vec![
        tool_call_response_with("call_big", "seq 1 200"),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(TRUNCATION_CONFIG_TOML);

    let output = env.run(&["你好"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_jsonl(&env.session_file());
    let result = tool_result(&records, "call_big");
    assert_eq!(result["truncated"], true, "大输出应标记截断");

    let stdout_path = result["output_files"]["stdout"]
        .as_str()
        .expect("截断应记录 stdout 文件");
    assert!(
        !stdout_path.starts_with('/'),
        "数据根内应为相对路径：{stdout_path}"
    );
    assert!(
        stdout_path.starts_with("sessions/"),
        "应为相对数据根且位于 sessions/ 下：{stdout_path}"
    );

    let full_path = env.data_home.join(stdout_path);
    let full = std::fs::read_to_string(&full_path)
        .unwrap_or_else(|err| panic!("完整输出文件应存在 {}：{err}", full_path.display()));
    let in_record = result["stdout"].as_str().unwrap_or_default();
    assert!(
        full.chars().count() > in_record.chars().count(),
        "完整文件（{} 字符）应长于记录内文本（{} 字符）",
        full.chars().count(),
        in_record.chars().count()
    );
    assert!(full.trim_end().ends_with("200"), "完整文件应含全部输出");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_outside_data_root_keeps_absolute_output_path() {
    let (addr, _state) = start_server(vec![
        tool_call_response_with("call_big", "seq 1 200"),
        text_response("完成"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(TRUNCATION_CONFIG_TOML);

    // 会话文件放在数据根之外的兄弟目录。
    let session_path = env
        .data_home
        .parent()
        .unwrap()
        .join("outside")
        .join("custom.jsonl");
    assert!(!session_path.starts_with(&env.data_home));

    let output = env
        .command()
        .arg("--session")
        .arg(&session_path)
        .arg("你好")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_jsonl(&session_path);
    let result = tool_result(&records, "call_big");
    assert_eq!(result["truncated"], true);

    let stdout_path = result["output_files"]["stdout"]
        .as_str()
        .expect("截断应记录 stdout 文件");
    let path = Path::new(stdout_path);
    assert!(
        path.is_absolute(),
        "数据根之外必须回退记绝对路径：{stdout_path}"
    );
    assert!(path.exists(), "输出文件应存在：{stdout_path}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_llm_hook_exits_with_config_code() {
    let env = TestEnv::new(0);
    // 删除 llm.toml：`cat` 钩子失败，属配置/钩子错误（退出码 3）。
    std::fs::remove_file(env.config_home.join("llm.toml")).unwrap();

    let output = env.run(&["你好"]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "llm 配置缺失应为配置错误，stderr：{}",
        stderr_of(&output)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_flag_is_usage_error() {
    let env = TestEnv::new(0);
    let output = env.run(&["--definitely-not-a-flag"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "未知参数应为用法错误，stderr：{}",
        stderr_of(&output)
    );
    assert!(
        stdout_of(&output).is_empty(),
        "用法错误不应污染 stdout：{}",
        stdout_of(&output)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_on_missing_session_creates_no_directory() {
    // `--status` 是只读运维：解析会话不得物化空的 `sessions/<sid>/` 目录。
    let env = TestEnv::new(0);

    let output = env.run(&["--status"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&output)
    );
    assert!(
        !env.data_home.join("sessions").exists(),
        "--status 不得创建会话目录：{}",
        env.data_home.join("sessions").display()
    );

    let clear = env.run(&["--clear"]);
    assert_eq!(
        clear.status.code(),
        Some(0),
        "stderr：{}",
        stderr_of(&clear)
    );
    assert!(
        !env.data_home.join("sessions").exists(),
        "--clear 不得创建会话目录：{}",
        env.data_home.join("sessions").display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_word_prompt_joined() {
    let (addr, _state) = start_server(vec![text_response("完成")]).await;
    let env = TestEnv::new(addr.port());

    let output = env.run(&["你好", "世界", "！"]);
    assert!(output.status.success(), "stderr：{}", stderr_of(&output));

    let records = read_jsonl(&env.session_file());
    assert_eq!(records[0]["type"], "user");
    assert_eq!(
        records[0]["content"], "你好 世界 ！",
        "多词位置参数应以单个空格连接"
    );
}
