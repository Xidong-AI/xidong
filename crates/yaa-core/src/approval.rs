//! 审批策略：把 `origin=ai` 的 shell 命令按配置规则裁决为放行 / 询问 / 拒绝。
//!
//! 规则是 glob（`*` 跨任意字符含空格、`?` 单字符），对整段命令做整串匹配。裁决遵循
//! 「最具体优先 + deny 短路」，并对多命令段做聚合。**这是防手滑与审计手段，不是安全
//! 边界**：命令拆分是 best-effort，包装器、重定向、变量展开等绕过依然可达。

use serde::{Deserialize, Serialize};

/// 单条规则的裁决动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

/// 一条审批规则。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub pattern: String,
    pub action: Action,
}

/// 一条命令（或整条命令聚合后）的裁决结果。
///
/// `matched_rule` 为命中规则的原文本；无规则命中时为 `None`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub action: Action,
    pub matched_rule: Option<String>,
}

/// 评估整条命令，按命令段拆分后聚合（D42）。
pub fn evaluate(rules: &[Rule], command: &str) -> Verdict {
    let Some(segments) = split_segments(command) else {
        // 引号不配平即无法确定资源边界，按 ask 处理，绝不静默放行。
        return ask(None);
    };
    let mut verdict = Verdict {
        action: Action::Allow,
        matched_rule: None,
    };
    for segment in segments {
        let segment_verdict = evaluate_segment(rules, &segment);
        match segment_verdict.action {
            Action::Deny => return segment_verdict,
            Action::Ask if verdict.action == Action::Allow => verdict = segment_verdict,
            _ => {}
        }
    }
    verdict
}

/// 单命令段裁决：deny 短路优先，否则最具体者胜、同分取后者，无匹配为 allow。
fn evaluate_segment(rules: &[Rule], segment: &str) -> Verdict {
    let command = strip_env_prefix(segment);
    if command.is_empty() {
        // 只有环境赋值却无实际命令，同样无法确定意图。
        return ask(None);
    }
    for rule in rules {
        if rule.action == Action::Deny && glob_match(&rule.pattern, command) {
            return Verdict {
                action: Action::Deny,
                matched_rule: Some(rule.pattern.clone()),
            };
        }
    }
    let mut best: Option<(&Rule, usize)> = None;
    for rule in rules {
        if glob_match(&rule.pattern, command) {
            let specificity = specificity(&rule.pattern);
            // 同分取后者：`>=` 让靠后的规则覆盖靠前者。
            if best.is_none_or(|(_, current)| specificity >= current) {
                best = Some((rule, specificity));
            }
        }
    }
    match best {
        Some((rule, _)) => Verdict {
            action: rule.action,
            matched_rule: Some(rule.pattern.clone()),
        },
        None => Verdict {
            action: Action::Allow,
            matched_rule: None,
        },
    }
}

fn ask(matched_rule: Option<String>) -> Verdict {
    Verdict {
        action: Action::Ask,
        matched_rule,
    }
}

/// 首个通配符之前的字面字符数；无通配符则为整串长度。
fn specificity(pattern: &str) -> usize {
    match pattern.char_indices().find(|(_, c)| *c == '*' || *c == '?') {
        Some((index, _)) => pattern[..index].chars().count(),
        None => pattern.chars().count(),
    }
}

/// glob 匹配，整串锚定：`*` 跨任意字符（含空格），`?` 匹配单个字符。
///
/// 标准回溯实现：记录最近一个 `*` 的位置，失配时回退多吞一个字符。
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((star_pi, star_ti)) = star {
            pi = star_pi + 1;
            ti = star_ti + 1;
            star = Some((star_pi, star_ti + 1));
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// 按 shell 分隔符朴素拆段，识别引号以免在引号内误切。
///
/// 引号不配平时返回 `None`（交调用方按 ask 处理）。单个 `&` 不是分隔符。
fn split_segments(command: &str) -> Option<Vec<String>> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let (mut in_single, mut in_double) = (false, false);
    while let Some(c) = chars.next() {
        if in_single {
            current.push(c);
            if c == '\'' {
                in_single = false;
            }
            continue;
        }
        if in_double {
            current.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            } else if c == '"' {
                in_double = false;
            }
            continue;
        }
        match c {
            '\'' => {
                in_single = true;
                current.push(c);
            }
            '"' => {
                in_double = true;
                current.push(c);
            }
            '\\' => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ';' | '\n' | '|' => {
                // `||` 拆两次等价于拆一次，`|` 亦然，故无需区分。
                if c == '|' && chars.peek() == Some(&'|') {
                    chars.next();
                }
                push_segment(&mut segments, &mut current);
            }
            '&' => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                    push_segment(&mut segments, &mut current);
                } else {
                    current.push(c);
                }
            }
            _ => current.push(c),
        }
    }
    if in_single || in_double {
        return None;
    }
    push_segment(&mut segments, &mut current);
    Some(segments)
}

fn push_segment(segments: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
    current.clear();
}

/// 循环剥离段首的 `VAR=val` 环境赋值，返回剩余命令（已 trim）。
fn strip_env_prefix(segment: &str) -> &str {
    let mut rest = segment.trim();
    loop {
        let end = rest
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map(|(index, _)| index)
            .unwrap_or(rest.len());
        let token = &rest[..end];
        match token.split_once('=') {
            Some((name, _)) if is_env_name(name) => rest = rest[end..].trim_start(),
            _ => break,
        }
    }
    rest
}

/// 环境变量名：`[A-Za-z_][A-Za-z0-9_]*`。
fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, action: Action) -> Rule {
        Rule {
            pattern: pattern.into(),
            action,
        }
    }

    fn action_of(rules: &[Rule], command: &str) -> Action {
        evaluate(rules, command).action
    }

    #[test]
    fn deny_short_circuits_catch_all() {
        // catch-all 的 ask 在后，具体 deny 在前，仍必须 deny（D41 社区吐点）。
        let rules = [rule("*", Action::Ask), rule("rm *", Action::Deny)];
        let verdict = evaluate(&rules, "rm -rf /");
        assert_eq!(verdict.action, Action::Deny);
        assert_eq!(verdict.matched_rule.as_deref(), Some("rm *"));
    }

    #[test]
    fn most_specific_wins() {
        let rules = [rule("git *", Action::Ask), rule("git push *", Action::Deny)];
        assert_eq!(action_of(&rules, "git push origin"), Action::Deny);
    }

    #[test]
    fn broader_rule_applies_when_specific_absent() {
        let rules = [rule("git *", Action::Ask)];
        assert_eq!(action_of(&rules, "git status"), Action::Ask);
        assert_eq!(action_of(&rules, "git push origin"), Action::Ask);
    }

    #[test]
    fn specificity_tie_takes_last() {
        // `git *` 与 `git ?*` 的具体度同为 4（"git "），后者胜出。
        let last_allow = [rule("git *", Action::Ask), rule("git ?*", Action::Allow)];
        assert_eq!(action_of(&last_allow, "git status"), Action::Allow);
        let last_ask = [rule("git ?*", Action::Allow), rule("git *", Action::Ask)];
        assert_eq!(action_of(&last_ask, "git status"), Action::Ask);
    }

    #[test]
    fn no_match_defaults_to_allow() {
        let verdict = evaluate(&[], "anything at all");
        assert_eq!(verdict.action, Action::Allow);
        assert_eq!(verdict.matched_rule, None);
    }

    #[test]
    fn exact_pattern_is_anchored() {
        let rules = [rule("git status", Action::Ask)];
        assert_eq!(action_of(&rules, "git status"), Action::Ask);
        assert_eq!(action_of(&rules, "git statusx"), Action::Allow);
        assert_eq!(action_of(&rules, "git status --short"), Action::Allow);
    }

    #[test]
    fn question_mark_matches_single_char() {
        let rules = [rule("git ?tatus", Action::Ask)];
        assert_eq!(action_of(&rules, "git status"), Action::Ask);
        assert_eq!(action_of(&rules, "git sttatus"), Action::Allow);
    }

    #[test]
    fn star_matches_spaces() {
        let rules = [rule("git *", Action::Ask)];
        assert_eq!(action_of(&rules, r#"git commit -m "a b""#), Action::Ask);
    }

    #[test]
    fn matched_rule_carries_pattern_text() {
        let rules = [rule("curl *", Action::Ask)];
        let verdict = evaluate(&rules, "curl x");
        assert_eq!(verdict.action, Action::Ask);
        assert_eq!(verdict.matched_rule.as_deref(), Some("curl *"));
    }

    #[test]
    fn and_chain_aggregates_to_deny() {
        let rules = [rule("rm *", Action::Deny)];
        assert_eq!(action_of(&rules, "echo a && rm -rf /"), Action::Deny);
    }

    #[test]
    fn pipe_aggregates_to_ask() {
        let rules = [rule("curl *", Action::Ask)];
        assert_eq!(action_of(&rules, "ls | curl x"), Action::Ask);
    }

    #[test]
    fn or_chain_is_split() {
        let rules = [rule("curl *", Action::Ask)];
        assert_eq!(action_of(&rules, "false || curl x"), Action::Ask);
    }

    #[test]
    fn newline_is_split() {
        let rules = [rule("curl *", Action::Ask)];
        assert_eq!(action_of(&rules, "echo a\ncurl x"), Action::Ask);
    }

    #[test]
    fn semicolon_is_split() {
        let rules = [rule("curl *", Action::Ask)];
        assert_eq!(action_of(&rules, "echo a; curl x"), Action::Ask);
    }

    #[test]
    fn env_prefix_is_stripped() {
        let rules = [rule("git *", Action::Ask)];
        assert_eq!(action_of(&rules, "VAR=x git status"), Action::Ask);
        assert_eq!(action_of(&rules, "A=1 B=2 git status"), Action::Ask);
    }

    #[test]
    fn env_only_segment_is_ask() {
        // 剥离后为空，无法确定意图，绝不静默放行。
        assert_eq!(action_of(&[], "FOO=bar"), Action::Ask);
    }

    #[test]
    fn unbalanced_quotes_are_ask() {
        assert_eq!(action_of(&[], r#"echo "unterminated"#), Action::Ask);
        assert_eq!(action_of(&[], "echo 'oops"), Action::Ask);
    }

    #[test]
    fn separators_inside_quotes_do_not_split() {
        assert_eq!(
            action_of(&[rule("curl *", Action::Ask)], r#"echo "a;b""#),
            Action::Allow
        );
    }

    #[test]
    fn empty_command_is_allow() {
        let rules = [rule("*", Action::Deny)];
        assert_eq!(action_of(&rules, ""), Action::Allow);
        assert_eq!(action_of(&rules, "   \n  "), Action::Allow);
        assert_eq!(action_of(&rules, ";;"), Action::Allow);
    }
}
