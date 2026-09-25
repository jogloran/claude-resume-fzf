use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// One resumable Claude Code session.
struct Session {
    id: String,
    cwd: String,
    /// Best label: Claude's AI-generated title, else the first user prompt.
    label: String,
    /// Flattened transcript text (prompts + replies) for full-text search.
    haystack: String,
    branch: Option<String>,
    mtime: u64,
    file: PathBuf,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME not set"))
}

fn projects_dir() -> PathBuf {
    home().join(".claude/projects")
}

/// Shorten a path by replacing the home prefix with `~`.
fn tilde(path: &str) -> String {
    let h = home();
    let h = h.to_string_lossy();
    match path.strip_prefix(h.as_ref()) {
        Some(rest) => format!("~{rest}"),
        None => path.to_string(),
    }
}

/// Best-effort recovery of a session's cwd from its encoded project-folder name
/// (Claude replaces `/` with `-`, e.g. `-home-jdoe-foo` -> `/home/jdoe/foo`).
///
/// This is ambiguous when a real path component contains `-`, so we only accept
/// the decoded path if it actually exists as a directory — a wrong guess is
/// simply rejected rather than shown as a phantom, unresumable entry.
fn decode_project_dir(folder: &str) -> Option<String> {
    let decoded = folder.replace('-', "/");
    if Path::new(&decoded).is_dir() {
        Some(decoded)
    } else {
        None
    }
}

/// Extract plain text from a user message's `content` field.
fn text_from_content(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let mut out = String::new();
            for b in blocks {
                // Skip tool results / non-text blocks.
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        out.push_str(t);
                    }
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        _ => None,
    }
}

/// Read a session file, pulling out cwd and the first real user prompt.
/// The full transcript haystack is only built when `full` search is requested.
fn parse_session(file: PathBuf, full: bool) -> Option<Session> {
    let id = file.file_stem()?.to_string_lossy().into_owned();
    let file_mtime = fs::metadata(&file)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let f = fs::File::open(&file).ok()?;
    let reader = BufReader::new(f);

    let mut cwd: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut title: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut haystack = String::new();
    // Last real transcript activity, from each line's own `timestamp` field.
    // Bookkeeping lines appended well after a session ends (cost-state,
    // last-prompt, ai-title, ...) carry no timestamp and are ignored here,
    // so they can't make a stale session look freshly used.
    let mut last_activity: Option<u64> = None;

    // Scan the whole file: the `ai-title` entry is written late in a session,
    // so we can't stop early once cwd + first prompt are found.
    for line in reader.lines().map_while(Result::ok) {
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if cwd.is_none() {
            if let Some(c) = v.get("cwd").and_then(Value::as_str) {
                cwd = Some(c.to_string());
                branch = v
                    .get("gitBranch")
                    .and_then(Value::as_str)
                    .filter(|b| !b.is_empty())
                    .map(String::from);
            }
        }
        if let Some(ts) = v.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339) {
            last_activity = Some(last_activity.map_or(ts, |cur| cur.max(ts)));
        }
        match v.get("type").and_then(Value::as_str) {
            // Claude's own generated session title — the best label when present.
            Some("ai-title") => {
                if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                    let t = t.trim();
                    if !t.is_empty() {
                        title = Some(t.to_string());
                    }
                }
            }
            Some(ty @ ("user" | "assistant")) if !is_sidechain(&v) => {
                if let Some(content) = v.pointer("/message/content") {
                    if let Some(t) = text_from_content(content) {
                        let t = t.trim();
                        // Ignore command wrappers / meta noise.
                        if !t.is_empty() && !t.starts_with('<') && !t.starts_with("Caveat:") {
                            if ty == "user" && prompt.is_none() {
                                prompt = Some(t.to_string());
                            }
                            if full {
                                append_haystack(&mut haystack, t);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Fall back to decoding the folder name for sessions with no recorded cwd.
    let cwd = cwd.or_else(|| {
        file.parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .and_then(decode_project_dir)
    })?;

    let label = title
        .or(prompt)
        .unwrap_or_else(|| "(no prompt)".to_string());
    Some(Session {
        id,
        cwd,
        label,
        haystack,
        branch,
        mtime: last_activity.unwrap_or(file_mtime),
        file,
    })
}

fn is_sidechain(v: &Value) -> bool {
    v.get("isSidechain").and_then(Value::as_bool) == Some(true)
}

/// Parse a UTC RFC 3339 timestamp of the exact form Claude Code emits
/// (`YYYY-MM-DDTHH:MM:SS(.fff)?Z`) into Unix seconds. Fractional seconds and
/// any other suffix are ignored since we only need second resolution.
fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u64 = s.get(11..13)?.parse().ok()?;
    let min: u64 = s.get(14..16)?.parse().ok()?;
    let sec: u64 = s.get(17..19)?.parse().ok()?;
    let days = days_from_civil(year, month, day)?;
    Some((days as u64) * 86400 + hour * 3600 + min * 60 + sec)
}

/// Days since the Unix epoch for a Gregorian calendar date, via Howard
/// Hinnant's `days_from_civil` algorithm (proleptic Gregorian, valid for any
/// real-world date).
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

/// Append whitespace-flattened text to the search haystack. Flattening drops
/// tabs/newlines so the whole transcript stays on one fzf record.
fn append_haystack(hay: &mut String, text: &str) {
    for word in text.split_whitespace() {
        hay.push_str(word);
        hay.push(' ');
    }
}

fn collect_sessions(full: bool) -> Vec<Session> {
    let mut sessions = Vec::new();
    let dir = projects_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return sessions,
    };
    for proj in entries.flatten() {
        let p = proj.path();
        if !p.is_dir() {
            continue;
        }
        if let Ok(files) = fs::read_dir(&p) {
            for f in files.flatten() {
                let fp = f.path();
                if fp.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    if let Some(s) = parse_session(fp, full) {
                        sessions.push(s);
                    }
                }
            }
        }
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));
    sessions
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn rel_time(mtime: u64) -> String {
    rel_time_from(now_secs(), mtime)
}

fn rel_time_from(now: u64, mtime: u64) -> String {
    let d = now.saturating_sub(mtime);
    if d < 60 {
        format!("{d}s")
    } else if d < 3600 {
        format!("{}m", d / 60)
    } else if d < 86400 {
        format!("{}h", d / 3600)
    } else {
        format!("{}d", d / 86400)
    }
}

fn truncate(s: &str, max: usize) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > max {
        let t: String = one_line.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    } else {
        one_line
    }
}

// ANSI colors for the fzf list.
const DIM: &str = "\x1b[90m";
const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[35m";
const RESET: &str = "\x1b[0m";

/// Render one session's visible list row: relative time, directory, git
/// branch (if any, in its own color), and label.
fn format_row(s: &Session) -> String {
    let branch = match &s.branch {
        Some(b) => format!(" {MAGENTA}[{b}]{RESET}"),
        None => String::new(),
    };
    format!(
        "{DIM}{:>4}{RESET}  {CYAN}{}{RESET}{branch}  {}",
        rel_time(s.mtime),
        tilde(&s.cwd),
        truncate(&s.label, 80),
    )
}

/// Interactive picker: list sessions in fzf, then resume the chosen one.
/// When `full` is set, the whole transcript is searchable; otherwise fzf
/// searches only the visible label (title/first prompt) and directory.
pub fn run(full: bool) -> ! {
    let sessions = collect_sessions(full);
    if sessions.is_empty() {
        eprintln!(
            "No Claude sessions found under {}",
            projects_dir().display()
        );
        std::process::exit(1);
    }

    let self_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("claude-resume-fzf"));

    // Build tab-delimited input: display col + hidden cwd/id/file cols.
    let mut input = String::new();
    for s in &sessions {
        let display = format_row(s);
        // In full mode the haystack trails the visible label so fzf searches
        // the whole transcript; it sits off-screen (with --no-hscroll) and dimmed.
        let searchable = if full {
            format!("{display}  {DIM}{}{RESET}", s.haystack)
        } else {
            display
        };
        input.push_str(&format!(
            "{searchable}\t{}\t{}\t{}\n",
            s.cwd,
            s.id,
            s.file.display()
        ));
    }

    // Pass the live query to the preview so matches can be highlighted.
    let preview_cmd = format!("{} --preview {{4}} {{q}}", self_exe.display());

    let mut child = Command::new("fzf")
        .args([
            "--ansi",
            "--delimiter=\t",
            "--with-nth=1",
            "--no-hscroll",
            if full {
                "--prompt=session (full-text)> "
            } else {
                "--prompt=session> "
            },
            "--height=100%",
            "--layout=reverse",
            "--preview-window=down:60%:wrap:follow",
        ])
        // Fuzzy matching over a 20 KB transcript blob matches almost everything,
        // so full-text mode uses exact substring matching instead.
        .args(if full { &["--exact"][..] } else { &[][..] })
        .arg("--preview")
        .arg(&preview_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("failed to launch fzf (is it installed?): {e}");
            std::process::exit(1);
        });

    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .expect("write to fzf");

    let out = child.wait_with_output().expect("wait fzf");
    if !out.status.success() {
        // User pressed ESC / no selection.
        std::process::exit(130);
    }

    let line = String::from_utf8_lossy(&out.stdout);
    let line = line.trim_end_matches('\n');
    let cols: Vec<&str> = line.split('\t').collect();
    if cols.len() < 3 {
        eprintln!("unexpected fzf output");
        std::process::exit(1);
    }
    let cwd = cols[1];
    let id = cols[2];

    if !Path::new(cwd).is_dir() {
        eprintln!("session cwd no longer exists: {cwd}");
        std::process::exit(1);
    }

    eprintln!("→ cd {cwd} && claude --resume {id}");
    let err = Command::new("claude")
        .arg("--resume")
        .arg(id)
        .current_dir(cwd)
        .exec();
    // exec only returns on failure.
    eprintln!("failed to launch claude: {err}");
    std::process::exit(1);
}

/// Render a readable preview of a session file for fzf's preview pane.
/// `query` is fzf's live search string; matching terms are highlighted.
pub fn preview(file: &str, query: &str) {
    let f = match fs::File::open(file) {
        Ok(f) => f,
        Err(e) => {
            println!("cannot read session: {e}");
            return;
        }
    };
    let reader = BufReader::new(f);

    // Metadata lives at different points in the file (title/last-prompt are late),
    // so collect everything first, then render a stable header + transcript.
    let mut cwd: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut version: Option<String> = None;
    let mut title: Option<String> = None;
    let mut last_prompt: Option<String> = None;
    let mut turns: Vec<(&'static str, &'static str, String)> = Vec::new();

    for line in reader.lines().map_while(Result::ok) {
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if cwd.is_none() {
            if let Some(c) = v.get("cwd").and_then(Value::as_str) {
                cwd = Some(c.to_string());
                branch = v
                    .get("gitBranch")
                    .and_then(Value::as_str)
                    .filter(|b| !b.is_empty())
                    .map(String::from);
                version = v.get("version").and_then(Value::as_str).map(String::from);
            }
        }
        match v.get("type").and_then(Value::as_str) {
            Some("ai-title") => {
                title = v.get("aiTitle").and_then(Value::as_str).map(String::from);
            }
            Some("last-prompt") => {
                last_prompt = v
                    .get("lastPrompt")
                    .and_then(Value::as_str)
                    .map(String::from);
            }
            Some("user") if !is_sidechain(&v) => push_turn(&v, "you", "\x1b[33m", &mut turns),
            Some("assistant") => push_turn(&v, "claude", "\x1b[32m", &mut turns),
            _ => {}
        }
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if let Some(t) = &title {
        let _ = writeln!(out, "{CYAN}title:{RESET}  {t}");
    }
    if let Some(c) = &cwd {
        let _ = writeln!(out, "{CYAN}dir:{RESET}    {}", tilde(c));
    }
    if let Some(b) = &branch {
        let _ = writeln!(out, "{CYAN}branch:{RESET} {b}");
    }
    if let Some(ver) = &version {
        let _ = writeln!(out, "{CYAN}claude:{RESET} {ver}");
    }
    let width = preview_width();
    if let Some(lp) = &last_prompt {
        write_wrapped(&mut out, "latest:", CYAN, &truncate(lp, 800), query, width);
    }
    let _ = writeln!(out, "{DIM}{}{RESET}", "─".repeat(40));
    for (label, color, text) in turns.iter().take(40) {
        write_wrapped(&mut out, &format!("{label}:"), color, &truncate(text, 1200), query, width);
    }
}

/// Preview pane width, as fzf reports it via `FZF_PREVIEW_COLUMNS`; falls
/// back to 80 when unset or unparsable (e.g. run outside fzf for testing).
fn preview_width() -> usize {
    std::env::var("FZF_PREVIEW_COLUMNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&w| w > 10)
        .unwrap_or(80)
}

/// Word-wrap `text` to `width` columns, printing a `label:` prefix on the
/// first line and aligning continuation lines under it. Wrapping happens
/// before highlighting so embedded ANSI codes never skew the wrap width.
fn write_wrapped(
    out: &mut impl Write,
    label: &str,
    color: &str,
    text: &str,
    query: &str,
    width: usize,
) {
    let prefix = format!("{label} ");
    let indent = " ".repeat(prefix.chars().count());
    let body_width = width.saturating_sub(indent.len()).max(10);
    for (i, line) in textwrap::wrap(text, body_width).iter().enumerate() {
        let hl = highlight(line, query);
        if i == 0 {
            let _ = writeln!(out, "{color}{prefix}{RESET}{hl}");
        } else {
            let _ = writeln!(out, "{indent}{hl}");
        }
    }
}

/// Wrap case-insensitive occurrences of each query term in a highlight color.
/// ASCII-only matching keeps byte offsets aligned with char boundaries.
fn highlight(text: &str, query: &str) -> String {
    const HL: &str = "\x1b[43;30m"; // black on yellow
    let terms: Vec<String> = query
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .filter(|t| !t.is_empty())
        .collect();
    if terms.is_empty() {
        return text.to_string();
    }
    let lower = text.to_ascii_lowercase();
    let mut marks = vec![false; text.len()];
    for term in &terms {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(term.as_str()) {
            let s = from + pos;
            let e = s + term.len();
            marks[s..e].iter_mut().for_each(|m| *m = true);
            from = e;
        }
    }
    let mut out = String::new();
    let mut inside = false;
    for (i, ch) in text.char_indices() {
        if marks[i] && !inside {
            out.push_str(HL);
            inside = true;
        } else if !marks[i] && inside {
            out.push_str(RESET);
            inside = false;
        }
        out.push(ch);
    }
    if inside {
        out.push_str(RESET);
    }
    out
}

/// Extract a printable transcript turn from a message entry, if any.
fn push_turn(
    v: &Value,
    label: &'static str,
    color: &'static str,
    turns: &mut Vec<(&'static str, &'static str, String)>,
) {
    if let Some(content) = v.pointer("/message/content") {
        if let Some(t) = text_from_content(content) {
            let t = t.trim();
            if !t.is_empty() && !t.starts_with('<') {
                turns.push((label, color, t.to_string()));
            }
        }
    }
}

/// Shared entry point used by both binaries. `args` excludes the program name.
pub fn main_with_args(args: Vec<String>) -> ! {
    match args.first().map(String::as_str) {
        Some("--preview") => {
            if let Some(file) = args.get(1) {
                let query = args.get(2).map(String::as_str).unwrap_or("");
                preview(file, query);
            }
            std::process::exit(0);
        }
        Some("-h") | Some("--help") => {
            println!(
                "claude-resume-fzf — fuzzy-find and resume Claude Code sessions\n\n\
                 Usage: claude-resume-fzf [OPTIONS]   (alias: ccresume)\n\n\
                 Lists every session under ~/.claude/projects in fzf, newest first.\n\
                 Select one to cd into its directory and run `claude --resume`.\n\
                 Search matches the session title and directory by default.\n\n\
                 Options:\n\
                 \x20 -a, --all     also search the full transcript (exact substring match)\n\
                 \x20 -h, --help    show this help and exit\n\n\
                 Keys (inside fzf):\n\
                 \x20 Enter         resume the selected session in its directory\n\
                 \x20 Esc           quit without doing anything"
            );
            std::process::exit(0);
        }
        Some("-a") | Some("--all") => run(true),
        _ => run(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_from_string_and_blocks() {
        assert_eq!(
            text_from_content(&json!("hello")),
            Some("hello".to_string())
        );
        let blocks = json!([
            {"type": "text", "text": "part one "},
            {"type": "tool_use", "name": "Bash"},
            {"type": "text", "text": "part two"}
        ]);
        assert_eq!(
            text_from_content(&blocks),
            Some("part one part two".to_string())
        );
        // Only tool blocks -> nothing printable.
        let tool_only = json!([{"type": "tool_result", "content": "x"}]);
        assert_eq!(text_from_content(&tool_only), None);
    }

    #[test]
    fn truncate_collapses_whitespace_and_caps_length() {
        assert_eq!(truncate("  a\n  b\tc ", 80), "a b c");
        let long = "x".repeat(100);
        let out = truncate(&long, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn rel_time_buckets() {
        assert_eq!(rel_time_from(30, 0), "30s");
        assert_eq!(rel_time_from(120, 0), "2m");
        assert_eq!(rel_time_from(7200, 0), "2h");
        assert_eq!(rel_time_from(172_800, 0), "2d");
        // Clock skew must not underflow.
        assert_eq!(rel_time_from(0, 100), "0s");
    }

    #[test]
    fn parse_rfc3339_known_values() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("1970-01-01T00:00:01.500Z"), Some(1));
        // 2026-09-21T21:43:13.984Z, cross-checked against Python's datetime.timestamp().
        assert_eq!(parse_rfc3339("2026-09-21T21:43:13.984Z"), Some(1_790_026_993));
        assert_eq!(parse_rfc3339("not a timestamp"), None);
        assert_eq!(parse_rfc3339(""), None);
    }

    #[test]
    fn parse_session_uses_last_transcript_timestamp_not_file_mtime() {
        let dir = std::env::temp_dir().join(format!("crf-ts-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("22222222-3333-4444-5555-666666666666.jsonl");
        let body = [
            json!({"type": "user", "cwd": "/tmp", "timestamp": "2026-09-21T21:00:00Z", "message": {"content": "hi"}}),
            json!({"type": "assistant", "timestamp": "2026-09-21T21:43:13.984Z", "message": {"content": [{"type": "text", "text": "sure"}]}}),
            // Bookkeeping lines with no timestamp, appended after the real conversation ended.
            json!({"type": "last-prompt", "lastPrompt": "hi"}),
            json!({"type": "cost-state", "totalCostUSD": 0.01}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        fs::write(&file, body).unwrap();

        let s = parse_session(file.clone(), false).expect("session parsed");
        assert_eq!(s.mtime, 1_790_026_993);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_session_captures_git_branch() {
        let dir = std::env::temp_dir().join(format!("crf-branch-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("33333333-4444-5555-6666-777777777777.jsonl");
        let body = json!({
            "type": "user",
            "cwd": "/tmp",
            "gitBranch": "feature/foo",
            "message": {"content": "hi"},
        })
        .to_string();
        fs::write(&file, body).unwrap();

        let s = parse_session(file.clone(), false).expect("session parsed");
        assert_eq!(s.branch.as_deref(), Some("feature/foo"));

        // Empty gitBranch (detached HEAD / non-git dir) should surface as None.
        let file2 = dir.join("44444444-5555-6666-7777-888888888888.jsonl");
        let body2 = json!({"type": "user", "cwd": "/tmp", "gitBranch": "", "message": {"content": "hi"}})
            .to_string();
        fs::write(&file2, body2).unwrap();
        let s2 = parse_session(file2, false).expect("session parsed");
        assert_eq!(s2.branch, None);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn format_row_colors_branch_separately_from_dir() {
        let cwd = home().join("proj").to_string_lossy().into_owned();
        let s = Session {
            id: "id".into(),
            cwd,
            label: "Fix the bug".into(),
            haystack: String::new(),
            branch: Some("feature/foo".into()),
            mtime: 0,
            file: PathBuf::from("/dev/null"),
        };
        let row = format_row(&s);
        assert!(row.contains(&format!("{CYAN}~/proj{RESET}")));
        assert!(row.contains(&format!("{MAGENTA}[feature/foo]{RESET}")));

        let mut no_branch = s;
        no_branch.branch = None;
        assert!(!format_row(&no_branch).contains(MAGENTA));
    }

    #[test]
    fn is_sidechain_detection() {
        assert!(is_sidechain(&json!({"isSidechain": true})));
        assert!(!is_sidechain(&json!({"isSidechain": false})));
        assert!(!is_sidechain(&json!({})));
    }

    #[test]
    fn parse_session_prefers_ai_title_over_first_prompt() {
        let dir = std::env::temp_dir().join(format!("crf-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("11111111-2222-3333-4444-555555555555.jsonl");
        let body = [
            json!({"type": "user", "cwd": "/tmp", "message": {"content": "first prompt here"}}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "sure"}]}}),
            json!({"type": "ai-title", "aiTitle": "Fix the login bug"}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        fs::write(&file, body).unwrap();

        let s = parse_session(file.clone(), true).expect("session parsed");
        assert_eq!(s.label, "Fix the login bug");
        assert_eq!(s.cwd, "/tmp");
        assert_eq!(s.id, "11111111-2222-3333-4444-555555555555");
        assert!(s.haystack.contains("first prompt here"));

        // Default (non-full) mode skips building the haystack.
        let s = parse_session(file, false).expect("session parsed");
        assert!(s.haystack.is_empty());

        fs::remove_dir_all(&dir).ok();
    }
}
