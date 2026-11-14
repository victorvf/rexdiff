mod app;
mod git;
mod highlight;
mod theme;
mod tree;
mod wrap;

use std::path::PathBuf;
use std::process::{Command, ExitCode};

const HELP: &str = "\
rexdiff — your git changes in a Rex pane, next to your terminal

Usage:
  rexdiff [show] [--ratio PCT] [--below]   open the panel next to the current pane (default: 45%)
  rexdiff hide                             close the panel
  rexdiff toggle                           open or close it
      --cwd DIR                            use another folder (accepts file://…)
      --focus                              focus the panel when it opens
  rexdiff view                             run the panel in the current terminal

In the panel:
  ↑↓ / j k        previous / next row (folders and files)
  ← → / h l       previous folder (above) / next folder (below)
  space / Enter   on a file: stage/unstage it (✓ = reviewed); on a folder: open/close
  Tab / Shift-Tab next / previous file, wrapping around
  f / b           scroll the diff one page   J K  one line   g / G  top / bottom
  x               toggle between changed hunks only and the whole file
  c               commit what's staged: type the message, Enter commits, Esc cancels
  d               discard the changes of the file or folder under the cursor (y confirms)
  1 / 2           Changes / Branches screen

On the Branches screen:
  ↑↓ / j k        move between local branches
  space           check out the branch (a remote one gets a local branch that tracks it;
                  ◆ marks branches open in another worktree, which can't be checked out here)
  Enter           commit history of the branch (Esc goes back)
  p               pull the branch (current: git pull; others: fast-forward only)
  P (Shift+P)     push the branch (to its upstream, or `origin` with -u)
  m               pull the branch into the current one (git pull origin <branch>)
  n               create a new branch from the selected one (from its remote side, freshly fetched)
  d / D           delete the local branch (y confirms); D deletes it even if it isn't merged
  f               fetch from all remotes (git fetch --all --prune)
  Esc / 1         back to Changes
  + -             tree height                r    reload the repository   q  quit
  The mouse works too: click, scroll, and drag the divider.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("show");
    let rest = if args.is_empty() { &args[..] } else { &args[1..] };
    let result = match cmd {
        "show" => show(rest),
        "hide" => hide().map(|_| ()),
        "toggle" => match hide() {
            Ok(true) => Ok(()),
            Ok(false) => show(rest),
            Err(e) => Err(e),
        },
        "view" => view(),
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            Ok(())
        }
        s if s.starts_with("--") => show(&args),
        other => Err(format!("unknown command: {other}\n\n{HELP}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rexdiff: {e}");
            ExitCode::FAILURE
        }
    }
}

fn view() -> Result<(), String> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return Err("`view` needs a terminal".into());
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    app::run(cwd).map_err(|e| e.to_string())
}

struct RexCtx {
    session: String,
    block: String,
}

fn rex_ctx() -> Result<RexCtx, String> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    match (get("REX_SESSION"), get("REX_BLOCK")) {
        (Some(session), Some(block)) => Ok(RexCtx { session, block }),
        _ => Err("run this inside a Rex terminal (REX_SESSION/REX_BLOCK are not set)".into()),
    }
}

/// File that records which rexdiff pane belongs to this terminal.
fn state_file(block: &str) -> PathBuf {
    let name: String = block.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    state_dir().join(name)
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"))
        .join("rexdiff")
}

/// The `rex` command. When rexdiff runs from a shortcut, PATH is the Rex
/// server's, which may not include `rex`; then use the one bundled with the app.
fn rex() -> Command {
    if let Some(bin) = std::env::var_os("REX_BIN") {
        return Command::new(bin);
    }
    let in_path = std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("rex").is_file()));
    if !in_path {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let bundled = home.join(
            "Library/Application Support/com.superlogical.rex/RexServer/Versions/Current/Rex Server.app/Contents/MacOS/rex",
        );
        if bundled.is_file() {
            return Command::new(bundled);
        }
    }
    Command::new("rex")
}

fn alive(ctx: &RexCtx, id: &str) -> bool {
    rex()
        .args(["block", "inspect", "-s", &ctx.session, id, "--json"])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn open_pane(ctx: &RexCtx) -> Option<String> {
    let id = std::fs::read_to_string(state_file(&ctx.block)).ok()?.trim().to_string();
    alive(ctx, &id).then_some(id)
}

/// If the current pane is itself a rexdiff panel, returns its state file.
fn owner_state_of(block: &str) -> Option<PathBuf> {
    std::fs::read_dir(state_dir()).ok()?.flatten().map(|e| e.path()).find(|p| {
        std::fs::read_to_string(p).is_ok_and(|id| id.trim() == block)
    })
}

/// Accepts a path or a `file://host/path` URL (how Rex reports a terminal's folder).
fn parse_cwd(s: &str) -> PathBuf {
    let Some(rest) = s.strip_prefix("file://") else { return PathBuf::from(s) };
    let path = &rest[rest.find('/').unwrap_or(rest.len())..];
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&path[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}

fn show(args: &[String]) -> Result<(), String> {
    let mut ratio = "45".to_string();
    let mut dir = "right";
    let mut focus = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--below" => dir = "below",
            "--focus" => focus = true,
            "--ratio" => ratio = it.next().cloned().ok_or("--ratio needs a number")?,
            s if s.starts_with("--ratio=") => ratio = s["--ratio=".len()..].to_string(),
            "--cwd" => {
                let dir = it.next().ok_or("--cwd needs a folder")?;
                if !dir.is_empty() {
                    std::env::set_current_dir(parse_cwd(dir))
                        .map_err(|e| format!("could not change to {dir}: {e}"))?;
                }
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    let ctx = rex_ctx()?;
    if open_pane(&ctx).is_some() {
        println!("rexdiff is already open");
        return Ok(());
    }

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let out = rex()
        .args(["split", "-s", &ctx.session, "-b", &ctx.block, "--split", dir, "--ratio", &ratio])
        .arg(if focus { "--focus=true" } else { "--focus=false" })
        .args(["--label", "rexdiff", "--json", "-c"])
        .arg(&cwd)
        .arg("--")
        .arg(&exe)
        .arg("view")
        .output()
        .map_err(|e| format!("could not run `rex`: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("unexpected response from rex: {e}"))?;
    let id = json["block_ids"][0].as_str().ok_or("rex did not return the pane id")?;
    let file = state_file(&ctx.block);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, id);
    Ok(())
}

/// Closes the panel. Returns `true` if one was open.
fn hide() -> Result<bool, String> {
    let ctx = rex_ctx()?;
    // Shortcut pressed with the panel itself focused: close it.
    let (id, state) = match open_pane(&ctx) {
        Some(id) => (id, state_file(&ctx.block)),
        None => match owner_state_of(&ctx.block) {
            Some(state) if alive(&ctx, &ctx.block) => (ctx.block.clone(), state),
            _ => return Ok(false),
        },
    };
    let status = rex()
        .args(["block", "close", "-s", &ctx.session, &id])
        .status()
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(state);
    Ok(status.success())
}
