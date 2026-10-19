//! `crash.rs` 与后续 harness 共用的最小测试基础设施。
//!
//! 这些辅助直接摘自 `e2e.rs`（`TestEnv`、假 OpenAI 服务端、记录读取与轮询工具）。
//! 阶段 6 刀 5 只把崩溃矩阵需要的子集提取到这里；`e2e.rs` 保持原状，避免在并行改动
//! 期间大范围移动既有测试代码。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// 只做初始化簿记（`init = "true"`）的最小配置。
pub const PLAIN_CONFIG: &str = r#"
[hooks]
init = "true"

[agent]
max_steps = 10
"#;

/// `init` 故意慢 1 秒：给测试留出「own 已发布、尚未消费」的观察窗口。
pub const SLOW_INIT_CONFIG: &str = r#"
[hooks]
init = "sleep 1"

[agent]
max_steps = 10
"#;

/// 假服务端共享状态。
#[derive(Clone)]
pub struct ServerState {
    /// 所有请求体，按到达顺序。
    pub requests: Arc<Mutex<Vec<Value>>>,
    /// 脚本化响应队列。
    pub responses: Arc<Mutex<VecDeque<Value>>>,
    /// 首个请求返回前执行一次的外部副作用。
    pub before_response: Arc<Mutex<Option<Command>>>,
}

async fn chat(State(state): State<ServerState>, Json(body): Json<Value>) -> Json<Value> {
    state.requests.lock().unwrap().push(body);
    // 同步执行一次外部写入/等待，模拟模型请求期间的并发事件或阻塞。
    // 先释放状态锁再阻塞，避免一个慢请求卡住其他并发请求。
    let side_effect = state.before_response.lock().unwrap().take();
    if let Some(mut command) = side_effect {
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
pub async fn start_server(responses: Vec<Value>) -> (SocketAddr, ServerState) {
    start_server_with(responses, None).await
}

/// 启动假服务端，并在首个请求返回前执行一次 `side_effect`（阻塞至其结束）。
pub async fn start_server_with_side_effect(
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
pub fn text_response(content: &str) -> Value {
    json!({
        "choices": [{
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }]
    })
}

/// 工具调用响应。
pub fn tool_call_response(call_id: &str, command: &str) -> Value {
    let arguments = json!({ "command": command }).to_string();
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": Value::Null,
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
pub fn llm_toml_with(port: u16, endpoint_format: &str) -> String {
    format!(
        "endpoint_format = \"{endpoint_format}\"\n\
         base_url = \"http://127.0.0.1:{port}\"\n\
         api_key = \"sk-secret\"\n\
         model = \"fake-model\"\n"
    )
}

/// 隔离的临时运行环境。
pub struct TestEnv {
    root: TempDir,
    pub work: PathBuf,
    pub config_home: PathBuf,
    pub data_home: PathBuf,
}

impl TestEnv {
    pub fn new(port: u16) -> Self {
        let root = TempDir::new().unwrap();
        let work = root.path().join("work");
        let config_home = root.path().join("config");
        let data_home = root.path().join("data");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::create_dir_all(&data_home).unwrap();

        std::fs::write(config_home.join("config.toml"), PLAIN_CONFIG).unwrap();
        std::fs::write(config_home.join("llm.toml"), llm_toml_with(port, "openai")).unwrap();

        Self {
            root,
            work,
            config_home,
            data_home,
        }
    }

    /// 临时环境根目录（存放测试自己的标记文件等）。
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn write_config(&self, text: &str) {
        std::fs::write(self.config_home.join("config.toml"), text).unwrap();
    }

    /// 构造指向真实二进制、环境隔离的命令。
    pub fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_yaa"));
        command
            .current_dir(&self.work)
            .env("YAA_CONFIG_HOME", &self.config_home)
            .env("YAA_DATA_HOME", &self.data_home)
            .env("HOME", &self.work)
            .env_remove("YAA_SESSION");
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    /// 默认会话数据库 `<data>/sessions/<sid>/session.db`。
    pub fn session_db(&self) -> PathBuf {
        let canon = self.work.canonicalize().unwrap();
        let mut hasher = Sha256::new();
        hasher.update(canon.as_os_str().as_encoded_bytes());
        let digest = format!("{:x}", hasher.finalize());
        let sid = &digest[..32];
        self.data_home.join("sessions").join(sid).join("session.db")
    }

    /// 默认会话的 inbox 根目录。
    pub fn inbox_root(&self) -> PathBuf {
        self.session_db()
            .parent()
            .expect("session.db 应有父目录")
            .join("inbox")
    }
}

/// 读取会话数据库的全部记录。
pub fn read_records(db: &Path) -> Vec<yaa_msg::Record> {
    yaa_store::load(&yaa_store::Session::new(db)).unwrap()
}

/// 用 store API 预置历史记录。
pub fn seed(db: &Path, records: &[yaa_msg::Record]) {
    let session = yaa_store::Session::new(db);
    yaa_store::append(&session, records).unwrap();
}

/// 数据库 `PRAGMA integrity_check` 是否为 `ok`；库不存在返回 false。
pub fn integrity_ok(db: &Path) -> bool {
    let Ok(conn) = rusqlite::Connection::open(db) else {
        return false;
    };
    conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        .is_ok_and(|result| result == "ok")
}

pub fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// 轮询等待条件成立，超时则失败。
pub fn wait_until(mut condition: impl FnMut() -> bool, description: &str) {
    wait_until_timeout(
        std::time::Duration::from_secs(5),
        &mut condition,
        description,
    );
}

/// 带自定义超时的轮询等待。
pub fn wait_until_timeout(
    timeout: std::time::Duration,
    condition: &mut impl FnMut() -> bool,
    description: &str,
) {
    let deadline = std::time::Instant::now() + timeout;
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "等待超时：{description}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// 轮询等待文件出现。
pub fn wait_for_path(path: &Path, timeout: std::time::Duration) {
    wait_until_timeout(
        timeout,
        &mut || path.exists(),
        &format!("文件出现：{}", path.display()),
    );
}

/// 递归收集目录下满足条件的文件。
pub fn find_files(dir: &Path, predicate: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(find_files(&path, predicate));
        } else if predicate(&path) {
            found.push(path);
        }
    }
    found
}

/// SIGKILL 子进程并回收，避免僵尸。
pub fn kill_and_wait(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 把文件的修改时间回拨到 `age` 之前（构造陈旧残留）。
pub fn backdate(path: &Path, age: std::time::Duration) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(std::time::SystemTime::now() - age)
        .unwrap();
}
