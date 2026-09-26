use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use termimad::MadSkin;

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

/// Local timezone's current offset from UTC, in seconds.
fn local_utc_offset(now: u64) -> i64 {
    let t = now as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    #[allow(clippy::useless_conversion)] // c_long is i32 on 32-bit targets
    i64::from(tm.tm_gmtoff)
}

fn rel_time(mtime: u64) -> String {
    let now = now_secs();
    rel_time_from(now, mtime, local_utc_offset(now))
}

const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]; // day 0 = 1970-01-01

/// Before today's local midnight but within the previous 6 days, show the
/// weekday name; otherwise a relative age. Uses today's UTC offset for the
/// whole week, so a DST change in that window can shift the boundary by an hour.
fn rel_time_from(now: u64, mtime: u64, utc_offset: i64) -> String {
    let local_day = |t: u64| (t as i64 + utc_offset).div_euclid(86400);
    let days_ago = local_day(now) - local_day(mtime);
    if (1..7).contains(&days_ago) {
        return WEEKDAYS[local_day(mtime).rem_euclid(7) as usize].to_string();
    }
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

/// Build the tab-delimited fzf input: display col + hidden cwd/id/file cols,
/// one line per session. Shared by the initial list and the `--list`
/// subcommand fzf calls to reload after a delete.
fn build_input(full: bool) -> String {
    let mut input = String::new();
    for s in &collect_sessions(full) {
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
    input
}

/// Delete a session file under `~/.claude/projects`.
pub fn delete_session(file: &str) {
    delete_session_under(file, &projects_dir());
}

/// Delete a session file, refusing anything outside `root` as a safety net
/// against a malformed or tampered path reaching this command. Also removes
/// the parent project folder if that was its last session. `root` is
/// injectable so tests can point it at a scratch directory.
fn delete_session_under(file: &str, root: &Path) {
    let path = Path::new(file);
    let projects = match fs::canonicalize(root) {
        Ok(p) => p,
        Err(_) => return,
    };
    let canon = match fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => return,
    };
    if !canon.starts_with(&projects) {
        eprintln!("refusing to delete outside {}: {}", projects.display(), canon.display());
        return;
    }
    let _ = fs::remove_file(&canon);
    if let Some(parent) = canon.parent() {
        if fs::read_dir(parent).is_ok_and(|mut it| it.next().is_none()) {
            let _ = fs::remove_dir(parent);
        }
    }
}

/// Seconds within which a second ctrl-x on the *same* session confirms the
/// delete; ctrl-x on a different session (or after the window lapses) just
/// re-arms instead.
const CONFIRM_WINDOW_SECS: u64 = 4;

/// Where the pending "armed" delete is recorded between the two ctrl-x
/// presses (two separate process invocations, since each key press runs a
/// fresh `execute-silent`). Scoped per-user since /tmp can be shared.
fn armed_state_path() -> PathBuf {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    std::env::temp_dir().join(format!("ccresume-armed-{user}"))
}

fn read_armed_state(state_path: &Path) -> Option<(String, u64)> {
    let s = fs::read_to_string(state_path).ok()?;
    let (file, ts) = s.split_once('\t')?;
    Some((file.to_string(), ts.trim().parse().ok()?))
}

/// True if `file` was armed at `state_path` within the confirm window as of `now`.
fn is_confirming(file: &str, state_path: &Path, now: u64) -> bool {
    matches!(
        read_armed_state(state_path),
        Some((armed_file, ts)) if armed_file == file && now.saturating_sub(ts) <= CONFIRM_WINDOW_SECS
    )
}

/// First ctrl-x on a session arms it (recorded to disk) without deleting.
/// A second ctrl-x on that *same* session within `CONFIRM_WINDOW_SECS`
/// deletes it. Anything else (different session, or the window lapsed)
/// re-arms rather than deleting, so a stray keypress can't destroy the
/// wrong session.
pub fn arm_or_delete(file: &str) {
    arm_or_delete_at(file, &armed_state_path(), &projects_dir());
}

fn arm_or_delete_at(file: &str, state_path: &Path, root: &Path) {
    let now = now_secs();
    if is_confirming(file, state_path, now) {
        delete_session_under(file, root);
        let _ = fs::remove_file(state_path);
        return;
    }
    let _ = fs::write(state_path, format!("{file}\t{now}"));
}

/// Header text for fzf's `transform-header`, reflecting whether `file` is
/// currently armed for deletion.
pub fn header_status(file: &str) -> String {
    header_status_at(file, &armed_state_path())
}

fn header_status_at(file: &str, state_path: &Path) -> String {
    if is_confirming(file, state_path, now_secs()) {
        "enter: resume   ctrl-x again to confirm delete!".to_string()
    } else {
        "enter: resume   ctrl-x: delete session".to_string()
    }
}

/// Interactive picker: list sessions in fzf, then resume the chosen one.
/// When `full` is set, the whole transcript is searchable; otherwise fzf
/// searches only the visible label (title/first prompt) and directory.
pub fn run(full: bool) -> ! {
    let input = build_input(full);
    if input.is_empty() {
        eprintln!(
            "No Claude sessions found under {}",
            projects_dir().display()
        );
        std::process::exit(1);
    }

    let self_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("claude-resume-fzf"));

    let preview_cmd = format!("{} --preview {{4}}", self_exe.display());
    let list_flag = if full { "--list-full" } else { "--list" };
    // ctrl-x arms the highlighted session and updates the header to prompt
    // for confirmation; a second ctrl-x on the same session within a few
    // seconds actually deletes it and reloads the list in place, without
    // exiting fzf. execute-silent avoids flashing the screen for the delete;
    // transform-header re-reads the (just-updated) arm state to show it.
    let delete_bind = format!(
        "ctrl-x:execute-silent({exe} --arm-or-delete {{4}})+reload({exe} {list})+transform-header({exe} --header-status {{4}})",
        exe = self_exe.display(),
        list = list_flag,
    );

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
            "--header=enter: resume   ctrl-x: delete session",
        ])
        // Fuzzy matching over a 20 KB transcript blob matches almost everything,
        // so full-text mode uses exact substring matching instead.
        .args(if full { &["--exact"][..] } else { &[][..] })
        .arg("--preview")
        .arg(&preview_cmd)
        .arg("--bind")
        .arg(&delete_bind)
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
pub fn preview(file: &str) {
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
    let skin = preview_skin();
    if let Some(lp) = &last_prompt {
        write_markdown(&mut out, &skin, "latest:", CYAN, &truncate_preserving_structure(lp, 800), width);
    }
    let _ = writeln!(out, "{DIM}{}{RESET}", "─".repeat(40));
    for (label, color, text) in turns.iter().take(40) {
        write_markdown(
            &mut out,
            &skin,
            &format!("{label}:"),
            color,
            &truncate_preserving_structure(text, 4000),
            width,
        );
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

/// Skin termimad uses to render message bodies. Left mostly at termimad's
/// defaults (tuned for a dark terminal); only bold is nudged to match the
/// rest of the preview's cyan accent instead of termimad's default white.
fn preview_skin() -> MadSkin {
    let mut skin = MadSkin::default();
    skin.bold.set_fg(termimad::crossterm::style::Color::Cyan);
    skin
}

/// Print a `label:` line, then `text` rendered as Markdown (headers, bold,
/// code blocks, lists, ...) via termimad, indented two spaces under it.
fn write_markdown(out: &mut impl Write, skin: &MadSkin, label: &str, color: &str, text: &str, width: usize) {
    let _ = writeln!(out, "{color}{label}{RESET}");
    let rendered = skin.text(text, Some(width.saturating_sub(2).max(10))).to_string();
    for line in rendered.lines() {
        let _ = writeln!(out, "  {line}");
    }
}

/// Truncate `text` to at most `max_chars`, preserving newlines so Markdown
/// structure (code fences, lists, paragraphs) survives for the renderer —
/// unlike `truncate()`, which flattens everything to one line for the
/// single-line list row.
fn truncate_preserving_structure(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        let t: String = text.chars().take(max_chars).collect();
        format!("{t}…")
    } else {
        text.to_string()
    }
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
                preview(file);
            }
            std::process::exit(0);
        }
        // Internal: used by fzf's reload() binding to refresh the list after a delete.
        Some("--list") => {
            print!("{}", build_input(false));
            std::process::exit(0);
        }
        Some("--list-full") => {
            print!("{}", build_input(true));
            std::process::exit(0);
        }
        // Internal: used by fzf's ctrl-x binding. First call on a session arms
        // it; a second call on the same session within the confirm window
        // deletes it. Not meant to be run by hand.
        Some("--arm-or-delete") => {
            if let Some(file) = args.get(1) {
                arm_or_delete(file);
            }
            std::process::exit(0);
        }
        // Internal: used by fzf's transform-header to reflect arm state.
        Some("--header-status") => {
            let file = args.get(1).map(String::as_str).unwrap_or("");
            println!("{}", header_status(file));
            std::process::exit(0);
        }
        // Delete a session outright, no confirmation. Not wired to any fzf
        // binding; kept as a scriptable escape hatch.
        Some("--delete") => {
            if let Some(file) = args.get(1) {
                delete_session(file);
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
                 \x20 ctrl-x        press twice to delete the selected session\n\
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
    fn delete_session_removes_file_and_empty_parent() {
        let root = std::env::temp_dir().join(format!("crf-delete-test-{}", std::process::id()));
        let proj_dir = root.join("-tmp-someproject");
        fs::create_dir_all(&proj_dir).unwrap();
        let file = proj_dir.join("session-a.jsonl");
        fs::write(&file, "{}").unwrap();

        delete_session_under(file.to_str().unwrap(), &root);

        assert!(!file.exists());
        assert!(!proj_dir.exists(), "empty parent project dir should be removed too");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn delete_session_keeps_parent_with_other_sessions() {
        let root = std::env::temp_dir().join(format!("crf-delete-test2-{}", std::process::id()));
        let proj_dir = root.join("-tmp-someproject");
        fs::create_dir_all(&proj_dir).unwrap();
        let file_a = proj_dir.join("session-a.jsonl");
        let file_b = proj_dir.join("session-b.jsonl");
        fs::write(&file_a, "{}").unwrap();
        fs::write(&file_b, "{}").unwrap();

        delete_session_under(file_a.to_str().unwrap(), &root);

        assert!(!file_a.exists());
        assert!(proj_dir.exists(), "parent dir still has session-b, must stay");
        assert!(file_b.exists());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn delete_session_refuses_path_outside_root() {
        let root = std::env::temp_dir().join(format!("crf-delete-test3-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let outside = std::env::temp_dir().join(format!("crf-outside-{}.jsonl", std::process::id()));
        fs::write(&outside, "{}").unwrap();

        delete_session_under(outside.to_str().unwrap(), &root);

        assert!(outside.exists(), "must not delete anything outside root");

        fs::remove_file(&outside).ok();
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn first_ctrl_x_arms_without_deleting() {
        let root = std::env::temp_dir().join(format!("crf-arm-test-{}", std::process::id()));
        let proj_dir = root.join("-tmp-someproject");
        fs::create_dir_all(&proj_dir).unwrap();
        let file = proj_dir.join("session-a.jsonl");
        fs::write(&file, "{}").unwrap();
        let state = root.join("armed-state");

        arm_or_delete_at(file.to_str().unwrap(), &state, &root);

        assert!(file.exists(), "first press must only arm, not delete");
        assert_eq!(
            header_status_at(file.to_str().unwrap(), &state),
            "enter: resume   ctrl-x again to confirm delete!"
        );
        // A different session shouldn't show as armed even while this one is.
        assert_eq!(
            header_status_at("/somewhere/else.jsonl", &state),
            "enter: resume   ctrl-x: delete session"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn second_ctrl_x_on_same_session_deletes() {
        let root = std::env::temp_dir().join(format!("crf-arm-test2-{}", std::process::id()));
        let proj_dir = root.join("-tmp-someproject");
        fs::create_dir_all(&proj_dir).unwrap();
        let file = proj_dir.join("session-a.jsonl");
        fs::write(&file, "{}").unwrap();
        let state = root.join("armed-state");
        let f = file.to_str().unwrap();

        arm_or_delete_at(f, &state, &root); // arm
        arm_or_delete_at(f, &state, &root); // confirm

        assert!(!file.exists(), "second press on the same session must delete");
        assert!(
            !state.exists(),
            "arm state should be cleared after a confirmed delete"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ctrl_x_on_a_different_session_rearms_instead_of_deleting() {
        let root = std::env::temp_dir().join(format!("crf-arm-test3-{}", std::process::id()));
        let proj_dir = root.join("-tmp-someproject");
        fs::create_dir_all(&proj_dir).unwrap();
        let file_a = proj_dir.join("session-a.jsonl");
        let file_b = proj_dir.join("session-b.jsonl");
        fs::write(&file_a, "{}").unwrap();
        fs::write(&file_b, "{}").unwrap();
        let state = root.join("armed-state");

        arm_or_delete_at(file_a.to_str().unwrap(), &state, &root); // arm a
        arm_or_delete_at(file_b.to_str().unwrap(), &state, &root); // move to b

        assert!(file_a.exists(), "switching selection must not delete the old arm target");
        assert!(file_b.exists(), "moving to a new session only re-arms it");
        assert_eq!(
            header_status_at(file_b.to_str().unwrap(), &state),
            "enter: resume   ctrl-x again to confirm delete!"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stale_arm_past_the_confirm_window_rearms_instead_of_deleting() {
        let root = std::env::temp_dir().join(format!("crf-arm-test4-{}", std::process::id()));
        let proj_dir = root.join("-tmp-someproject");
        fs::create_dir_all(&proj_dir).unwrap();
        let file = proj_dir.join("session-a.jsonl");
        fs::write(&file, "{}").unwrap();
        let state = root.join("armed-state");
        let f = file.to_str().unwrap();

        // Simulate an arm from long ago, past CONFIRM_WINDOW_SECS.
        fs::write(&state, format!("{f}\t{}", now_secs() - CONFIRM_WINDOW_SECS - 1)).unwrap();

        arm_or_delete_at(f, &state, &root);

        assert!(file.exists(), "a stale arm must not silently confirm a delete");

        fs::remove_dir_all(&root).ok();
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
    fn truncate_preserving_structure_keeps_newlines() {
        let md = "line one\n\n- a\n- b\n\n```\ncode\n```";
        assert_eq!(truncate_preserving_structure(md, 1000), md);
        let long = "x".repeat(100);
        let out = truncate_preserving_structure(&long, 10);
        assert_eq!(out.chars().count(), 11); // 10 chars + ellipsis
        assert!(out.ends_with('…'));
    }

    #[test]
    fn rel_time_buckets() {
        assert_eq!(rel_time_from(30, 0, 0), "30s");
        assert_eq!(rel_time_from(120, 0, 0), "2m");
        assert_eq!(rel_time_from(7200, 0, 0), "2h");
        assert_eq!(rel_time_from(8 * 86400, 0, 0), "8d");
        // Clock skew must not underflow.
        assert_eq!(rel_time_from(0, 100, 0), "0s");
    }

    #[test]
    fn rel_time_weekday_names_within_past_week() {
        // 2026-09-25 (Fri) 10:00 UTC.
        let now = 1_790_330_400;
        let h = 3600;
        // Earlier today stays relative.
        assert_eq!(rel_time_from(now, now - 9 * h, 0), "9h");
        // Just before midnight yesterday.
        assert_eq!(rel_time_from(now, now - 10 * h - 1, 0), "Thu");
        assert_eq!(rel_time_from(now, now - 3 * 86400, 0), "Tue");
        assert_eq!(rel_time_from(now, now - 6 * 86400, 0), "Sat");
        // A week ago falls back to the day count, not an ambiguous "Fri".
        assert_eq!(rel_time_from(now, now - 7 * 86400, 0), "7d");
        // Local midnight respects the UTC offset: 11h back is still "today" at UTC+2.
        assert_eq!(rel_time_from(now, now - 11 * h, 2 * h as i64), "11h");
        assert_eq!(rel_time_from(now, now - 11 * h, 0), "Thu");
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
