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
//! `5` 达到 `max_steps`，`6` 有待批命令（阶段 2）。`run` 只在非预期 I/O 失败时返回 `Err`。

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use yaa_msg::{ApprovalDecision, Record, ToolOrigin};
use yaa_store::InboxPhase;

use crate::approval::{self, Action};
use crate::compaction::{self, cache::SummaryCache};
use crate::config::Config;
use crate::display::{self, DisplayItem, DisplayProjector, Sink};
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
    /// 存在待批命令（阶段 2）。
    Pending,
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
            ExitCode::Pending => 6,
            ExitCode::Error => 1,
        }
    }
}

/// 接管理应对既有未决待批的策略。
///
/// 区分「有新提示词」与「只有 notify」：前者按 D45 作废旧待批；后者不替用户作废，
/// 只提示 `--approve` 并继续跟随（阶段 4 P0-5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingPolicy {
    /// 作废既有未决待批（补 `approval_resolved: deny` 与失败结果）。
    Abandon,
    /// 保留既有未决待批，等待用户 `--approve`。
    Keep,
}

/// 普通 run 作废旧待批时补写的失败 `tool_result` 文案。
const ABANDONED_RESULT: &str = "上一轮待批命令已随新提示词作废，未执行。";

/// 崩溃恢复时为无结果的 `tool_call` 补写的中断文案。
const INTERRUPTED_RESULT: &str = "runner interrupted";

/// 模型抽象。
///
/// 阶段 1 只需要一次「请求→回复」，不引入流式与重试抽象（重试在 [`llm::chat`] 内），
/// 因此保持最小形状并兼容 `dyn`。
pub trait Model: Send + Sync + compaction::Summarizer {
    /// 发送一次对话请求。
    fn chat<'a>(
        &'a self,
        messages: &'a [ChatMessage],
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ChatResponse>> + Send + 'a>>;

    /// 记录使用的模型名；默认空串（测试替身无需实现）。
    fn name(&self) -> &str {
        ""
    }

    /// 模型上下文窗口（token）；默认 `None`（测试替身无需实现），此时渲染期压缩静默禁用。
    fn context_window(&self) -> Option<u32> {
        None
    }
}

/// 真实的 OpenAI 兼容模型。
pub struct OpenAiModel {
    /// 供应配置。
    pub cfg: LlmConfig,
    /// 复用的 HTTP 客户端；超时由 `[agent] request_timeout_secs` 经
    /// [`llm::build_client`] 构造。跨步复用以保留连接池与 keep-alive。
    pub client: reqwest::Client,
}

impl Model for OpenAiModel {
    fn chat<'a>(
        &'a self,
        messages: &'a [ChatMessage],
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ChatResponse>> + Send + 'a>> {
        Box::pin(async move {
            let tools = tools::shell::tools();
            llm::chat(&self.client, &self.cfg, messages, &tools).await
        })
    }

    fn name(&self) -> &str {
        &self.cfg.model
    }

    fn context_window(&self) -> Option<u32> {
        self.cfg.context_window
    }
}

impl compaction::Summarizer for OpenAiModel {
    fn summarize<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        max_tokens: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + 'a>> {
        Box::pin(async move { llm::summarize(&self.client, &self.cfg, messages, max_tokens).await })
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
    /// `--auto`：自动放行命中 `ask` 的工具调用（显式 `deny` 仍强制）。
    pub auto: bool,
    /// 展示条目接收端。
    pub sink: &'a dyn Sink,
}

impl Runner<'_> {
    /// 运行一个回合：把新提示词发布到 inbox 后交给 [`Runner::run_queued`] 消费。
    ///
    /// CLI 走 [`Runner::run_queued`]（先发布 own 再抢锁）；此包装保留给库调用与测试，
    /// 语义等价：自己发布再自己消费，最终历史与阶段 1 一致。
    ///
    /// `make_model` 在 `llm` 钩子解析成功后恰好被调用一次，模型由本次循环独占，
    /// 从而保证 `llm` 钩子每次 `run` 只执行一次。
    pub async fn run<M, F>(&self, prompt_text: &str, make_model: F) -> anyhow::Result<ExitCode>
    where
        F: FnOnce(LlmConfig) -> M,
        M: Model,
    {
        let own = Record::user(prompt_text);
        // 与 CLI 的 own 发布保持同一语义：捕获当前 generation 后带守卫发布，
        // 避免并发归档把消息写进新 generation（0→1 建库仍允许）。
        let generation = yaa_store::generation(&self.session)?;
        yaa_store::publish_with_generation(
            &self.session,
            InboxPhase::Pending,
            &own,
            Some(generation),
        )?;
        self.run_queued(own, PendingPolicy::Abandon, make_model)
            .await
    }

    /// 运行一个已排队的回合：`own` 是调用方**先行发布**到 inbox 的用户记录。
    ///
    /// 消费顺序（阶段 4 刀 3）：
    /// 1. 作废旧待批、崩溃恢复（与阶段 1 的 `run` 一致）；
    /// 2. spool 补投：只消费 spool 自身，不合并、不提前消费 inbox；
    /// 3. 无 `system(source=init)` 簿记则执行 `init` 钩子（钩子内 `--notify` 发到
    ///    `inbox/init/`），随后消费 `init/`、写簿记；init 失败仍消费 `init/` 并落 `own`；
    /// 4. 消费文件名（ULID）小于 `own.id()` 的 `pending/` 注入；
    /// 5. 消费 `own`；
    /// 6. `llm` 钩子后进入 [`Runner::run_loop`]（每轮开头安全点继续消费 inbox）。
    ///
    /// `make_model` 在 `llm` 钩子解析成功后恰好被调用一次。
    ///
    /// `policy` 决定是否作废既有未决待批：普通 run / 含新 `User` 的接管传
    /// [`PendingPolicy::Abandon`]，纯 follower 的 notify 接管传 [`PendingPolicy::Keep`]。
    pub async fn run_queued<M, F>(
        &self,
        own: Record,
        policy: PendingPolicy,
        make_model: F,
    ) -> anyhow::Result<ExitCode>
    where
        F: FnOnce(LlmConfig) -> M,
        M: Model,
    {
        self.run_queued_inner(own, policy, true, make_model).await
    }

    /// [`Runner::run_queued`] 的实现；`warn_cwd` 为假时跳过工作目录回退告警（接管已发过）。
    async fn run_queued_inner<M, F>(
        &self,
        own: Record,
        policy: PendingPolicy,
        warn_cwd: bool,
        make_model: F,
    ) -> anyhow::Result<ExitCode>
    where
        F: FnOnce(LlmConfig) -> M,
        M: Model,
    {
        if warn_cwd {
            self.warn_cwd();
        }

        // 1. 写入前的水位：之后只展示水位之后的新记录。是否初始化改由「簿记」判定：
        //    只要历史里没有 `system(source=init)` 记录（含 `--notify` 先行发布与消费的
        //    场景），就执行一次 `init`；因此上轮 init 失败的会话下次运行会自动重试。
        let existing = yaa_store::load(&self.session)?;
        // 归档库不可写入：必须在 init/作废/恢复/写 user 及执行任何命令之前拒绝。
        if self.reject_archived()? {
            return Ok(ExitCode::Error);
        }
        let mut watermark = existing.len();

        // 2. 新提示词视为放弃旧待批（D45），再补崩溃孤儿；顺序同阶段 1，使未决 pending
        //    保留「已作废」文案而非被误判为运行中断。纯 follower 的 notify 接管（Keep）
        //    不替用户作废旧待批。
        if policy == PendingPolicy::Abandon {
            self.abandon_old_pending(&mut watermark).await?;
        }
        self.recover_orphan_tool_calls(&mut watermark).await?;

        // 3. spool 补投：发布失败降级的通知在 init 之前只消费 spool 自身，不触碰
        //    inbox，保持 `init/` 与 `pending/` 的既有消费顺序。失败时保留文件，下一轮
        //    继续重试。
        self.replay_spool(&mut watermark).await?;

        let initialized = existing
            .iter()
            .any(|record| matches!(record, Record::System(system) if system.source == "init"));

        // 4. 无初始化簿记才执行 `init`；成功后消费 init/ 注入并写簿记，失败仍消费 init/、
        //    落 own 并按配置错误退出（不消费其他 pending）。
        if !initialized {
            match self.run_init_hook().await {
                Ok(()) => {
                    self.consume_messages(self.gather_phase(InboxPhase::Init)?, &mut watermark)
                        .await?;
                    yaa_store::append(&self.session, &[Record::system("init", "", None, None)])?;
                }
                Err(exit) => {
                    self.consume_messages(self.gather_phase(InboxPhase::Init)?, &mut watermark)
                        .await?;
                    self.consume_own(&own, &mut watermark).await?;
                    return Ok(exit);
                }
            }
        }

        // 5. 先消费 own 之前（ULID 更小）的 pending 注入，再消费 own。
        let earlier = self.gather_phase_before(InboxPhase::Pending, own.id())?;
        self.consume_messages(earlier, &mut watermark).await?;
        self.consume_own(&own, &mut watermark).await?;

        // 6. llm 钩子（每次运行一次，不写记录），并用解析出的配置构造模型。
        let llm_config = match self.resolve_llm_hook().await {
            Ok(config) => config,
            Err(exit) => return Ok(exit),
        };
        let model = make_model(llm_config);

        self.run_loop(&model, watermark, true).await
    }

    /// 消费一条 `own` 记录：幂等追加后删除对应 inbox 文件。
    async fn consume_own(&self, own: &Record, watermark: &mut usize) -> anyhow::Result<()> {
        self.append_idempotent_and_emit(std::slice::from_ref(own), watermark)
            .await?;
        for message in yaa_store::scan_inbox(&self.session, InboxPhase::Pending)? {
            if message.id() == own.id() {
                yaa_store::remove_inbox(&message)?;
                break;
            }
        }
        Ok(())
    }

    /// 扫描一个 phase 的全部消息（按文件名升序）。
    fn gather_phase(&self, phase: InboxPhase) -> anyhow::Result<Vec<yaa_store::InboxMessage>> {
        Ok(yaa_store::scan_inbox(&self.session, phase)?)
    }

    /// 扫描一个 phase 中 id 严格小于 `bound` 的消息（用于「own 之前」的注入）。
    fn gather_phase_before(
        &self,
        phase: InboxPhase,
        bound: &str,
    ) -> anyhow::Result<Vec<yaa_store::InboxMessage>> {
        let mut messages = yaa_store::scan_inbox(&self.session, phase)?;
        messages.retain(|message| message.id() < bound);
        Ok(messages)
    }

    /// 收集 `init/` 与 `pending/` 的全部消息，按 id（ULID）合并升序；供安全点消费。
    fn gather_all_inbox(&self) -> anyhow::Result<Vec<yaa_store::InboxMessage>> {
        let mut messages = yaa_store::scan_inbox(&self.session, InboxPhase::Init)?;
        messages.extend(yaa_store::scan_inbox(&self.session, InboxPhase::Pending)?);
        messages.sort_by(|left, right| left.id().cmp(right.id()));
        Ok(messages)
    }

    /// 消费一批 inbox 消息：整批幂等追加后逐个删除；返回其中是否含 `User`。
    ///
    /// 崩溃在追加与删除之间时消息文件会重现，由 `append_idempotent` 按 id 去重。
    async fn consume_messages(
        &self,
        messages: Vec<yaa_store::InboxMessage>,
        watermark: &mut usize,
    ) -> anyhow::Result<bool> {
        if messages.is_empty() {
            return Ok(false);
        }
        let saw_user = messages
            .iter()
            .any(|message| matches!(message.record, Record::User(_)));
        let records: Vec<Record> = messages
            .iter()
            .map(|message| message.record.clone())
            .collect();
        self.append_idempotent_and_emit(&records, watermark).await?;
        for message in &messages {
            yaa_store::remove_inbox(message)?;
        }
        Ok(saw_user)
    }

    /// 补投 spool 中的通知：只消费 spool 自身，绝不合并或提前消费 inbox。
    ///
    /// 位置在 recover 之后、init 之前：上一轮发布失败的通知先落库，随后 init 钩子
    /// 与 `init/` 消息仍按既有顺序消费。只由 leader/接管调用；follower 不补投，只在
    /// 检测到滞留时提示。追加成功但删除失败时文件会留在下一轮，由
    /// `append_idempotent` 按 id 去重；删除失败只提示，不阻断本轮运行（F12）。
    async fn replay_spool(&self, watermark: &mut usize) -> anyhow::Result<()> {
        let spool = yaa_store::scan_spool(&self.session)?;
        if spool.is_empty() {
            return Ok(());
        }
        let records: Vec<Record> = spool.iter().map(|message| message.record.clone()).collect();
        self.append_idempotent_and_emit(&records, watermark).await?;
        for message in &spool {
            if let Err(err) = yaa_store::remove_spool(message) {
                self.sink.emit(DisplayItem::Notice(format!(
                    "警告：删除 spool 文件失败（{err}），将在下一轮重试：{}",
                    message.path.display()
                )));
            }
        }
        Ok(())
    }

    /// 跟随模式：`own` 已发布但未抢到 `runner.lock`（另一 runner 正在跑）。
    ///
    /// 只读跟随：不写 events、不跑钩子、不执行工具。启动水位 `(baseline_gen, start_seq)`
    /// 取「发布 `own` 时同一 clear SH 临界区内的原子水位」，故不回放历史；仅把其后新到达
    /// 的记录经 [`DisplayProjector`] 增量展示。启动时用 `start_seq` 之前的历史回填命令表
    /// （不产出条目），使之后到达的 `approval_resolved` 仍能展示命令（B5）。
    ///
    /// generation 规则（B1）：
    /// - **建库不算归档**：`baseline_gen == 0 && 读到 1` 时更新基线、`next_seq = 0` 续跟；
    /// - 真归档（其余变化）→ 提示「会话已归档」并退出 0，且**不重新 append own**；
    /// - 拿到 `runner.lock` 后再复检一次 generation，消除「读水位后、抢锁前被 clear」的
    ///   TOCTOU。
    ///
    /// 抢到锁后：`own` 已落库且 inbox 空、历史空闲、无未决审批 → 退出 0（最终回复由 CLI
    /// 依据 `own` 打印）；纯跟随遇未决审批 → 提示 `--approve` 并留在跟随；其余情形
    /// **只接管一次**。接管策略按 inbox 是否含新 `User` 区分（B4）。
    pub async fn follow<M, F>(
        &self,
        own: Record,
        start_seq: i64,
        baseline_gen: i64,
        make_model: F,
    ) -> anyhow::Result<ExitCode>
    where
        F: FnOnce(LlmConfig) -> M,
        M: Model,
    {
        self.warn_cwd();
        let mut make_model = Some(make_model);
        let mut generation = baseline_gen;
        let mut next_seq = start_seq;
        // B5：只把发布水位之前的历史交给投影器回填命令表；`load()` 一次，不重放。
        let prefix = yaa_store::load(&self.session)?;
        let prefix_len = usize::try_from(start_seq)
            .unwrap_or(usize::MAX)
            .min(prefix.len());
        let mut projector = DisplayProjector::from_records(&prefix[..prefix_len]);
        let mut backoff = Duration::from_millis(20);
        let mut pending_notice_sent = false;
        let mut spool_notice_sent = false;

        loop {
            let (rows, new_seq, read_generation) =
                yaa_store::read_after_with_generation(&self.session, next_seq)?;
            if read_generation != generation {
                if generation == 0 && read_generation == 1 {
                    // 首次建库：不是归档；从新库起点重新读取。
                    generation = 1;
                    next_seq = 0;
                    continue;
                }
                self.sink
                    .emit(DisplayItem::Notice("会话已归档，停止跟随。".to_string()));
                return Ok(ExitCode::Success);
            }
            // follower 不补投 spool；只提示滞留数量，避免静默丢通知。
            // 在抢 runner.lock 之前检查：leader 持锁期间也要能看到滞留。
            let spool_count = yaa_store::count_spool(&self.session)?;
            if spool_count > 0 && !spool_notice_sent {
                self.sink.emit(DisplayItem::Notice(format!(
                    "有 {spool_count} 条通知暂存在 spool，将在下次非跟随运行时补投。"
                )));
                spool_notice_sent = true;
            }

            if rows.is_empty() {
                tokio::time::sleep(jitter(backoff)).await;
                backoff = (backoff * 2).min(Duration::from_millis(250));
            } else {
                self.display_rows(&mut projector, rows);
                next_seq = new_seq;
                backoff = Duration::from_millis(20);
            }

            // follower 不补投 spool；只提示滞留数量，避免静默丢通知。此检查必须在
            // 抢 `runner.lock` 之前：锁被 leader 持有时本次循环根本走不到后面。
            let Some(guard) = yaa_store::FileLock::try_exclusive(&self.session.runner_lock_path())?
            else {
                continue;
            };
            // 抢锁后复检 generation：读水位与抢锁之间可能被 `--clear` 归档。
            let current_generation = yaa_store::generation(&self.session)?;
            if current_generation != generation {
                if generation == 0 && current_generation == 1 {
                    generation = 1;
                    next_seq = 0;
                    drop(guard);
                    continue;
                }
                self.sink
                    .emit(DisplayItem::Notice("会话已归档，停止跟随。".to_string()));
                return Ok(ExitCode::Success);
            }
            // 补读旧 runner 释放前的最后一批，避免退出竞态丢展示。
            let (rows, new_seq, read_generation) =
                yaa_store::read_after_with_generation(&self.session, next_seq)?;
            if read_generation != generation {
                // 持 runner.lock 时 clear 不可能归档；仍按统一规则处理以防外部 bump。
                drop(guard);
                if generation == 0 && read_generation == 1 {
                    generation = 1;
                    next_seq = 0;
                    continue;
                }
                self.sink
                    .emit(DisplayItem::Notice("会话已归档，停止跟随。".to_string()));
                return Ok(ExitCode::Success);
            }
            if !rows.is_empty() {
                self.display_rows(&mut projector, rows);
                next_seq = new_seq;
            }

            let records = yaa_store::load(&self.session)?;
            let own_consumed = records.iter().any(|record| record.id() == own.id());
            let pending = unresolved_pending(&records);
            let inbox = self.gather_all_inbox()?;
            let inbox_empty = inbox.is_empty();
            let inbox_has_user = inbox
                .iter()
                .any(|message| matches!(message.record, Record::User(_)));

            if inbox_empty && own_consumed && pending.is_empty() && display::is_idle(&records) {
                // B6：own 之后的轮末 `ai` 为空时与 leader 一样明确告知，不静默退出。
                if display::final_ai_for_own(&records, own.id()).is_some_and(str::is_empty) {
                    self.sink.emit(DisplayItem::Notice(
                        "模型返回了空回复（既无文本也无工具调用）。".to_string(),
                    ));
                }
                // 历史空闲：不再有可跑的工作，退出（最终回复由 CLI 打印）。
                return Ok(ExitCode::Success);
            }
            if own_consumed && !pending.is_empty() && !inbox_has_user {
                // B4：纯跟随（无新 User）遇未决审批：不替用户作废/拒绝，提示后继续跟随。
                if !pending_notice_sent {
                    self.sink.emit(DisplayItem::Notice(
                        "有待批准命令，请运行 `yaa --approve`。".to_string(),
                    ));
                    pending_notice_sent = true;
                }
                drop(guard);
                continue;
            }

            // 其余情形接管，只尝试一次；有新 User 才按 D45 作废旧待批。
            let policy = if inbox_has_user {
                PendingPolicy::Abandon
            } else {
                PendingPolicy::Keep
            };
            let make_model = make_model.take().expect("接管只发生一次");
            return self.run_takeover(own, policy, make_model).await;
        }
    }

    /// 接管：前一个 runner 已释放锁，本次由我继续处理该会话。
    ///
    /// 复用 [`Runner::run_queued`] 的消费顺序（作废旧待批 → 崩溃恢复 → init → 消费
    /// pending 与 `own` → `run_loop`）；`own` 可能已被前一个 runner 消费，按 id 幂等去重。
    /// 「只尝试一次」由调用方 [`Runner::follow`] 保证：接管返回后不再重新进入跟随。
    /// 工作目录回退告警已由 [`Runner::follow`] 发出，接管不再重复（B7）。
    pub async fn run_takeover<M, F>(
        &self,
        own: Record,
        policy: PendingPolicy,
        make_model: F,
    ) -> anyhow::Result<ExitCode>
    where
        F: FnOnce(LlmConfig) -> M,
        M: Model,
    {
        self.run_queued_inner(own, policy, false, make_model).await
    }

    /// 把一批 `(seq, record)` 增量交给投影器并逐条上报。
    ///
    /// 每批 `push` 后立刻 `flush`：真实历史中 `ai` 与其 `tool_call` 在同一事务原子落盘，
    /// 因此一批读到的末尾 `ai` 之后不会再有同批 `tool_call`，可安全按轮末正文产出。
    fn display_rows(&self, projector: &mut DisplayProjector, rows: Vec<(i64, Record)>) {
        let records: Vec<Record> = rows.into_iter().map(|(_, record)| record).collect();
        for item in projector.push(&records) {
            self.sink.emit(item);
        }
        for item in projector.flush() {
            self.sink.emit(item);
        }
    }

    ///
    /// 与 [`Runner::run`] 的差异：不执行 `init`、不写 `user`；只执行一条 pending 的
    /// 命令（写 `tool_result` 与 `approval_resolved: approve`），然后复用 [`Runner::run_loop`]。
    pub async fn approve_pending<M, F>(&self, make_model: F) -> anyhow::Result<ExitCode>
    where
        F: FnOnce(LlmConfig) -> M,
        M: Model,
    {
        self.warn_cwd();

        // 1. 先取最旧未决 pending；没有则提示并成功返回。此路径**不解析 llm 钩子**，
        //    否则钩子损坏时用户连「没有待批准」都看不到。
        let records = yaa_store::load(&self.session)?;
        // 归档库不可写入：必须在恢复补写、执行 pending 命令之前拒绝。
        if self.reject_archived()? {
            return Ok(ExitCode::Error);
        }
        // 1b. `--approve` 也做崩溃恢复，但只补真正的孤儿 `tool_call`；未决 pending
        //     不在此作废（本次就要放行最旧一条）。
        let mut watermark = records.len();
        self.recover_orphan_tool_calls(&mut watermark).await?;
        let unresolved = unresolved_pending(&records);
        let Some((pending_index, pending)) = unresolved.into_iter().next() else {
            self.sink
                .emit(DisplayItem::Notice("没有待批准的命令。".to_string()));
            return Ok(ExitCode::Success);
        };

        // 2. 有 pending 才解析 `llm` 钩子（每次运行一次，不写记录）；失败以配置错误
        //    退出，且尚未执行任何命令。
        let llm_config = match self.resolve_llm_hook().await {
            Ok(config) => config,
            Err(exit) => return Ok(exit),
        };
        let model = make_model(llm_config);

        // 3. 水位已在恢复后取定：本轮只展示新追加的 `tool_result` 与
        //    `approval_resolved`，不重放历史（多个 pending 时不会牵连展示其余待批）。
        //    被放行的 pending 单独补显一次，让用户知道这次放行的是哪条。
        // 前缀 + 起点 = 只投影该条 pending；命令表仍基于全量前缀（对本条非必需，
        // 但与 `emit_new` 的全量记录约定保持一致）。
        self.emit_items(&records[..=pending_index], pending_index);

        let resolved = Record::approval_resolved(&pending.call_id, ApprovalDecision::Approve);
        // 4. 若该 `call_id` 已有 `tool_result`（上次已执行、但在写 resolved 前中断），
        //    只补 `approval_resolved: approve`，绝不重复执行、不新增结果。
        let already_executed = records.iter().any(
            |record| matches!(record, Record::ToolResult(result) if result.call_id == pending.call_id),
        );
        if already_executed {
            yaa_store::append(&self.session, &[resolved])?;
        } else {
            // 5. 执行该 pending 的命令，追加 `tool_result`，再追加 `approval_resolved: approve`。
            //    先执行后落 resolved 的顺序与 `design.md` 一致（结果先于决策可见）。
            let limits = self.config.shell.to_limits();
            let outputs_dir = session::outputs_dir(&self.session);
            let parsed = tools::shell::ParsedCall {
                command: pending.command.clone(),
                description: None,
                error: None,
            };
            let result = self
                .execute_call(
                    tools::shell::NAME,
                    &pending.call_id,
                    &parsed,
                    &limits,
                    &outputs_dir,
                )
                .await;
            yaa_store::append(&self.session, &[result])?;
            yaa_store::append(&self.session, &[resolved])?;
        }

        self.run_loop(&model, watermark, false).await
    }

    /// agent loop 主体，供 [`Runner::run_queued`] 与 [`Runner::approve_pending`] 共用。
    ///
    /// 从 `watermark` 起只展示增量记录；循环内每次重读历史再渲染，保证外部
    /// `yaa --notify` 在模型请求期间的写入不被跳过（D39）。
    ///
    /// `consume` 为真时（普通 run / 接管），每轮开头是安全点：上一轮工具批次已完成，
    /// 此时消费全部 inbox（`init/`+`pending/` 按 ULID 合并升序）后再渲染、调用模型；
    /// 消费到含 `User` 的消息时重置 `max_steps` 计数（system notify 不重置）。
    /// `--approve` 传假，保持「不消费 User 注入」的语义。
    async fn run_loop<M: Model>(
        &self,
        model: &M,
        mut watermark: usize,
        consume: bool,
    ) -> anyhow::Result<ExitCode> {
        let schema_hash = tools::shell::schema_hash();
        let limits = self.config.shell.to_limits();
        let outputs_dir = session::outputs_dir(&self.session);
        // 本次运行的摘要链尾：生成一次后后续步骤复用，避免重复调用摘要模型。
        let mut compaction_state: Option<compaction::cache::SummaryEntry> = None;

        let max_steps = self.config.agent.max_steps;
        let mut step = 0u32;
        loop {
            // 安全点：上一轮工具批次已完整落盘、无未决审批时才消费 inbox（I3）。
            if consume
                && self
                    .consume_messages(self.gather_all_inbox()?, &mut watermark)
                    .await?
            {
                // 本批含新的 User：为这一批重置步数预算。
                step = 0;
            }
            if step >= max_steps {
                break;
            }

            let records = yaa_store::load(&self.session)?;
            let plan = self
                .plan_messages(&records, model, &mut compaction_state)
                .await;

            // 先展示自上次以来新增的记录（注入、用户、工具批次），再推进水位。
            self.emit_new(&records, &mut watermark);
            // 压缩是渲染期变换：警告与摘要条目在记录增量之外单独展示（D56）。
            if let Some(warning) = &plan.warning {
                self.sink.emit(DisplayItem::Notice(warning.clone()));
            }
            if let Some(entry) = &plan.entry
                && let Some(applied) = &plan.applied
            {
                self.sink.emit(DisplayItem::Compaction {
                    covered: applied.covered.len(),
                    model: entry.model.clone(),
                    summary: entry.summary.clone(),
                });
            }
            let messages = plan.messages;

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
                // B3：最终模型调用期间到达的 notify 也必须在本次 run 内消费，不能因
                // 「无 tool_calls」直接返回而滞留到下一次运行。不重置 step（notify 不
                // 属于新 user 批次），仍由下一轮开头的 `max_steps` 检查兜底。
                if consume && !self.gather_all_inbox()?.is_empty() {
                    continue;
                }
                return Ok(ExitCode::Success);
            }

            // 同批中所有命中 ask 的调用都要落 pending，allow/deny/不可执行的调用也要
            // 各自落记录；全部处理完后才以退出码 6 结束，避免后续调用被伪装成中断。
            let mut pending_any = false;
            for (call, parsed) in tool_calls.iter().zip(&parsed_calls) {
                if self
                    .gate_tool_call(call, parsed, &limits, &outputs_dir, &mut watermark)
                    .await?
                {
                    pending_any = true;
                }
            }
            if pending_any {
                return Ok(ExitCode::Pending);
            }
            step += 1;
        }

        // 达到步数上限不在此发事件：`ExitCode::MaxSteps` 已是明确信号，具体警告与
        // 「是否仍打印部分回复」的显示策略完全由 CLI 决定。
        Ok(ExitCode::MaxSteps)
    }

    /// 计算本步要发给模型的消息：必要时修剪工具正文与压缩早期历史。
    ///
    /// `state` 是本次运行的摘要链尾：一旦生成摘要，后续步骤复用而不重复调用模型。
    /// 首次进入时从侧车缓存读取，实现同区间跨运行复用（D50）；生成的新条目在此落盘。
    async fn plan_messages<M: Model>(
        &self,
        records: &[Record],
        model: &M,
        state: &mut Option<compaction::cache::SummaryEntry>,
    ) -> compaction::Plan {
        let cfg = &self.config.compaction;
        let Some(budgets) = compaction::Budgets::from_config(cfg, model.context_window()) else {
            return compaction::Plan {
                messages: render::render(records),
                applied: None,
                entry: None,
                warning: None,
            };
        };
        let estimator = compaction::Estimator::new(cfg.chars_per_token);
        let cache = SummaryCache::new(&self.session.root);
        let previous = match state.clone() {
            Some(entry) => Some(entry),
            None => cache.load_latest().unwrap_or(None),
        };
        let plan = compaction::compact(
            records,
            model.name(),
            &estimator,
            &budgets,
            cfg,
            previous.as_ref(),
            model,
        )
        .await;
        if let Some(entry) = &plan.entry {
            if let Err(err) = cache.store(entry) {
                self.sink.emit(DisplayItem::Notice(format!(
                    "压缩摘要落盘失败，本次仍使用内存摘要：{err:#}"
                )));
            }
            *state = Some(entry.clone());
        }
        plan
    }

    /// 对一次 `origin=ai` 的工具调用求审批裁决并按落地表处理。
    ///
    /// 返回 `true` 表示该调用命中非 auto 的 `ask`、已落 `approval_pending`；调用方在
    /// 处理完整批后据此以退出码 6 结束。返回 `false` 表示已放行/拒绝/不可执行并继续。
    async fn gate_tool_call(
        &self,
        call: &crate::message::AssistantToolCall,
        parsed: &tools::shell::ParsedCall,
        limits: &yaa_exec::ShellLimits,
        outputs_dir: &Path,
        watermark: &mut usize,
    ) -> anyhow::Result<bool> {
        // R1：参数非法、工具名不符或命令为空者根本不可执行，绝不进入审批分支
        // （否则 `--approve` 会把非法 pending 当合法命令执行，语义与 `--auto` 相反）。
        // 落一条失败 `tool_result` 让模型下一跳可见，并继续循环、不退出。
        if let Some(reason) = inexecutable_reason(call, parsed) {
            let result = Record::tool_result(&call.id, "", &reason, -1, false, false, None);
            self.append_and_emit(std::slice::from_ref(&result), watermark)
                .await?;
            return Ok(false);
        }

        let verdict = self.evaluate_call(parsed);
        // `matched_rule` 为 `None`（无规则命中）时以空串落盘，展示层按无规则呈现。
        let matched = verdict.matched_rule.clone().unwrap_or_default();

        match verdict.action {
            Action::Allow => {
                let result = self
                    .execute_call(&call.name, &call.id, parsed, limits, outputs_dir)
                    .await;
                self.append_and_emit(std::slice::from_ref(&result), watermark)
                    .await?;
            }
            Action::Deny => {
                // 命中即拒：pending + resolved(deny) + 失败结果，不退出，模型下一跳可见。
                let batch = [
                    Record::approval_pending(&call.id, parsed.command.clone(), matched.clone()),
                    Record::approval_resolved(&call.id, ApprovalDecision::Deny),
                    Record::tool_result(
                        &call.id,
                        "",
                        denied_result(&matched),
                        -1,
                        false,
                        false,
                        None,
                    ),
                ];
                self.sink.emit(DisplayItem::Notice(format!(
                    "已拒绝命令：{}（命中规则 {}）。",
                    parsed.command, matched
                )));
                self.append_and_emit(&batch, watermark).await?;
            }
            // `--auto` 放行 ask：写 pending + resolved(approve) 后照常执行。
            Action::Ask if self.auto => {
                let batch = [
                    Record::approval_pending(&call.id, parsed.command.clone(), matched.clone()),
                    Record::approval_resolved(&call.id, ApprovalDecision::Approve),
                ];
                self.append_and_emit(&batch, watermark).await?;
                let result = self
                    .execute_call(&call.name, &call.id, parsed, limits, outputs_dir)
                    .await;
                self.append_and_emit(std::slice::from_ref(&result), watermark)
                    .await?;
            }
            // 非 auto 的 ask：只写 pending，不执行、不写 tool_result；由调用方在处理完
            // 同批其余调用后以退出码 6 结束，保证批内每条 tool_call 都有归属记录。
            Action::Ask => {
                let batch = [Record::approval_pending(
                    &call.id,
                    parsed.command.clone(),
                    matched.clone(),
                )];
                self.append_and_emit(&batch, watermark).await?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 计算一次**可执行**工具调用的审批裁决。
    ///
    /// 不可执行调用（工具名不符、参数非法、命令为空）已在 [`Runner::gate_tool_call`]
    /// 中直接落失败结果，不会到达此处；命令资源拆分失败（如引号不配平）由
    /// [`approval::evaluate`] 内部按 `ask` 处理。
    fn evaluate_call(&self, parsed: &tools::shell::ParsedCall) -> approval::Verdict {
        approval::evaluate(&self.config.approval.rules, &parsed.command)
    }

    /// 执行一次 shell 工具调用并返回记录（错误也编码为记录）。
    ///
    /// 子进程环境由 [`session::yaa_env`] 统一构造：`YAA_SESSION`/`YAA_NOTIFY`/`YAA_PARENT`
    /// 来自环境链，`YAA_NOTIFY_CALL_ID` 为本条 `call_id`；`YAA_NOTIFY_*` 限额由
    /// [`tools::shell::execute`] 追加（单一来源，不在此重复拼接）。
    async fn execute_call(
        &self,
        name: &str,
        call_id: &str,
        parsed: &tools::shell::ParsedCall,
        limits: &yaa_exec::ShellLimits,
        outputs_dir: &Path,
    ) -> Record {
        let extra_env: Vec<(String, String)> =
            session::yaa_env(&self.session.db, &process_env, Some(call_id))
                .into_iter()
                .collect();
        tools::shell::execute(
            name,
            call_id,
            parsed,
            limits,
            outputs_dir,
            self.data_root,
            &extra_env,
        )
        .await
    }

    /// 追加一批记录并展示增量，保证每条恰好展示一次。
    async fn append_and_emit(
        &self,
        records: &[Record],
        watermark: &mut usize,
    ) -> anyhow::Result<()> {
        yaa_store::append(&self.session, records)?;
        let after = yaa_store::load(&self.session)?;
        self.emit_new(&after, watermark);
        Ok(())
    }

    /// 幂等追加一批记录并展示增量：已存在的 id 被忽略，不重复、不报错。
    async fn append_idempotent_and_emit(
        &self,
        records: &[Record],
        watermark: &mut usize,
    ) -> anyhow::Result<()> {
        yaa_store::append_idempotent(&self.session, records)?;
        let after = yaa_store::load(&self.session)?;
        self.emit_new(&after, watermark);
        Ok(())
    }

    /// 启动普通 run 时把未决 pending 作废：补 `approval_resolved: deny` 与失败结果（D45）。
    ///
    /// 若该 `call_id` 已有 `tool_result`（执行后、写 `resolved` 前被强杀的中断态），
    /// 则只补 `approval_resolved: deny`，不再追加失败结果——否则日志出现两条结果，
    /// 且渲染取首条成功结果会与「已拒绝」语义矛盾（与 `--approve` 的判定对称）。
    async fn abandon_old_pending(&self, watermark: &mut usize) -> anyhow::Result<()> {
        let records = yaa_store::load(&self.session)?;
        for (_, pending) in unresolved_pending(&records) {
            let already_executed = records.iter().any(
                |record| matches!(record, Record::ToolResult(result) if result.call_id == pending.call_id),
            );
            let mut batch = vec![Record::approval_resolved(
                &pending.call_id,
                ApprovalDecision::Deny,
            )];
            if !already_executed {
                batch.push(Record::tool_result(
                    &pending.call_id,
                    "",
                    ABANDONED_RESULT,
                    -1,
                    false,
                    false,
                    None,
                ));
            }
            self.append_and_emit(&batch, watermark).await?;
        }
        Ok(())
    }

    /// 归档库拒绝写入：返回 `true` 表示已上报错误、调用方应立即以退出码 1 结束。
    ///
    /// 只在产生任何副作用（init、作废旧 pending、崩溃恢复、写 user、执行命令）之前调用。
    fn reject_archived(&self) -> anyhow::Result<bool> {
        if yaa_store::is_archived(&self.session)? {
            self.sink.emit(DisplayItem::Error(format!(
                "错误：会话已归档，不可写入：{}",
                self.session.db.display()
            )));
            return Ok(true);
        }
        Ok(false)
    }

    /// 崩溃恢复：为 `origin=ai` 且无 `tool_result` 的工具调用补写中断结果。
    ///
    /// 未决 `approval_pending` 不在此处理：普通 run 已由 [`Runner::abandon_old_pending`]
    /// 先作废，`--approve` 则要把最旧一条留给本次放行。已有结果或未决 pending 的
    /// `call_id` 一律跳过，因此重复运行是幂等的。
    async fn recover_orphan_tool_calls(&self, watermark: &mut usize) -> anyhow::Result<()> {
        let records = yaa_store::load(&self.session)?;
        let orphan_calls = orphan_tool_calls(&records);
        if orphan_calls.is_empty() {
            return Ok(());
        }
        let batch: Vec<Record> = orphan_calls
            .iter()
            .map(|call_id| {
                Record::tool_result(call_id, "", INTERRUPTED_RESULT, -1, false, false, None)
            })
            .collect();
        self.append_and_emit(&batch, watermark).await
    }

    /// 工作目录无法规范化时的统一告警。
    fn warn_cwd(&self) {
        if self.cwd_warned {
            self.sink.emit(DisplayItem::Notice(
                "警告：工作目录无法规范化，已回退到词法绝对路径计算会话 id。".to_string(),
            ));
        }
    }

    /// 执行 `init` 钩子一次。stdout 忽略（写入上下文只经 `yaa --notify`）。
    ///
    /// 失败（spawn/IO/超限/非零退出码）返回 `ExitCode::Config`。
    async fn run_init_hook(&self) -> Result<(), ExitCode> {
        // 钩子内的 `yaa --notify` 发到 `inbox/init/`：runner 在 init 之后优先消费，
        // 保持「notify → 簿记 → user」的既有顺序。
        let mut env = self.hook_env();
        // F1：init 通知必须落本会话的 `init/`，不能继承父会话的 `YAA_NOTIFY`
        // （否则嵌套子会话的系统提示词会被投进父会话 pending）。仍保留 `YAA_PARENT`，
        // 钩子需要回父时可显式 `--to "$YAA_PARENT"`。
        if let Some(self_db) = env.get("YAA_SESSION").cloned() {
            env.insert("YAA_NOTIFY".to_string(), self_db);
        }
        env.insert("YAA_INBOX_PHASE".to_string(), "init".to_string());
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
            // 成功时按设计丢弃 stderr：`llm` 钩子的加载过程不展示（README/plan），
            // 且 stderr 可能含敏感诊断；只有失败路径才上报 stderr。
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
    ///
    /// 传入的是**全量** `records` 与展示起点；`display::items_from` 需要全量记录才能
    /// 从更早批次的 `tool_call` 为审批决策块回填命令。
    fn emit_new(&self, records: &[Record], watermark: &mut usize) {
        let start = (*watermark).min(records.len());
        self.emit_items(records, start);
        *watermark = records.len();
    }

    /// 从 `start` 起投影记录为展示条目并逐条上报；`records` 须为全量记录。
    fn emit_items(&self, records: &[Record], start: usize) {
        for item in display::items_from(records, start) {
            self.sink.emit(item);
        }
    }

    /// 钩子进程的环境变量（继承当前进程环境）。
    fn hook_env(&self) -> BTreeMap<String, String> {
        self.hook_env_with(&process_env)
    }

    /// [`Runner::hook_env`] 的实现；`incoming` 为环境继承 getter，便于测试注入。
    fn hook_env_with(
        &self,
        incoming: &impl Fn(&str) -> Option<String>,
    ) -> BTreeMap<String, String> {
        // 会话变量由 `yaa_env` 统一构造：钩子不携带 shell 工具专有的
        // `YAA_NOTIFY_CALL_ID`，并把继承值显式清空（F2），避免父工具调用的 call_id
        // 泄漏进钩子发布的通知归因。
        let mut env = session::yaa_env(&self.session.db, incoming, None);
        env.insert("YAA_NOTIFY_CALL_ID".to_string(), String::new());
        let config = self.config_root.to_string_lossy().into_owned();
        env.insert("GLB_CFG".to_string(), config.clone());
        env.insert("YAA_CONFIG_HOME".to_string(), config);
        env.insert(
            "YAA_DATA_HOME".to_string(),
            self.data_root.to_string_lossy().into_owned(),
        );
        // 把 `[shell]` 限额以 `YAA_NOTIFY_*` 注入钩子：钩子内的 `yaa --notify` 据此
        // 采用与主进程相同的截断/超时参数，避免 `--config` 在外层生效、内层却回退默认（A6）。
        env.insert(
            "YAA_NOTIFY_TIMEOUT_SECS".to_string(),
            self.config.shell.timeout_secs.to_string(),
        );
        env.insert(
            "YAA_NOTIFY_MAX_STREAM_CHARS".to_string(),
            self.config.shell.max_stream_chars.to_string(),
        );
        env.insert(
            "YAA_NOTIFY_HEAD_CHARS".to_string(),
            self.config.shell.head_chars.to_string(),
        );
        env.insert(
            "YAA_NOTIFY_TAIL_CHARS".to_string(),
            self.config.shell.tail_chars.to_string(),
        );
        // 当前可执行文件路径，供默认钩子不依赖 PATH 地调用自身；取不到则省略。
        if let Ok(exe) = std::env::current_exe() {
            env.insert("YAA_BIN".to_string(), exe.to_string_lossy().into_owned());
        }
        env
    }
}

/// 读取进程环境变量；缺失返回 `None`。
///
/// 作为 [`session::yaa_env`] 的 `incoming` getter，使环境链从当前进程继承。
fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// 在 `base` 上叠加 ±20% 抖动，避免多 follower 同步轮询。
fn jitter(base: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    // 映射到 [0.8, 1.2)：`nanos % 4000` 落在 0..4000。
    let factor = 0.8 + f64::from(nanos % 4000) / 10_000.0;
    base.mul_f64(factor)
}

/// 一条未决待批：有 `approval_pending` 而无同 `call_id` 的 `approval_resolved`。
struct Pending {
    call_id: String,
    command: String,
}

/// 按记录顺序枚举未决 pending，附带其在记录中的下标（供 `--approve` 定位水位）。
fn unresolved_pending(records: &[Record]) -> Vec<(usize, Pending)> {
    let resolved: HashSet<&str> = records
        .iter()
        .filter_map(|record| match record {
            Record::ApprovalResolved(resolved) => Some(resolved.call_id.as_str()),
            _ => None,
        })
        .collect();
    records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| match record {
            Record::ApprovalPending(pending) if !resolved.contains(pending.call_id.as_str()) => {
                Some((
                    index,
                    Pending {
                        call_id: pending.call_id.clone(),
                        command: pending.command.clone(),
                    },
                ))
            }
            _ => None,
        })
        .collect()
}

/// 无对应 `tool_result` 且不属于未决待批的 `origin=ai` 工具调用 `call_id`。
///
/// 保持记录顺序，供崩溃恢复按原批次补写中断结果。
fn orphan_tool_calls(records: &[Record]) -> Vec<String> {
    let results: HashSet<&str> = records
        .iter()
        .filter_map(|record| match record {
            Record::ToolResult(result) => Some(result.call_id.as_str()),
            _ => None,
        })
        .collect();
    let pending: HashSet<String> = unresolved_pending(records)
        .into_iter()
        .map(|(_, pending)| pending.call_id)
        .collect();
    records
        .iter()
        .filter_map(|record| match record {
            Record::ToolCall(call)
                if call.origin == ToolOrigin::Ai
                    && !results.contains(call.call_id.as_str())
                    && !pending.contains(call.call_id.as_str()) =>
            {
                Some(call.call_id.clone())
            }
            _ => None,
        })
        .collect()
}

/// 判定一次 `origin=ai` 工具调用是否可执行；不可执行时返回失败原因。
///
/// 与「命令资源拆分失败按 ask 处理」区分：这里只拦工具名不符、参数 JSON 非法或
/// `command` 字段缺失/为空，均为**结构上无法执行**的调用；引号不配平等拆分问题
/// `parsed.error` 为 `None`，仍交由审批按 `ask` 处理。
fn inexecutable_reason(
    call: &crate::message::AssistantToolCall,
    parsed: &tools::shell::ParsedCall,
) -> Option<String> {
    if call.name != tools::shell::NAME {
        return Some(format!(
            "不支持的工具：{}（阶段 1 仅有 shell）。",
            call.name
        ));
    }
    if let Some(error) = &parsed.error {
        return Some(error.clone());
    }
    if parsed.command.trim().is_empty() {
        return Some("命令为空，未执行。".to_string());
    }
    None
}

/// 被拒工具结果的文案；无命中规则（拆分失败按 ask 后人工拒绝）时不写规则。
fn denied_result(rule: &str) -> String {
    if rule.is_empty() {
        "命令被审批规则拒绝。".to_string()
    } else {
        format!("命令被审批规则拒绝：命中规则 `{rule}`。")
    }
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

    impl compaction::Summarizer for FakeModel {
        fn summarize<'a>(
            &'a self,
            _messages: &'a [ChatMessage],
            _max_tokens: u32,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + 'a>> {
            Box::pin(async move { Ok("## Primary Request and Intent\n- 测试摘要".to_string()) })
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
        calls_response(vec![crate::message::AssistantToolCall {
            id: call_id.to_string(),
            name: tools::shell::NAME.to_string(),
            arguments: arguments.to_string(),
        }])
    }

    /// 一次回复携带多个工具调用（用于同批多 ask / 混合裁决测试）。
    fn calls_response(calls: Vec<crate::message::AssistantToolCall>) -> ChatResponse {
        ChatResponse {
            content: None,
            reasoning: None,
            tool_calls: calls,
            finish_reason: Some("tool_calls".to_string()),
        }
    }

    /// 构造一次 `shell` 工具调用。
    fn shell_call(id: &str, arguments: &str) -> crate::message::AssistantToolCall {
        crate::message::AssistantToolCall {
            id: id.to_string(),
            name: tools::shell::NAME.to_string(),
            arguments: arguments.to_string(),
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
                auto: false,
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
            Record::ApprovalPending(_) => "approval_pending",
            Record::ApprovalResolved(_) => "approval_resolved",
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
        assert_eq!(ExitCode::Pending.code(), 6);
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
        let expected = std::path::absolute(&harness.session.db)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(written, expected, "init 钩子应收到 YAA_SESSION 绝对路径");
    }

    /// 阶段 5 环境链：钩子 env 带 `YAA_NOTIFY`；嵌套时带 `YAA_PARENT`；
    /// `YAA_NOTIFY_CALL_ID` 显式清空（F2：不得继承父工具调用的 call_id）。
    #[test]
    fn hook_env_carries_notify_and_nested_parent() {
        let harness = Harness::new(test_config("true"));
        let sink = NullSink;
        let runner = harness.runner(&sink);
        let expected = std::path::absolute(&harness.session.db)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let env = runner.hook_env_with(&|_| None);
        assert_eq!(env["YAA_SESSION"], expected);
        assert_eq!(
            env["YAA_NOTIFY"], expected,
            "无继承时 YAA_NOTIFY 默认为自己"
        );
        assert!(!env.contains_key("YAA_PARENT"), "无继承不应有 YAA_PARENT");
        assert_eq!(
            env["YAA_NOTIFY_CALL_ID"], "",
            "钩子必须把继承的 YAA_NOTIFY_CALL_ID 清空"
        );

        let parent = "/data/parent/session.db";
        let incoming = |key: &str| (key == "YAA_SESSION").then(|| parent.to_string());
        let env = runner.hook_env_with(&incoming);
        assert_eq!(env["YAA_SESSION"], expected, "YAA_SESSION 无条件重设");
        assert_eq!(env["YAA_PARENT"], parent, "继承的 YAA_SESSION 记为父会话");
        assert_eq!(env["YAA_NOTIFY_CALL_ID"], "", "继承的 call_id 必须清空");
    }

    #[tokio::test]
    async fn notify_record_is_projected_as_injection() {
        // 预置一条通知，验证它进入模型上下文时保留命令前缀与信封。
        // （真实 `yaa --notify` 的落盘路径由 yaa-cli 的 e2e 覆盖。）
        let harness = Harness::new(test_config("true"));
        yaa_store::append(
            &harness.session,
            &[Record::system(
                "notify",
                "<注释>\n内容\n</注释>",
                Some("echo hi".to_string()),
                None,
            )],
        )
        .unwrap();
        let sink = NullSink;
        let model = FakeModel::new(vec![text_response("好")]);

        harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();

        let seen = model.seen();
        let first = seen.first().expect("应发起一次模型请求");
        let projected = first
            .iter()
            .find(|message| message.role == "system")
            .and_then(|message| message.content.as_deref())
            .expect("应投影 system 消息");
        assert!(
            projected.contains("[执行命令] echo hi"),
            "通知应带命令前缀：{projected}"
        );
        assert!(
            projected.contains("<注释>\n内容\n</注释>"),
            "通知信封应保留：{projected}"
        );
    }

    /// 带 `--auto` 的 Runner（在默认 runner 上覆盖 `auto`）。
    impl Harness {
        fn runner_auto<'a>(&'a self, sink: &'a dyn Sink, auto: bool) -> Runner<'a> {
            Runner {
                auto,
                ..self.runner(sink)
            }
        }
    }

    fn rule(pattern: &str, action: Action) -> crate::approval::Rule {
        crate::approval::Rule {
            pattern: pattern.to_string(),
            action,
        }
    }

    fn approval_config(pattern: &str, action: Action) -> Config {
        let mut config = test_config("true");
        config.approval.rules = vec![rule(pattern, action)];
        config
    }

    #[tokio::test]
    async fn ask_interception_writes_pending_and_returns_pending() {
        let harness = Harness::new(approval_config("echo *", Action::Ask));
        let sink = RecordingSink::default();
        let model = FakeModel::new(vec![call_response("call_1", r#"{"command":"echo hi"}"#)]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Pending);

        let records = harness.records();
        assert_eq!(
            records.iter().map(record_label).collect::<Vec<_>>(),
            vec!["system", "user", "ai", "tool_call", "approval_pending"],
            "ask 应只写 pending，不写 tool_result"
        );
        let Record::ApprovalPending(pending) = &records[4] else {
            panic!("末条应为 approval_pending");
        };
        assert_eq!(pending.call_id, "call_1");
        assert_eq!(pending.command, "echo hi");
        assert_eq!(pending.matched_rule, "echo *");

        assert!(
            sink.items().iter().any(|item| matches!(
                item,
                DisplayItem::ApprovalPending { command, matched_rule, .. }
                    if command == "echo hi" && matched_rule == "echo *"
            )),
            "应展示待批条目"
        );
    }

    /// R2：同批多个 ask 都要落 pending，之后以退出码 6 结束，不得伪装成中断。
    #[tokio::test]
    async fn multiple_asks_in_one_batch_all_recorded() {
        let harness = Harness::new(approval_config("echo *", Action::Ask));
        let sink = RecordingSink::default();
        let model = FakeModel::new(vec![calls_response(vec![
            shell_call("call_1", r#"{"command":"echo a"}"#),
            shell_call("call_2", r#"{"command":"echo b"}"#),
        ])]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Pending);

        let records = harness.records();
        let pendings: Vec<&str> = records
            .iter()
            .filter_map(|record| match record {
                Record::ApprovalPending(pending) => Some(pending.call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(pendings, vec!["call_1", "call_2"], "同批 ask 都应落盘");

        // 批内每个 tool_call 都应有 pending 配对，不得产生游离记录。
        let call_ids: Vec<&str> = records
            .iter()
            .filter_map(|record| match record {
                Record::ToolCall(call) => Some(call.call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(call_ids, pendings, "每个 tool_call 都应有 approval_pending");

        assert!(
            !records.iter().any(|r| matches!(r, Record::ToolResult(_))),
            "ask 拦截不得写 tool_result"
        );
        assert_eq!(
            sink.items()
                .iter()
                .filter(|item| matches!(item, DisplayItem::ApprovalPending { .. }))
                .count(),
            2,
            "两条待批都应展示"
        );
    }

    /// R2：同批中 allow 的调用照常执行并落结果，ask 的落 pending 后以 6 退出。
    #[tokio::test]
    async fn mixed_allow_and_ask_same_batch() {
        // 规则只问 `curl *`；`echo hi` 无匹配默认 allow。
        let harness = Harness::new(approval_config("curl *", Action::Ask));
        let sink = RecordingSink::default();
        let model = FakeModel::new(vec![calls_response(vec![
            shell_call("call_1", r#"{"command":"echo hi"}"#),
            shell_call("call_2", r#"{"command":"curl x"}"#),
        ])]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Pending);

        let records = harness.records();
        assert_eq!(
            records.iter().map(record_label).collect::<Vec<_>>(),
            vec![
                "system",
                "user",
                "ai",
                "tool_call",
                "tool_call",
                "tool_result",
                "approval_pending"
            ],
            "allow 先执行落结果，ask 落 pending"
        );
        let Record::ToolResult(result) = &records[5] else {
            panic!("allow 调用应有 tool_result");
        };
        assert_eq!(result.call_id, "call_1");
        assert_eq!(result.stdout, "hi\n");
        let Record::ApprovalPending(pending) = &records[6] else {
            panic!("ask 调用应有 approval_pending");
        };
        assert_eq!(pending.call_id, "call_2");
    }

    #[tokio::test]
    async fn auto_approves_ask_and_executes() {
        let harness = Harness::new(approval_config("echo *", Action::Ask));
        let sink = RecordingSink::default();
        let model = FakeModel::new(vec![
            call_response("call_1", r#"{"command":"echo hi"}"#),
            text_response("完成"),
        ]);

        let exit = harness
            .runner_auto(&sink, true)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success);

        let records = harness.records();
        assert_eq!(
            records.iter().map(record_label).collect::<Vec<_>>(),
            vec![
                "system",
                "user",
                "ai",
                "tool_call",
                "approval_pending",
                "approval_resolved",
                "tool_result",
                "ai"
            ]
        );
        let Record::ApprovalResolved(resolved) = &records[5] else {
            panic!("应有 approval_resolved");
        };
        assert_eq!(resolved.decision, ApprovalDecision::Approve);
        let Record::ToolResult(result) = &records[6] else {
            panic!("应有 tool_result");
        };
        assert_eq!(result.stdout, "hi\n");
    }

    #[tokio::test]
    async fn deny_writes_failed_result_and_continues() {
        let harness = Harness::new(approval_config("rm *", Action::Deny));
        let sink = RecordingSink::default();
        let model = FakeModel::new(vec![
            call_response("call_1", r#"{"command":"rm -rf /tmp/x"}"#),
            text_response("完成"),
        ]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success, "deny 不应中断循环");

        let records = harness.records();
        assert_eq!(
            records.iter().map(record_label).collect::<Vec<_>>(),
            vec![
                "system",
                "user",
                "ai",
                "tool_call",
                "approval_pending",
                "approval_resolved",
                "tool_result",
                "ai"
            ]
        );
        let Record::ApprovalResolved(resolved) = &records[5] else {
            panic!("应有 approval_resolved");
        };
        assert_eq!(resolved.decision, ApprovalDecision::Deny);
        let Record::ToolResult(result) = &records[6] else {
            panic!("应有 tool_result");
        };
        assert_eq!(result.code, -1);
        assert!(result.stderr.contains("拒绝"), "拒绝说明应可见：{result:?}");
        // 下一跳请求应带上失败结果（模型可见被拒）。
        let second = &model.seen()[1];
        assert!(
            second.iter().any(|message| message.role == "tool"
                && message
                    .content
                    .as_deref()
                    .is_some_and(|c| c.contains("拒绝"))),
            "模型下一跳应看到被拒结果"
        );
    }

    /// R1：未知工具名不是可执行调用，不得产生 pending，应落失败结果并继续循环。
    #[tokio::test]
    async fn unknown_tool_fails_without_pending() {
        let harness = Harness::new(approval_config("touch *", Action::Ask));
        let sink = RecordingSink::default();
        let model = FakeModel::new(vec![
            calls_response(vec![crate::message::AssistantToolCall {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: r#"{"path":"/etc/passwd"}"#.to_string(),
            }]),
            text_response("完成"),
        ]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success, "不可执行调用不应以 6 退出");

        let records = harness.records();
        assert!(
            !records
                .iter()
                .any(|r| matches!(r, Record::ApprovalPending(_))),
            "未知工具不得产生 approval_pending"
        );
        let result = records
            .iter()
            .find_map(|r| match r {
                Record::ToolResult(result) => Some(result),
                _ => None,
            })
            .expect("应落一条失败 tool_result");
        assert_eq!(result.code, -1);
        assert!(
            result.stderr.contains("read_file"),
            "失败文案应说明未知工具：{result:?}"
        );
    }

    /// R1：`command` 缺失或非字符串导致解析失败时，同样不得产生 pending。
    #[tokio::test]
    async fn invalid_arguments_fail_without_pending() {
        let harness = Harness::new(approval_config("touch *", Action::Ask));
        let sink = RecordingSink::default();
        let model = FakeModel::new(vec![
            call_response("call_1", "not json"),
            text_response("完成"),
        ]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success, "非法参数不应以 6 退出");

        let records = harness.records();
        assert!(
            !records
                .iter()
                .any(|r| matches!(r, Record::ApprovalPending(_))),
            "非法参数不得产生 approval_pending"
        );
        let result = records
            .iter()
            .find_map(|r| match r {
                Record::ToolResult(result) => Some(result),
                _ => None,
            })
            .expect("应落一条失败 tool_result");
        assert_eq!(result.code, -1);
        assert!(
            result.stderr.contains("JSON") || result.stderr.contains("command"),
            "失败文案应说明参数非法：{result:?}"
        );
    }

    /// R1：命令为空白时不得产生 pending，也不得执行。
    #[tokio::test]
    async fn empty_command_fails_without_pending() {
        let harness = Harness::new(approval_config("touch *", Action::Ask));
        let sink = NullSink;
        let model = FakeModel::new(vec![
            call_response("call_1", r#"{"command":"   "}"#),
            text_response("完成"),
        ]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success);

        let records = harness.records();
        assert!(
            !records
                .iter()
                .any(|r| matches!(r, Record::ApprovalPending(_))),
            "空命令不得产生 approval_pending"
        );
        assert!(
            records
                .iter()
                .any(|r| matches!(r, Record::ToolResult(result) if result.code == -1)),
            "空命令应落失败 tool_result"
        );
    }

    /// R1(b)：引号不配平属「命令资源拆分失败」，`parsed.error` 为 `None`，仍须按 ask 处理。
    #[tokio::test]
    async fn unbalanced_quotes_in_command_still_ask() {
        let harness = Harness::new(test_config("true"));
        let sink = RecordingSink::default();
        let model = FakeModel::new(vec![call_response(
            "call_1",
            r#"{"command":"echo \"oops"}"#,
        )]);

        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Pending, "拆分失败不得静默放行");

        let records = harness.records();
        let Record::ApprovalPending(pending) = records.last().unwrap() else {
            panic!("末条应为 approval_pending");
        };
        assert_eq!(pending.matched_rule, "", "拆分失败无命中规则");
        assert!(
            !records.iter().any(|r| matches!(r, Record::ToolResult(_))),
            "ask 拦截不得写 tool_result"
        );
    }

    #[tokio::test]
    async fn new_prompt_abandons_old_pending_before_user() {
        let harness = Harness::new(approval_config("echo *", Action::Ask));
        let sink = NullSink;

        let first = FakeModel::new(vec![call_response("call_1", r#"{"command":"echo hi"}"#)]);
        let exit = harness
            .runner(&sink)
            .run("第一次", |_config| first.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Pending);

        let second = FakeModel::new(vec![text_response("新答")]);
        let exit = harness
            .runner(&sink)
            .run("新提示词", |_config| second.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success);

        let records = harness.records();
        let labels: Vec<&str> = records.iter().map(record_label).collect();
        assert_eq!(
            labels,
            vec![
                "system",
                "user",
                "ai",
                "tool_call",
                "approval_pending",
                "approval_resolved",
                "tool_result",
                "user",
                "ai"
            ],
            "旧 pending 应在新 user 之前作废：{labels:?}"
        );
        let Record::ApprovalResolved(resolved) = &records[5] else {
            panic!("应补 approval_resolved");
        };
        assert_eq!(resolved.decision, ApprovalDecision::Deny);
        let Record::ToolResult(result) = &records[6] else {
            panic!("应补失败 tool_result");
        };
        assert_eq!(result.code, -1);
        assert!(result.stderr.contains("作废"), "作废说明：{result:?}");
    }

    /// D1：中断态（已有 `tool_result` 却缺 `approval_resolved`）下，普通 run 作废旧待批
    /// 只补 `approval_resolved: deny`，不得再追加失败结果（与 `--approve` 判定对称）。
    #[tokio::test]
    async fn abandon_skips_extra_result_when_already_executed() {
        let harness = Harness::new(approval_config("echo *", Action::Ask));
        let sink = NullSink;

        // 构造中断态：tool_call + pending + 已执行结果，缺 resolved。
        yaa_store::append(
            &harness.session,
            &[
                Record::tool_call(ToolOrigin::Ai, "call_1", "echo hi", None),
                Record::approval_pending("call_1", "echo hi", "echo *"),
                Record::tool_result("call_1", "REAL-OUTPUT\n", "", 0, false, false, None),
            ],
        )
        .unwrap();

        let model = FakeModel::new(vec![text_response("新答")]);
        let exit = harness
            .runner(&sink)
            .run("新提示词", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success);

        let records = harness.records();
        let results: Vec<&Record> = records
            .iter()
            .filter(|record| matches!(record, Record::ToolResult(_)))
            .collect();
        assert_eq!(results.len(), 1, "不得重复追加失败结果：{records:?}");
        let Record::ToolResult(result) = results[0] else {
            unreachable!("过滤已限定为 tool_result")
        };
        assert_eq!(result.stdout, "REAL-OUTPUT\n", "既有成功结果应保留");

        let resolved = records
            .iter()
            .find_map(|record| match record {
                Record::ApprovalResolved(resolved) => Some(resolved),
                _ => None,
            })
            .expect("应补 approval_resolved");
        assert_eq!(resolved.decision, ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn approve_pending_executes_oldest_and_continues_loop() {
        let harness = Harness::new(approval_config("echo *", Action::Ask));
        let sink = RecordingSink::default();

        let first = FakeModel::new(vec![call_response("call_1", r#"{"command":"echo hi"}"#)]);
        let exit = harness
            .runner(&sink)
            .run("你好", |_config| first.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Pending);

        let second = FakeModel::new(vec![text_response("完成")]);
        let approve_sink = RecordingSink::default();
        let exit = harness
            .runner(&approve_sink)
            .approve_pending(|_config| second.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success);

        let records = harness.records();
        assert_eq!(
            records.iter().map(record_label).collect::<Vec<_>>(),
            vec![
                "system",
                "user",
                "ai",
                "tool_call",
                "approval_pending",
                "tool_result",
                "approval_resolved",
                "ai"
            ],
            "先执行写 tool_result，再写 resolved: approve，然后继续循环"
        );
        let Record::ToolResult(result) = &records[5] else {
            panic!("应有 tool_result");
        };
        assert_eq!(result.stdout, "hi\n");

        // 同一条 pending 只放行一次：不会重复执行，也不会再产生 pending。
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(r, Record::ApprovalPending(_)))
                .count(),
            1
        );
        // 展示：pending、已批准各自恰好一次（本次 `--approve` 进程中）。
        let items = approve_sink.items();
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item, DisplayItem::ApprovalPending { .. }))
                .count(),
            1
        );
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item, DisplayItem::ApprovalResolved { approved: true, .. }))
                .count(),
            1
        );
    }

    /// R5(a)：`--approve` 只展示被放行的那条待批，不重放其余历史待批。
    #[tokio::test]
    async fn approve_display_does_not_replay_other_pendings() {
        let harness = Harness::new(approval_config("echo *", Action::Ask));
        let sink = NullSink;
        let model = FakeModel::new(vec![calls_response(vec![
            shell_call("call_1", r#"{"command":"echo a"}"#),
            shell_call("call_2", r#"{"command":"echo b"}"#),
        ])]);
        let exit = harness
            .runner(&sink)
            .run("你好", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Pending);

        let approve_sink = RecordingSink::default();
        let model = FakeModel::new(vec![text_response("完成")]);
        let exit = harness
            .runner(&approve_sink)
            .approve_pending(|_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success);

        let pendings: Vec<String> = approve_sink
            .items()
            .into_iter()
            .filter_map(|item| match item {
                DisplayItem::ApprovalPending { call_id, .. } => Some(call_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            pendings,
            vec!["call_1".to_string()],
            "只应展示被放行的那条待批，不重放 call_2"
        );
    }

    /// R3：该 `call_id` 已有 `tool_result`（上次执行后、写 resolved 前中断）时，
    /// 放行只补 `approval_resolved: approve`，绝不重复执行命令。
    #[tokio::test]
    async fn approve_skips_execution_when_result_already_exists() {
        let harness = Harness::new(approval_config("echo *", Action::Ask));
        let sink = NullSink;

        let first = FakeModel::new(vec![call_response("call_1", r#"{"command":"echo hi"}"#)]);
        let exit = harness
            .runner(&sink)
            .run("你好", |_config| first.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Pending);

        // 模拟「执行了命令、写了结果，但在写 resolved 前进程被杀」。
        yaa_store::append(
            &harness.session,
            &[Record::tool_result(
                "call_1",
                "已有结果\n",
                "",
                0,
                false,
                false,
                None,
            )],
        )
        .unwrap();

        let model = FakeModel::new(vec![text_response("完成")]);
        let exit = harness
            .runner(&sink)
            .approve_pending(|_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success);

        let records = harness.records();
        let results: Vec<&Record> = records
            .iter()
            .filter(|r| matches!(r, Record::ToolResult(result) if result.call_id == "call_1"))
            .collect();
        assert_eq!(results.len(), 1, "不得重复执行新增 tool_result");
        let Record::ToolResult(result) = results[0] else {
            unreachable!();
        };
        assert_eq!(result.stdout, "已有结果\n", "应保留既有结果");
        assert!(
            records.iter().any(|r| matches!(
                r,
                Record::ApprovalResolved(resolved)
                    if resolved.call_id == "call_1"
                        && resolved.decision == ApprovalDecision::Approve
            )),
            "应补 approval_resolved: approve：{records:?}"
        );
    }

    /// R4：无未决 pending 时不得解析 `llm` 钩子（钩子损坏也应成功提示）。
    #[tokio::test]
    async fn approve_without_pending_skips_llm_hook() {
        let mut config = approval_config("echo *", Action::Ask);
        config.hooks.llm = "exit 3".to_string();
        let harness = Harness::new(config);
        let sink = RecordingSink::default();
        let model = FakeModel::new(vec![]);

        let exit = harness
            .runner(&sink)
            .approve_pending(|_config| model.clone())
            .await
            .unwrap();
        assert_eq!(
            exit,
            ExitCode::Success,
            "没有待批时不应因 llm 钩子损坏而退出配置错误"
        );
        assert!(
            sink.items().iter().any(|item| matches!(
                item,
                DisplayItem::Notice(text) if text.contains("没有待批准")
            )),
            "应提示没有待批准：{:?}",
            sink.items()
        );
    }

    /// R4：有 pending 但 `llm` 钩子损坏时以配置错误退出，且不执行任何命令。
    #[tokio::test]
    async fn approve_with_pending_and_broken_llm_hook_does_not_execute() {
        let mut config = approval_config("echo *", Action::Ask);
        config.hooks.llm = "exit 3".to_string();
        let harness = Harness::new(config);
        let sink = NullSink;
        yaa_store::append(
            &harness.session,
            &[
                Record::tool_call(ToolOrigin::Ai, "call_1", "echo hi", None),
                Record::approval_pending("call_1", "echo hi", "echo *"),
            ],
        )
        .unwrap();
        let before = harness.records().len();

        let model = FakeModel::new(vec![]);
        let exit = harness
            .runner(&sink)
            .approve_pending(|_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Config);
        assert_eq!(
            harness.records().len(),
            before,
            "llm 钩子失败时不得执行命令或写记录"
        );
        assert!(
            !harness
                .records()
                .iter()
                .any(|r| matches!(r, Record::ToolResult(_) | Record::ApprovalResolved(_))),
            "不得补 tool_result / approval_resolved"
        );
    }

    /// D-S10：普通 run 启动时为无结果的 `origin=ai` 工具调用补写中断结果。
    #[tokio::test]
    async fn run_recovers_orphan_tool_call_with_interrupted_result() {
        let harness = Harness::new(test_config("true"));
        let sink = NullSink;
        yaa_store::append(
            &harness.session,
            &[
                Record::user("旧问"),
                Record::ai("fake-model", "h", "", None),
                Record::tool_call(ToolOrigin::Ai, "orphan", "sleep 1", None),
            ],
        )
        .unwrap();

        let model = FakeModel::new(vec![text_response("新答")]);
        let exit = harness
            .runner(&sink)
            .run("新问", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success);

        let records = harness.records();
        let result = records
            .iter()
            .find_map(|record| match record {
                Record::ToolResult(result) if result.call_id == "orphan" => Some(result),
                _ => None,
            })
            .expect("应补一条中断结果");
        assert_eq!(result.code, -1);
        assert!(result.stderr.contains("interrupted"), "{result:?}");
        assert!(result.stdout.is_empty());
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, Record::ToolResult(r) if r.call_id == "orphan"))
                .count(),
            1,
            "重复运行不得重复补写"
        );
    }

    /// D-S10：`--approve` 只补孤儿调用，未决 pending 留给本次放行、不得被中断结果顶替。
    #[tokio::test]
    async fn approve_recovers_only_orphans_and_keeps_pending() {
        let harness = Harness::new(approval_config("echo *", Action::Ask));
        let sink = NullSink;
        yaa_store::append(
            &harness.session,
            &[
                Record::user("旧问"),
                Record::ai("fake-model", "h", "", None),
                Record::tool_call(ToolOrigin::Ai, "orphan", "sleep 1", None),
                Record::tool_call(ToolOrigin::Ai, "call_1", "echo hi", None),
                Record::approval_pending("call_1", "echo hi", "echo *"),
            ],
        )
        .unwrap();

        let model = FakeModel::new(vec![text_response("完成")]);
        let exit = harness
            .runner(&sink)
            .approve_pending(|_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Success);

        let records = harness.records();
        let orphan = records
            .iter()
            .find_map(|record| match record {
                Record::ToolResult(result) if result.call_id == "orphan" => Some(result),
                _ => None,
            })
            .expect("孤儿调用应补中断结果");
        assert_eq!(orphan.code, -1);
        let approved = records
            .iter()
            .find_map(|record| match record {
                Record::ToolResult(result) if result.call_id == "call_1" => Some(result),
                _ => None,
            })
            .expect("待批命令应被真正执行");
        assert_eq!(approved.stdout, "hi\n");
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, Record::ToolResult(r) if r.call_id == "call_1"))
                .count(),
            1,
            "未决 pending 不得被中断结果顶替"
        );
    }

    /// S3：归档库上的 `run` 在写 user / 执行 init 前拒绝，退出码 1。
    #[tokio::test]
    async fn run_on_archived_session_is_rejected_without_writes() {
        let harness = Harness::new(test_config("true"));
        let sink = RecordingSink::default();
        yaa_store::append(&harness.session, &[Record::user("既有")]).unwrap();
        yaa_store::set_archived(&harness.session).unwrap();
        let before = harness.records();

        let model = FakeModel::new(vec![text_response("不应调用")]);
        let exit = harness
            .runner(&sink)
            .run("新问", |_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Error);
        assert_eq!(harness.records(), before, "归档库不得新增记录");
        assert!(model.seen().is_empty(), "归档库不得调用模型");
        assert!(
            sink.items().iter().any(|item| matches!(
                item,
                DisplayItem::Error(text) if text.contains("已归档")
            )),
            "应上报已归档错误：{:?}",
            sink.items()
        );
    }

    /// S3：归档库上的 `--approve` 不执行 pending 命令，退出码 1。
    #[tokio::test]
    async fn approve_on_archived_session_does_not_execute() {
        let harness = Harness::new(approval_config("touch *", Action::Ask));
        let sink = RecordingSink::default();
        let marker = harness.cwd.join("marker");
        yaa_store::append(
            &harness.session,
            &[
                Record::tool_call(
                    ToolOrigin::Ai,
                    "call_1",
                    format!("touch {}", marker.display()),
                    None,
                ),
                Record::approval_pending(
                    "call_1",
                    format!("touch {}", marker.display()),
                    "touch *",
                ),
            ],
        )
        .unwrap();
        yaa_store::set_archived(&harness.session).unwrap();
        let before = harness.records();

        let model = FakeModel::new(vec![text_response("不应调用")]);
        let exit = harness
            .runner(&sink)
            .approve_pending(|_config| model.clone())
            .await
            .unwrap();
        assert_eq!(exit, ExitCode::Error);
        assert_eq!(harness.records(), before, "归档库不得新增记录");
        assert!(!marker.exists(), "归档库不得执行 pending 命令");
        assert!(
            sink.items().iter().any(|item| matches!(
                item,
                DisplayItem::Error(text) if text.contains("已归档")
            )),
            "应上报已归档错误：{:?}",
            sink.items()
        );
    }
}
