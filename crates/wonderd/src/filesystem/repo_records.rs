//! What a thread's own records say about the repositories it worked in.
//!
//! Every runtime reports its work as the same thread item shapes (the Claude
//! and OpenCode bridges project their tool calls into Codex's): `fileChange`
//! items name the files an edit touched, `commandExecution` items carry the
//! shell text and, for Codex and OpenCode, the folder it ran in, and
//! `mcpToolCall` items carry a tool's action (a notebook edit's path, a fetched
//! URL). This module reads those raw, unredacted items on the Mac and turns
//! them into observations: a folder and what the thread did there, or a pull
//! request and how the thread used it. Nothing here touches the disk; the
//! caller resolves each folder to its repository inside the deny list.

use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// Items read per thread, newest first.
pub(super) const MAX_ITEMS: usize = 2_000;
/// Shell text longer than this is not parsed.
const MAX_COMMAND_BYTES: usize = 16 * 1024;
/// Commands within one shell line, and observations per thread.
const MAX_SEGMENTS: usize = 64;
const MAX_OBSERVATIONS: usize = 512;

/// What the thread did in a folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Action {
    /// Edited a file, or ran a Git command that changes the repository.
    Edited,
    /// Ran `git init` or `git worktree add` there.
    Created,
    /// Cloned a repository into it.
    Cloned,
}

/// How the thread used a pull request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Relation {
    /// `gh pr create` printed it, or it is the work branch's own pull request.
    Opened,
    /// Checked out, viewed, reviewed, diffed, merged or fetched.
    Reviewing,
}

/// A pull request named by its canonical GitHub address.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PullAddress {
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) number: u64,
}

impl PullAddress {
    pub(super) fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
    pub(crate) fn url(&self) -> String {
        format!(
            "https://github.com/{}/{}/pull/{}",
            self.owner, self.repo, self.number
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PullTarget {
    Address(PullAddress),
    /// A number, in the repository `-R owner/name` names or else the folder's.
    Number {
        number: u64,
        slug: Option<String>,
    },
    /// `gh pr create` whose output did not include the address: the folder's
    /// current branch names it.
    Branch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PullObservation {
    pub(super) target: PullTarget,
    pub(super) relation: Relation,
    /// The folder the command ran in, when the record says.
    pub(super) folder: PathBuf,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Observations {
    /// Folders or files and what was done there, newest first.
    pub(super) paths: Vec<(PathBuf, Action)>,
    pub(super) pull_requests: Vec<PullObservation>,
    /// More records than the bounds allow were present.
    pub(super) truncated: bool,
    /// A `git push` ran.
    pushed: bool,
}

/// What one saved thread item says the thread published on GitHub.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Published {
    /// Pull requests it opened: the addresses `gh pr create` printed, or a
    /// pull request tool returned.
    pub(crate) opened: Vec<PullAddress>,
    /// Branches it pushed, as (`owner/name`, branch), from what `git push`
    /// printed. Default branches are left out: they are not the thread's own.
    pub(crate) pushed: Vec<(String, String)>,
}

/// Reads one thread item (any runtime's `commandExecution`, or a tool call)
/// for pull requests it opened and branches it pushed. Only the item's own
/// command and output count: a link the agent merely mentioned, viewed or
/// merged is not a pull request it opened.
pub(crate) fn published(item: &Value) -> Published {
    let mut published = Published::default();
    match item.get("type").and_then(Value::as_str) {
        Some("commandExecution") => {
            let Some(command) = item.get("command").and_then(Value::as_str) else {
                return published;
            };
            let output = item
                .get("aggregatedOutput")
                .or_else(|| item.get("output"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut found = Observations::default();
            observe_command(&mut found, command, Path::new("/"), Some(output), None, 0);
            for observation in found.pull_requests {
                if let (PullTarget::Address(address), Relation::Opened) =
                    (observation.target, observation.relation)
                {
                    // A link the command itself names (a body, a later view)
                    // is not one `gh pr create` printed.
                    if !command.contains(&address.url()) && !published.opened.contains(&address) {
                        published.opened.push(address);
                    }
                }
            }
            if found.pushed {
                published.pushed = pushed_branches(output);
            }
        }
        Some("mcpToolCall") | Some("dynamicToolCall") => {
            let tool = item.get("tool").and_then(Value::as_str).unwrap_or("");
            if tool.contains("create_pull_request") {
                let result = item.get("result").map(Value::to_string).unwrap_or_default();
                published.opened = pull_addresses(&result);
            }
        }
        _ => {}
    }
    published
}

/// The branches `git push` reports updating, under the GitHub repository of
/// the `To …` line before them. Rejected and deleted refs and tags are not.
fn pushed_branches(output: &str) -> Vec<(String, String)> {
    let mut pushed: Vec<(String, String)> = Vec::new();
    let mut slug = None;
    for line in output.lines().take(10_000) {
        if let Some(remote) = line.trim_start().strip_prefix("To ") {
            slug = remote_slug(remote.trim());
            continue;
        }
        let Some(slug) = &slug else { continue };
        let line = line.trim();
        let updated = line.starts_with("* [new branch]")
            || line.starts_with("+ ")
            || line.split_whitespace().next().is_some_and(|range| {
                range.contains("..") && range.bytes().all(|b| b.is_ascii_hexdigit() || b == b'.')
            });
        if !updated {
            continue;
        }
        let Some((_, target)) = line.split_once(" -> ") else {
            continue;
        };
        let branch = target.split_whitespace().next().unwrap_or("");
        let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
        if crate::pull_requests::valid_branch(branch)
            && !matches!(branch, "main" | "master")
            && !pushed.iter().any(|(s, b)| s == slug && b == branch)
        {
            pushed.push((slug.clone(), branch.to_owned()));
        }
    }
    pushed
}

impl Observations {
    fn add_path(&mut self, path: PathBuf, action: Action) {
        if self.paths.len() + self.pull_requests.len() >= MAX_OBSERVATIONS {
            self.truncated = true;
        } else if !self.paths.contains(&(path.clone(), action)) {
            self.paths.push((path, action));
        }
    }
    fn add_pull(&mut self, observation: PullObservation) {
        if self.paths.len() + self.pull_requests.len() >= MAX_OBSERVATIONS {
            self.truncated = true;
        } else if !self.pull_requests.iter().any(|known| {
            known.target == observation.target && known.relation == observation.relation
        }) {
            self.pull_requests.push(observation);
        }
    }
}

/// Reads raw thread items (newest first) for the folders and pull requests
/// the thread worked with. `cwd` is the thread's working folder, used when a
/// record names no folder (Claude's shell tool) or a relative path.
pub(super) fn observe(items: &[Value], cwd: &Path, home: Option<&Path>) -> Observations {
    let mut found = Observations {
        truncated: items.len() > MAX_ITEMS,
        ..Default::default()
    };
    for item in items.iter().take(MAX_ITEMS) {
        match item.get("type").and_then(Value::as_str) {
            Some("fileChange") => {
                for change in item
                    .get("changes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(path) = change.get("path").and_then(Value::as_str) {
                        if let Some(path) = absolute(cwd, path, home) {
                            found.add_path(path, Action::Edited);
                        }
                    }
                }
            }
            Some("commandExecution") => {
                let Some(command) = item.get("command").and_then(Value::as_str) else {
                    continue;
                };
                let folder = item
                    .get("cwd")
                    .and_then(Value::as_str)
                    .and_then(|dir| absolute(cwd, dir, home))
                    .unwrap_or_else(|| cwd.to_path_buf());
                let output = item.get("aggregatedOutput").and_then(Value::as_str);
                observe_command(&mut found, command, &folder, output, home, 0);
            }
            Some("mcpToolCall") | Some("dynamicToolCall") => {
                let action = item.get("toolAction");
                if action.and_then(|a| a.get("kind")).and_then(Value::as_str) == Some("edit") {
                    for path in action
                        .and_then(|a| a.get("paths"))
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        if let Some(path) = absolute(cwd, path, home) {
                            found.add_path(path, Action::Edited);
                        }
                    }
                }
                let url = action
                    .and_then(|a| a.get("url"))
                    .or_else(|| item.get("arguments").and_then(|a| a.get("url")))
                    .and_then(Value::as_str);
                for address in url.map(pull_addresses).unwrap_or_default() {
                    found.add_pull(PullObservation {
                        target: PullTarget::Address(address),
                        relation: Relation::Reviewing,
                        folder: cwd.to_path_buf(),
                    });
                }
            }
            _ => {}
        }
    }
    found
}

/// A record's path as an absolute, lexically normalized path.
pub(super) fn absolute(cwd: &Path, value: &str, home: Option<&Path>) -> Option<PathBuf> {
    if value.is_empty() || value.len() > 4096 || value.contains('\0') {
        return None;
    }
    let joined = if let Some(rest) = value.strip_prefix("~/") {
        home?.join(rest)
    } else if value == "~" {
        home?.to_path_buf()
    } else {
        cwd.join(value)
    };
    let mut normal = PathBuf::new();
    for component in joined.components() {
        match component {
            // Windows keeps its drive (or share) prefix: without it no path is absolute.
            Component::Prefix(_) | Component::RootDir => normal.push(component),
            Component::Normal(part) => normal.push(part),
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
        }
    }
    normal.is_absolute().then_some(normal)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Token {
    Word(String),
    /// `&&`, `||`, `;`, `|`, `&`, a newline or a parenthesis: a new command.
    Break,
}

/// Splits shell text into words and command separators, honouring quotes and
/// escapes. Expansions are kept as literal text; a word that still holds `$`
/// or a backtick is not trusted as a folder. Here-document bodies are text,
/// not commands, and are skipped.
pub(super) fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars().peekable();
    // Where the current line's tokens start, for its here-document operators.
    let mut line_start = 0;
    let finish = |tokens: &mut Vec<Token>, word: &mut String, in_word: &mut bool| {
        if *in_word {
            tokens.push(Token::Word(std::mem::take(word)));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.peek() {
                            Some(&next @ ('"' | '\\' | '$' | '`')) => {
                                word.push(next);
                                chars.next();
                            }
                            _ => word.push('\\'),
                        },
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    if next != '\n' {
                        in_word = true;
                        word.push(next);
                    }
                }
            }
            '>' | '<' => {
                in_word = true;
                word.push(c);
                if chars.peek() == Some(&'&') {
                    word.push('&');
                    chars.next();
                }
            }
            ';' | '|' | '&' | '\n' | '(' | ')' => {
                finish(&mut tokens, &mut word, &mut in_word);
                if matches!(c, '|' | '&') && chars.peek() == Some(&c) {
                    chars.next();
                }
                if c == '\n' {
                    skip_here_documents(&tokens[line_start..], &mut chars);
                }
                if tokens.last() != Some(&Token::Break) {
                    tokens.push(Token::Break);
                }
                if c == '\n' {
                    line_start = tokens.len();
                }
            }
            '#' if !in_word => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                skip_here_documents(&tokens[line_start..], &mut chars);
                if tokens.last() != Some(&Token::Break) {
                    tokens.push(Token::Break);
                }
                line_start = tokens.len();
            }
            c if c.is_whitespace() => finish(&mut tokens, &mut word, &mut in_word),
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    finish(&mut tokens, &mut word, &mut in_word);
    tokens
}

/// After a line that opened here-documents (`<<EOF`, `<<-'EOF'`, `<< EOF`),
/// skips their bodies up to each closing delimiter line.
fn skip_here_documents(line: &[Token], chars: &mut std::iter::Peekable<std::str::Chars>) {
    let words: Vec<&str> = line
        .iter()
        .filter_map(|token| match token {
            Token::Word(word) => Some(word.as_str()),
            Token::Break => None,
        })
        .collect();
    let mut delimiters = Vec::new();
    for (index, word) in words.iter().enumerate() {
        let Some(at) = word.find("<<") else { continue };
        let rest = &word[at + 2..];
        if rest.starts_with('<') {
            continue; // A here-string.
        }
        let (strip_tabs, rest) = match rest.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, rest),
        };
        let delimiter = if rest.is_empty() {
            words.get(index + 1).copied().unwrap_or("")
        } else {
            rest
        };
        if !delimiter.is_empty() {
            delimiters.push((delimiter.to_owned(), strip_tabs));
        }
    }
    for (delimiter, strip_tabs) in delimiters {
        loop {
            let mut body_line = String::new();
            let mut ended = true;
            for c in chars.by_ref() {
                if c == '\n' {
                    ended = false;
                    break;
                }
                body_line.push(c);
            }
            let body_line = if strip_tabs {
                body_line.trim_start_matches('\t')
            } else {
                body_line.as_str()
            };
            if body_line == delimiter || ended {
                break;
            }
        }
    }
}

pub(super) fn plain(word: &str) -> bool {
    !word.is_empty() && !word.contains(['$', '`', '*', '?']) && !word.starts_with(['>', '<'])
}

/// Reads one shell line, following `cd` between its commands.
fn observe_command(
    found: &mut Observations,
    command: &str,
    folder: &Path,
    output: Option<&str>,
    home: Option<&Path>,
    depth: u8,
) {
    if command.len() > MAX_COMMAND_BYTES || depth > 2 {
        found.truncated = true;
        return;
    }
    let tokens = tokenize(command);
    let mut cwd = folder.to_path_buf();
    let mut segments = 0;
    for segment in tokens.split(|token| *token == Token::Break) {
        let words: Vec<&str> = segment
            .iter()
            .filter_map(|token| match token {
                Token::Word(word) => Some(word.as_str()),
                Token::Break => None,
            })
            .filter(|word| !word.starts_with(['>', '<']) && !word.ends_with(['>', '<']))
            .collect();
        if words.is_empty() {
            continue;
        }
        segments += 1;
        if segments > MAX_SEGMENTS {
            found.truncated = true;
            break;
        }
        let words = strip_prefixes(&words);
        let Some((&program, args)) = words.split_first() else {
            continue;
        };
        let name = program.rsplit('/').next().unwrap_or(program);
        match name {
            "cd" | "pushd" => {
                if let Some(target) = args.iter().find(|a| !a.starts_with('-')) {
                    match plain(target)
                        .then(|| absolute(&cwd, target, home))
                        .flatten()
                    {
                        Some(next) => cwd = next,
                        // An unreadable target: later commands' folder is unknown.
                        None => return,
                    }
                }
            }
            "sh" | "bash" | "zsh" => {
                if let Some(index) = args
                    .iter()
                    .position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
                {
                    if let Some(script) = args.get(index + 1) {
                        observe_command(found, script, &cwd, output, home, depth + 1);
                    }
                }
            }
            "git" => observe_git(found, args, &cwd, home),
            "gh" => observe_gh(found, args, &cwd, output),
            _ => {}
        }
        // A pull request address anywhere else in the line was read, for
        // example with curl or a browser.
        if name != "gh" {
            for word in args {
                for address in pull_addresses(word) {
                    found.add_pull(PullObservation {
                        target: PullTarget::Address(address),
                        relation: Relation::Reviewing,
                        folder: cwd.clone(),
                    });
                }
            }
        }
    }
}

/// Drops leading environment assignments and wrappers such as `env`, `time`
/// and `command`, which do not change what runs or where.
fn strip_prefixes<'a>(words: &'a [&'a str]) -> &'a [&'a str] {
    let mut rest = words;
    while let Some((first, tail)) = rest.split_first() {
        let assignment = first.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        });
        if assignment
            || matches!(
                *first,
                "env" | "command" | "time" | "exec" | "nohup" | "builtin"
            )
        {
            rest = tail;
        } else {
            break;
        }
    }
    rest
}

/// Git subcommands that change a repository or its working tree.
const MUTATING_GIT: &[&str] = &[
    "add",
    "am",
    "apply",
    "branch",
    "checkout",
    "cherry-pick",
    "commit",
    "merge",
    "mv",
    "pull",
    "push",
    "rebase",
    "reset",
    "restore",
    "revert",
    "rm",
    "stash",
    "switch",
    "tag",
];

fn observe_git(found: &mut Observations, args: &[&str], cwd: &Path, home: Option<&Path>) {
    let mut dir = cwd.to_path_buf();
    let mut index = 0;
    // Global options before the subcommand.
    while let Some(&arg) = args.get(index) {
        match arg {
            "-C" => {
                let Some(target) = args.get(index + 1).filter(|t| plain(t)) else {
                    return;
                };
                match absolute(&dir, target, home) {
                    Some(next) => dir = next,
                    None => return,
                }
                index += 2;
            }
            "-c" | "--git-dir" | "--namespace" | "--exec-path" | "--config-env" => index += 2,
            "--work-tree" => {
                let Some(target) = args.get(index + 1).filter(|t| plain(t)) else {
                    return;
                };
                dir = match absolute(&dir, target, home) {
                    Some(next) => next,
                    None => return,
                };
                index += 2;
            }
            _ if arg.starts_with("--work-tree=") => {
                let target = &arg["--work-tree=".len()..];
                if !plain(target) {
                    return;
                }
                dir = match absolute(&dir, target, home) {
                    Some(next) => next,
                    None => return,
                };
                index += 1;
            }
            _ if arg.starts_with('-') => index += 1,
            _ => break,
        }
    }
    let Some(&subcommand) = args.get(index) else {
        return;
    };
    let rest = &args[index + 1..];
    match subcommand {
        "init" => {
            let target = positionals(
                rest,
                &[
                    "-b",
                    "--initial-branch",
                    "--template",
                    "--separate-git-dir",
                    "--object-format",
                    "--ref-format",
                    "--shared",
                ],
            )
            .first()
            .copied();
            let target = match target {
                Some(t) if !plain(t) => return,
                Some(t) => absolute(&dir, t, home),
                None => Some(dir.clone()),
            };
            if let Some(target) = target {
                found.add_path(target, Action::Created);
            }
        }
        "clone" => {
            let positional = positionals(
                rest,
                &[
                    "-b",
                    "--branch",
                    "-o",
                    "--origin",
                    "--depth",
                    "--reference",
                    "--reference-if-able",
                    "-c",
                    "--config",
                    "--filter",
                    "-u",
                    "--upload-pack",
                    "--template",
                    "--separate-git-dir",
                    "-j",
                    "--jobs",
                    "--shallow-since",
                    "--shallow-exclude",
                    "--server-option",
                    "--bundle-uri",
                    "--revision",
                ],
            );
            let Some(source) = positional.first() else {
                return;
            };
            let name = match positional.get(1) {
                Some(target) => {
                    if !plain(target) {
                        return;
                    }
                    target.to_string()
                }
                None => {
                    // Git's own default: the last path part, without `.git`.
                    let trimmed = source.trim_end_matches('/');
                    let last = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
                    let last = last.strip_suffix(".git").unwrap_or(last);
                    if !plain(last) || last.is_empty() || last == ".." {
                        return;
                    }
                    last.to_owned()
                }
            };
            if let Some(target) = absolute(&dir, &name, home) {
                found.add_path(target, Action::Cloned);
            }
        }
        "worktree" => {
            if rest.first() == Some(&"add") {
                let positional = positionals(&rest[1..], &["-b", "-B", "--reason"]);
                if let Some(target) = positional.first().filter(|t| plain(t)) {
                    if let Some(target) = absolute(&dir, target, home) {
                        found.add_path(target, Action::Created);
                    }
                }
            }
        }
        _ if MUTATING_GIT.contains(&subcommand) => {
            found.pushed |= subcommand == "push";
            found.add_path(dir, Action::Edited)
        }
        _ => {}
    }
}

/// Arguments that are not options or option values. `value_options` take the
/// next word unless written `--name=value`.
fn positionals<'a>(args: &[&'a str], value_options: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut index = 0;
    let mut options_done = false;
    while let Some(&arg) = args.get(index) {
        index += 1;
        if options_done || !arg.starts_with('-') || arg == "-" {
            out.push(arg);
        } else if arg == "--" {
            options_done = true;
        } else if value_options.contains(&arg) {
            index += 1;
        }
    }
    out
}

fn observe_gh(found: &mut Observations, args: &[&str], cwd: &Path, output: Option<&str>) {
    let (Some(&"pr"), Some(&subcommand)) = (args.first(), args.get(1)) else {
        return;
    };
    let rest = &args[2..];
    let mut slug = None;
    let mut index = 0;
    while let Some(&arg) = rest.get(index) {
        if arg == "-R" || arg == "--repo" {
            slug = rest.get(index + 1).and_then(|value| repo_slug(value));
        } else if let Some(value) = arg.strip_prefix("--repo=") {
            slug = repo_slug(value);
        }
        index += 1;
    }
    let relation = match subcommand {
        "create" | "new" => Relation::Opened,
        "checkout" | "co" | "view" | "review" | "diff" | "merge" | "checks" | "comment" => {
            Relation::Reviewing
        }
        _ => return,
    };
    if relation == Relation::Opened {
        let printed = output.map(pull_addresses).unwrap_or_default();
        if printed.is_empty() {
            found.add_pull(PullObservation {
                target: PullTarget::Branch,
                relation,
                folder: cwd.to_path_buf(),
            });
        }
        for address in printed {
            found.add_pull(PullObservation {
                target: PullTarget::Address(address),
                relation,
                folder: cwd.to_path_buf(),
            });
        }
        return;
    }
    let value_options = [
        "-R",
        "--repo",
        "-b",
        "--body",
        "-F",
        "--body-file",
        "-t",
        "--subject",
        "--match-head-commit",
        "-q",
        "--jq",
        "-T",
        "--template",
        "--json",
        "--author-email",
        "--color",
        "--patch",
        "-A",
        "--auth",
        "--remote",
        "--branch",
    ];
    let Some(target) = positionals(rest, &value_options).first().copied() else {
        return;
    };
    let target = if let Some(address) = pull_addresses(target).into_iter().next() {
        PullTarget::Address(address)
    } else if let Some(number) = target
        .trim_start_matches('#')
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0 && *n < 1_000_000_000)
    {
        match slug.as_deref().and_then(|slug| slug.split_once('/')) {
            Some((owner, repo)) => PullTarget::Address(PullAddress {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                number,
            }),
            None => PullTarget::Number { number, slug: None },
        }
    } else {
        return;
    };
    found.add_pull(PullObservation {
        target,
        relation,
        folder: cwd.to_path_buf(),
    });
}

fn valid_owner(owner: &str) -> bool {
    (1..=39).contains(&owner.len())
        && !owner.starts_with('-')
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn valid_repo(repo: &str) -> bool {
    (1..=100).contains(&repo.len())
        && !repo.starts_with('.')
        && repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `owner/name` or `github.com/owner/name`, as `gh -R` accepts.
fn repo_slug(value: &str) -> Option<String> {
    let value = value
        .trim_start_matches("https://")
        .trim_start_matches("github.com/");
    let (owner, repo) = value.trim_end_matches('/').split_once('/')?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (valid_owner(owner) && valid_repo(repo)).then(|| format!("{owner}/{repo}"))
}

/// The GitHub repository a remote URL points at, never the URL itself (it
/// can carry a credential). Read with the repository identity rules, so only
/// github.com itself (not `evilgithub.com` or a path containing
/// `github.com/`) counts; the remote's spelling is kept for `gh -R`.
pub(super) fn remote_slug(url: &str) -> Option<String> {
    let remote = crate::projects::identity::parse_remote(url)?;
    let [owner, repo] = remote.segments.as_slice() else {
        return None;
    };
    (remote.host == "github.com" && valid_owner(owner) && valid_repo(repo))
        .then(|| format!("{owner}/{repo}"))
}

/// Pull request addresses on github.com in `text`, in order of appearance.
pub(super) fn pull_addresses(text: &str) -> Vec<PullAddress> {
    const PREFIX: &str = "https://github.com/";
    let mut found: Vec<PullAddress> = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(PREFIX) {
        rest = &rest[start + PREFIX.len()..];
        let take = |s: &str, allowed: fn(u8) -> bool| s.bytes().take_while(|b| allowed(*b)).count();
        let owner_len = take(rest, |b| b.is_ascii_alphanumeric() || b == b'-');
        let owner = &rest[..owner_len];
        let Some(after) = rest[owner_len..].strip_prefix('/') else {
            continue;
        };
        let repo_len = take(after, |b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
        });
        let repo = &after[..repo_len];
        let Some(after) = after[repo_len..].strip_prefix("/pull/") else {
            continue;
        };
        let digits = take(after, |b| b.is_ascii_digit());
        let Some(number) = (1..=9)
            .contains(&digits)
            .then(|| after[..digits].parse::<u64>().ok())
            .flatten()
            .filter(|n| *n > 0)
        else {
            continue;
        };
        if !valid_owner(owner) || !valid_repo(repo) {
            continue;
        }
        let address = PullAddress {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            number,
        };
        if !found.contains(&address) {
            found.push(address);
        }
    }
    found
}

// Unix fixtures (absolute /paths and POSIX shell lines); Windows paths are not covered yet.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    fn command(command: &str, cwd: Option<&str>, output: Option<&str>) -> Value {
        json!({"type":"commandExecution","id":"c","command":command,"cwd":cwd,"aggregatedOutput":output})
    }

    fn observe_one(item: Value) -> Observations {
        observe(&[item], Path::new("/w/a"), Some(Path::new("/Users/me")))
    }

    // Codex wraps its commands in a login shell; Claude's Bash tool has no cwd,
    // so the thread's folder applies. `cd` carries across `&&`.
    #[test]
    fn shell_lines_follow_cd_and_unwrap_login_shells() {
        let codex = observe_one(command(
            "/bin/zsh -lc 'mkdir B && cd B && git init -q -b main'",
            Some("/w/a"),
            None,
        ));
        assert_eq!(codex.paths, [(PathBuf::from("/w/a/B"), Action::Created)]);
        let claude = observe_one(command(
            "cd ../other && git commit -am \"x; y\"",
            None,
            None,
        ));
        assert_eq!(claude.paths, [(PathBuf::from("/w/other"), Action::Edited)]);
        let unknown = observe_one(command("cd \"$TMP\" && git init", None, None));
        assert!(
            unknown.paths.is_empty(),
            "an expanded folder is not guessed"
        );
        // A here-document body is text: its apostrophes and command-like
        // lines neither hide the commands after it nor count as commands.
        let body = observe_one(command(
            "cat > body.md <<'EOF'\nIt's done; cd /tmp && git init\nEOF\ncat <<-END > x\n\tgit init\n\tEND\ngh pr create --body-file body.md",
            Some("/w/a"),
            Some("https://github.com/o/r/pull/4\n"),
        ));
        assert!(body.paths.is_empty(), "{:?}", body.paths);
        assert_eq!(
            body.pull_requests,
            [PullObservation {
                target: PullTarget::Address(PullAddress {
                    owner: "o".into(),
                    repo: "r".into(),
                    number: 4
                }),
                relation: Relation::Opened,
                folder: PathBuf::from("/w/a"),
            }]
        );
    }

    #[test]
    fn git_global_options_clone_and_worktrees_name_their_folders() {
        let cases = [
            (
                "git -C ../lib status && git -C ../lib commit -m x",
                vec![("/w/lib", Action::Edited)],
            ),
            (
                "git -C ~/src/tool push",
                vec![("/Users/me/src/tool", Action::Edited)],
            ),
            (
                "git clone --depth 1 https://github.com/o/tool.git",
                vec![("/w/a/tool", Action::Cloned)],
            ),
            (
                "git clone -b dev git@github.com:o/tool.git ../t2",
                vec![("/w/t2", Action::Cloned)],
            ),
            (
                "git worktree add -b fix ../a-fix main",
                vec![("/w/a-fix", Action::Created)],
            ),
            ("git init ../fresh", vec![("/w/fresh", Action::Created)]),
            ("git log --oneline | head && git diff", vec![]),
            (
                "env GIT_DIR=x git -c core.x=1 checkout -b n",
                vec![("/w/a", Action::Edited)],
            ),
        ];
        for (line, expected) in cases {
            let found = observe_one(command(line, None, None));
            let expected: Vec<_> = expected
                .into_iter()
                .map(|(path, action)| (PathBuf::from(path), action))
                .collect();
            assert_eq!(found.paths, expected, "{line}");
        }
    }

    #[test]
    fn gh_commands_name_pull_requests_and_how_they_were_used() {
        let review = observe_one(command(
            "gh pr review https://github.com/o/c/pull/7 --approve",
            None,
            None,
        ));
        assert_eq!(
            review.pull_requests,
            [PullObservation {
                target: PullTarget::Address(PullAddress {
                    owner: "o".into(),
                    repo: "c".into(),
                    number: 7
                }),
                relation: Relation::Reviewing,
                folder: "/w/a".into(),
            }]
        );
        let numbered = observe_one(command("gh pr checkout 12", Some("/w/b"), None));
        assert_eq!(
            numbered.pull_requests[0].target,
            PullTarget::Number {
                number: 12,
                slug: None
            }
        );
        assert_eq!(numbered.pull_requests[0].folder, PathBuf::from("/w/b"));
        let other_repo = observe_one(command("gh pr diff 3 -R o/x", None, None));
        assert_eq!(
            other_repo.pull_requests[0].target,
            PullTarget::Address(PullAddress {
                owner: "o".into(),
                repo: "x".into(),
                number: 3
            })
        );
        let created = observe_one(command(
            "git push -u origin HEAD && gh pr create --fill",
            None,
            Some("https://github.com/o/a/pull/41\n"),
        ));
        assert_eq!(created.pull_requests[0].relation, Relation::Opened);
        assert_eq!(
            created.pull_requests[0].target,
            PullTarget::Address(PullAddress {
                owner: "o".into(),
                repo: "a".into(),
                number: 41
            })
        );
        let pending = observe_one(command("gh pr create --fill", None, None));
        assert_eq!(pending.pull_requests[0].target, PullTarget::Branch);
        let read = observe_one(command(
            "curl -sL https://github.com/o/d/pull/5.diff",
            None,
            None,
        ));
        assert_eq!(read.pull_requests[0].relation, Relation::Reviewing);
        assert!(observe_one(command("gh pr list", None, None))
            .pull_requests
            .is_empty());
    }

    // File edits from every runtime share one shape; a notebook edit and a
    // fetched pull request page come from tool calls.
    #[test]
    fn file_changes_and_tool_calls_name_paths_and_pages() {
        let items = [
            json!({"type":"fileChange","changes":[{"path":"/w/a/src/x.rs"},{"path":"rel/y.rs"}]}),
            json!({"type":"mcpToolCall","server":"Claude","tool":"NotebookEdit","toolAction":{"kind":"edit","paths":["/w/n/book.ipynb"]}}),
            json!({"type":"mcpToolCall","server":"Claude","tool":"WebFetch","toolAction":{"kind":"fetch","url":"https://github.com/o/e/pull/9/files"}}),
            json!({"type":"mcpToolCall","server":"Claude","tool":"Read","toolAction":{"kind":"read","paths":["/elsewhere/z"]}}),
            json!({"type":"agentMessage","text":"see https://github.com/o/f/pull/1"}),
        ];
        let found = observe(&items, Path::new("/w/a"), None);
        assert_eq!(
            found.paths,
            [
                (PathBuf::from("/w/a/src/x.rs"), Action::Edited),
                (PathBuf::from("/w/a/rel/y.rs"), Action::Edited),
                (PathBuf::from("/w/n/book.ipynb"), Action::Edited),
            ]
        );
        assert_eq!(found.pull_requests.len(), 1);
        assert_eq!(
            found.pull_requests[0].target,
            PullTarget::Address(PullAddress {
                owner: "o".into(),
                repo: "e".into(),
                number: 9
            })
        );
    }

    #[test]
    fn records_are_bounded() {
        let many: Vec<Value> = (0..MAX_ITEMS + 5)
            .map(|i| json!({"type":"fileChange","changes":[{"path":format!("/w/{i}/f")}]}))
            .collect();
        let found = observe(&many, Path::new("/w"), None);
        assert!(found.truncated);
        assert_eq!(found.paths.len(), MAX_OBSERVATIONS);
        let long = "git init; ".repeat(MAX_SEGMENTS + 10);
        let found = observe_one(command(&long, None, None));
        assert!(found.truncated);
        let huge = "x".repeat(MAX_COMMAND_BYTES + 1);
        assert!(observe_one(command(&huge, None, None)).truncated);
    }

    #[test]
    fn remote_urls_reduce_to_their_repository_only() {
        for (url, slug) in [
            ("https://github.com/o/r.git", Some("o/r")),
            ("https://user:secret@github.com/o/r", Some("o/r")),
            ("git@github.com:o/r.git", Some("o/r")),
            ("ssh://git@github.com/o/r", Some("o/r")),
            ("https://gitlab.com/o/r", None),
            ("https://github.com/o/r/extra", None),
            ("https://github.com/Owner/Repo", Some("Owner/Repo")),
            ("ssh://git@ssh.github.com:443/o/r.git", Some("o/r")),
            ("https://evilgithub.com/o/r", None),
            ("https://evil.example/github.com/o/r", None),
            ("git@evil.example:github.com/o/r", None),
        ] {
            assert_eq!(remote_slug(url).as_deref(), slug, "{url}");
        }
    }
}
