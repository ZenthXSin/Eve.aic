//! 前缀规则命令策略，规则语义参考 openai/codex 的 execpolicy（Apache-2.0）：
//! 每条规则按顺序匹配命令的前几个参数，某一位可以列出几个可选值；结论为放行、需确认或禁止，
//! 多条规则命中时取最严格的一项。规则自带“应匹配 / 不应匹配”的例子，加载时逐一核对，
//! 例子与规则不符就拒绝加载，避免写错的规则悄悄失效。
//!
//! 与 Codex 不同，规则文件用 JSON 而不是 Starlark；`sh -c` / `bash -lc` 等脚本按 `;`、`&&`、`||`、
//! `|` 拆成几段分别判断。脚本解析是尽力而为，不是安全边界：变量展开、命令替换等无法静态确定的部分不做判断。
use eve_developer_api::{CommandDecision, CommandEvaluation, CommandPolicy, CommandRuleMatch};
use serde::Deserialize;
use std::path::Path;

const DEFAULT_RULES: &str = include_str!("default-rules.json");
const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "dash", "ash"];

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum TokenSpec {
    One(String),
    Any(Vec<String>),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum Example {
    Tokens(Vec<String>),
    Line(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleSpec {
    pattern: Vec<TokenSpec>,
    #[serde(default = "allow")]
    decision: CommandDecision,
    #[serde(default)]
    justification: Option<String>,
    #[serde(default, rename = "match")]
    should_match: Vec<Example>,
    #[serde(default)]
    not_match: Vec<Example>,
}
fn allow() -> CommandDecision {
    CommandDecision::Allow
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    rules: Vec<RuleSpec>,
}

#[derive(Clone, Debug)]
struct Rule {
    pattern: Vec<Vec<String>>,
    decision: CommandDecision,
    justification: Option<String>,
}
impl Rule {
    fn matches(&self, command: &[String]) -> Option<Vec<String>> {
        if command.len() < self.pattern.len() {
            return None;
        }
        self.pattern
            .iter()
            .zip(command)
            .all(|(alternatives, token)| alternatives.iter().any(|a| a == token))
            .then(|| command[..self.pattern.len()].to_vec())
    }
}

#[derive(Clone, Debug, Default)]
pub struct PrefixRulePolicy {
    rules: Vec<Rule>,
}

fn tokens(example: &Example) -> Result<Vec<String>, String> {
    match example {
        Example::Tokens(tokens) => Ok(tokens.clone()),
        Example::Line(line) => shlex::split(line).ok_or_else(|| format!("例子无法拆分：{line}")),
    }
}

impl PrefixRulePolicy {
    /// 解析 JSON 规则，并核对每条规则自带的例子。
    pub fn from_json(text: &str) -> Result<Self, String> {
        let file: RulesFile =
            serde_json::from_str(text).map_err(|e| format!("命令策略规则无法解析：{e}"))?;
        let mut rules = Vec::new();
        for (index, spec) in file.rules.into_iter().enumerate() {
            let pattern: Vec<Vec<String>> = spec
                .pattern
                .iter()
                .map(|token| match token {
                    TokenSpec::One(value) => vec![value.clone()],
                    TokenSpec::Any(values) => values.clone(),
                })
                .collect();
            if pattern.is_empty()
                || pattern.iter().any(|alternatives| {
                    alternatives.is_empty() || alternatives.iter().any(String::is_empty)
                })
            {
                return Err(format!("第 {} 条规则的 pattern 无效", index + 1));
            }
            let rule = Rule {
                pattern,
                decision: spec.decision,
                justification: spec.justification.filter(|j| !j.trim().is_empty()),
            };
            for example in &spec.should_match {
                let command = tokens(example)?;
                if rule.matches(&command).is_none() {
                    return Err(format!(
                        "第 {} 条规则应匹配的例子没有匹配：{}",
                        index + 1,
                        command.join(" ")
                    ));
                }
            }
            for example in &spec.not_match {
                let command = tokens(example)?;
                if rule.matches(&command).is_some() {
                    return Err(format!(
                        "第 {} 条规则不应匹配的例子匹配了：{}",
                        index + 1,
                        command.join(" ")
                    ));
                }
            }
            rules.push(rule);
        }
        Ok(Self { rules })
    }

    /// 内置规则：拦下停机、格式化、递归删除系统目录，要求确认改写远端历史、丢弃修改等操作。
    pub fn builtin() -> Self {
        Self::from_json(DEFAULT_RULES).expect("内置命令策略规则必须有效")
    }

    /// 追加另一组规则；判断时所有规则一起参与，取最严格的结论。
    pub fn merged(mut self, other: Self) -> Self {
        self.rules.extend(other.rules);
        self
    }

    fn evaluate_one(&self, command: &[String], matches: &mut Vec<CommandRuleMatch>) {
        let Some(first) = command.first() else {
            return;
        };
        // 绝对路径的程序同时按文件名匹配，`/usr/bin/git` 与 `git` 用同一组规则。
        let base = Path::new(first)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| *name != first.as_str());
        let mut candidates = vec![command.to_vec()];
        if let Some(base) = base {
            let mut renamed = command.to_vec();
            renamed[0] = base.to_string();
            candidates.push(renamed);
        }
        for rule in &self.rules {
            if let Some(prefix) = candidates.iter().find_map(|c| rule.matches(c)) {
                matches.push(CommandRuleMatch {
                    matched_prefix: prefix,
                    decision: rule.decision,
                    justification: rule.justification.clone(),
                });
            }
        }
    }
}

/// `sh -c '…'` 一类调用里的脚本，按分隔符拆成几段命令；不是这种形式时返回 None。
fn shell_script(program: &str, args: &[String]) -> Option<Vec<Vec<String>>> {
    let name = Path::new(program).file_name()?.to_str()?;
    if !SHELLS.contains(&name) {
        return None;
    }
    let position = args
        .iter()
        .position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'))?;
    let script = args.get(position + 1)?;
    let words = shlex::split(script)?;
    // 引号里的 `;`、`|`、`&` 也会被当作分隔（例如 `echo 'a; reboot'`），只会多判断，不会漏判。
    let mut commands = vec![Vec::new()];
    for word in words {
        let mut piece = String::new();
        for c in word.chars() {
            if matches!(c, ';' | '|' | '&') {
                if !piece.is_empty() {
                    commands
                        .last_mut()
                        .expect("至少一段")
                        .push(std::mem::take(&mut piece));
                }
                commands.push(Vec::new());
            } else {
                piece.push(c);
            }
        }
        if !piece.is_empty() {
            commands.last_mut().expect("至少一段").push(piece);
        }
    }
    commands.retain(|command| !command.is_empty());
    Some(commands)
}

impl CommandPolicy for PrefixRulePolicy {
    fn evaluate(&self, program: &str, args: &[String]) -> CommandEvaluation {
        let mut matches = Vec::new();
        let mut whole = vec![program.to_string()];
        whole.extend(args.iter().cloned());
        self.evaluate_one(&whole, &mut matches);
        for command in shell_script(program, args).unwrap_or_default() {
            self.evaluate_one(&command, &mut matches);
        }
        CommandEvaluation {
            decision: matches.iter().map(|m| m.decision).max(),
            matches,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide(policy: &PrefixRulePolicy, line: &str) -> Option<CommandDecision> {
        let words = shlex::split(line).unwrap();
        policy.evaluate(&words[0], &words[1..]).decision
    }

    #[test]
    fn builtin_rules_load_and_take_the_strictest_decision() {
        let policy = PrefixRulePolicy::builtin();
        use CommandDecision::*;
        assert_eq!(
            decide(&policy, "rm -rf /"),
            Some(Forbidden),
            "同时命中禁止与确认，取禁止"
        );
        assert_eq!(decide(&policy, "rm -rf build"), Some(Prompt));
        assert_eq!(
            decide(&policy, "/usr/bin/git push --force"),
            Some(Prompt),
            "绝对路径按文件名匹配"
        );
        assert_eq!(decide(&policy, "git push origin main"), None);
        assert_eq!(decide(&policy, "cargo test"), None);
        let evaluation = policy.evaluate("shutdown", &["-h".into(), "now".into()]);
        assert!(
            evaluation.reason().contains("停机"),
            "{}",
            evaluation.reason()
        );
    }

    #[test]
    fn shell_scripts_are_split_into_commands() {
        let policy = PrefixRulePolicy::builtin();
        use CommandDecision::*;
        assert_eq!(
            decide(&policy, "sh -c 'cd repo && git reset --hard'"),
            Some(Prompt)
        );
        assert_eq!(
            decide(&policy, "bash -lc 'echo hi; reboot'"),
            Some(Forbidden)
        );
        assert_eq!(decide(&policy, "bash -c 'ls | grep x'"), None);
        assert_eq!(decide(&policy, "sh -c 'echo a;rm -rf ~'"), Some(Forbidden));
    }

    #[test]
    fn examples_are_checked_when_rules_load() {
        let wrong_match =
            r#"{"rules":[{"pattern":["git","push"],"decision":"prompt","match":["git pull"]}]}"#;
        assert!(
            PrefixRulePolicy::from_json(wrong_match)
                .unwrap_err()
                .contains("应匹配")
        );
        let wrong_not = r#"{"rules":[{"pattern":["git"],"not_match":["git status"]}]}"#;
        assert!(
            PrefixRulePolicy::from_json(wrong_not)
                .unwrap_err()
                .contains("不应匹配")
        );
        assert!(PrefixRulePolicy::from_json(r#"{"rules":[{"pattern":[]}]}"#).is_err());
        assert!(PrefixRulePolicy::from_json(r#"{"rules":[{"pattern":["x"],"oops":1}]}"#).is_err());

        let extra = PrefixRulePolicy::from_json(
            r#"{"rules":[{"pattern":["npm","publish"],"decision":"forbidden","justification":"不发布包","match":["npm publish --tag x"]}]}"#,
        )
        .unwrap();
        let policy = PrefixRulePolicy::builtin().merged(extra);
        assert_eq!(
            decide(&policy, "npm publish"),
            Some(CommandDecision::Forbidden)
        );
        assert_eq!(decide(&policy, "npm install"), None);
    }
}
