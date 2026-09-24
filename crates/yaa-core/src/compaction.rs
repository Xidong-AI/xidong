//! 渲染期压缩（阶段 3）。
//!
//! 压缩只发生在渲染阶段，**不改 `default.jsonl`**：摘要产物写会话根下的
//! `summaries/` 侧车缓存（D50），投影时把被遮蔽区间替换为一条摘要消息。触发链、
//! 选区间与剪枝规则照搬 DSH `compaction-basic` / `compaction-tool-result-pruner`，
//! 具体决策见 `docs/plan.md` 阶段 3（D48–D58）。
//!
//! 本模块提供零件与编排：token 估算（[`token`]）、工具正文修剪（[`prune`]）、
//! 可遮蔽区间选择（[`region`]）、检查点框定与摘要输入（[`summary`]）、侧车缓存
//! （[`cache`]），以及把它们串起来的 [`compact`]。摘要调用经 [`Summarizer`] 抽象，
//! 避免 `compaction` 反向依赖 `agent`。

pub mod cache;
pub mod prune;
pub mod region;
pub mod summary;
pub mod token;

pub use token::Estimator;

use std::future::Future;
use std::pin::Pin;

use yaa_msg::Record;

use crate::config::CompactionConfig;
use crate::message::ChatMessage;
use crate::render::render;

/// 摘要能力：由 `agent::Model` 实现。
///
/// 抽出成 trait 而非直接依赖 `agent::Model`，使压缩编排可独立单测、且不构成
/// `agent → compaction → agent` 的模块环。
pub trait Summarizer {
    /// 发送一次摘要请求，返回摘要正文；`max_tokens` 为输出上限。
    fn summarize<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        max_tokens: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + 'a>>;
}

/// 由 [`CompactionConfig`] 派生的具体 token 预算。
///
/// 阈值与保留都是「窗口 × 比例」的具体 token 数；窗口缺失或整支禁用时为 `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    /// 投影 token 达到该值即触发压缩。
    pub threshold_tokens: usize,
    /// 逐字保留的近期尾部 token 预算。
    pub retain_tokens: usize,
}

impl Budgets {
    /// 由策略与模型窗口派生；`enabled = false` 或窗口缺失时返回 `None`（压缩静默禁用，
    /// D49）。窗口来自 [`crate::llm::LlmConfig::context_window`]（供应侧）。
    pub fn from_config(cfg: &CompactionConfig, context_window: Option<u32>) -> Option<Self> {
        if !cfg.enabled {
            return None;
        }
        let window = context_window? as usize;
        let threshold_tokens = (window as f64 * cfg.threshold_ratio).floor() as usize;
        let retain_tokens = (window as f64 * cfg.retain_ratio).floor() as usize;
        Some(Self {
            threshold_tokens,
            retain_tokens,
        })
    }
}

/// 一次生效的摘要：遮蔽哪些记录、替换成什么消息。
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// 被遮蔽的记录 id，按物理顺序（含前驱摘要覆盖的记录）。
    pub covered: Vec<String>,
    /// 替换这批记录的单条摘要消息（`user` 角色，含前导语与标签）。
    pub message: ChatMessage,
}

/// 一次压缩评估的完整结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// 实际应发给模型的消息序列（已应用摘要与工具正文修剪）。
    pub messages: Vec<ChatMessage>,
    /// 生效的摘要（`None` 表示未压缩）。
    pub applied: Option<Applied>,
    /// 本次新生成的侧车条目（命中缓存或未压缩时 `None`）；调用方负责落盘与展示。
    pub entry: Option<cache::SummaryEntry>,
    /// 非致命警告（摘要失败、摘要不比原文小、无可用区间等），由调用方打到 stderr（D55）。
    pub warning: Option<String>,
}

/// 计算本步的消息投影：不超阈值原样渲染；超阈值先修剪工具正文，再必要时摘要。
///
/// 摘要回调 `summarizer` 只在确需新摘要时被调用一次。返回的 [`Plan::messages`]
/// 即最终请求体，[`Plan::entry`] 由调用方落盘（本函数不写文件，便于纯逻辑单测）。
pub async fn compact<S: Summarizer>(
    records: &[Record],
    model: &str,
    estimator: &Estimator,
    budgets: &Budgets,
    cfg: &CompactionConfig,
    previous: Option<&cache::SummaryEntry>,
    summarizer: &S,
) -> Plan {
    // 稳定前缀 = 首条 `user` 之前的记录（`init` 簿记、初始化注入）。永不进入区间。
    let prefix_len = records
        .iter()
        .position(|record| matches!(record, Record::User(_)))
        .unwrap_or(records.len());
    let prefix_msgs = render(&records[..prefix_len]);

    // 前驱摘要仅在确实覆盖「首条 user 起的连续前缀」时可用（否则视为失效缓存）。
    let previous = previous.filter(|entry| previous_matches(records, prefix_len, entry));
    let body_start = match previous {
        Some(entry) => prefix_len + entry.covers.len(),
        None => prefix_len,
    };
    let body_records = &records[body_start..];

    // 完整投影（测量与修剪都基于它）。
    let mut messages = prefix_msgs.clone();
    if let Some(entry) = previous {
        messages.push(summary::checkpoint_message(&entry.summary));
    }
    messages.extend(render(body_records));

    let applied_previous = previous.map(|entry| Applied {
        covered: entry.covers.clone(),
        message: summary::checkpoint_message(&entry.summary),
    });

    if estimator.estimate_messages(&messages) < budgets.threshold_tokens {
        return Plan {
            messages,
            applied: applied_previous,
            entry: None,
            warning: None,
        };
    }

    // 先修剪工具正文，重测；修剪已足够时跳过摘要。
    let prune_budgets = prune::PruneBudgets::from_config(cfg);
    let mut pruned = messages;
    prune::prune_messages(&mut pruned, &prune_budgets);
    if estimator.estimate_messages(&pruned) < budgets.threshold_tokens {
        return Plan {
            messages: pruned,
            applied: applied_previous,
            entry: None,
            warning: None,
        };
    }

    // 记录视图：`previous` 摘要节点（若有）+ 其后的原始记录。下标 0 恒为起点。
    let previous_in_view = previous.is_some();
    let mut view_tokens: Vec<usize> = Vec::with_capacity(body_records.len() + 1);
    let mut view_boundaries: Vec<bool> = Vec::with_capacity(body_records.len() + 1);
    if let Some(entry) = previous {
        view_tokens.push(estimator.estimate_text(&entry.summary));
        view_boundaries.push(true);
    }
    for record in body_records {
        view_tokens.push(estimator.estimate_record(record));
        view_boundaries.push(region::is_step_boundary(record));
    }

    let Some((start, end)) = region::select_range(&view_tokens, &view_boundaries, 0, budgets.retain_tokens)
    else {
        return Plan {
            messages: pruned,
            applied: applied_previous,
            entry: None,
            warning: None,
        };
    };
    debug_assert_eq!(start, 0, "起点恒为视图首节点");

    // 视图下标 → 记录：`previous` 占位 `offset`（0 或 1）。
    let offset = usize::from(previous_in_view);
    let shadow_count = (end + 1).saturating_sub(offset).min(body_records.len());
    let shadow_body = &body_records[..shadow_count];
    let kept_body = &body_records[shadow_count..];

    let mut covers: Vec<String> = Vec::new();
    if let Some(entry) = previous {
        covers.extend(entry.covers.iter().cloned());
    }
    covers.extend(shadow_body.iter().map(|record| record.id().to_string()));
    if covers.is_empty() {
        return Plan {
            messages: pruned,
            applied: applied_previous,
            entry: None,
            warning: None,
        };
    }

    // 摘要输入：稳定前缀（语境）+ 被遮蔽消息（含前驱摘要文本，实现链式递进）+ 指令。
    let mut shadow_msgs: Vec<ChatMessage> = Vec::with_capacity(shadow_body.len() + 1);
    if let Some(entry) = previous {
        shadow_msgs.push(summary::checkpoint_message(&entry.summary));
    }
    shadow_msgs.extend(render(shadow_body));
    let input = summary::summary_input(prefix_msgs.first(), &shadow_msgs);

    let summary_text = match summarizer.summarize(&input, cfg.max_tokens).await {
        Ok(text) => text,
        Err(err) => {
            return Plan {
                messages: pruned,
                applied: applied_previous,
                entry: None,
                warning: Some(format!("压缩摘要失败，已跳过本次压缩：{err:#}")),
            };
        }
    };
    let checkpoint = summary::checkpoint_message(&summary_text);
    if estimator.estimate_message(&checkpoint) >= estimator.estimate_messages(&shadow_msgs) {
        return Plan {
            messages: pruned,
            applied: applied_previous,
            entry: None,
            warning: Some("压缩摘要不比被遮蔽内容小，已跳过本次压缩。".to_string()),
        };
    }

    // 重新组装：稳定前缀 + 摘要 + 保留的近期尾部；再修剪一次（保留区也可能含超长工具正文）。
    let mut final_messages = prefix_msgs;
    final_messages.push(checkpoint.clone());
    final_messages.extend(render(kept_body));
    prune::prune_messages(&mut final_messages, &prune_budgets);

    let entry = cache::SummaryEntry {
        id: cache::SummaryCache::key(model, &covers),
        model: model.to_string(),
        previous: previous.map(|entry| entry.id.clone()),
        covers: covers.clone(),
        summary: summary_text,
        created_ts: yaa_msg::now_ms(),
        tokens: Some(estimator.estimate_message(&checkpoint)),
    };
    Plan {
        messages: final_messages,
        applied: Some(Applied {
            covered: covers,
            message: checkpoint,
        }),
        entry: Some(entry),
        warning: None,
    }
}

/// 前驱摘要是否仍覆盖当前历史的前缀。
fn previous_matches(records: &[Record], prefix_len: usize, previous: &cache::SummaryEntry) -> bool {
    let len = previous.covers.len();
    if prefix_len + len > records.len() {
        return false;
    }
    records[prefix_len..prefix_len + len]
        .iter()
        .map(|record| record.id())
        .eq(previous.covers.iter().map(String::as_str))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use yaa_msg::ToolOrigin;

    /// 固定返回脚本的摘要器；记录每次收到的输入。
    struct FakeSummarizer {
        reply: Mutex<Option<String>>,
        calls: Mutex<Vec<Vec<ChatMessage>>>,
    }

    impl FakeSummarizer {
        fn new(reply: &str) -> Self {
            Self {
                reply: Mutex::new(Some(reply.to_string())),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn failing() -> Self {
            Self {
                reply: Mutex::new(None),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Vec<ChatMessage>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Summarizer for FakeSummarizer {
        fn summarize<'a>(
            &'a self,
            messages: &'a [ChatMessage],
            _max_tokens: u32,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + 'a>> {
            self.calls.lock().unwrap().push(messages.to_vec());
            let reply = self.reply.lock().unwrap().clone();
            Box::pin(async move {
                reply.ok_or_else(|| anyhow::anyhow!("摘要调用失败"))
            })
        }
    }

    fn estimator() -> Estimator {
        Estimator::new(1)
    }

    fn budgets(threshold: usize, retain: usize) -> Budgets {
        Budgets {
            threshold_tokens: threshold,
            retain_tokens: retain,
        }
    }

    fn cfg() -> CompactionConfig {
        CompactionConfig {
            chars_per_token: 1,
            max_tokens: 512,
            prune_threshold_chars: 100_000,
            prune_head_chars: 10,
            prune_tail_chars: 5,
            ..CompactionConfig::default()
        }
    }

    /// 构造 `n` 个「user + ai」回合，每段文本 `size` 字符。
    fn conversation(turns: usize, size: usize) -> Vec<Record> {
        let mut records = vec![Record::system("init", "", None, None)];
        for index in 0..turns {
            records.push(Record::user(format!("问 {index}{}", "x".repeat(size))));
            records.push(Record::ai("m", "h", "y".repeat(size), None));
        }
        records
    }

    #[tokio::test]
    async fn below_threshold_is_projected_verbatim() {
        let records = conversation(2, 10);
        let summarizer = FakeSummarizer::new("摘要");
        let plan = compact(
            &records,
            "m",
            &estimator(),
            &budgets(10_000, 5),
            &cfg(),
            None,
            &summarizer,
        )
        .await;
        assert!(plan.applied.is_none() && plan.entry.is_none());
        assert!(plan.warning.is_none());
        assert!(summarizer.calls().is_empty(), "未超阈值不应调用摘要");
        // `init` 簿记不投影，也没有稳定 `system` 前缀，故只剩 2 个回合的 4 条消息。
        assert_eq!(plan.messages.len(), 4);
    }

    #[tokio::test]
    async fn over_threshold_summarizes_and_shrinks() {
        let records = conversation(20, 50);
        let summarizer = FakeSummarizer::new("## Primary Request and Intent\n- 目标");
        let plan = compact(
            &records,
            "m",
            &estimator(),
            &budgets(400, 40),
            &cfg(),
            None,
            &summarizer,
        )
        .await;
        let applied = plan.applied.expect("应应用摘要");
        assert!(!applied.covered.is_empty());
        let entry = plan.entry.expect("应生成侧车条目");
        assert_eq!(entry.covers, applied.covered);
        assert!(entry.summary.contains("Primary Request"));
        // 摘要替换了被遮蔽区间：投影体积应显著小于原文。
        let before: usize = records.iter().map(|r| estimator().estimate_record(r)).sum();
        let after = estimator().estimate_messages(&plan.messages);
        assert!(after < before, "压缩后投影应更小：{after} vs {before}");
        assert_eq!(summarizer.calls().len(), 1);
    }

    #[tokio::test]
    async fn prune_alone_can_avoid_summarization() {
        // 原始投影超阈值，但工具正文修剪后已低于阈值 → 不应调用摘要（验收 5）。
        let mut records = vec![
            Record::system("init", "", None, None),
            Record::user("问"),
            Record::ai("m", "h", "", None),
            Record::tool_call(ToolOrigin::Ai, "c1", "big", None),
            Record::tool_result("c1", "x".repeat(4000), "", 0, false, false, None),
        ];
        records.push(Record::ai("m", "h", "收尾", None));
        let mut compact_cfg = cfg();
        compact_cfg.prune_threshold_chars = 100;
        compact_cfg.prune_head_chars = 10;
        compact_cfg.prune_tail_chars = 5;

        let summarizer = FakeSummarizer::new("不应被调用");
        let plan = compact(
            &records,
            "m",
            &estimator(),
            &budgets(100, 5),
            &compact_cfg,
            None,
            &summarizer,
        )
        .await;
        assert!(plan.applied.is_none() && plan.entry.is_none());
        assert!(summarizer.calls().is_empty(), "修剪已足够时不应调用摘要");
        assert!(
            plan.messages.iter().any(|message| {
                message.role == "tool"
                    && message
                        .content
                        .as_deref()
                        .is_some_and(|text| text.contains(prune::PRUNE_MARKER))
            }),
            "工具正文应被修剪并带标记：{:?}",
            plan.messages
        );
    }

    #[tokio::test]
    async fn summary_failure_degrades_with_warning() {
        let records = conversation(20, 50);
        let summarizer = FakeSummarizer::failing();
        let plan = compact(
            &records,
            "m",
            &estimator(),
            &budgets(400, 40),
            &cfg(),
            None,
            &summarizer,
        )
        .await;
        assert!(plan.applied.is_none() && plan.entry.is_none());
        assert!(
            plan.warning.as_deref().is_some_and(|text| text.contains("失败")),
            "失败应给出警告：{:?}",
            plan.warning
        );
    }

    #[tokio::test]
    async fn chained_summary_feeds_previous_text_not_shadowed_records() {
        let records = conversation(20, 50);
        let first = FakeSummarizer::new("第一版摘要");
        let plan = compact(
            &records,
            "m",
            &estimator(),
            &budgets(400, 40),
            &cfg(),
            None,
            &first,
        )
        .await;
        let entry = plan.entry.expect("首次应生成摘要");

        // 再追加一批内容，制造第二次压缩。
        let mut grown = records.clone();
        for index in 0..20 {
            grown.push(Record::user(format!("追加 {index}{}", "z".repeat(50))));
            grown.push(Record::ai("m", "h", "w".repeat(50), None));
        }
        let second = FakeSummarizer::new("第二版摘要");
        let plan = compact(
            &grown,
            "m",
            &estimator(),
            &budgets(400, 40),
            &cfg(),
            Some(&entry),
            &second,
        )
        .await;
        let calls = second.calls();
        assert_eq!(calls.len(), 1);
        let input = &calls[0];
        assert!(
            input.iter().any(|message| {
                message
                    .content
                    .as_deref()
                    .is_some_and(|text| text.contains("第一版摘要"))
            }),
            "第二次摘要输入应含前驱摘要文本"
        );
        assert!(
            !input.iter().any(|message| {
                message
                    .content
                    .as_deref()
                    .is_some_and(|text| text.contains("问 0"))
            }),
            "已被前驱摘要遮蔽的原文不得再次进入摘要输入"
        );
        let next = plan.entry.expect("第二次也应生成摘要");
        assert_eq!(next.previous.as_deref(), Some(entry.id.as_str()));
    }

    #[tokio::test]
    async fn invalid_previous_is_ignored() {
        let records = conversation(20, 50);
        let stale = cache::SummaryEntry {
            id: "stale".to_string(),
            model: "m".to_string(),
            previous: None,
            covers: vec!["不存在的记录".to_string()],
            summary: "旧摘要".to_string(),
            created_ts: 1,
            tokens: None,
        };
        let summarizer = FakeSummarizer::new("新摘要");
        let plan = compact(
            &records,
            "m",
            &estimator(),
            &budgets(400, 40),
            &cfg(),
            Some(&stale),
            &summarizer,
        )
        .await;
        // 失效前驱不得被引用。
        if let Some(entry) = &plan.entry {
            assert!(entry.previous.is_none(), "失效前驱不应成为链上环");
        }
        let input = &summarizer.calls()[0];
        assert!(
            !input.iter().any(|message| {
                message.content.as_deref() == Some("旧摘要")
            }),
            "失效前驱的摘要文本不应进入输入"
        );
    }

    #[tokio::test]
    async fn ai_tool_step_is_not_split() {
        // 一个助手步骤（ai + tool_call + tool_result）不得被切分。
        let mut records = vec![Record::system("init", "", None, None)];
        for index in 0..15 {
            records.push(Record::user(format!("问 {index}{}", "a".repeat(80))));
            records.push(Record::ai("m", "h", "", None));
            records.push(Record::tool_call(
                ToolOrigin::Ai,
                format!("c{index}"),
                "echo",
                None,
            ));
            records.push(Record::tool_result(
                format!("c{index}"),
                "b".repeat(80),
                "",
                0,
                false,
                false,
                None,
            ));
        }
        let summarizer = FakeSummarizer::new("摘要");
        let plan = compact(
            &records,
            "m",
            &estimator(),
            &budgets(300, 30),
            &cfg(),
            None,
            &summarizer,
        )
        .await;
        assert!(plan.applied.is_some());
        // 保留区不得以孤儿 tool 消息开头（切点必须落在步骤边界）。
        let checkpoint_index = plan
            .messages
            .iter()
            .position(is_checkpoint)
            .expect("应有摘要消息");
        if let Some(message) = plan.messages.get(checkpoint_index + 1) {
            assert_ne!(message.role, "tool", "保留区不得以孤儿工具消息开头");
        }
    }

    fn is_checkpoint(message: &ChatMessage) -> bool {
        message
            .content
            .as_deref()
            .is_some_and(|text| text.contains(summary::SUMMARY_OPEN_TAG))
    }
}
