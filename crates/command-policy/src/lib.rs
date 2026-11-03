//! 前缀规则命令策略，规则语义参考 openai/codex 的 execpolicy（Apache-2.0）：
//! 每条规则按顺序匹配命令的前几个参数，某一位可以列出几个可选值；结论为放行、需审核或禁止，
//! 多条规则命中时取最严格的一项。规则自带“应匹配 / 不应匹配”的例子，加载时逐一核对，
//! 例子与规则不符就拒绝加载，避免写错的规则悄悄失效。
//!
//! 与 Codex 不同，规则文件用 JSON 而不是 Starlark。只有固定系统路径下、完整参数通过检查的少量
//! 只读命令可以免审核；未知程序、解释器、脚本及包装器始终至少需审核，追加 Allow 规则不能放宽。
//! shell 文本拆分只用于发现更严格的禁令，不能证明脚本安全，也不能替代独立行动审核。
use eve_developer_api::{CommandDecision, CommandEvaluation, CommandPolicy, CommandRuleMatch};
use serde::Deserialize;
use std::path::Path;

const DEFAULT_RULES: &str = include_str!("default-rules.json");
const SHELLS: [&str; 9] = [
    "sh", "bash", "zsh", "dash", "ash", "ksh", "mksh", "fish", "csh",
];

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

    /// 内置规则：拦下停机、格式化、递归删除系统目录，其余非明确只读的命令自动送审。
    pub fn builtin() -> Self {
        Self::from_json(DEFAULT_RULES).expect("内置命令策略规则必须有效")
    }

    /// 追加另一组规则；判断时所有规则一起参与，取最严格的结论。
    pub fn merged(mut self, other: Self) -> Self {
        self.rules.extend(other.rules);
        self
    }

    /// 对已知包装器和参数位置做保守归一化；未知动态语法不静默放行。
    fn evaluate_semantics(
        &self,
        command: &[String],
        matches: &mut Vec<CommandRuleMatch>,
        depth: usize,
    ) {
        if command.is_empty() {
            return;
        }
        self.evaluate_one(command, matches);
        let base = Path::new(&command[0])
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&command[0]);
        let args = &command[1..];
        let readonly = readonly_system_command(&command[0], args);
        matches.push(CommandRuleMatch {
            matched_prefix: vec![command[0].clone()],
            decision: if readonly { CommandDecision::Allow } else { CommandDecision::Prompt },
            justification: Some(if readonly {
                "固定系统路径与完整参数符合只读规则"
            } else {
                "不能证明完整命令在安全只读范围内；未知程序、脚本、解释器和包装器必须独立审核，前缀放行不能代替审核"
            }.into()),
        });
        if base == "git" {
            let mut position = 0;
            while let Some(option) = args.get(position).filter(|a| a.starts_with('-')) {
                position += if matches!(
                    option.as_str(),
                    "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
                ) {
                    2
                } else {
                    1
                };
            }
            if let Some(operation) = args.get(position) {
                let rest = &args[position + 1..];
                let dangerous = match operation.as_str() {
                    "push" => rest.iter().any(|a| {
                        a == "-f"
                            || a == "-d"
                            || a.starts_with("--force")
                            || a == "--mirror"
                            || a == "--delete"
                    }),
                    "reset" => rest.iter().any(|a| a == "--hard"),
                    "clean" => rest
                        .iter()
                        .any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('f')),
                    "branch" => rest
                        .iter()
                        .any(|a| a == "-D" || a == "-d" || a == "--delete"),
                    "checkout" | "restore" => rest
                        .iter()
                        .any(|a| a == "." || a == "--" || a == "--force" || a == "-f"),
                    _ => false,
                };
                if dangerous {
                    require_review(
                        matches,
                        base,
                        "Git 参数会删除或丢弃工作、改写远端历史，执行前审核完整命令",
                    );
                }
            }
        }
        if base == "rm"
            && args.iter().any(|a| {
                a == "--recursive"
                    || (a.starts_with('-')
                        && !a.starts_with("--")
                        && (a.contains('r') || a.contains('R')))
            })
        {
            require_review(matches, base, "递归删除，必须先审核完整目标与影响范围");
            // 不能让重排选项、-- 或尾斜杠把已禁止的系统目录删除降级为可审核动作。
            if args
                .iter()
                .any(|argument| protected_delete_target(argument))
            {
                matches.push(CommandRuleMatch {
                    matched_prefix: vec![base.into()],
                    decision: CommandDecision::Forbidden,
                    justification: Some("递归删除系统或家目录，参数重排不能绕过禁令".into()),
                });
            }
        }
        if matches!(
            base,
            "env" | "sudo" | "command" | "exec" | "nohup" | "timeout" | "time"
        ) {
            if depth >= 8 {
                require_review(matches, base, "命令包装层级过深，执行前审核");
                return;
            }
            require_review(
                matches,
                base,
                "包装命令可能改变执行身份、环境或目标，执行前审核",
            );
            if let Some(child) = wrapped_command(base, args) {
                self.evaluate_semantics(child, matches, depth + 1);
            }
        }
        if SHELLS.contains(&base) && args.iter().any(|a| a.starts_with('-') && a.contains('c')) {
            if depth >= 8 {
                matches.push(CommandRuleMatch {
                    matched_prefix: vec![base.into()],
                    decision: CommandDecision::Prompt,
                    justification: Some("shell嵌套层级过深，执行前审核完整命令".into()),
                });
                return;
            }
            let commands = shell_script(&command[0], args);
            if commands.is_none()
                || args
                    .iter()
                    .any(|a| a.contains(['$', '`', '\n', '(', ')', '{', '}', '<', '>']))
            {
                matches.push(CommandRuleMatch {
                    matched_prefix: vec![base.into()],
                    decision: CommandDecision::Prompt,
                    justification: Some(
                        "脚本包含动态展开、重定向或无法静态解析的语法，执行前必须审核完整命令"
                            .into(),
                    ),
                });
            }
            if let Some(commands) = commands {
                for child in commands {
                    self.evaluate_semantics(&child, matches, depth + 1);
                }
            }
        }
    }

    fn evaluate_one(&self, command: &[String], matches: &mut Vec<CommandRuleMatch>) {
        let Some(first) = command.first() else {
            return;
        };
        // 文件名归一化只提供前缀规则候选；免审核仍要求完整系统路径与只读参数。
        // 因此 /tmp/ls、./git 等工作区程序不能借系统工具的 Allow 规则绕过审核。
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

fn require_review(matches: &mut Vec<CommandRuleMatch>, base: &str, reason: &str) {
    matches.push(CommandRuleMatch {
        matched_prefix: vec![base.into()],
        decision: CommandDecision::Prompt,
        justification: Some(reason.into()),
    });
}

fn protected_delete_target(value: &str) -> bool {
    if !value.starts_with('/') {
        return matches!(value.trim_end_matches('/'), "~" | "$HOME");
    }
    let mut parts = Vec::new();
    for part in Path::new(value).components() {
        match part {
            std::path::Component::Normal(name) => parts.push(name),
            std::path::Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    parts.is_empty()
        || (parts.len() == 1
            && matches!(
                parts[0].to_str(),
                Some("*" | "home" | "root" | "etc" | "usr" | "var")
            ))
}

/// 纯 argv 策略不知道工作目录和文件授权，也不解析 PATH。因此不把工作区里的同名程序、
/// 文件内容读取或受仓库配置影响的 Git 操作视为免审核命令。固定系统目录由宿主信任边界保护。
fn readonly_system_command(program: &str, args: &[String]) -> bool {
    let Some(base) = program
        .strip_prefix("/usr/bin/")
        .or_else(|| program.strip_prefix("/bin/"))
    else {
        return false;
    };
    if base.contains('/') || args.iter().any(|arg| arg.contains('\0')) {
        return false;
    }
    match base {
        "true" | "false" => args.is_empty(),
        // 直接启动系统 echo，参数中的 $()、;、> 等是普通字符，不是 shell 语法。
        "echo" => true,
        "pwd" => args.iter().all(|arg| {
            matches!(
                arg.as_str(),
                "-L" | "-P" | "--logical" | "--physical" | "--help" | "--version"
            )
        }),
        "whoami" => {
            args.is_empty()
                || (args.len() == 1 && matches!(args[0].as_str(), "--help" | "--version"))
        }
        "uname" => args.iter().all(|arg| {
            matches!(
                arg.as_str(),
                "--all"
                    | "--kernel-name"
                    | "--nodename"
                    | "--kernel-release"
                    | "--kernel-version"
                    | "--machine"
                    | "--processor"
                    | "--hardware-platform"
                    | "--operating-system"
                    | "--help"
                    | "--version"
            ) || short_flags(arg, "asnrvmpio")
        }),
        "id" => args.iter().all(|arg| {
            matches!(
                arg.as_str(),
                "--user"
                    | "--group"
                    | "--groups"
                    | "--name"
                    | "--real"
                    | "--zero"
                    | "--help"
                    | "--version"
            ) || short_flags(arg, "ugGnrza")
        }),
        "ls" => {
            let mut positional = false;
            args.iter().all(|arg| {
                if arg == "--" && !positional {
                    positional = true;
                    true
                } else if arg == "." {
                    true
                } else if positional {
                    false
                } else {
                    matches!(
                        arg.as_str(),
                        "--all"
                            | "--almost-all"
                            | "--directory"
                            | "--human-readable"
                            | "--numeric-uid-gid"
                            | "--reverse"
                            | "--size"
                            | "--classify"
                            | "--help"
                            | "--version"
                            | "--color=never"
                            | "--color=auto"
                            | "--color=always"
                    ) || short_flags(arg, "aAldhniSrstUFp1")
                }
            })
        }
        _ => false,
    }
}

fn short_flags(value: &str, allowed: &str) -> bool {
    value.starts_with('-')
        && value.len() > 1
        && value[1..].chars().all(|flag| allowed.contains(flag))
}

/// 只提取有确定参数语义的包装命令，避免把 sudo 用户名、timeout 时长或 env 变量名
/// 当成可执行文件而漏掉下层禁令。未知参数仍需审核，不猜测它是否消费下一个参数。
fn wrapped_command<'a>(program: &str, args: &'a [String]) -> Option<&'a [String]> {
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if argument == "--" {
            index += 1;
            break;
        }
        if matches!(program, "env" | "sudo") && environment_assignment(argument) {
            index += 1;
            continue;
        }
        if !argument.starts_with('-') {
            break;
        }
        let value_options: &[&str] = match program {
            "env" => &["-u", "--unset", "-C", "--chdir", "--argv0"],
            "sudo" => &[
                "-u",
                "--user",
                "-g",
                "--group",
                "-h",
                "--host",
                "-p",
                "--prompt",
                "-C",
                "--close-from",
                "-R",
                "--chroot",
                "-D",
                "--chdir",
                "-T",
                "--command-timeout",
                "-r",
                "--role",
                "-t",
                "--type",
            ],
            "timeout" => &["-s", "--signal", "-k", "--kill-after"],
            "exec" => &["-a"],
            "time" => &["-f", "--format", "-o", "--output"],
            _ => &[],
        };
        let flag_options: &[&str] = match program {
            "env" => &[
                "-",
                "-i",
                "--ignore-environment",
                "-0",
                "--null",
                "-v",
                "--debug",
            ],
            "sudo" => &[
                "-A",
                "--askpass",
                "-b",
                "--background",
                "-E",
                "--preserve-env",
                "-H",
                "--set-home",
                "-i",
                "--login",
                "-K",
                "--remove-timestamp",
                "-k",
                "--reset-timestamp",
                "-n",
                "--non-interactive",
                "-P",
                "--preserve-groups",
                "-S",
                "--stdin",
                "-s",
                "--shell",
            ],
            "timeout" => &["--preserve-status", "--foreground", "-v", "--verbose"],
            "command" => &["-p"],
            "exec" => &["-c", "-l"],
            "time" => &[
                "-a",
                "--append",
                "-p",
                "--portability",
                "-q",
                "--quiet",
                "-v",
                "--verbose",
            ],
            _ => &[],
        };
        if value_options.contains(&argument.as_str()) {
            args.get(index + 1)?;
            index += 2;
        } else if flag_options.contains(&argument.as_str())
            || value_options.iter().any(|option| {
                if option.starts_with("--") {
                    argument.starts_with(&format!("{option}="))
                } else {
                    argument.starts_with(option) && argument.len() > option.len()
                }
            })
        {
            index += 1;
        } else {
            return None;
        }
    }
    if matches!(program, "env" | "sudo") {
        // env 即使在 -- 后仍接受 NAME=VALUE；sudo 同样允许在命令前设置环境变量。
        while args
            .get(index)
            .is_some_and(|argument| environment_assignment(argument))
        {
            index += 1;
        }
    }
    if program == "timeout" {
        // 时长是 timeout 自己的第一个位置参数。
        args.get(index)?;
        index += 1;
    }
    args.get(index..).filter(|child| !child.is_empty())
}

fn environment_assignment(argument: &str) -> bool {
    argument.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().enumerate().all(|(index, character)| {
                character == '_'
                    || character.is_ascii_alphabetic()
                    || (index > 0 && character.is_ascii_digit())
            })
    })
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
    // 引号里的分隔符也可能误报，动态 shell 语法也可能漏掉具体禁令；本函数仅加强结论，
    // 脚本本身的 Prompt 下限独立存在，不能将这里解析出的只读片段用于放行。
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
        self.evaluate_semantics(&whole, &mut matches, 0);
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
        assert_eq!(decide(&policy, "git push origin main"), Some(Prompt));
        assert_eq!(decide(&policy, "cargo test"), Some(Prompt));
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
        assert_eq!(decide(&policy, "bash -c 'ls | grep x'"), Some(Prompt));
        assert_eq!(decide(&policy, "sh -c 'echo a;rm -rf ~'"), Some(Forbidden));
    }

    #[test]
    fn dynamic_scripts_wrappers_and_reordered_destructive_options_require_review() {
        let policy = PrefixRulePolicy::builtin();
        for command in [
            "git -C repo push origin main --force",
            "git -c color.ui=never reset HEAD --hard",
            "rm --force -R build",
            "sh -c '$(cat command)'",
            "bash -lc 'echo x > file'",
            "env X=y git push origin --force",
            "timeout 5 git reset --hard",
        ] {
            assert!(
                decide(&policy, command) >= Some(CommandDecision::Prompt),
                "{command}"
            );
        }
        assert_eq!(
            decide(&policy, "sudo reboot"),
            Some(CommandDecision::Forbidden)
        );
        assert_eq!(
            decide(&policy, "git -C repo status"),
            Some(CommandDecision::Prompt)
        );
    }

    #[test]
    fn nested_shells_preserve_forbidden_and_dynamic_review_decisions() {
        let policy = PrefixRulePolicy::builtin();
        assert_eq!(
            policy
                .evaluate("sh", &["-c".into(), "sh -c reboot".into()])
                .decision,
            Some(CommandDecision::Forbidden)
        );
        assert_eq!(
            policy
                .evaluate("bash", &["-lc".into(), "sh -c 'git reset --hard'".into()])
                .decision,
            Some(CommandDecision::Prompt)
        );
        assert_eq!(
            policy
                .evaluate("sh", &["-c".into(), "sh -c 'echo $HOME'".into()])
                .decision,
            Some(CommandDecision::Prompt)
        );
        let mut nested = "reboot".to_owned();
        for _ in 0..12 {
            nested = shlex::try_join(["sh", "-c", &nested]).unwrap();
        }
        assert!(
            policy.evaluate("sh", &["-c".into(), nested]).decision >= Some(CommandDecision::Prompt)
        );
        assert_eq!(
            policy
                .evaluate("sh", &["-c".into(), "sh -c 'echo hello'".into()])
                .decision,
            Some(CommandDecision::Prompt)
        );
    }

    #[test]
    fn unknown_programs_interpreters_and_scripts_never_default_to_allow() {
        for policy in [PrefixRulePolicy::default(), PrefixRulePolicy::builtin()] {
            for command in [
                "unknown-deploy --production",
                "./safe-looking-program",
                "/tmp/ls",
                "sh",
                "sh script.sh",
                "bash --noprofile script.sh",
                "bash -s",
                "fish -c '/bin/echo hello'",
                "python3 -c 'import os; os.remove(\"data\")'",
                "python3 script.py",
                "python -m package",
                "node -e 'require(\"fs\").unlinkSync(\"data\")'",
                "perl -e 'unlink \"data\"'",
                "ruby script.rb",
                "php script.php",
                "lua script.lua",
                "cmd.exe /c del data",
                "powershell -Command Remove-Item data",
                "pwsh -File script.ps1",
                "busybox sh script.sh",
                "sqlite3 db 'DELETE FROM users'",
            ] {
                assert_eq!(
                    decide(&policy, command),
                    Some(CommandDecision::Prompt),
                    "{command}"
                );
            }
        }
    }

    #[test]
    fn compound_shell_text_never_proves_a_script_safe() {
        let policy = PrefixRulePolicy::builtin();
        for script in [
            "/bin/echo hello",
            "/bin/ls . && /bin/pwd",
            "/bin/ls . | /bin/echo hello",
            "/bin/pwd || /bin/echo fallback",
            "/bin/echo `cat action`",
            "/bin/echo $(cat action)",
            "/bin/echo > output",
            "/bin/echo <(cat input)",
            "while true; do /bin/echo hello; done",
            "/bin/echo hello\n./unknown",
        ] {
            assert_eq!(
                policy
                    .evaluate("/bin/sh", &["-c".into(), script.into()])
                    .decision,
                Some(CommandDecision::Prompt),
                "{script}"
            );
        }
        assert_eq!(
            policy
                .evaluate("/bin/echo", &["$(reboot); > output".into()])
                .decision,
            Some(CommandDecision::Allow),
            "直接 argv 里的 shell 字符没有执行语义"
        );
    }

    #[test]
    fn only_complete_readonly_argv_at_trusted_system_paths_can_skip_review() {
        let policy = PrefixRulePolicy::builtin();
        for command in [
            "/bin/pwd",
            "/usr/bin/pwd -P",
            "/usr/bin/whoami",
            "/bin/uname -a",
            "/usr/bin/id -un",
            "/bin/ls -alh .",
            "/usr/bin/ls --color=never -- .",
            "/bin/true",
            "/bin/false",
            "/usr/bin/echo hello",
        ] {
            assert_eq!(
                decide(&policy, command),
                Some(CommandDecision::Allow),
                "{command}"
            );
        }
        for command in [
            "ls .",
            "./ls .",
            "/tmp/ls .",
            "/usr/bin/../local/bin/ls .",
            "/bin/ls --unknown",
            "/bin/ls /root/.ssh",
            "/bin/ls -- -R",
            "/bin/ls -R .",
            "/bin/id --unknown",
            "/bin/uname --unknown",
            "/bin/pwd arbitrary",
            "/bin/true script",
            "/usr/bin/cat /root/.ssh/id_rsa",
            "git status",
            "git -c core.fsmonitor=./script status",
        ] {
            assert_eq!(
                decide(&policy, command),
                Some(CommandDecision::Prompt),
                "{command}"
            );
        }
    }

    #[test]
    fn broad_allow_prefixes_cannot_remove_review_or_forbidden_floors() {
        let commands = [
            "bash script.sh",
            "python3 -c 'print(1)'",
            "git status",
            "/tmp/ls",
            "unknown-binary --dangerous",
            "find . -delete",
            "find . -exec sh script.sh ';'",
            "sed -n '1w /tmp/x' input",
            "sed -n '1e sh script.sh' input",
            "awk 'BEGIN{system(\"sh script.sh\")}'",
            "sort -o /tmp/x input",
            "uniq input /tmp/x",
            "file -C -m data",
            "date -s tomorrow",
            "rg --pre=./script pattern .",
            "cmd.exe /c del data",
            "pwsh -File script.ps1",
        ];
        for command in commands {
            let words = shlex::split(command).unwrap();
            let name = Path::new(&words[0]).file_name().unwrap().to_str().unwrap();
            let rules =
                serde_json::json!({"rules":[{"pattern":[name],"decision":"allow"}]}).to_string();
            let policy =
                PrefixRulePolicy::builtin().merged(PrefixRulePolicy::from_json(&rules).unwrap());
            assert_eq!(
                decide(&policy, command),
                Some(CommandDecision::Prompt),
                "{command}"
            );
        }
        let policy = PrefixRulePolicy::builtin().merged(PrefixRulePolicy::from_json(
            r#"{"rules":[{"pattern":["reboot"],"decision":"allow"},{"pattern":["/bin/pwd"],"decision":"prompt"}]}"#
        ).unwrap());
        assert_eq!(decide(&policy, "reboot"), Some(CommandDecision::Forbidden));
        assert_eq!(decide(&policy, "/bin/pwd"), Some(CommandDecision::Prompt));
    }

    #[test]
    fn wrapper_options_and_reordered_system_deletion_preserve_forbidden() {
        let policy = PrefixRulePolicy::builtin();
        for command in [
            "sudo -u root reboot",
            "sudo --user=root reboot",
            "sudo -uroot reboot",
            "timeout 5 reboot",
            "timeout -s TERM -k 1 5 reboot",
            "env -u X reboot",
            "env --unset=X reboot",
            "env A=b reboot",
            "env -i -- reboot",
            "env -- A=b reboot",
            "sudo FOO=bar reboot",
            "nohup /sbin/reboot",
            "command -p reboot",
            "exec -a alias reboot",
            "time -o timing reboot",
            "sudo -u root timeout 5 env -u X reboot",
            "rm --force -R -- /",
            "rm --recursive -- /root/",
            "rm /etc/. -fr",
            "rm //etc -fr",
            "rm -R /tmp/../root",
        ] {
            assert_eq!(
                decide(&policy, command),
                Some(CommandDecision::Forbidden),
                "{command}"
            );
        }
        for command in [
            "sudo --unknown root reboot",
            "env -S 'reboot'",
            "timeout -s TERM",
            "rm -rf build",
        ] {
            assert_eq!(
                decide(&policy, command),
                Some(CommandDecision::Prompt),
                "{command}"
            );
        }
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
        assert_eq!(
            decide(&policy, "npm install"),
            Some(CommandDecision::Prompt)
        );
    }
}
