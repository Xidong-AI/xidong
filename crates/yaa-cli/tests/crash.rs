//! 阶段 6 刀 5：崩溃注入矩阵。
//!
//! 手法以 **B 状态注入**为主（确定性、低成本）：手工构造崩溃后可能出现的磁盘状态，
//! 再用真实二进制重试；**A 随机 kill 烟雾**用于验证 SQLite 事务与文件原子替换的边界。
//!
//! 每个场景结束都会调用 `yaa_core::test_support` 的检查器：未收敛状态用
//! `check_consistency`，成功重试后用 `check_converged`。失败信息带
//! `Violations::describe()`。
//!
//! 注入点 ↔ 测试名映射（与 `docs/design.md` 的 14 格矩阵一致）：
//! 1. append 事务中 → [`kill_during_append_never_leaves_half_batch`]
//! 2. `ai`+`tool_call` 批后、工具执行中 →
//!    [`restart_after_sigkill_recovers_interrupted_tool_call`]（e2e）
//! 3. 工具执行中、结果前 → 同 2
//! 4. `tool_result` 后、轮末 `ai` 前 → [`existing_tool_result_is_not_duplicated_on_resume`]
//! 5. notify 发布中（tmp 后 rename 前）→ `notify_cleans_stale_inbox_tmp`（e2e）
//! 6. inbox 消费 append 后、删除前 → [`replayed_consumed_inbox_file_is_idempotent`]
//! 7. spool 写入/补投删除前 →
//!    `notify_falls_back_to_spool_and_next_run_replays_once`（e2e）
//! 8. 归档 VACUUM 中 → [`kill_during_archive_vacuum_keeps_primary_readable`]
//! 9. 归档 rename 后、清空前 → [`crash_after_archive_rename_before_clear_can_retry`]
//! 10. 归档清空事务中 → [`kill_during_archive_clear_transaction_is_atomic`]
//! 11. init 建库中 → [`zero_byte_database_self_heals`]、
//!     [`partial_database_reports_clear_error_then_retry`]、
//!     [`concurrent_first_runs_on_zero_byte_database`]
//! 12. 接管释放锁瞬间 → [`second_takeover_after_two_kills_converges`]
//! 13. 模型请求中 kill → [`kill_during_model_request_loses_no_message`]
//! 14. 发布者写 tmp 前 SIGKILL → 文档格（不承诺，见 `docs/design.md` 保障分级）
//!
//! 额外覆盖 INV7：发布者发布成功后被 SIGKILL，目标下一次 run 仍恰消费一次。

mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::*;
use yaa_msg::{Record, ToolOrigin};

/// 单个会话数据库对应的高层句柄。
fn session(db: &Path) -> yaa_store::Session {
    yaa_store::Session::new(db)
}

fn assert_consistent(db: &Path) {
    let violations = yaa_core::test_support::check_consistency(&session(db));
    assert!(violations.is_empty(), "{}", violations.describe());
}

fn assert_converged(db: &Path) {
    let violations = yaa_core::test_support::check_converged(&session(db));
    assert!(violations.is_empty(), "{}", violations.describe());
}

fn user_contents(records: &[Record]) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::User(user) => Some(user.content.clone()),
            _ => None,
        })
        .collect()
}

fn ai_contents(records: &[Record]) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::Ai(ai) => Some(ai.content.clone()),
            _ => None,
        })
        .collect()
}

fn results_for<'a>(records: &'a [Record], call_id: &str) -> Vec<&'a yaa_msg::ToolResult> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::ToolResult(result) if result.call_id == call_id => Some(result),
            _ => None,
        })
        .collect()
}

fn record_ids(records: &[Record]) -> HashSet<String> {
    records
        .iter()
        .map(|record| record.id().to_owned())
        .collect()
}

fn pending_ids(db: &Path) -> HashSet<String> {
    yaa_store::scan_inbox(&session(db), yaa_store::InboxPhase::Pending)
        .unwrap()
        .into_iter()
        .map(|message| message.id().to_owned())
        .collect()
}

/// INV1：崩溃前已提交（events）或已发布（inbox/pending）的记录，在成功重跑后必须都在
/// events 中。只在确有磁盘状态时收集，避免把「尚未发布」误当成丢失。
fn durable_ids(db: &Path) -> HashSet<String> {
    let mut ids = pending_ids(db);
    if db.exists() {
        ids.extend(record_ids(&read_records(db)));
    }
    ids
}

fn assert_durable_subset(db: &Path, durable: &HashSet<String>, context: &str) {
    let final_ids = record_ids(&read_records(db));
    let mut missing: Vec<_> = durable.difference(&final_ids).cloned().collect();
    missing.sort_unstable();
    assert!(
        missing.is_empty(),
        "{context}：已提交/已发布的记录在收敛后丢失：{missing:?}"
    );
}

/// 基于当前时间的毫秒抖动，避免引入随机数依赖。
fn random_millis(max: u64) -> u64 {
    if max == 0 {
        return 0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % (max + 1)
}

fn archives(dir: &Path) -> Vec<PathBuf> {
    find_files(dir, &|path| {
        path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            // `unique_archive_path` 会先占位一个 0 字节文件；只有 rename 后才非空。
            name.starts_with("sessions-")
                && name.ends_with(".db")
                && !name.contains(".tmp.")
                && path.metadata().is_ok_and(|metadata| metadata.len() > 0)
        })
    })
}

fn archive_record_count(dir: &Path) -> usize {
    archives(dir)
        .iter()
        .map(|path| read_records(path).len())
        .sum()
}

fn temp_files(dir: &Path) -> Vec<PathBuf> {
    find_files(dir, &|path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().contains(".tmp."))
    })
}

/// 等待会话中出现指定 `call_id` 的 `tool_call`。
fn wait_for_call(db: &Path, call_id: &str) {
    wait_until(
        || {
            read_records(db)
                .iter()
                .any(|record| matches!(record, Record::ToolCall(call) if call.call_id == call_id))
        },
        &format!("等待 tool_call {call_id} 落盘"),
    );
}

/// 1. append 事务中：多轮真实 kill -9，`ai` 与其 `tool_call` 必须同批提交。
///
/// 服务端在首个响应前写标记文件；看到标记后随机延时再杀进程，覆盖「响应已发出、
/// 批次正在提交」的窗口。若批次提交完成，则 `tool_call` 前必有同批 `ai`；若未提交，
/// 两者都不存在。每轮随后重跑一次，验证崩溃恢复可收敛。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_during_append_never_leaves_half_batch() {
    let mut observed_batch = false;
    for round in 0..6 {
        let marker_dir = tempfile::TempDir::new().unwrap();
        let marker = marker_dir.path().join(format!("request-{round}"));
        let mut side_effect = Command::new("sh");
        side_effect.args([
            "-c",
            &format!("printf 1 > '{}'; sleep 0.002", marker.display()),
        ]);
        let (addr, _state) = start_server_with_side_effect(
            vec![
                tool_call_response("call_crash", "true"),
                text_response("恢复答"),
                text_response("恢复答"),
            ],
            side_effect,
        )
        .await;
        let env = TestEnv::new(addr.port());
        env.write_config(PLAIN_CONFIG);
        let db = env.session_db();

        let mut child = env
            .command()
            .arg("崩溃问")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for_path(&marker, Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(random_millis(8)));
        kill_and_wait(&mut child);

        // INV1：先记下崩溃瞬间已提交的批次与仍在 inbox 的发布，重跑后做集合包含断言。
        let durable = durable_ids(&db);

        if db.exists() {
            assert!(integrity_ok(&db), "第 {round} 轮：kill 后库损坏");
            let records = read_records(&db);
            if records
                .iter()
                .any(|record| matches!(record, Record::Ai(_) | Record::ToolCall(_)))
            {
                observed_batch = true;
            }
            for (index, record) in records.iter().enumerate() {
                if let Record::ToolCall(call) = record
                    && call.call_id == "call_crash"
                {
                    assert!(
                        index > 0 && matches!(records[index - 1], Record::Ai(_)),
                        "第 {round} 轮出现半批（tool_call 无同批 ai）：{records:?}"
                    );
                }
            }
            assert_consistent(&db);
        }

        // 重跑：若批次已提交但结果未写，崩溃恢复补一条中断结果；模型随后作答。
        let output = env.run(&["恢复问"]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "第 {round} 轮重跑失败：{}",
            stderr_of(&output)
        );
        assert!(integrity_ok(&db), "第 {round} 轮重跑后库损坏");
        assert_durable_subset(&db, &durable, &format!("第 {round} 轮"));
        assert_converged(&db);
    }
    assert!(
        observed_batch,
        "至少应有一轮命中响应之后（append 窗口）的 kill"
    );
}

/// 2（矩阵第 4 点）：`tool_result` 已落、轮末 `ai` 未落时崩溃。
///
/// 手工 seed 到该状态后重跑：不得补第二条成功结果；模型请求必须携带已有结果续跑。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_tool_result_is_not_duplicated_on_resume() {
    let (addr, state) = start_server(vec![text_response("续跑答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    seed(
        &db,
        &[
            Record::user("原始请求"),
            Record::ai("m", "h", "", None),
            Record::tool_call(ToolOrigin::Ai, "call_done", "echo hi", None),
            Record::tool_result("call_done", "hi\n", "", 0, false, false, None),
        ],
    );

    let output = env.run(&["继续"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert!(stdout_of(&output).contains("续跑答"));

    let records = read_records(&db);
    let results = results_for(&records, "call_done");
    assert_eq!(results.len(), 1, "不得追加第二条 tool_result：{records:?}");
    assert_eq!(results[0].code, 0, "既有成功结果应保留");
    assert!(
        !results
            .iter()
            .any(|result| result.stderr.contains("interrupted")),
        "已有结果时不得补中断结果：{records:?}"
    );

    let requests = state.requests.lock().unwrap();
    let body = requests.first().expect("应发起模型请求");
    let messages = body["messages"].as_array().expect("消息应为数组");
    assert!(
        messages
            .iter()
            .any(|message| { message["role"] == "tool" && message["tool_call_id"] == "call_done" }),
        "模型请求应包含已有工具结果：{body}"
    );
    assert_valid_openai_sequence(messages);
    drop(requests);

    assert_consistent(&db);
    assert_converged(&db);
}

/// 3（矩阵第 6 点）：inbox 消费 append 后、删除前崩溃。
///
/// 在 `init` 慢速窗口里抓取已发布的 `pending/<id>.json` 副本；首轮消费落库后把副本
/// 拷回，再跑一轮：同一 id 只允许一条事件，文件最终被删除。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replayed_consumed_inbox_file_is_idempotent() {
    let (addr, _state) = start_server(vec![text_response("首答"), text_response("再答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(SLOW_INIT_CONFIG);
    let db = env.session_db();
    let pending_dir = env.inbox_root().join("pending");

    let child = env
        .command()
        .arg("初次")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // init 钩子睡眠期间 own 已发布但尚未消费，抓取原始文件。
    wait_until(
        || std::fs::read_dir(&pending_dir).is_ok_and(|mut entries| entries.next().is_some()),
        "等待 own 发布到 inbox/pending",
    );
    let published = std::fs::read_dir(&pending_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .expect("pending 中应有 own 文件");
    let own_id = published
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let backup = env.root().join("consumed-own.json");
    std::fs::copy(&published, &backup).unwrap();

    let first = child.wait_with_output().unwrap();
    assert_eq!(first.status.code(), Some(0), "{}", stderr_of(&first));
    assert!(
        yaa_store::count_inbox(&session(&db), yaa_store::InboxPhase::Pending).unwrap() == 0,
        "首轮应已删除已消费文件"
    );

    // 模拟「append 成功、删除前崩溃」：已消费文件重现。
    std::fs::copy(&backup, pending_dir.join(format!("{own_id}.json"))).unwrap();

    let second = env.run(&["再次"]);
    assert_eq!(second.status.code(), Some(0), "{}", stderr_of(&second));

    let records = read_records(&db);
    let same_id = records
        .iter()
        .filter(|record| record.id() == own_id)
        .count();
    assert_eq!(same_id, 1, "重现文件必须按 id 去重：{records:?}");
    assert_eq!(
        user_contents(&records)
            .iter()
            .filter(|content| content.as_str() == "初次")
            .count(),
        1,
        "原始 user 只应有一条：{records:?}"
    );
    assert!(
        yaa_store::count_inbox(&session(&db), yaa_store::InboxPhase::Pending).unwrap() == 0,
        "重现文件应被清理"
    );
    assert_converged(&db);
}

/// 4（矩阵第 8 点）：归档 `VACUUM INTO` 中 kill。
///
/// 大库 + 观察临时快照出现后立刻 SIGKILL：主库仍完整可读；把残留 tmp 回拨为陈旧后
/// 重跑 `--clear`，成功且不丢记录，陈旧 tmp 被清理。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_during_archive_vacuum_keeps_primary_readable() {
    let env = TestEnv::new(0);
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    seed_large(&db, 30_000);
    let before = read_records(&db).len();
    assert_eq!(before, 30_000);

    let history = env.data_home.join("history");
    let mut child = env
        .command()
        .arg("--clear")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut saw_tmp = false;
    loop {
        if !temp_files(&history).is_empty() {
            saw_tmp = true;
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待 VACUUM 临时文件超时"
        );
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    if child.try_wait().unwrap().is_none() {
        std::thread::sleep(Duration::from_millis(random_millis(2)));
        kill_and_wait(&mut child);
    }
    assert!(saw_tmp, "60MB 级 VACUUM 应能观察到 sessions-*.tmp.*");

    assert!(integrity_ok(&db), "kill 后主库必须仍可读且 integrity ok");
    assert_eq!(
        read_records(&db).len(),
        before,
        "VACUUM 中 kill 不得清空主库"
    );
    let leftovers = temp_files(&history);
    assert!(!leftovers.is_empty(), "应留下临时快照残留");
    for tmp in &leftovers {
        backdate(tmp, Duration::from_secs(2 * 3600));
    }

    let retry = env.run(&["--clear"]);
    assert_eq!(retry.status.code(), Some(0), "{}", stderr_of(&retry));
    assert!(read_records(&db).is_empty(), "重试成功后主库应清空");
    assert_eq!(archive_record_count(&history), before, "重试归档不得丢记录");
    assert!(
        temp_files(&history).is_empty(),
        "陈旧临时快照应被清理：{:?}",
        temp_files(&history)
    );
    assert_converged(&db);
}

/// 5（矩阵第 9 点）：归档 rename 后、清空主库前崩溃。
///
/// 手工构造「快照已存在 + 主库仍有全部数据」；重跑 `--clear` 成功，旧快照保留，
/// 新快照同样完整，记录不丢。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_after_archive_rename_before_clear_can_retry() {
    let env = TestEnv::new(0);
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    seed_records(&db, 200);
    let before = read_records(&db).len();

    let sid = session(&db).sid;
    let archive_dir = env.data_home.join("history").join(&sid);
    std::fs::create_dir_all(&archive_dir).unwrap();
    let snapshot = archive_dir.join("sessions-1.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("VACUUM INTO ?1", [snapshot.to_str().unwrap()])
        .unwrap();
    // 与真实归档快照一致：`archive_snapshot` 会在 rename 前把快照标记为 `archived=1`。
    conn.execute("UPDATE session SET archived = 1 WHERE id = 1", [])
        .unwrap();
    drop(conn);
    assert_eq!(read_records(&snapshot).len(), before);

    let retry = env.run(&["--clear"]);
    assert_eq!(retry.status.code(), Some(0), "{}", stderr_of(&retry));
    assert!(read_records(&db).is_empty(), "重试成功后主库应清空");
    assert!(snapshot.exists(), "既有完整快照不得被删除");
    assert_eq!(read_records(&snapshot).len(), before);
    assert_eq!(
        archive_record_count(&archive_dir),
        before * 2,
        "旧快照 + 新快照都应完整保留"
    );
    assert_converged(&db);
}

/// 6（矩阵第 10 点）：归档清空事务中 kill 烟雾。
///
/// 观察到最终快照 rename 后随机杀进程；主库记录数只能是「全部保留」或「全部清空」，
/// 不允许中间态，且 integrity ok。全部保留时重试 `--clear` 成功。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_during_archive_clear_transaction_is_atomic() {
    let mut observed_uncommitted = false;
    for round in 0..3 {
        let env = TestEnv::new(0);
        env.write_config(PLAIN_CONFIG);
        let db = env.session_db();
        // 大事务让「rename 后、DELETE 提交前」的窗口足够宽，避免断言恒真。
        seed_records(&db, 50_000);
        let before = read_records(&db).len();
        let history = env.data_home.join("history");

        let mut child = env
            .command()
            .arg("--clear")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if !archives(&history).is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "第 {round} 轮等待归档 rename 超时"
            );
            if child.try_wait().unwrap().is_some() {
                break;
            }
            // 紧密轮询：rename 一旦可见立即 kill，尽量落在清空事务提交前。
            std::thread::sleep(Duration::from_micros(100));
        }
        if child.try_wait().unwrap().is_none() {
            kill_and_wait(&mut child);
        }

        assert!(integrity_ok(&db), "第 {round} 轮主库 integrity 失败");
        let remaining = read_records(&db).len();
        assert!(
            remaining == 0 || remaining == before,
            "第 {round} 轮清空事务出现中间态：{remaining}/{before}"
        );
        if remaining == before {
            observed_uncommitted = true;
        }
        assert_consistent(&db);
        assert_eq!(
            archive_record_count(&history),
            before,
            "第 {round} 轮快照应完整保留全部记录"
        );

        if remaining > 0 {
            let retry = env.run(&["--clear"]);
            assert_eq!(
                retry.status.code(),
                Some(0),
                "第 {round} 轮重试失败：{}",
                stderr_of(&retry)
            );
            assert!(read_records(&db).is_empty());
            assert_eq!(
                archive_record_count(&history),
                before * 2,
                "既有快照 + 重试快照都应完整保留"
            );
        }
        assert_converged(&db);
    }
    assert!(
        observed_uncommitted,
        "至少一轮应观察到清空事务未提交（remaining == before），以证明原子性断言的观察面"
    );
}

/// 7a（矩阵第 11 点）：0 字节库是 init 崩溃的回滚态，下一次运行应自愈。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_byte_database_self_heals() {
    let (addr, _state) = start_server(vec![text_response("自愈答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(&db, b"").unwrap();

    let output = env.run(&["你好"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert!(stdout_of(&output).contains("自愈答"));
    assert!(integrity_ok(&db), "自愈后库应完整");

    let records = read_records(&db);
    assert!(
        records
            .iter()
            .any(|record| matches!(record, Record::System(system) if system.source == "init")),
        "应完成 init 簿记：{records:?}"
    );
    assert_eq!(user_contents(&records), vec!["你好".to_string()]);
    assert_converged(&db);
}

/// 7b（矩阵第 11 点）：非空但只有半套 schema 的文件。
///
/// 不是 yaa 库时给出明确错误（而不是静默重建或误报归档）；用户清理半成品后重试成功。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_database_reports_clear_error_then_retry() {
    let (addr, _state) = start_server(vec![text_response("重建答")]).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (id INTEGER PRIMARY KEY);
         PRAGMA user_version = 1;",
    )
    .unwrap();
    drop(conn);

    let broken = env.run(&["坏库问"]);
    assert_ne!(broken.status.code(), Some(0), "半 schema 库应明确失败");
    assert!(
        stderr_of(&broken).contains("不是 yaa 会话数据库"),
        "stderr: {}",
        stderr_of(&broken)
    );
    assert!(db.exists(), "失败路径不得删除用户文件");

    // 用户清理无法识别的半成品（模拟人工修复）后，下一次运行正常建库。
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", db.display()));
        let _ = std::fs::remove_file(path);
    }
    let retry = env.run(&["重建问"]);
    assert_eq!(retry.status.code(), Some(0), "{}", stderr_of(&retry));
    assert!(stdout_of(&retry).contains("重建答"));
    assert_converged(&db);
}

/// 7c（矩阵第 11 点）：0 字节库上的并发首跑不得把建库误报为归档。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_runs_on_zero_byte_database() {
    let (addr, _state) = start_server(vec![
        text_response("并发甲"),
        text_response("并发乙"),
        text_response("备用"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(&db, b"").unwrap();

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
            "{name} 失败：{}",
            stderr_of(output)
        );
        assert!(
            !stderr_of(output).contains("已归档"),
            "{name} 不得把建库误报为归档：{}",
            stderr_of(output)
        );
    }
    assert!(integrity_ok(&db));
    let users = user_contents(&read_records(&db));
    assert!(users.contains(&"甲问".to_string()), "{users:?}");
    assert!(users.contains(&"乙问".to_string()), "{users:?}");
    assert_converged(&db);
}

/// 8（矩阵第 13 点）：模型请求中 kill。
///
/// 假服务端收到请求后阻塞并写标记；杀掉 run 进程后重跑：既有 user 不重复、答案不丢，
/// 第一次请求没有产生任何 AI/工具记录。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_during_model_request_loses_no_message() {
    let marker_dir = tempfile::TempDir::new().unwrap();
    let marker = marker_dir.path().join("model-request");
    let mut block = Command::new("sh");
    block.args(["-c", &format!("printf 1 > '{}'; sleep 1", marker.display())]);
    let (addr, _state) =
        start_server_with_side_effect(vec![text_response("答一"), text_response("答二")], block)
            .await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();

    let mut child = env
        .command()
        .arg("第一次")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_path(&marker, Duration::from_secs(5));
    kill_and_wait(&mut child);

    let durable = durable_ids(&db);

    if db.exists() {
        let records = read_records(&db);
        assert!(
            !records.iter().any(|record| matches!(record, Record::Ai(_))),
            "模型请求中 kill 不应落 AI：{records:?}"
        );
        assert_consistent(&db);
    }

    let retry = env.run(&["第二次"]);
    assert_eq!(retry.status.code(), Some(0), "{}", stderr_of(&retry));
    assert!(stdout_of(&retry).contains('答'), "{}", stdout_of(&retry));

    let records = read_records(&db);
    let users = user_contents(&records);
    assert_eq!(
        users
            .iter()
            .filter(|content| content.as_str() == "第一次")
            .count(),
        1,
        "被中断的 user 不得重复：{records:?}"
    );
    assert_eq!(
        users
            .iter()
            .filter(|content| content.as_str() == "第二次")
            .count(),
        1,
        "重跑 user 应恰好一条：{records:?}"
    );
    assert!(
        !ai_contents(&records).is_empty(),
        "重跑应有答案：{records:?}"
    );
    assert_durable_subset(&db, &durable, "模型请求中 kill");
    assert_converged(&db);
}

/// 10（INV7，矩阵第 7 点补充）：发布者 `--notify --to <目标>` 在发布成功后 SIGKILL，
/// 目标会话下一次 `run` 仍消费该消息；断言不丢、恰一条。
///
/// 通知在命令执行后才发布，进程随后立即退出，因此这里忙轮询 `pending/` 出现后立刻
/// kill，尽量命中「已 rename、尚未退出」的窗口；至少一轮必须真正 kill 到进程。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publisher_killed_after_publish_is_consumed_exactly_once() {
    const ROUNDS: usize = 8;
    let responses = (0..ROUNDS)
        .map(|index| text_response(&format!("消费答{index}")))
        .collect();
    let (addr, _state) = start_server(responses).await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let target = env.work.join("target.db");
    seed(&target, &[Record::user("目标初始")]);
    let mut killed_after_publish = false;

    for round in 0..ROUNDS {
        let before = pending_ids(&target);
        let mut child = env
            .command()
            .arg(format!("--notify=发布者崩溃{round}"))
            .arg("--to")
            .arg(&target)
            .arg("--")
            .arg("printf")
            .arg("X")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        // 忙轮询「发布文件已出现」，尽量在进程退出前抓住发布完成后的窗口。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if pending_ids(&target).len() > before.len() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "第 {round} 轮等待 notify 发布超时"
            );
            std::thread::yield_now();
        }
        if child.try_wait().unwrap().is_none() {
            // 发布已成功（pending 文件已出现），此刻 kill 发布者。
            kill_and_wait(&mut child);
            killed_after_publish = true;
        } else {
            let _ = child.wait();
        }

        let published = pending_ids(&target);
        let new_ids: Vec<_> = published.difference(&before).cloned().collect();
        assert_eq!(
            new_ids.len(),
            1,
            "第 {round} 轮应恰发布一条消息：{published:?}"
        );
        let message_id = new_ids[0].clone();

        let output = env.run(&[
            "--session",
            target.to_str().unwrap(),
            &format!("消费{round}"),
        ]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "第 {round} 轮消费失败：{}",
            stderr_of(&output)
        );

        let records = read_records(&target);
        assert_eq!(
            records
                .iter()
                .filter(|record| record.id() == message_id)
                .count(),
            1,
            "第 {round} 轮发布的消息应恰落库一条：{records:?}"
        );
        assert!(
            pending_ids(&target).is_empty(),
            "第 {round} 轮消费后 pending 应清空"
        );
        assert_converged(&target);
    }
    assert!(
        killed_after_publish,
        "至少一轮应在发布成功后 kill 到发布者进程"
    );
}

/// 9（矩阵第 12 点）：leader kill → follower 接管 → 再 kill → 第二个接管者收敛。
///
/// 两个长工具提供两次接管窗口；每次接管都补一条中断结果。断言 own 幂等、
/// 每个 call 恰好一条结果、轮末答案恰好一条。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_takeover_after_two_kills_converges() {
    let (addr, _state) = start_server(vec![
        tool_call_response("call_leader", "sleep 5"),
        tool_call_response("call_takeover", "sleep 5"),
        text_response("最终答"),
    ])
    .await;
    let env = TestEnv::new(addr.port());
    env.write_config(PLAIN_CONFIG);
    let db = env.session_db();

    let mut leader = env
        .command()
        .arg("甲问")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_call(&db, "call_leader");

    // 第一个 follower 进入跟随，随后 leader 被杀，follower 接管并进入第二个长工具。
    let mut follower = env
        .command()
        .arg("乙问")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(
        || {
            yaa_store::count_inbox(&session(&db), yaa_store::InboxPhase::Pending)
                .is_ok_and(|count| count >= 1)
        },
        "等待第一个 follower 发布 own",
    );
    std::thread::sleep(Duration::from_millis(200));
    kill_and_wait(&mut leader);
    // INV1：第一次崩溃瞬间已提交/已发布的记录，后续接力与收敛都不得丢失。
    let mut durable = durable_ids(&db);

    wait_for_call(&db, "call_takeover");

    // 第二个客户端在第一个接管者执行长工具期间进入跟随；再杀接管者。
    let second = env
        .command()
        .arg("丙问")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until(
        || {
            yaa_store::count_inbox(&session(&db), yaa_store::InboxPhase::Pending)
                .is_ok_and(|count| count >= 1)
        },
        "等待第二个客户端发布 own",
    );
    std::thread::sleep(Duration::from_millis(200));
    kill_and_wait(&mut follower);
    durable.extend(durable_ids(&db));
    let second_out = second.wait_with_output().unwrap();

    assert_eq!(
        second_out.status.code(),
        Some(0),
        "第二个接管者失败：{}",
        stderr_of(&second_out)
    );
    assert!(
        stdout_of(&second_out).contains("最终答"),
        "stdout: {}",
        stdout_of(&second_out)
    );

    let records = read_records(&db);
    for (call_id, content) in [("call_leader", "甲问"), ("call_takeover", "乙问")] {
        let results = results_for(&records, call_id);
        assert_eq!(results.len(), 1, "{call_id} 应恰好一条结果：{records:?}");
        assert!(
            results[0].stderr.contains("interrupted"),
            "{call_id} 应为中断结果：{results:?}"
        );
        assert_eq!(results[0].code, -1, "{call_id} 的中断结果应为失败码");
        assert_eq!(
            user_contents(&records)
                .iter()
                .filter(|value| value.as_str() == content)
                .count(),
            1,
            "own {content} 应幂等"
        );
    }
    let answers: Vec<_> = ai_contents(&records)
        .into_iter()
        .filter(|content| content == "最终答")
        .collect();
    assert_eq!(answers.len(), 1, "轮末答案应恰好一条：{records:?}");
    assert_durable_subset(&db, &durable, "两次接管后");
    assert_converged(&db);
}

/// 写入指定数量、每条约 2KB 的记录，制造足够长的 VACUUM 窗口。
fn seed_large(db: &Path, count: usize) {
    let records: Vec<Record> = (0..count)
        .map(|index| Record::user(format!("{index}-{}", "x".repeat(2048))))
        .collect();
    seed(db, &records);
}

/// 写入 `count` 条小记录（归档事务测试用）。
fn seed_records(db: &Path, count: usize) {
    let records: Vec<Record> = (0..count)
        .map(|index| Record::user(format!("记录 {index}")))
        .collect();
    seed(db, &records);
}

/// 校验 OpenAI 消息序列合法（与 e2e 同义，供本文件复用）。
fn assert_valid_openai_sequence(messages: &[serde_json::Value]) {
    assert!(!messages.is_empty(), "消息序列不应为空");
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        if message["role"] == "assistant"
            && let Some(calls) = message.get("tool_calls").and_then(|value| value.as_array())
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
