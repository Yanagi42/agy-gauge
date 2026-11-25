//! Multi-agent session resume & workspace launcher helpers for Windows.
//!
//! Supported CLI resume dispatch by `SourceKind`:
//!   - `SourceKind::Cli | SourceKind::Custom` -> `agy --conversation <id>`
//!   - `SourceKind::ClaudeCode` -> `claude --resume <id>` (subagents route to parent session)
//!   - `SourceKind::Codex` -> `codex resume <id>` (subagents route to parent session)
//!   - `SourceKind::GeminiCli` -> `gemini --resume <id>`
//!   - `SourceKind::App | SourceKind::Ide` -> `BlockedReason::NonCliSource`
//!
//! Safety & Guarantees:
//!   - Strictly read-only on all local agent data directories (`~/.gemini`, `~/.claude`, `~/.codex`).
//!   - Strictly validates `conversation_id` (`[A-Za-z0-9_-]{1,128}`, no leading `-`) before
//!     formatting commands or spawning processes.
//!   - Safely quotes `.exe`, `.cmd`, and `.bat` binaries (including Windows npm `.cmd` shims) across
//!     `wt.exe`, `pwsh.exe`/`powershell.exe`, and `cmd.exe /D /K` with zero shell injection.
//!   - Non-blocking spawns (`Stdio::null()`), never blocks the Win32 UI message loop.
//!   - Uses `ShellExecuteW(..., "explore", ...)` + `is_dir()` check to avoid accidental execution.

use std::ffi::OsStr;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::CREATE_NEW_CONSOLE;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::stats::{SessionInfo, SourceKind};

const MAX_WT_TITLE_CHARS: usize = 36;

/// Structured reason why a session cannot be directly resumed via a terminal CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedReason {
    ArchivedOnly,
    SubagentOnly,
    NonCliSource,
    InvalidConversationId,
}

impl BlockedReason {
    pub fn message_cn(self) -> &'static str {
        match self {
            BlockedReason::ArchivedOnly => "已归档会话（原日志已清理，无法恢复）",
            BlockedReason::SubagentOnly => "子会话不支持直连恢复，请跳转父会话恢复",
            BlockedReason::NonCliSource => "桌面 App / IDE 会话暂不支持通过终端 CLI 恢复",
            BlockedReason::InvalidConversationId => "无效的会话 ID，拒绝启动终端",
        }
    }
}

/// CLI binary and resume subcommand/flag specification for a resumable `SourceKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeCliSpec {
    pub source: SourceKind,
    /// Human-readable command name (`agy`, `claude`, `codex`, `gemini`).
    pub command_name: &'static str,
    /// Single flag or subcommand placed before `<conversation_id>` (`--conversation`, `--resume`, `resume`).
    pub resume_arg: &'static str,
}

/// Maps a `SourceKind` to its terminal CLI resume spec, or `Err(BlockedReason::NonCliSource)`
/// for desktop App / IDE sessions.
pub fn resume_cli_spec(source: SourceKind) -> Result<ResumeCliSpec, BlockedReason> {
    match source {
        SourceKind::Cli | SourceKind::Custom => Ok(ResumeCliSpec {
            source,
            command_name: "agy",
            resume_arg: "--conversation",
        }),
        SourceKind::ClaudeCode => Ok(ResumeCliSpec {
            source,
            command_name: "claude",
            resume_arg: "--resume",
        }),
        SourceKind::Codex => Ok(ResumeCliSpec {
            source,
            command_name: "codex",
            resume_arg: "resume",
        }),
        SourceKind::GeminiCli => Ok(ResumeCliSpec {
            source,
            command_name: "gemini",
            resume_arg: "--resume",
        }),
        SourceKind::App | SourceKind::Ide => Err(BlockedReason::NonCliSource),
    }
}

/// Returns `true` when `source` supports direct terminal CLI session resumption.
pub fn is_cli_resumable_source(source: SourceKind) -> bool {
    resume_cli_spec(source).is_ok()
}

/// Returns the CLI command name (`agy`, `claude`, `codex`, `gemini`) for `source`, or `None` for App/IDE.
pub fn cli_binary_name_for_source(source: SourceKind) -> Option<&'static str> {
    resume_cli_spec(source).ok().map(|spec| spec.command_name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeCapability {
    CliDirect,
    ArchivedOnly,
    Subagent {
        parent_id: Option<String>,
        parent_resumable: bool,
    },
    UnsupportedSource(SourceKind),
}

impl ResumeCapability {
    pub fn is_direct_resumable(&self) -> bool {
        matches!(self, ResumeCapability::CliDirect)
    }

    pub fn blocked_reason(&self) -> Option<BlockedReason> {
        match self {
            ResumeCapability::CliDirect => None,
            ResumeCapability::ArchivedOnly => Some(BlockedReason::ArchivedOnly),
            ResumeCapability::Subagent { .. } => Some(BlockedReason::SubagentOnly),
            ResumeCapability::UnsupportedSource(_) => Some(BlockedReason::NonCliSource),
        }
    }

    pub fn disabled_reason_cn(&self) -> Option<&'static str> {
        match self {
            ResumeCapability::CliDirect => None,
            ResumeCapability::ArchivedOnly => Some("已归档会话（原日志已清理，无法恢复）"),
            ResumeCapability::Subagent {
                parent_resumable: true,
                ..
            } => Some("子会话不支持直连恢复，请跳转父会话恢复"),
            ResumeCapability::Subagent {
                parent_resumable: false,
                ..
            } => Some("子会话不支持终端恢复"),
            ResumeCapability::UnsupportedSource(SourceKind::App) => {
                Some("App 桌面端会话暂不支持通过终端 CLI 恢复")
            }
            ResumeCapability::UnsupportedSource(SourceKind::Ide) => {
                Some("IDE 会话暂不支持通过终端 CLI 恢复")
            }
            ResumeCapability::UnsupportedSource(_) => {
                Some("该来源会话暂不支持通过终端 CLI 恢复")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchBackend {
    WindowsTerminal,
    Pwsh,
    Cmd,
}

impl LaunchBackend {
    pub fn label(self) -> &'static str {
        match self {
            LaunchBackend::WindowsTerminal => "Windows Terminal",
            LaunchBackend::Pwsh => "PowerShell",
            LaunchBackend::Cmd => "命令提示符",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOutcome {
    pub backend: LaunchBackend,
    pub used_dir: PathBuf,
    pub workspace_fallback: bool,
}

/// Resolves the resumable parent session for a subagent within the same `AgentFamily`.
pub fn find_resumable_parent<'a>(
    session: &SessionInfo,
    all_sessions: &'a [SessionInfo],
) -> Option<&'a SessionInfo> {
    if !session.is_subagent {
        return None;
    }
    let pid = session.parent_id.as_deref()?;
    let family = session.source.agent_family();
    let parent = all_sessions
        .iter()
        .find(|s| s.conversation_id == pid && s.source.agent_family() == family)?;
    if !parent.is_archived
        && !parent.is_subagent
        && is_cli_resumable_source(parent.source)
        && validate_conversation_id(&parent.conversation_id)
    {
        Some(parent)
    } else {
        None
    }
}

/// Classifies whether a session can be resumed directly via its agent CLI (`agy`, `claude`,
/// `codex`, `gemini`), or must jump to a parent session / be blocked.
pub fn classify_resume_capability(
    session: &SessionInfo,
    all_sessions: &[SessionInfo],
) -> ResumeCapability {
    if session.is_archived {
        return ResumeCapability::ArchivedOnly;
    }
    if session.is_subagent {
        let parent_resumable = find_resumable_parent(session, all_sessions).is_some();
        return ResumeCapability::Subagent {
            parent_id: session.parent_id.clone(),
            parent_resumable,
        };
    }
    if !is_cli_resumable_source(session.source) {
        return ResumeCapability::UnsupportedSource(session.source);
    }
    ResumeCapability::CliDirect
}

/// Strict allowlist validation for conversation IDs (UUID or safe `[A-Za-z0-9_-]{1,128}`,
/// disallowing leading `-` to prevent CLI flag injection).
pub fn validate_conversation_id(cid: &str) -> bool {
    let bytes = cid.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 || bytes[0] == b'-' {
        return false;
    }
    bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Returns `true` when `path` points to a Windows batch shim (`.cmd` or `.bat`).
pub fn is_batch_shim(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
        .unwrap_or(false)
}

/// Pure normalization of a workspace string into a clean Windows directory path string.
/// Rejects placeholders (`(未知工作区)`, `(unknown)`), empty strings, control chars, and URLs.
pub fn normalize_workspace_path_str(ws: &str) -> Option<String> {
    let trimmed = ws.trim();
    if trimmed.is_empty()
        || trimmed == "(未知工作区)"
        || trimmed == "(unknown)"
        || trimmed.chars().any(char::is_control)
    {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("file://") {
        return None;
    }

    let mut norm = trimmed.replace('/', "\\");
    // Preserve drive roots like `C:\` and UNC roots like `\\server\share`, trim redundant trailing `\`.
    while norm.ends_with('\\') {
        let is_drive_root = norm.len() == 3
            && norm.as_bytes()[0].is_ascii_alphabetic()
            && norm.as_bytes()[1] == b':';
        if is_drive_root {
            break;
        }
        norm.pop();
    }
    if norm.is_empty() || norm == "\\" {
        return None;
    }
    // Convert bare drive like `C:` into `C:\`.
    if norm.len() == 2 && norm.as_bytes()[0].is_ascii_alphabetic() && norm.as_bytes()[1] == b':' {
        norm.push('\\');
    }
    Some(norm)
}

/// Normalizes a workspace string and verifies it points to an existing local directory.
pub fn normalize_workspace_dir(ws: &str) -> Option<PathBuf> {
    let norm = normalize_workspace_path_str(ws)?;
    let path = PathBuf::from(norm);
    if fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false) {
        Some(path)
    } else {
        None
    }
}

fn is_existing_executable_entry(candidate: &Path) -> bool {
    candidate.is_file()
        || fs::symlink_metadata(candidate)
            .map(|m| !m.is_dir())
            .unwrap_or(false)
}

fn find_exe_in_path(names: &[&str]) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for name in names {
            let candidate = dir.join(name);
            if is_existing_executable_entry(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Searches `dirs` in order for `<command_name>.exe`, `<command_name>.cmd`, then `<command_name>.bat`.
/// Deliberately skips extensionless POSIX scripts (such as npm's Unix shims) and directories.
pub fn find_cli_in_dirs(command_name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let candidates = [
        format!("{command_name}.exe"),
        format!("{command_name}.cmd"),
        format!("{command_name}.bat"),
    ];
    for dir in dirs {
        for name in &candidates {
            let candidate = dir.join(name);
            if is_existing_executable_entry(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn find_cli_in_path(command_name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();
    find_cli_in_dirs(command_name, &dirs)
}

fn first_existing_file(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|p| is_existing_executable_entry(p))
        .cloned()
}

/// Resolves the CLI executable/shim (`.exe`, `.cmd`, `.bat`) for `source` via `PATH`,
/// falling back to common Windows install locations (`%LOCALAPPDATA%\agy\bin`,
/// `%USERPROFILE%\.local\bin`, `%APPDATA%\npm`) so startup-launched instances still
/// find CLIs even when Explorer inherited an older `PATH`.
pub fn resolve_cli_exe_for_source(source: SourceKind) -> Option<PathBuf> {
    let spec = resume_cli_spec(source).ok()?;
    if let Some(found) = find_cli_in_path(spec.command_name) {
        return Some(found);
    }

    let local_app_data = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let app_data = std::env::var_os("APPDATA").map(PathBuf::from);
    let user_profile = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);

    let mut fallbacks = Vec::new();
    match source {
        SourceKind::Cli | SourceKind::Custom => {
            if let Some(local) = &local_app_data {
                let bin = local.join("agy").join("bin");
                fallbacks.push(bin.join("agy.exe"));
                fallbacks.push(bin.join("agy.cmd"));
                fallbacks.push(bin.join("agy.bat"));
            }
        }
        SourceKind::ClaudeCode => {
            if let Some(home) = &user_profile {
                fallbacks.push(home.join(".local").join("bin").join("claude.exe"));
            }
            if let Some(local) = &local_app_data {
                fallbacks.push(local.join("Programs").join("claude").join("claude.exe"));
            }
            if let Some(roaming) = &app_data {
                let npm = roaming.join("npm");
                fallbacks.push(npm.join("claude.exe"));
                fallbacks.push(npm.join("claude.cmd"));
                fallbacks.push(npm.join("claude.bat"));
            }
        }
        SourceKind::Codex => {
            if let Some(roaming) = &app_data {
                let npm = roaming.join("npm");
                fallbacks.push(npm.join("codex.exe"));
                fallbacks.push(npm.join("codex.cmd"));
                fallbacks.push(npm.join("codex.bat"));
            }
            if let Some(home) = &user_profile {
                fallbacks.push(home.join(".local").join("bin").join("codex.exe"));
            }
        }
        SourceKind::GeminiCli => {
            if let Some(roaming) = &app_data {
                let npm = roaming.join("npm");
                fallbacks.push(npm.join("gemini.exe"));
                fallbacks.push(npm.join("gemini.cmd"));
                fallbacks.push(npm.join("gemini.bat"));
            }
            if let Some(home) = &user_profile {
                fallbacks.push(home.join(".local").join("bin").join("gemini.exe"));
            }
        }
        SourceKind::App | SourceKind::Ide => return None,
    }
    first_existing_file(&fallbacks)
}

/// Resolves `agy` (`.exe`/`.cmd`/`.bat`) via `PATH`, falling back to `%LOCALAPPDATA%\agy\bin`.
pub fn resolve_agy_exe() -> Option<PathBuf> {
    resolve_cli_exe_for_source(SourceKind::Cli)
}

fn resolve_wt_exe() -> Option<PathBuf> {
    if let Some(found) = find_exe_in_path(&["wt.exe"]) {
        return Some(found);
    }
    let local_app_data = std::env::var_os("LOCALAPPDATA")?;
    let fallback = PathBuf::from(local_app_data)
        .join("Microsoft")
        .join("WindowsApps")
        .join("wt.exe");
    if is_existing_executable_entry(&fallback) {
        Some(fallback)
    } else {
        None
    }
}

fn resolve_pwsh_exe() -> Option<PathBuf> {
    if let Some(found) = find_exe_in_path(&["pwsh.exe", "powershell.exe"]) {
        return Some(found);
    }
    let pwsh7 = PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe");
    if pwsh7.is_file() {
        return Some(pwsh7);
    }
    None
}

fn user_home_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let p = PathBuf::from(home);
        if p.is_dir() {
            return p;
        }
    }
    PathBuf::from(r"C:\")
}

/// Wraps text in PowerShell single-quoted literal syntax (`'` escaped as `''`).
pub fn ps_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn is_cmd_safe_path_str(exe_str: &str) -> bool {
    !exe_str.is_empty()
        && !exe_str.chars().any(|c| {
            c.is_control()
                || matches!(
                    c,
                    '"' | '&' | '|' | '<' | '>' | '%' | '^' | '!'
                )
        })
}

fn validate_exe_path_for_shell(exe: &Path, require_cmd_safe: bool, command_name: &str) -> Result<String, String> {
    let exe_str = exe.to_string_lossy().into_owned();
    if exe_str.is_empty() || exe_str.chars().any(char::is_control) {
        return Err(format!("{command_name} 路径为空或包含控制字符，拒绝启动"));
    }
    if (require_cmd_safe || is_batch_shim(exe)) && !is_cmd_safe_path_str(&exe_str) {
        return Err(format!(
            "{command_name} 路径包含不支持 cmd.exe 启动的特殊字符"
        ));
    }
    Ok(exe_str)
}

/// Formats a copy-ready PowerShell command to resume `cid` for `source` in optional directory `ws`.
pub fn format_resume_command_for_source(
    source: SourceKind,
    cid: &str,
    ws: Option<&Path>,
) -> Result<String, String> {
    if !validate_conversation_id(cid) {
        return Err("无效的会话 ID，无法生成恢复命令".to_string());
    }
    let spec = resume_cli_spec(source)
        .map_err(|_| format!("{} 会话暂不支持通过终端 CLI 恢复", source.label_short()))?;
    let invoke = format!("{} {} {}", spec.command_name, spec.resume_arg, cid);
    let clean_dir = ws
        .and_then(|p| p.to_str())
        .and_then(normalize_workspace_path_str);
    match clean_dir {
        Some(dir) => Ok(format!(
            "Set-Location -LiteralPath {}; {}",
            ps_single_quote(&dir),
            invoke
        )),
        None => Ok(invoke),
    }
}

/// Convenience wrapper formatting a copy-ready PowerShell resume command directly from `SessionInfo`.
pub fn format_resume_command_for_session(session: &SessionInfo) -> Result<String, String> {
    let ws = normalize_workspace_path_str(&session.workspace).map(PathBuf::from);
    format_resume_command_for_source(session.source, &session.conversation_id, ws.as_deref())
}

/// Formats a copy-ready PowerShell command to resume an Antigravity CLI session `cid` in optional directory `ws`.
pub fn format_resume_command(cid: &str, ws: Option<&Path>) -> Result<String, String> {
    format_resume_command_for_source(SourceKind::Cli, cid, ws)
}

/// Sanitizes a session title for `wt.exe --title`: strips `;` (WT command separator),
/// quotes, and control chars, then caps at `MAX_WT_TITLE_CHARS`.
pub fn sanitize_wt_title(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut prev_space = true;
    for ch in raw.chars() {
        if ch == ';' || ch == '"' || ch == '\'' || ch.is_control() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() > MAX_WT_TITLE_CHARS {
        let mut cut: String = trimmed
            .chars()
            .take(MAX_WT_TITLE_CHARS.saturating_sub(1))
            .collect();
        cut.push('…');
        Some(cut)
    } else {
        Some(trimmed.to_string())
    }
}

/// Escapes a directory path for `wt.exe -d`:
/// - converts `/` to `\`
/// - converts drive roots (`C:\`) to `C:\.` and strips other trailing `\` so a trailing
///   backslash never escapes a closing quote in Win32 `CommandLineToArgvW`
/// - escapes `;` to `\;` so `wt.exe` does not treat it as a subcommand delimiter
pub fn escape_wt_dir_arg(dir: &Path) -> String {
    let raw = dir.to_string_lossy().replace('/', "\\");
    let mut trimmed = raw.trim().to_string();
    while trimmed.ends_with('\\') {
        let is_drive_root = trimmed.len() == 3
            && trimmed.as_bytes()[0].is_ascii_alphabetic()
            && trimmed.as_bytes()[1] == b':';
        if is_drive_root {
            trimmed.push('.');
            break;
        }
        trimmed.pop();
    }
    if trimmed.len() == 2
        && trimmed.as_bytes()[0].is_ascii_alphabetic()
        && trimmed.as_bytes()[1] == b':'
    {
        trimmed.push_str(r"\.");
    }
    trimmed.replace(';', r"\;")
}

fn escape_wt_token(s: &str) -> String {
    s.replace(';', r"\;")
}

fn build_wt_args_inner(
    spec: ResumeCliSpec,
    cli_exe: &Path,
    exe_str: &str,
    cid: &str,
    work_dir: &Path,
    tab_title: Option<&str>,
    shim_shell_exe: Option<&Path>,
) -> Vec<String> {
    let mut args = vec![
        "-w".to_string(),
        "0".to_string(),
        "new-tab".to_string(),
        "-d".to_string(),
        escape_wt_dir_arg(work_dir),
    ];
    if let Some(title) = tab_title.and_then(sanitize_wt_title) {
        args.push("--title".to_string());
        args.push(title);
    }
    args.push("--".to_string());
    if is_batch_shim(cli_exe) {
        // Windows Terminal cannot spawn `.cmd`/`.bat` shims directly via CreateProcessW;
        // wrap via PowerShell single-quoted call operator so paths with spaces, `(x86)`,
        // apostrophes, or `;` work safely with zero cmd quote-stripping or injection issues.
        let shell_token = shim_shell_exe
            .map(|p| escape_wt_token(&p.to_string_lossy()))
            .unwrap_or_else(|| "powershell.exe".to_string());
        let ps_cmd = format!(
            "& {} {} {}",
            ps_single_quote(exe_str),
            spec.resume_arg,
            cid
        );
        args.push(shell_token);
        args.push("-NoLogo".to_string());
        args.push("-NoExit".to_string());
        args.push("-Command".to_string());
        args.push(escape_wt_token(&ps_cmd));
    } else {
        args.push(escape_wt_token(exe_str));
        args.push(spec.resume_arg.to_string());
        args.push(cid.to_string());
    }
    args
}

/// Builds the argument vector for `wt.exe` to resume a session of `source` in `work_dir`.
/// Handles both native `.exe` binaries and Windows `.cmd`/`.bat` shims safely.
pub fn build_wt_args_for_source(
    source: SourceKind,
    cli_exe: &Path,
    cid: &str,
    work_dir: &Path,
    tab_title: Option<&str>,
) -> Result<Vec<String>, String> {
    if !validate_conversation_id(cid) {
        return Err("无效的会话 ID".to_string());
    }
    let spec = resume_cli_spec(source)
        .map_err(|_| format!("{} 会话暂不支持通过终端 CLI 恢复", source.label_short()))?;
    let exe_str = validate_exe_path_for_shell(cli_exe, false, spec.command_name)?;
    Ok(build_wt_args_inner(
        spec, cli_exe, &exe_str, cid, work_dir, tab_title, None,
    ))
}

/// Builds the argument vector for `wt.exe` to resume an Antigravity CLI session `cid` in `work_dir`.
pub fn build_wt_args(
    agy_exe: &Path,
    cid: &str,
    work_dir: &Path,
    tab_title: Option<&str>,
) -> Vec<String> {
    let spec = ResumeCliSpec {
        source: SourceKind::Cli,
        command_name: "agy",
        resume_arg: "--conversation",
    };
    let exe_str = agy_exe.to_string_lossy();
    build_wt_args_inner(spec, agy_exe, &exe_str, cid, work_dir, tab_title, None)
}

/// Builds the argument vector for `pwsh.exe` / `powershell.exe` fallback launch for `source`.
/// Safely single-quotes both `.exe` and `.cmd`/`.bat` paths.
pub fn build_pwsh_args_for_source(
    source: SourceKind,
    cli_exe: &Path,
    cid: &str,
) -> Result<Vec<String>, String> {
    if !validate_conversation_id(cid) {
        return Err("无效的会话 ID".to_string());
    }
    let spec = resume_cli_spec(source)
        .map_err(|_| format!("{} 会话暂不支持通过终端 CLI 恢复", source.label_short()))?;
    let exe_str = validate_exe_path_for_shell(cli_exe, false, spec.command_name)?;
    let cmd = format!(
        "& {} {} {}",
        ps_single_quote(&exe_str),
        spec.resume_arg,
        cid
    );
    Ok(vec![
        "-NoLogo".to_string(),
        "-NoExit".to_string(),
        "-Command".to_string(),
        cmd,
    ])
}

/// Builds the argument vector for `pwsh.exe` / `powershell.exe` fallback launch for Antigravity CLI.
pub fn build_pwsh_args(agy_exe: &Path, cid: &str) -> Vec<String> {
    let cmd = format!(
        "& {} --conversation {}",
        ps_single_quote(&agy_exe.to_string_lossy()),
        cid
    );
    vec![
        "-NoLogo".to_string(),
        "-NoExit".to_string(),
        "-Command".to_string(),
        cmd,
    ]
}

/// Builds the raw command string passed inside outer quotes after `cmd.exe /D /K` via
/// `CommandExt::raw_arg` for `source`.
/// Outer double quotes are stripped by `cmd.exe /K`, preserving inner quotes around `cli_exe`
/// even when the executable path contains spaces or parentheses. Batch shims (`.cmd`/`.bat`)
/// are prefixed with `call` so control returns cleanly to the interactive shell.
pub fn build_cmd_k_raw_arg_for_source(
    source: SourceKind,
    cli_exe: &Path,
    cid: &str,
) -> Result<String, String> {
    if !validate_conversation_id(cid) {
        return Err("无效的会话 ID".to_string());
    }
    let spec = resume_cli_spec(source)
        .map_err(|_| format!("{} 会话暂不支持通过终端 CLI 恢复", source.label_short()))?;
    let exe_str = validate_exe_path_for_shell(cli_exe, true, spec.command_name)?;
    if is_batch_shim(cli_exe) {
        Ok(format!("call \"{}\" {} {}", exe_str, spec.resume_arg, cid))
    } else {
        Ok(format!("\"{}\" {} {}", exe_str, spec.resume_arg, cid))
    }
}

/// Builds the raw command string passed after `cmd.exe /K` for Antigravity CLI.
pub fn build_cmd_k_raw_arg(agy_exe: &Path, cid: &str) -> Result<String, String> {
    build_cmd_k_raw_arg_for_source(SourceKind::Cli, agy_exe, cid)
}

fn resolve_launch_dir(ws: Option<&Path>) -> (PathBuf, bool) {
    match ws {
        Some(p) => match p.to_str().and_then(normalize_workspace_dir) {
            Some(valid) => (valid, false),
            None if p.is_dir() => (p.to_path_buf(), false),
            None => (user_home_dir(), true),
        },
        None => (user_home_dir(), true),
    }
}

/// Non-blockingly spawns a terminal window/tab to resume a session of `source` (`agy`, `claude`,
/// `codex`, or `gemini`) in `ws`. Falls back to `%USERPROFILE%` when `ws` is missing or deleted
/// (`workspace_fallback = true`). Fallback chain: `wt.exe` -> `pwsh.exe`/`powershell.exe` -> `cmd.exe /D /K`.
pub fn launch_resume_session_for_source(
    source: SourceKind,
    cid: &str,
    ws: Option<&Path>,
    tab_title: Option<&str>,
) -> Result<LaunchOutcome, String> {
    if !validate_conversation_id(cid) {
        return Err("无效的会话 ID，拒绝启动终端".to_string());
    }
    let spec = resume_cli_spec(source)
        .map_err(|_| format!("{} 会话暂不支持通过终端 CLI 恢复", source.label_short()))?;
    let cli_exe = resolve_cli_exe_for_source(source).ok_or_else(|| {
        format!(
            "未找到 {} 命令，请检查 PATH 或已为您复制恢复命令",
            spec.command_name
        )
    })?;
    let exe_str = validate_exe_path_for_shell(&cli_exe, false, spec.command_name)?;
    let (used_dir, workspace_fallback) = resolve_launch_dir(ws);
    let pwsh_exe = resolve_pwsh_exe();

    if let Some(wt_exe) = resolve_wt_exe() {
        let wt_args = build_wt_args_inner(
            spec,
            &cli_exe,
            &exe_str,
            cid,
            &used_dir,
            tab_title,
            pwsh_exe.as_deref(),
        );
        if Command::new(&wt_exe)
            .args(&wt_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
        {
            return Ok(LaunchOutcome {
                backend: LaunchBackend::WindowsTerminal,
                used_dir,
                workspace_fallback,
            });
        }
    }

    if let Some(pwsh) = pwsh_exe {
        let pwsh_args = build_pwsh_args_for_source(source, &cli_exe, cid)?;
        if Command::new(&pwsh)
            .current_dir(&used_dir)
            .args(&pwsh_args)
            .creation_flags(CREATE_NEW_CONSOLE.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
        {
            return Ok(LaunchOutcome {
                backend: LaunchBackend::Pwsh,
                used_dir,
                workspace_fallback,
            });
        }
    }

    let raw_inner = build_cmd_k_raw_arg_for_source(source, &cli_exe, cid)?;
    let cmd_raw = format!("\"{}\"", raw_inner);
    Command::new("cmd.exe")
        .current_dir(&used_dir)
        .arg("/D")
        .arg("/K")
        .raw_arg(OsStr::new(&cmd_raw))
        .creation_flags(CREATE_NEW_CONSOLE.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("启动终端失败: {e}"))?;

    Ok(LaunchOutcome {
        backend: LaunchBackend::Cmd,
        used_dir,
        workspace_fallback,
    })
}

/// Convenience wrapper launching the matching agent CLI directly from `SessionInfo`.
pub fn launch_resume_for_session(session: &SessionInfo) -> Result<LaunchOutcome, String> {
    if session.is_archived {
        return Err(BlockedReason::ArchivedOnly.message_cn().to_string());
    }
    if session.is_subagent {
        return Err(BlockedReason::SubagentOnly.message_cn().to_string());
    }
    let ws = normalize_workspace_path_str(&session.workspace).map(PathBuf::from);
    launch_resume_session_for_source(
        session.source,
        &session.conversation_id,
        ws.as_deref(),
        Some(&session.title),
    )
}

/// Non-blockingly spawns a terminal window/tab to resume `agy --conversation <cid>` in `ws`.
/// Falls back to `%USERPROFILE%` when `ws` is missing or deleted (`workspace_fallback = true`).
/// Fallback chain: `wt.exe` -> `pwsh.exe`/`powershell.exe` -> `cmd.exe /D /K`.
pub fn launch_resume_session(
    cid: &str,
    ws: Option<&Path>,
    tab_title: Option<&str>,
) -> Result<LaunchOutcome, String> {
    launch_resume_session_for_source(SourceKind::Cli, cid, ws, tab_title)
}

/// Opens an existing local workspace directory in Windows Explorer using `ShellExecuteW`
/// with the `"explore"` verb after verifying `is_dir()`.
pub fn open_workspace_folder(hwnd: HWND, ws: &str) -> Result<PathBuf, String> {
    let dir = normalize_workspace_dir(ws)
        .ok_or_else(|| "工作区目录不存在或不可访问".to_string())?;
    let wide: Vec<u16> = dir
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let res = unsafe {
        ShellExecuteW(
            hwnd,
            w!("explore"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = res.0 as isize;
    if code <= 32 {
        return Err(format!("打开资源管理器失败 (code={code})"));
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::TokenCounts;
    use std::collections::BTreeSet;

    fn mk_session(
        cid: &str,
        source: SourceKind,
        is_archived: bool,
        is_subagent: bool,
        parent_id: Option<&str>,
    ) -> SessionInfo {
        SessionInfo {
            conversation_id: cid.to_string(),
            title: "t".to_string(),
            workspace: String::new(),
            source,
            is_archived,
            is_subagent,
            parent_id: parent_id.map(String::from),
            parent_title: String::new(),
            turns: Vec::new(),
            counts: TokenCounts::default(),
            models: BTreeSet::new(),
            first_time: None,
            last_time: None,
            last_file_mtime_utc: None,
            is_running_hint: false,
        }
    }

    #[test]
    fn test_validate_conversation_id() {
        assert!(validate_conversation_id("8c1095e2-1558-4a06-bfde-446204f978aa"));
        assert!(validate_conversation_id("sess_01-ABC"));
        assert!(!validate_conversation_id(""));
        assert!(!validate_conversation_id("--dangerously-skip-permissions"));
        assert!(!validate_conversation_id("-r"));
        assert!(!validate_conversation_id("abc; calc.exe"));
        assert!(!validate_conversation_id("abc def"));
        assert!(!validate_conversation_id("abc\"def"));
        assert!(!validate_conversation_id("abc|whoami"));
        assert!(!validate_conversation_id("abc&calc"));
        assert!(!validate_conversation_id("abc$env"));
        assert!(!validate_conversation_id(&"a".repeat(129)));
    }

    #[test]
    fn test_resume_cli_spec_and_blocked_sources() {
        let cli = resume_cli_spec(SourceKind::Cli).unwrap();
        assert_eq!((cli.command_name, cli.resume_arg), ("agy", "--conversation"));

        let custom = resume_cli_spec(SourceKind::Custom).unwrap();
        assert_eq!((custom.command_name, custom.resume_arg), ("agy", "--conversation"));

        let claude = resume_cli_spec(SourceKind::ClaudeCode).unwrap();
        assert_eq!((claude.command_name, claude.resume_arg), ("claude", "--resume"));

        let codex = resume_cli_spec(SourceKind::Codex).unwrap();
        assert_eq!((codex.command_name, codex.resume_arg), ("codex", "resume"));

        let gemini = resume_cli_spec(SourceKind::GeminiCli).unwrap();
        assert_eq!((gemini.command_name, gemini.resume_arg), ("gemini", "--resume"));

        assert_eq!(resume_cli_spec(SourceKind::App), Err(BlockedReason::NonCliSource));
        assert_eq!(resume_cli_spec(SourceKind::Ide), Err(BlockedReason::NonCliSource));
    }

    #[test]
    fn test_classify_resume_capability_multi_agent_and_subagent_routing() {
        let agy_cli = mk_session("11111111-1111-1111-1111-111111111111", SourceKind::Cli, false, false, None);
        let agy_custom = mk_session("11111111-1111-1111-1111-111111111112", SourceKind::Custom, false, false, None);
        let arch = mk_session("22222222-2222-2222-2222-222222222222", SourceKind::Cli, true, false, None);
        let app = mk_session("33333333-3333-3333-3333-333333333333", SourceKind::App, false, false, None);
        let ide = mk_session("33333333-3333-3333-3333-333333333334", SourceKind::Ide, false, false, None);
        let sub_ok = mk_session("44444444-4444-4444-4444-444444444444", SourceKind::Cli, false, true, Some(&agy_cli.conversation_id));
        let sub_arch_parent = mk_session("55555555-5555-5555-5555-555555555555", SourceKind::Cli, false, true, Some(&arch.conversation_id));

        // Multi-agent CLI main & subagent sessions
        let claude_main = mk_session("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", SourceKind::ClaudeCode, false, false, None);
        let claude_sub = mk_session(
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa__agent_01",
            SourceKind::ClaudeCode,
            false,
            true,
            Some(&claude_main.conversation_id),
        );
        // Cross-family collision guard: Claude subagent pointing to an AGY session ID must NOT link
        let claude_sub_wrong_family = mk_session(
            "11111111-1111-1111-1111-111111111111__agent_99",
            SourceKind::ClaudeCode,
            false,
            true,
            Some(&agy_cli.conversation_id),
        );
        let codex_main = mk_session("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", SourceKind::Codex, false, false, None);
        let codex_sub = mk_session(
            "cccccccc-cccc-cccc-cccc-cccccccccccc",
            SourceKind::Codex,
            false,
            true,
            Some(&codex_main.conversation_id),
        );
        let gemini_main = mk_session("dddddddd-dddd-dddd-dddd-dddddddddddd", SourceKind::GeminiCli, false, false, None);

        let all = vec![
            agy_cli.clone(),
            agy_custom.clone(),
            arch.clone(),
            app.clone(),
            ide.clone(),
            sub_ok.clone(),
            sub_arch_parent.clone(),
            claude_main.clone(),
            claude_sub.clone(),
            claude_sub_wrong_family.clone(),
            codex_main.clone(),
            codex_sub.clone(),
            gemini_main.clone(),
        ];

        assert_eq!(classify_resume_capability(&agy_cli, &all), ResumeCapability::CliDirect);
        assert!(classify_resume_capability(&agy_cli, &all).is_direct_resumable());
        assert_eq!(classify_resume_capability(&agy_custom, &all), ResumeCapability::CliDirect);
        assert_eq!(classify_resume_capability(&claude_main, &all), ResumeCapability::CliDirect);
        assert_eq!(classify_resume_capability(&codex_main, &all), ResumeCapability::CliDirect);
        assert_eq!(classify_resume_capability(&gemini_main, &all), ResumeCapability::CliDirect);

        assert_eq!(classify_resume_capability(&arch, &all), ResumeCapability::ArchivedOnly);
        assert_eq!(
            classify_resume_capability(&arch, &all).blocked_reason(),
            Some(BlockedReason::ArchivedOnly)
        );

        let app_cap = classify_resume_capability(&app, &all);
        assert_eq!(app_cap, ResumeCapability::UnsupportedSource(SourceKind::App));
        assert_eq!(app_cap.blocked_reason(), Some(BlockedReason::NonCliSource));

        let ide_cap = classify_resume_capability(&ide, &all);
        assert_eq!(ide_cap, ResumeCapability::UnsupportedSource(SourceKind::Ide));
        assert_eq!(ide_cap.blocked_reason(), Some(BlockedReason::NonCliSource));

        assert_eq!(
            classify_resume_capability(&sub_ok, &all),
            ResumeCapability::Subagent {
                parent_id: Some(agy_cli.conversation_id.clone()),
                parent_resumable: true,
            }
        );
        assert_eq!(
            classify_resume_capability(&sub_arch_parent, &all),
            ResumeCapability::Subagent {
                parent_id: Some(arch.conversation_id.clone()),
                parent_resumable: false,
            }
        );
        assert_eq!(
            classify_resume_capability(&claude_sub, &all),
            ResumeCapability::Subagent {
                parent_id: Some(claude_main.conversation_id.clone()),
                parent_resumable: true,
            }
        );
        assert_eq!(
            find_resumable_parent(&claude_sub, &all).map(|p| p.conversation_id.as_str()),
            Some(claude_main.conversation_id.as_str())
        );
        assert_eq!(
            classify_resume_capability(&claude_sub_wrong_family, &all),
            ResumeCapability::Subagent {
                parent_id: Some(agy_cli.conversation_id.clone()),
                parent_resumable: false,
            }
        );
        assert_eq!(
            classify_resume_capability(&codex_sub, &all),
            ResumeCapability::Subagent {
                parent_id: Some(codex_main.conversation_id.clone()),
                parent_resumable: true,
            }
        );
    }

    #[test]
    fn test_normalize_workspace_paths() {
        assert_eq!(normalize_workspace_path_str(""), None);
        assert_eq!(normalize_workspace_path_str("(未知工作区)"), None);
        assert_eq!(normalize_workspace_path_str("(unknown)"), None);
        assert_eq!(normalize_workspace_path_str("https://example.com"), None);
        assert_eq!(normalize_workspace_path_str("C:/Users/alice/my proj/"), Some(r"C:\Users\alice\my proj".to_string()));
        assert_eq!(normalize_workspace_path_str("C:/"), Some(r"C:\".to_string()));
        assert_eq!(normalize_workspace_path_str("C:"), Some(r"C:\".to_string()));

        // Existing directory check via temp dir
        let dir = std::env::temp_dir().join(format!("agy_gauge_ws_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let slash_form = dir.to_string_lossy().replace('\\', "/");
        assert!(normalize_workspace_dir(&slash_form).is_some());
        let missing = dir.join("does_not_exist");
        assert!(normalize_workspace_dir(&missing.to_string_lossy()).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_format_resume_command_all_agents_and_invalid_ids() {
        let cid = "8c1095e2-1558-4a06-bfde-446204f978aa";
        let ws = Some(Path::new("C:/Users/O'Brien/my proj/"));

        assert_eq!(
            format_resume_command(cid, None).unwrap(),
            format!("agy --conversation {cid}")
        );
        assert_eq!(
            format_resume_command(cid, ws).unwrap(),
            format!("Set-Location -LiteralPath 'C:\\Users\\O''Brien\\my proj'; agy --conversation {cid}")
        );
        assert_eq!(
            format_resume_command_for_source(SourceKind::Custom, cid, None).unwrap(),
            format!("agy --conversation {cid}")
        );
        assert_eq!(
            format_resume_command_for_source(SourceKind::ClaudeCode, cid, None).unwrap(),
            format!("claude --resume {cid}")
        );
        assert_eq!(
            format_resume_command_for_source(SourceKind::ClaudeCode, cid, ws).unwrap(),
            format!("Set-Location -LiteralPath 'C:\\Users\\O''Brien\\my proj'; claude --resume {cid}")
        );
        assert_eq!(
            format_resume_command_for_source(SourceKind::Codex, cid, None).unwrap(),
            format!("codex resume {cid}")
        );
        assert_eq!(
            format_resume_command_for_source(SourceKind::Codex, cid, ws).unwrap(),
            format!("Set-Location -LiteralPath 'C:\\Users\\O''Brien\\my proj'; codex resume {cid}")
        );
        assert_eq!(
            format_resume_command_for_source(SourceKind::GeminiCli, cid, None).unwrap(),
            format!("gemini --resume {cid}")
        );
        assert_eq!(
            format_resume_command_for_source(SourceKind::GeminiCli, cid, ws).unwrap(),
            format!("Set-Location -LiteralPath 'C:\\Users\\O''Brien\\my proj'; gemini --resume {cid}")
        );

        // Non-CLI sources & invalid IDs are rejected
        assert!(format_resume_command_for_source(SourceKind::App, cid, None).is_err());
        assert!(format_resume_command_for_source(SourceKind::Ide, cid, None).is_err());
        assert!(format_resume_command("bad;id", None).is_err());
        assert!(format_resume_command_for_source(SourceKind::ClaudeCode, "--exec", None).is_err());
        assert!(format_resume_command_for_source(SourceKind::Codex, "id with space", None).is_err());
        assert!(format_resume_command_for_source(SourceKind::GeminiCli, "", None).is_err());
    }

    #[test]
    fn test_find_cli_in_dirs_resolution_priority() {
        let base = std::env::temp_dir().join(format!("agy_gauge_cli_path_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let dir1 = base.join("d1");
        let dir2 = base.join("d2");
        fs::create_dir_all(&dir1).unwrap();
        fs::create_dir_all(&dir2).unwrap();

        // Extensionless POSIX shim in dir1 must be ignored; .bat in dir2 found
        fs::write(dir1.join("claude"), "#!/bin/sh\n").unwrap();
        fs::write(dir2.join("claude.bat"), "@echo off\n").unwrap();
        assert_eq!(
            find_cli_in_dirs("claude", &[dir1.clone(), dir2.clone()]),
            Some(dir2.join("claude.bat"))
        );

        // .cmd beats .bat in same dir
        fs::write(dir2.join("claude.cmd"), "@echo off\n").unwrap();
        assert_eq!(
            find_cli_in_dirs("claude", &[dir1.clone(), dir2.clone()]),
            Some(dir2.join("claude.cmd"))
        );

        // .exe beats .cmd in same dir
        fs::write(dir2.join("claude.exe"), "MZ").unwrap();
        assert_eq!(
            find_cli_in_dirs("claude", &[dir1.clone(), dir2.clone()]),
            Some(dir2.join("claude.exe"))
        );

        // Earlier dir .cmd beats later dir .exe
        fs::write(dir1.join("claude.cmd"), "@echo off\n").unwrap();
        assert_eq!(
            find_cli_in_dirs("claude", &[dir1.clone(), dir2.clone()]),
            Some(dir1.join("claude.cmd"))
        );

        // A directory named `codex.exe` must not be returned as an executable
        fs::create_dir_all(dir1.join("codex.exe")).unwrap();
        assert_eq!(find_cli_in_dirs("codex", &[dir1.clone(), dir2.clone()]), None);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_wt_pwsh_and_cmd_arg_builders_and_shim_quoting() {
        let cid = "8c1095e2-1558-4a06-bfde-446204f978aa";
        let agy = Path::new(r"C:\Program Files\Antigravity\agy.exe");

        // Drive root becomes C:\. and semicolons are escaped; title strips `;` and quotes
        assert_eq!(escape_wt_dir_arg(Path::new(r"C:\")), r"C:\.");
        assert_eq!(escape_wt_dir_arg(Path::new(r"C:\proj\a;b\")), r"C:\proj\a\;b");
        assert_eq!(
            sanitize_wt_title("Fix A; \"refactor\" 'B'\nnow"),
            Some("Fix A refactor B now".to_string())
        );

        // Native .exe in WT, pwsh, and cmd
        let wt_args = build_wt_args(agy, cid, Path::new(r"C:\"), Some("Fix; Title"));
        assert_eq!(
            wt_args,
            vec![
                "-w",
                "0",
                "new-tab",
                "-d",
                r"C:\.",
                "--title",
                "Fix Title",
                "--",
                r"C:\Program Files\Antigravity\agy.exe",
                "--conversation",
                cid,
            ]
        );

        let pwsh_args = build_pwsh_args(agy, cid);
        assert_eq!(pwsh_args[0], "-NoLogo");
        assert_eq!(pwsh_args[1], "-NoExit");
        assert_eq!(pwsh_args[2], "-Command");
        assert_eq!(
            pwsh_args[3],
            format!("& 'C:\\Program Files\\Antigravity\\agy.exe' --conversation {cid}")
        );

        let cmd_raw = build_cmd_k_raw_arg(agy, cid).unwrap();
        assert_eq!(
            cmd_raw,
            format!("\"C:\\Program Files\\Antigravity\\agy.exe\" --conversation {cid}")
        );
        assert!(build_cmd_k_raw_arg(Path::new(r"C:\bad&path\agy.exe"), cid).is_err());

        // Windows npm `.cmd` shim with spaces, `(x86)`, apostrophe, and `;` for Claude Code
        let claude_cmd = Path::new(r"C:\Program Files (x86)\O'Brien;npm\claude.cmd");
        assert!(is_batch_shim(claude_cmd));
        let wt_claude =
            build_wt_args_for_source(SourceKind::ClaudeCode, claude_cmd, cid, Path::new(r"D:\work"), Some("Claude"))
                .unwrap();
        assert_eq!(
            wt_claude,
            vec![
                "-w",
                "0",
                "new-tab",
                "-d",
                r"D:\work",
                "--title",
                "Claude",
                "--",
                "powershell.exe",
                "-NoLogo",
                "-NoExit",
                "-Command",
                &format!("& 'C:\\Program Files (x86)\\O''Brien\\;npm\\claude.cmd' --resume {cid}"),
            ]
        );

        let pwsh_claude = build_pwsh_args_for_source(SourceKind::ClaudeCode, claude_cmd, cid).unwrap();
        assert_eq!(
            pwsh_claude[3],
            format!("& 'C:\\Program Files (x86)\\O''Brien;npm\\claude.cmd' --resume {cid}")
        );

        let cmd_claude = build_cmd_k_raw_arg_for_source(SourceKind::ClaudeCode, claude_cmd, cid).unwrap();
        assert_eq!(
            cmd_claude,
            format!("call \"C:\\Program Files (x86)\\O'Brien;npm\\claude.cmd\" --resume {cid}")
        );

        // Codex `.bat` shim and Gemini `.cmd` shim
        let codex_bat = Path::new(r"C:\Users\Alice Smith\AppData\Roaming\npm\codex.bat");
        assert!(is_batch_shim(codex_bat));
        assert_eq!(
            build_cmd_k_raw_arg_for_source(SourceKind::Codex, codex_bat, cid).unwrap(),
            format!("call \"C:\\Users\\Alice Smith\\AppData\\Roaming\\npm\\codex.bat\" resume {cid}")
        );
        let pwsh_codex = build_pwsh_args_for_source(SourceKind::Codex, codex_bat, cid).unwrap();
        assert_eq!(
            pwsh_codex[3],
            format!("& 'C:\\Users\\Alice Smith\\AppData\\Roaming\\npm\\codex.bat' resume {cid}")
        );

        let gemini_cmd = Path::new(r"C:\Users\Alice\AppData\Roaming\npm\gemini.cmd");
        assert_eq!(
            build_cmd_k_raw_arg_for_source(SourceKind::GeminiCli, gemini_cmd, cid).unwrap(),
            format!("call \"C:\\Users\\Alice\\AppData\\Roaming\\npm\\gemini.cmd\" --resume {cid}")
        );
        let wt_gemini =
            build_wt_args_for_source(SourceKind::GeminiCli, gemini_cmd, cid, Path::new(r"D:\repo"), None).unwrap();
        assert_eq!(
            wt_gemini,
            vec![
                "-w",
                "0",
                "new-tab",
                "-d",
                r"D:\repo",
                "--",
                "powershell.exe",
                "-NoLogo",
                "-NoExit",
                "-Command",
                &format!("& 'C:\\Users\\Alice\\AppData\\Roaming\\npm\\gemini.cmd' --resume {cid}"),
            ]
        );

        // Shell injection attempts in shim paths and conversation IDs are strictly rejected
        for bad_shim in [
            r"C:\bad&calc\claude.cmd",
            r"C:\bad|whoami\claude.cmd",
            r"C:\bad%COMSPEC%\claude.cmd",
            r"C:\bad^caret\claude.cmd",
            r"C:\bad!bang\claude.cmd",
            "C:\\bad\nline\\claude.cmd",
        ] {
            let p = Path::new(bad_shim);
            assert!(
                build_wt_args_for_source(SourceKind::ClaudeCode, p, cid, Path::new(r"C:\"), None).is_err(),
                "wt should reject {bad_shim:?}"
            );
            assert!(
                build_pwsh_args_for_source(SourceKind::ClaudeCode, p, cid).is_err(),
                "pwsh should reject {bad_shim:?}"
            );
            assert!(
                build_cmd_k_raw_arg_for_source(SourceKind::ClaudeCode, p, cid).is_err(),
                "cmd should reject {bad_shim:?}"
            );
        }

        // Invalid IDs & non-CLI sources rejected in all builders
        assert!(build_wt_args_for_source(SourceKind::Codex, codex_bat, "--evil", Path::new(r"C:\"), None).is_err());
        assert!(build_pwsh_args_for_source(SourceKind::Codex, codex_bat, "id;calc").is_err());
        assert!(build_cmd_k_raw_arg_for_source(SourceKind::Codex, codex_bat, "id&calc").is_err());
        assert!(build_wt_args_for_source(SourceKind::App, agy, cid, Path::new(r"C:\"), None).is_err());
        assert!(build_pwsh_args_for_source(SourceKind::Ide, agy, cid).is_err());
        assert!(build_cmd_k_raw_arg_for_source(SourceKind::App, agy, cid).is_err());
    }
}
