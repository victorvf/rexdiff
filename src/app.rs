//! The panel: state, keyboard/mouse input, and drawing.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::git::{self, BranchInfo, CommitInfo, FileDiff, Job, Msg, RowKind, Snapshot, Staged};
use crate::theme::{self, Palette};
use crate::tree::{self, Item, TreeRow};
use crate::wrap::{wrap_row, WrapCache};

const CONTEXT: u32 = 3;
const FULL_CONTEXT: u32 = 1_000_000;
/// Wait after a change on disk before reloading (coalesces bursts of events).
const FS_DEBOUNCE: Duration = Duration::from_millis(120);
/// Safety reload, in case a filesystem event gets lost.
const FALLBACK_REFRESH: Duration = Duration::from_secs(10);
const THEME_EVERY: Duration = Duration::from_secs(4);
/// Extra theme checks right after the panel opens: a brand-new Rex pane starts
/// with the server's default colors, and the app applies your theme a moment
/// later.
const THEME_EARLY: [Duration; 2] = [Duration::from_millis(600), Duration::from_millis(1500)];

#[derive(Default, Clone, Copy)]
struct Areas {
    side: bool,
    tree: Rect,
    /// Stacked: the hr row (it and the blank rows around it drag the divider).
    /// Side by side: the divider column.
    splitter: u16,
    /// File title, right above the diff.
    title: Rect,
    diff: Rect,
}

/// Stacked layout fixed rows: header, rule, margin, hr, margin, title, footer.
const CHROME_ROWS: u16 = 7;
/// From this width on (a full-screen or zoomed pane) the tree goes on the
/// left and the diff on the right.
const SIDE_MIN_WIDTH: u16 = 160;

/// The two screens: changed files with their diff, and the local branches.
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Changes,
    Branches,
    /// The commit history of one branch, opened from the Branches screen.
    History,
}

struct App {
    jobs: Sender<Job>,
    view: View,
    /// Column ranges of the "Changes" and "Branches" tabs in the header, for clicks.
    tabs: Cell<[(u16, u16); 2]>,

    branches: Option<Result<Vec<BranchInfo>, String>>,
    branch_cursor: usize,
    /// First visible branch row (updated while drawing).
    branch_off: Cell<usize>,

    hist_branch: String,
    history: Option<Result<Vec<CommitInfo>, String>>,
    hist_cursor: usize,
    hist_off: Cell<usize>,
    pal: Palette,
    detected: Option<theme::Detected>,

    snapshot: Option<Result<Snapshot, String>>,
    collapsed: HashSet<String>,
    rows: Vec<TreeRow>,
    cursor: usize,
    tree_off: usize,
    tree_pref: Option<u16>,
    tree_pref_w: Option<u16>,

    active: Option<String>,
    diff: Option<FileDiff>,
    /// Scroll position in visual lines (after wrapping).
    diff_scroll: usize,
    diff_gen: u64,
    wrap: WrapCache,
    full_context: bool,

    areas: Areas,
    dragging: bool,
    /// A short message in the footer (errors, mostly) and when it goes away.
    flash: Option<Flash>,
    /// The text box in the footer, while it's open: a commit message (`c`) or
    /// a new branch name (`n`).
    input: Option<InputBox>,
    /// A discard (`d`) waiting for `y`.
    confirm: Option<Confirm>,
    /// A commit in progress, and what it's doing.
    busy: Option<String>,
    /// The branch row an action runs on, and what it's doing ("pulling").
    busy_branch: Option<(String, &'static str)>,
    /// When the current action started, to animate the spinner.
    busy_since: Instant,
    quit: bool,
}

pub fn run(cwd: PathBuf) -> std::io::Result<()> {
    let (job_tx, job_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    {
        let cwd = cwd.clone();
        std::thread::spawn(move || git::run_worker(cwd, job_rx, msg_tx));
    }
    let _ = job_tx.send(Job::Refresh);

    let (fs_tx, fs_rx) = mpsc::channel();
    let _watcher = watch(&cwd, fs_tx);

    let mut terminal = ratatui::init();
    log_panics();
    let detected = theme::query(Duration::from_millis(400));
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;

    let mut app = App {
        jobs: job_tx,
        view: View::Changes,
        tabs: Cell::new([(0, 0); 2]),
        branches: None,
        branch_cursor: 0,
        branch_off: Cell::new(0),
        hist_branch: String::new(),
        history: None,
        hist_cursor: 0,
        hist_off: Cell::new(0),
        pal: Palette::from(detected),
        detected,
        snapshot: None,
        collapsed: HashSet::new(),
        rows: Vec::new(),
        cursor: 0,
        tree_off: 0,
        tree_pref: None,
        tree_pref_w: None,
        active: None,
        diff: None,
        diff_scroll: 0,
        diff_gen: 0,
        wrap: WrapCache::default(),
        full_context: false,
        areas: Areas::default(),
        dragging: false,
        flash: None,
        input: None,
        confirm: None,
        busy: None,
        busy_branch: None,
        busy_since: Instant::now(),
        quit: false,
    };

    let res = app.event_loop(&mut terminal, &msg_rx, &fs_rx);
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    res
}

/// Writes panics to `~/.local/state/rexdiff/panic.log`: the pane closes along
/// with the program, so the message on screen would be lost.
fn log_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let path = crate::state_dir().join("panic.log");
        let bt = std::backtrace::Backtrace::force_capture();
        let _ = std::fs::create_dir_all(crate::state_dir());
        let _ = std::fs::write(&path, format!("{info}\n\n{bt}\n"));
        previous(info);
    }));
}

/// Watches the repository folder and signals when something relevant changes.
fn watch(cwd: &Path, tx: Sender<()>) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    let repo = git2::Repository::discover(cwd).ok()?;
    let root = repo.workdir()?.to_path_buf();
    let root_cb = root.clone();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        let relevant = ev.paths.iter().any(|p| {
            let Ok(rel) = p.strip_prefix(&root_cb) else { return true };
            let mut comps = rel.components();
            if comps.next().is_some_and(|c| c.as_os_str() == ".git") {
                // Inside .git only the index, HEAD, and refs matter.
                let rest = comps.as_path();
                return rest == Path::new("index") || rest == Path::new("HEAD") || rest.starts_with("refs");
            }
            !repo.is_path_ignored(rel).unwrap_or(false)
        });
        if relevant {
            let _ = tx.send(());
        }
    })
    .ok()?;
    w.watch(&root, RecursiveMode::Recursive).ok()?;
    Some(w)
}

impl App {
    fn event_loop(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        msgs: &Receiver<Msg>,
        fs: &Receiver<()>,
    ) -> std::io::Result<()> {
        let mut dirty = true;
        let mut refresh_due: Option<Instant> = None;
        let mut last_refresh = Instant::now();
        let started = Instant::now();
        let mut early_checks = THEME_EARLY.iter().peekable();
        let mut last_theme = Instant::now();
        let mut last_input = Instant::now();

        while !self.quit {
            // Keep redrawing while something runs, so the spinner turns.
            if dirty || self.busy.is_some() {
                terminal.draw(|f| self.draw(f))?;
                dirty = false;
            }

            if event::poll(Duration::from_millis(40))? {
                loop {
                    match event::read()? {
                        Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
                        Event::Mouse(m) => self.on_mouse(m),
                        Event::Paste(text) => {
                            if let Some(input) = self.input.as_mut() {
                                input.text.push_str(&text.replace(['\n', '\r'], " "));
                            }
                        }
                        _ => {}
                    }
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
                last_input = Instant::now();
                dirty = true;
            }

            for msg in msgs.try_iter() {
                self.on_msg(msg);
                dirty = true;
            }
            if self.flash.as_ref().is_some_and(|f| Instant::now() >= f.until) {
                self.flash = None;
                dirty = true;
            }

            if fs.try_iter().count() > 0 && refresh_due.is_none() {
                refresh_due = Some(Instant::now() + FS_DEBOUNCE);
            }
            let now = Instant::now();
            if refresh_due.is_some_and(|t| now >= t) || now - last_refresh >= FALLBACK_REFRESH {
                let _ = self.jobs.send(Job::Refresh);
                match self.view {
                    View::Branches => {
                        let _ = self.jobs.send(Job::Branches);
                    }
                    View::History => {
                        let _ = self.jobs.send(Job::History { branch: self.hist_branch.clone() });
                    }
                    View::Changes => {}
                }
                refresh_due = None;
                last_refresh = now;
            }

            // Check the Rex theme a couple of times right after opening, then
            // every so often. The periodic check waits for the panel to be
            // idle, so a keypress doesn't get swallowed mid-reply.
            let early = early_checks.next_if(|&&at| now - started >= at).is_some();
            if early || (now - last_theme >= THEME_EVERY && now - last_input >= Duration::from_secs(2)) {
                last_theme = now;
                if let Some(d) = theme::query(Duration::from_millis(200))
                    && Some(d) != self.detected
                {
                    self.detected = Some(d);
                    self.pal = Palette::from(Some(d));
                    terminal.clear()?;
                    dirty = true;
                }
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- data

    fn files(&self) -> &[git::FileEntry] {
        match &self.snapshot {
            Some(Ok(s)) => &s.files,
            _ => &[],
        }
    }

    fn context(&self) -> u32 {
        if self.full_context { FULL_CONTEXT } else { CONTEXT }
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Snapshot(snap) => {
                let cursor_key = self.rows.get(self.cursor).map(|r| r.key(self.files()).to_string());
                self.snapshot = Some(snap);
                self.rebuild(cursor_key.as_deref());

                let files = self.files();
                let still_there = self.active.as_ref().is_some_and(|a| files.iter().any(|f| &f.path == a));
                if !still_there {
                    self.active = None;
                    self.diff = None;
                    // Start on the first file in the tree.
                    if let Some(i) = self.rows.iter().position(|r| matches!(r.item, Item::File { .. })) {
                        self.cursor = i;
                    }
                    self.sync_active_from_cursor();
                } else {
                    // The content may have changed: request the diff again, keeping the scroll.
                    self.request_diff();
                }
            }
            Msg::Error(e) => self.flash = Some(Flash::new(e, false)),
            Msg::Branches(list) => {
                // Keep the cursor on the same branch; the first time, on the current one.
                let keep = self.selected_branch().map(|b| b.name.clone());
                let first_load = self.branches.is_none();
                self.branches = Some(list);
                if let Some(Ok(l)) = &self.branches {
                    let pos = if first_load {
                        l.iter().position(|b| b.current)
                    } else {
                        keep.and_then(|n| l.iter().position(|b| b.name == n))
                    };
                    self.branch_cursor = pos.unwrap_or(self.branch_cursor).min(l.len().saturating_sub(1));
                }
            }
            Msg::History { branch, commits } => {
                if branch == self.hist_branch {
                    self.history = Some(commits);
                    let len = self.history_len();
                    self.hist_cursor = self.hist_cursor.min(len.saturating_sub(1));
                }
            }
            Msg::Progress(p) => self.busy = Some(p),
            Msg::Done(result) => {
                self.busy = None;
                self.busy_branch = None;
                self.flash = Some(match result {
                    Ok(summary) => Flash::new(format!("✓ {summary}"), true),
                    Err(e) => Flash::new(e, false),
                });
            }
            Msg::Diff(d) => {
                if Some(&d.path) == self.active.as_ref() && d.context == self.context() {
                    let same_file = self.diff.as_ref().is_some_and(|old| old.path == d.path);
                    if !same_file {
                        self.diff_scroll = 0;
                    }
                    self.diff = Some(d);
                    self.diff_gen += 1;
                }
            }
        }
    }

    fn rebuild(&mut self, cursor_key: Option<&str>) {
        let rows = tree::build(self.files(), &self.collapsed);
        let pos = cursor_key.and_then(|k| rows.iter().position(|r| r.key(self.files()) == k));
        self.rows = rows;
        self.cursor = pos.unwrap_or(self.cursor).min(self.rows.len().saturating_sub(1));
    }

    fn request_diff(&self) {
        if let Some(path) = &self.active {
            let _ = self.jobs.send(Job::Diff { path: path.clone(), context: self.context() });
        }
    }

    /// With the cursor on a file, show it; on a folder, show its first file
    /// (in tree order, even when the folder is collapsed).
    fn sync_active_from_cursor(&mut self) {
        let path = match self.rows.get(self.cursor).map(|r| &r.item) {
            Some(Item::File { idx, .. }) => self.files()[*idx].path.clone(),
            Some(Item::Dir { path, .. }) => match self.first_file_in(path) {
                Some(p) => p,
                None => return,
            },
            None => return,
        };
        self.set_active(path);
    }

    fn first_file_in(&self, dir: &str) -> Option<String> {
        let files = self.files();
        let prefix = format!("{dir}/");
        tree::build(files, &HashSet::new()).iter().find_map(|r| match r.item {
            Item::File { idx, .. } if files[idx].path.starts_with(&prefix) => Some(files[idx].path.clone()),
            _ => None,
        })
    }

    fn set_active(&mut self, path: String) {
        if self.active.as_ref() != Some(&path) {
            self.active = Some(path);
            self.request_diff();
        }
    }

    // ---------------------------------------------------------------- input

    /// One row up or down the tree, through folders and files.
    fn move_row(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
        self.sync_active_from_cursor();
    }

    /// Moves to the next (or previous) folder. An open folder puts the cursor
    /// on its first visible entry (its first file, or a collapsed subfolder);
    /// a collapsed folder keeps the cursor on the folder itself.
    fn step_folder(&mut self, forward: bool) {
        let candidates: Box<dyn Iterator<Item = usize>> = if forward {
            Box::new(self.cursor + 1..self.rows.len())
        } else {
            Box::new((0..self.cursor).rev())
        };
        for i in candidates {
            if !matches!(self.rows[i].item, Item::Dir { .. }) {
                continue;
            }
            let land = self.landing(i);
            // Skip folders whose landing spot is where we already are (or behind us),
            // e.g. the folder that holds the file the cursor is on.
            if (forward && land > self.cursor) || (!forward && land < self.cursor) {
                self.cursor = land;
                self.sync_active_from_cursor();
                return;
            }
        }
    }

    /// Where the cursor lands for the folder at `row`: down through open folders
    /// until it reaches a file or a collapsed folder.
    fn landing(&self, mut row: usize) -> usize {
        while let Some(Item::Dir { collapsed: false, .. }) = self.rows.get(row).map(|r| &r.item) {
            if row + 1 >= self.rows.len() {
                break;
            }
            row += 1;
        }
        row
    }

    /// Next file, wrapping around (Tab).
    fn jump_file(&mut self, forward: bool) {
        let n = self.rows.len();
        for step in 1..=n {
            let i = if forward { (self.cursor + step) % n } else { (self.cursor + n - step) % n };
            if matches!(self.rows[i].item, Item::File { .. }) {
                self.cursor = i;
                self.sync_active_from_cursor();
                return;
            }
        }
    }

    fn toggle_dir(&mut self, row: usize) {
        let Some(TreeRow { item: Item::Dir { path, .. }, .. }) = self.rows.get(row) else { return };
        let path = path.clone();
        if !self.collapsed.remove(&path) {
            self.collapsed.insert(path.clone());
        }
        self.rebuild(Some(&path));
    }

    /// Space: on a folder, open/close it; on a file, stage it (or unstage it
    /// if it's fully staged), to mark it as reviewed.
    fn toggle_here(&mut self) {
        match self.rows.get(self.cursor).map(|r| &r.item) {
            Some(Item::Dir { .. }) => self.toggle_dir(self.cursor),
            Some(Item::File { idx, .. }) => self.toggle_stage(*idx),
            None => {}
        }
    }

    fn toggle_stage(&mut self, idx: usize) {
        let Some(f) = self.files().get(idx) else { return };
        let add = f.staged != Staged::Fully;
        let _ = self.jobs.send(Job::Stage { path: f.path.clone(), add });
    }

    fn scroll_diff(&mut self, delta: isize) {
        let max = self.wrap.total().saturating_sub(self.areas.diff.height as usize);
        self.diff_scroll = (self.diff_scroll as isize + delta).clamp(0, max as isize) as usize;
    }

    fn on_key(&mut self, k: KeyEvent) {
        if self.input.is_some() {
            self.on_input_key(k);
            return;
        }
        // A pending confirmation: `y` goes ahead, any other key cancels.
        if let Some(c) = self.confirm.take() {
            if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.start_busy(c.busy, c.branch);
                let _ = self.jobs.send(c.job);
            }
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('q') => return self.quit = true,
            KeyCode::Char('c') if ctrl => return self.quit = true,
            KeyCode::Char('1') => return self.show_view(View::Changes),
            KeyCode::Char('2') => return self.show_view(View::Branches),
            _ => {}
        }
        match self.view {
            View::Branches => return self.on_branches_key(k),
            View::History => return self.on_history_key(k),
            View::Changes => {}
        }
        let page = self.areas.diff.height.max(2) as isize - 1;
        match k.code {
            KeyCode::Char('c') if !ctrl => self.open_commit_box(),
            KeyCode::Char('d') if !ctrl => self.ask_discard(),
            KeyCode::Char('d') if ctrl => self.scroll_diff(page / 2),
            KeyCode::Char('u') if ctrl => self.scroll_diff(-page / 2),
            KeyCode::Up | KeyCode::Char('k') => self.move_row(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_row(1),
            KeyCode::Left | KeyCode::Char('h') => self.step_folder(false),
            KeyCode::Right | KeyCode::Char('l') => self.step_folder(true),
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle_here(),
            KeyCode::Tab => self.jump_file(true),
            KeyCode::BackTab => self.jump_file(false),
            KeyCode::Char('J') => self.scroll_diff(1),
            KeyCode::Char('K') => self.scroll_diff(-1),
            KeyCode::Char('f') | KeyCode::PageDown => self.scroll_diff(page),
            KeyCode::Char('b') | KeyCode::PageUp => self.scroll_diff(-page),
            KeyCode::Char('g') | KeyCode::Home => self.diff_scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.scroll_diff(isize::MAX / 2),
            KeyCode::Char('x') => {
                self.full_context = !self.full_context;
                self.diff = None;
                self.request_diff();
            }
            KeyCode::Char('+') | KeyCode::Char('=') if self.areas.side => {
                self.tree_pref_w = Some(self.areas.tree.width + 2);
            }
            KeyCode::Char('-') if self.areas.side => {
                self.tree_pref_w = Some(self.areas.tree.width.saturating_sub(2));
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.tree_pref = Some(self.areas.tree.height + 1);
            }
            KeyCode::Char('-') => {
                self.tree_pref = Some(self.areas.tree.height.saturating_sub(1).max(1));
            }
            KeyCode::Char('r') => {
                let _ = self.jobs.send(Job::Refresh);
            }
            _ => {}
        }
    }

    fn show_view(&mut self, view: View) {
        if view == View::Branches {
            let _ = self.jobs.send(Job::Branches);
        }
        self.view = view;
    }

    fn selected_branch(&self) -> Option<&BranchInfo> {
        match &self.branches {
            Some(Ok(l)) => l.get(self.branch_cursor),
            _ => None,
        }
    }

    /// Branches screen: move, `space`/`Enter` checks out, `p` pulls, `P` pushes,
    /// `m` merges the selected branch into the current one.
    fn on_branches_key(&mut self, k: KeyEvent) {
        let len = match &self.branches {
            Some(Ok(l)) => l.len(),
            _ => 0,
        };
        let move_by = |c: usize, d: isize| (c as isize + d).clamp(0, len.saturating_sub(1) as isize) as usize;
        match k.code {
            KeyCode::Esc => self.view = View::Changes,
            KeyCode::Up | KeyCode::Char('k') => self.branch_cursor = move_by(self.branch_cursor, -1),
            KeyCode::Down | KeyCode::Char('j') => self.branch_cursor = move_by(self.branch_cursor, 1),
            KeyCode::Home | KeyCode::Char('g') => self.branch_cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.branch_cursor = len.saturating_sub(1),
            KeyCode::Char(' ') => self.checkout_selected(),
            KeyCode::Enter => self.open_history(),
            KeyCode::Char('P') => self.push_selected(),
            KeyCode::Char('p') => self.pull_selected(),
            KeyCode::Char('m') => self.merge_selected(),
            KeyCode::Char('d') => self.ask_delete_branch(false),
            KeyCode::Char('n') => self.open_new_branch_box(),
            KeyCode::Char('f') => self.fetch(),
            KeyCode::Char('D') => self.ask_delete_branch(true),
            KeyCode::Char('r') => {
                let _ = self.jobs.send(Job::Branches);
            }
            _ => {}
        }
    }

    fn open_history(&mut self) {
        let Some(b) = self.selected_branch() else { return };
        self.hist_branch = b.name.clone();
        self.history = None;
        self.hist_cursor = 0;
        self.hist_off.set(0);
        self.view = View::History;
        let _ = self.jobs.send(Job::History { branch: self.hist_branch.clone() });
    }

    fn history_len(&self) -> usize {
        match &self.history {
            Some(Ok(c)) => c.len(),
            _ => 0,
        }
    }

    /// History screen: move through the commits, Esc goes back to Branches.
    fn on_history_key(&mut self, k: KeyEvent) {
        let len = self.history_len();
        let page = (self.areas.diff.height.max(2) as isize).max(10);
        let move_by = |c: usize, d: isize| (c as isize + d).clamp(0, len.saturating_sub(1) as isize) as usize;
        match k.code {
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.view = View::Branches,
            KeyCode::Up | KeyCode::Char('k') => self.hist_cursor = move_by(self.hist_cursor, -1),
            KeyCode::Down | KeyCode::Char('j') => self.hist_cursor = move_by(self.hist_cursor, 1),
            KeyCode::PageDown | KeyCode::Char('f') => self.hist_cursor = move_by(self.hist_cursor, page),
            KeyCode::PageUp | KeyCode::Char('b') => self.hist_cursor = move_by(self.hist_cursor, -page),
            KeyCode::Home | KeyCode::Char('g') => self.hist_cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.hist_cursor = len.saturating_sub(1),
            _ => {}
        }
    }

    fn checkout_selected(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(b) = self.selected_branch() else { return };
        if b.current {
            self.flash = Some(Flash::new(format!("Already on {}", b.name), true));
            return;
        }
        if let Some(wt) = &b.worktree {
            let msg = format!("{} is open in worktree {}: switch there ({})", b.name, folder_name(wt), tilde(wt));
            self.flash = Some(Flash::new(msg, false));
            return;
        }
        let (name, remote) = (b.name.clone(), b.remote);
        self.start_busy(format!("Switching to {name}…"), Some((name.clone(), "switching")));
        let _ = self.jobs.send(Job::Checkout { name, remote });
    }

    /// The selected branch, unless it's a remote one: those can only be checked
    /// out, merged, or browsed, so the other actions say so in the footer.
    fn selected_local_branch(&mut self) -> Option<String> {
        let b = self.selected_branch()?;
        if b.remote {
            let msg = format!("{} is a remote branch: check it out first (space)", b.name);
            self.flash = Some(Flash::new(msg, false));
            return None;
        }
        Some(b.name.clone())
    }

    fn fetch(&mut self) {
        if self.busy.is_some() {
            return;
        }
        self.start_busy("Fetching…".into(), None);
        let _ = self.jobs.send(Job::Fetch);
    }

    fn merge_selected(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(b) = self.selected_branch() else { return };
        if b.current {
            self.flash = Some(Flash::new(format!("{} is the branch you're on: use p to pull it", b.name), false));
            return;
        }
        let (name, remote) = (b.name.clone(), b.remote);
        self.start_busy(format!("Pulling {name} into the current branch…"), Some((name.clone(), "pulling into current")));
        let _ = self.jobs.send(Job::PullInto { name, remote });
    }

    /// `n`: name a new branch that starts from the selected one.
    fn open_new_branch_box(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(b) = self.selected_branch() else { return };
        let kind = InputKind::NewBranch { from: b.name.clone(), from_remote: b.remote };
        self.flash = None;
        self.input = Some(InputBox { kind, text: String::new() });
    }

    fn pull_selected(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(name) = self.selected_local_branch() else { return };
        self.start_busy(format!("Pulling {name}…"), Some((name.clone(), "pulling")));
        let _ = self.jobs.send(Job::Pull { name });
    }

    fn push_selected(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(name) = self.selected_local_branch() else { return };
        self.start_busy(format!("Pushing {name}…"), Some((name.clone(), "pushing")));
        let _ = self.jobs.send(Job::Push { name });
    }

    /// `d`: get ready to throw away the changes of the file or folder under
    /// the cursor, and ask for confirmation in the footer.
    fn ask_discard(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(row) = self.rows.get(self.cursor) else { return };
        let files = self.files();
        let (label, picked): (String, Vec<&git::FileEntry>) = match &row.item {
            Item::File { idx, .. } => (files[*idx].path.clone(), vec![&files[*idx]]),
            Item::Dir { path, .. } => {
                let prefix = format!("{path}/");
                (prefix.clone(), files.iter().filter(|f| f.path.starts_with(&prefix)).collect())
            }
        };
        if picked.is_empty() {
            return;
        }
        let label = if picked.len() > 1 { format!("{label} ({} files)", picked.len()) } else { label };
        // New files aren't in HEAD: discarding them means deleting them.
        let (new, restore): (Vec<_>, Vec<_>) = picked.into_iter().partition(|f| f.status == 'A');
        let warning = match new.len() {
            0 => None,
            1 => Some("1 new file will be deleted for good.".to_string()),
            n => Some(format!("{n} new files will be deleted for good.")),
        };
        let confirm = Confirm {
            question: format!("Discard changes to {label}?"),
            warning,
            verb: "discard",
            busy: format!("Discarding {label}…"),
            branch: None,
            job: Job::Discard {
                restore: restore.iter().map(|f| f.path.clone()).collect(),
                new: new.iter().map(|f| f.path.clone()).collect(),
                label,
            },
        };
        self.flash = None;
        self.confirm = Some(confirm);
    }

    /// `d` / `D` on the Branches screen: ask before deleting the local branch.
    /// `d` lets git refuse a branch with unmerged commits; `D` deletes it anyway.
    fn ask_delete_branch(&mut self, force: bool) {
        if self.busy.is_some() {
            return;
        }
        let Some(b) = self.selected_branch() else { return };
        if b.current {
            self.flash = Some(Flash::new(format!("{} is the branch you're on: check out another one first", b.name), false));
            return;
        }
        if let Some(wt) = &b.worktree {
            let msg = format!("{} is open in worktree {}: remove it first ({})", b.name, folder_name(wt), tilde(wt));
            self.flash = Some(Flash::new(msg, false));
            return;
        }
        let Some(name) = self.selected_local_branch() else { return };
        let warning = force.then(|| "Commits that only exist on it will be lost.".to_string());
        self.flash = None;
        self.confirm = Some(Confirm {
            question: format!("{} branch {name}?", if force { "Force-delete" } else { "Delete" }),
            warning,
            verb: "delete",
            busy: format!("Deleting {name}…"),
            branch: Some((name.clone(), "deleting")),
            job: Job::DeleteBranch { name, force },
        });
    }

    fn start_busy(&mut self, text: String, branch: Option<(String, &'static str)>) {
        self.busy = Some(text);
        self.busy_branch = branch;
        self.busy_since = Instant::now();
    }

    /// The current frame of the spinner shown while an action runs.
    fn spinner(&self) -> &'static str {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[(self.busy_since.elapsed().as_millis() / 80) as usize % FRAMES.len()]
    }

    fn open_commit_box(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let staged = self.files().iter().filter(|f| f.staged != Staged::No).count();
        if staged == 0 {
            self.flash = Some(Flash::new("Nothing staged yet: press space on a file to stage it".into(), false));
            return;
        }
        self.flash = None;
        self.input = Some(InputBox { kind: InputKind::Commit, text: String::new() });
    }

    /// Typing in the footer box. Enter commits (or creates the branch), Esc
    /// cancels.
    fn on_input_key(&mut self, k: KeyEvent) {
        let Some(input) = self.input.as_mut() else { return };
        let msg = &mut input.text;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Char('c') if ctrl => self.input = None,
            KeyCode::Enter => {
                let text = msg.trim().to_string();
                if text.is_empty() {
                    return;
                }
                let Some(input) = self.input.take() else { return };
                match input.kind {
                    InputKind::Commit => {
                        self.start_busy("Committing…".into(), None);
                        let _ = self.jobs.send(Job::Commit { message: text });
                    }
                    InputKind::NewBranch { from, from_remote } => {
                        // Branch names can't have spaces: `fix login` → `fix-login`.
                        let name = text.split_whitespace().collect::<Vec<_>>().join("-");
                        self.start_busy(format!("Creating {name}…"), Some((from.clone(), "branching off")));
                        let _ = self.jobs.send(Job::NewBranch { name, from, from_remote });
                    }
                }
            }
            KeyCode::Backspace if k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) => {
                delete_word(msg);
            }
            KeyCode::Backspace => {
                msg.pop();
            }
            KeyCode::Char('u') if ctrl => msg.clear(),
            KeyCode::Char('w') if ctrl => delete_word(msg),
            KeyCode::Char(c) if !ctrl => msg.push(c),
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        if m.kind == MouseEventKind::Down(MouseButton::Left) && m.row == 0 {
            for (i, (x0, x1)) in self.tabs.get().iter().enumerate() {
                if m.column >= *x0 && m.column < *x1 {
                    return self.show_view(if i == 0 { View::Changes } else { View::Branches });
                }
            }
        }
        if self.view == View::History {
            let len = self.history_len();
            match m.kind {
                MouseEventKind::ScrollDown => self.hist_cursor = (self.hist_cursor + 1).min(len.saturating_sub(1)),
                MouseEventKind::ScrollUp => self.hist_cursor = self.hist_cursor.saturating_sub(1),
                // Row 2 is the title; commits start at row 3.
                MouseEventKind::Down(MouseButton::Left) if m.row >= 3 => {
                    let row = self.hist_off.get() + (m.row - 3) as usize;
                    if row < len {
                        self.hist_cursor = row;
                    }
                }
                _ => {}
            }
            return;
        }
        if self.view == View::Branches {
            let len = match &self.branches {
                Some(Ok(l)) => l.len(),
                _ => 0,
            };
            match m.kind {
                MouseEventKind::ScrollDown => self.branch_cursor = (self.branch_cursor + 1).min(len.saturating_sub(1)),
                MouseEventKind::ScrollUp => self.branch_cursor = self.branch_cursor.saturating_sub(1),
                MouseEventKind::Down(MouseButton::Left) if m.row >= 2 => {
                    let row = self.branch_off.get() + (m.row - 2) as usize;
                    if row < len {
                        self.branch_cursor = row;
                    }
                }
                _ => {}
            }
            return;
        }
        let a = self.areas;
        let inside = |r: Rect| m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height;
        let in_tree = inside(a.tree);
        let on_splitter = if a.side {
            m.column + 1 >= a.splitter && m.column <= a.splitter + 1 && m.row >= a.tree.y
        } else {
            a.splitter > 0 && m.row + 1 >= a.splitter && m.row <= a.splitter + 1
        };
        match m.kind {
            MouseEventKind::ScrollDown if in_tree => {
                let max = self.rows.len().saturating_sub(a.tree.height as usize);
                self.tree_off = (self.tree_off + 1).min(max);
            }
            MouseEventKind::ScrollUp if in_tree => self.tree_off = self.tree_off.saturating_sub(1),
            MouseEventKind::ScrollDown => self.scroll_diff(3),
            MouseEventKind::ScrollUp => self.scroll_diff(-3),
            MouseEventKind::Down(MouseButton::Left) if on_splitter => self.dragging = true,
            MouseEventKind::Down(MouseButton::Left) if in_tree => {
                let row = self.tree_off + (m.row - a.tree.y) as usize;
                if row < self.rows.len() {
                    self.cursor = row;
                    match self.rows[row].item {
                        Item::Dir { .. } => self.toggle_dir(row),
                        Item::File { idx, .. } => {
                            self.sync_active_from_cursor();
                            // A click right on the checkbox stages/unstages.
                            let checkbox = a.tree.x + 1 + self.rows[row].guide.width() as u16;
                            if m.column == checkbox {
                                self.toggle_stage(idx);
                            }
                        }
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging && a.side => {
                // The divider sits one column after the end of the tree.
                self.tree_pref_w = Some(m.column.saturating_sub(a.tree.x + 1));
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                // The pointer sits on the hr, one row below the end of the tree.
                self.tree_pref = Some(m.row.saturating_sub(a.tree.y + 1).max(1));
            }
            MouseEventKind::Up(MouseButton::Left) => self.dragging = false,
            _ => {}
        }
    }

    // ---------------------------------------------------------------- drawing

    fn layout(&mut self, area: Rect) {
        self.areas = if area.width >= SIDE_MIN_WIDTH { self.side_layout(area) } else { self.stacked_layout(area) };
        let tree_h = self.areas.tree.height;

        // Keep the cursor visible in the tree.
        let h = tree_h as usize;
        if self.cursor < self.tree_off {
            self.tree_off = self.cursor;
        } else if h > 0 && self.cursor >= self.tree_off + h {
            self.tree_off = self.cursor + 1 - h;
        }
        self.tree_off = self.tree_off.min(self.rows.len().saturating_sub(h));

        // Wrap the diff to the pane width; only redone when the diff or the width changes.
        match self.diff.as_ref().filter(|d| Some(&d.path) == self.active.as_ref()) {
            Some(d) => {
                let g = Geom::new(d, self.areas.diff);
                self.wrap.rebuild(&d.rows, g.text_w, self.diff_gen);
            }
            None => self.wrap = WrapCache::default(),
        }
        self.scroll_diff(0);
    }

    /// A blank row above the footer while the commit box is open, so it doesn't
    /// sit right against the diff.
    fn footer_gap(&self) -> u16 {
        u16::from(self.input.is_some())
    }

    /// Tree on top, diff below.
    fn stacked_layout(&self, area: Rect) -> Areas {
        // Header, rule, [tree], margin, hr, margin, title, [diff], footer.
        let avail = area.height.saturating_sub(CHROME_ROWS);
        let want = self.rows.len() as u16;
        // The tree takes up to ~32% of the free rows (the rest goes to the diff).
        let auto = want.min((avail * 8 / 25).max(3));
        let max_tree = avail.saturating_sub(3).max(1);
        let tree_h = self.tree_pref.unwrap_or(auto).clamp(1, max_tree).min(avail);
        let tree = Rect::new(area.x, area.y + 2, area.width, tree_h);
        let splitter = tree.y + tree_h + 1;
        let title = Rect::new(area.x, splitter + 2, area.width, 1);
        let diff_h = area.height.saturating_sub(tree_h + CHROME_ROWS + self.footer_gap());
        let diff = Rect::new(area.x, title.y + 1, area.width, diff_h);
        Areas { side: false, tree, splitter, title, diff }
    }

    /// Tree on the left, a vertical divider, diff on the right.
    fn side_layout(&self, area: Rect) -> Areas {
        // Header, rule, [tree | divider | title + diff], footer.
        let body_y = area.y + 2;
        let body_h = area.height.saturating_sub(3 + self.footer_gap());
        // As wide as the longest row needs, within 28 columns and 35% of the pane.
        let files = self.files();
        let needed = self
            .rows
            .iter()
            .map(|r| {
                let (label, adds, dels) = match &r.item {
                    Item::Dir { label, adds, dels, .. } => (label.width() + 3, *adds, *dels),
                    Item::File { idx, name } => (name.width() + 2, files[*idx].adds, files[*idx].dels),
                };
                let counts: usize = counts_text(adds, dels).iter().map(|(s, _)| s.width()).sum();
                (1 + r.guide.width() + label + 3 + counts + 1) as u16
            })
            .max()
            .unwrap_or(0);
        let auto = needed.clamp(28, (area.width * 35 / 100).max(28));
        let tree_w = self.tree_pref_w.unwrap_or(auto).clamp(20, area.width / 2);
        let tree = Rect::new(area.x, body_y, tree_w, body_h);
        let splitter = area.x + tree_w + 1;
        let dx = splitter + 2;
        let dw = (area.x + area.width).saturating_sub(dx);
        let title = Rect::new(dx, body_y, dw, 1);
        let diff = Rect::new(dx, body_y + 1, dw, body_h.saturating_sub(1));
        Areas { side: true, tree, splitter, title, diff }
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        if area.width < 10 || area.height < 6 {
            return;
        }
        self.layout(area);
        let buf = f.buffer_mut();

        let files = self.files();
        let p = &self.pal;
        let snapshot = match &self.snapshot {
            None => return center(buf, area, "Reading the repository…", p.faint),
            Some(Err(e)) => return center(buf, area, e, p.muted),
            Some(Ok(s)) => s,
        };

        self.tabs.set(self.draw_header(buf, area, snapshot));
        hline(buf, area.x, area.y + 1, area.width, p.rule);

        match self.view {
            View::Branches => {
                self.draw_branches(buf, area);
                self.draw_footer(buf, area);
                return;
            }
            View::History => {
                self.draw_history(buf, area);
                self.draw_footer(buf, area);
                return;
            }
            View::Changes => {}
        }

        if files.is_empty() {
            let rest = Rect::new(area.x, area.y + 2, area.width, area.height - 2);
            center(buf, rest, "No changes ✓  working tree clean", p.faint);
            // Still show the footer: the result of a commit that took everything lands here.
            self.draw_footer(buf, area);
            return;
        }

        self.draw_tree(buf);
        self.draw_splitter(buf, area);
        self.draw_diff(buf);
        let cursor = self.draw_footer(buf, area);
        if let Some(pos) = cursor {
            f.set_cursor_position(pos);
        }
    }

    /// Tabs, file count, branch, and totals. Returns the tabs' column ranges.
    fn draw_header(&self, buf: &mut Buffer, area: Rect, s: &Snapshot) -> [(u16, u16); 2] {
        let p = &self.pal;
        let y = area.y;
        let (adds, dels) = s.files.iter().fold((0, 0), |(a, d), f| (a + f.adds, d + f.dels));
        let right = area.x + area.width - 1;
        let totals_w = format!("+{adds} −{dels}").width() as u16;
        let rx = right.saturating_sub(totals_w);
        let mut x = rx;
        put(buf, &mut x, y, right, &format!("+{adds}"), Style::new().fg(p.green));
        put(buf, &mut x, y, right, &format!(" −{dels}"), Style::new().fg(p.red));

        let lim = rx.saturating_sub(1);
        let tab = |on: bool| {
            if on { Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED) } else { Style::new().fg(p.faint) }
        };
        let mut x = area.x + 1;
        let changes_x = x;
        put(buf, &mut x, y, lim, "Changes", tab(self.view == View::Changes));
        x += 1;
        let n = s.files.len();
        let pill = format!(" {n} {} ", if n == 1 { "file" } else { "files" });
        put(buf, &mut x, y, lim, &pill, Style::new().fg(p.muted).bg(p.pill_bg));
        let changes_end = x;
        x += 2;
        let branches_x = x;
        put(buf, &mut x, y, lim, "Branches", tab(matches!(self.view, View::Branches | View::History)));
        let branches_end = x;
        put(buf, &mut x, y, lim, &format!("   ⎇ {}", s.branch), Style::new().fg(p.muted));
        if self.view == View::Changes && n > 0 {
            let done = s.files.iter().filter(|f| f.staged == Staged::Fully).count();
            let st = Style::new().fg(if done == n { p.green } else { p.muted });
            put(buf, &mut x, y, lim, &format!("   ✓ {done}/{n} staged"), st);
        }
        [(changes_x, changes_end), (branches_x, branches_end)]
    }

    /// The history screen: a title row, then one row per commit.
    fn draw_history(&self, buf: &mut Buffer, area: Rect) {
        let p = &self.pal;
        let right = area.x + area.width - 1;

        // Title: ‹ branch · N commits · K not pushed
        let ty = area.y + 2;
        let mut x = area.x + 1;
        put(buf, &mut x, ty, right, "‹ ", Style::new().fg(p.faint));
        put(buf, &mut x, ty, right, &self.hist_branch, Style::new().add_modifier(Modifier::BOLD));
        if let Some(Ok(commits)) = &self.history {
            let n = commits.len();
            let more = if n >= 1000 { "+" } else { "" };
            put(buf, &mut x, ty, right, &format!("  ·  {n}{more} commits"), Style::new().fg(p.muted));
            let unpushed = commits.iter().filter(|c| c.unpushed).count();
            if unpushed > 0 {
                put(buf, &mut x, ty, right, &format!("  ·  ↑{unpushed} not pushed"), Style::new().fg(p.green));
            }
        }

        let list_area = Rect::new(area.x, area.y + 3, area.width, area.height.saturating_sub(4));
        let commits = match &self.history {
            None => return center(buf, list_area, "Loading history…", p.faint),
            Some(Err(e)) => return center(buf, list_area, e, p.muted),
            Some(Ok(c)) if c.is_empty() => return center(buf, list_area, "No commits", p.faint),
            Some(Ok(c)) => c,
        };

        let h = list_area.height as usize;
        let mut off = self.hist_off.get();
        if self.hist_cursor < off {
            off = self.hist_cursor;
        } else if h > 0 && self.hist_cursor >= off + h {
            off = self.hist_cursor + 1 - h;
        }
        self.hist_off.set(off);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        // The author only when there's room for it next to the message.
        let author_w = if area.width >= 110 { commits.iter().map(|c| c.author.width()).max().unwrap_or(0).min(20) } else { 0 };
        for (i, c) in commits.iter().enumerate().skip(off).take(h) {
            let y = list_area.y + (i - off) as u16;
            let base = if i == self.hist_cursor { Style::new().bg(p.sel_bg) } else { Style::new() };
            buf.set_style(Rect::new(area.x, y, area.width, 1), base);

            // Author and time on the right.
            let when = relative_time(now - c.time);
            let when_w = 13; // "11 months ago"
            let mut author = c.author.clone();
            while author.width() > author_w {
                author.pop();
            }
            let right_w = (if author_w > 0 { author_w + 2 } else { 0 } + when_w) as u16;
            let rx = right.saturating_sub(right_w);
            let lim = rx.saturating_sub(2);
            let mut x = rx;
            put(buf, &mut x, y, right, &author, base.fg(p.muted));
            let mut x = right.saturating_sub(when.width() as u16);
            put(buf, &mut x, y, right, &when, base.fg(p.faint));

            let mut x = area.x + 1;
            if c.unpushed {
                put(buf, &mut x, y, lim, "↑ ", base.fg(p.green));
            } else {
                x += 2;
            }
            put(buf, &mut x, y, lim, &c.short, base.fg(p.yellow));
            x += 1;
            if !c.refs.is_empty() {
                put(buf, &mut x, y, lim, &format!("({}) ", c.refs.join(", ")), base.fg(p.blue));
            }
            let subject_style = if c.merge { base.fg(p.muted) } else { base };
            put(buf, &mut x, y, lim, &c.subject, subject_style);
        }
    }

    /// The Branches screen: one row per local branch.
    fn draw_branches(&self, buf: &mut Buffer, area: Rect) {
        let p = &self.pal;
        let list_area = Rect::new(area.x, area.y + 2, area.width, area.height.saturating_sub(3));
        let list = match &self.branches {
            None => return center(buf, list_area, "Loading branches…", p.faint),
            Some(Err(e)) => return center(buf, list_area, e, p.muted),
            Some(Ok(l)) if l.is_empty() => {
                return center(buf, list_area, "No branches yet: make the first commit", p.faint);
            }
            Some(Ok(l)) => l,
        };

        // Keep the cursor visible.
        let h = list_area.height as usize;
        let mut off = self.branch_off.get();
        if self.branch_cursor < off {
            off = self.branch_cursor;
        } else if h > 0 && self.branch_cursor >= off + h {
            off = self.branch_cursor + 1 - h;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        // A branch open in another worktree shows its folder: `aux/worktree (sistema_2)`.
        let wt_suffix = |b: &BranchInfo| b.worktree.as_deref().map(|wt| format!(" ({})", folder_name(wt)));
        let name_w = list
            .iter()
            .map(|b| b.name.width() + wt_suffix(b).map_or(0, |s| s.width()))
            .max()
            .unwrap_or(0)
            .min(48) as u16;
        let sync = |b: &BranchInfo| -> Vec<(String, Color)> {
            if b.remote {
                return vec![("remote".into(), p.blue)];
            }
            match &b.upstream {
                None => vec![("not pushed".into(), p.yellow)],
                Some((u, 0, 0)) => vec![(format!("✓ {u}"), p.faint)],
                Some((u, ahead, behind)) => {
                    let mut v = Vec::new();
                    if *ahead > 0 {
                        v.push((format!("↑{ahead} "), p.green));
                    }
                    if *behind > 0 {
                        v.push((format!("↓{behind} "), p.red));
                    }
                    v.push((u.clone(), p.faint));
                    v
                }
            }
        };
        let sync_w = list
            .iter()
            .map(|b| sync(b).iter().map(|(t, _)| t.width()).sum::<usize>())
            .max()
            .unwrap_or(0) as u16;

        for (i, b) in list.iter().enumerate().skip(off).take(h) {
            let y = list_area.y + (i - off) as u16;
            let base = if i == self.branch_cursor { Style::new().bg(p.sel_bg) } else { Style::new() };
            buf.set_style(Rect::new(area.x, y, area.width, 1), base);
            let right = area.x + area.width - 1;

            // When, on the right.
            let when = relative_time(now - b.time);
            let mut x = right.saturating_sub(when.width() as u16);
            let lim = x.saturating_sub(2);
            put(buf, &mut x, y, right, &when, base.fg(p.faint));

            let working = self.busy_branch.as_ref().filter(|(n, _)| *n == b.name).map(|(_, verb)| *verb);
            let mut x = area.x + 1;
            if working.is_some() {
                put(buf, &mut x, y, lim, &format!("{} ", self.spinner()), base.fg(p.blue));
            } else if b.current {
                put(buf, &mut x, y, lim, "● ", base.fg(p.green));
            } else if b.worktree.is_some() {
                put(buf, &mut x, y, lim, "◆ ", base.fg(p.blue));
            } else {
                x += 2;
            }
            let name_style = match (b.current, b.remote) {
                (true, _) => base.add_modifier(Modifier::BOLD),
                (_, true) => base.fg(p.muted),
                _ => base,
            };
            let start = x;
            put(buf, &mut x, y, (start + name_w).min(lim), &b.name, name_style);
            if let Some(suffix) = wt_suffix(b) {
                put(buf, &mut x, y, (start + name_w).min(lim), &suffix, base.fg(p.blue));
            }
            x = start + name_w + 3;
            let start = x;
            match working {
                Some(verb) => put(buf, &mut x, y, lim, &format!("{verb}…"), base.fg(p.blue)),
                None => {
                    for (t, c) in sync(b) {
                        put(buf, &mut x, y, lim, &t, base.fg(c));
                    }
                }
            }
            x = start + sync_w + 3;
            put(buf, &mut x, y, lim, &b.subject, base.fg(p.muted));
        }
        self.branch_off.set(off);
    }

    fn draw_tree(&self, buf: &mut Buffer) {
        let p = &self.pal;
        let files = self.files();
        let a = self.areas.tree;
        for (i, row) in self.rows.iter().enumerate().skip(self.tree_off).take(a.height as usize) {
            let y = a.y + (i - self.tree_off) as u16;
            let selected = i == self.cursor;
            let base = if selected { Style::new().bg(p.sel_bg) } else { Style::new() };
            buf.set_style(Rect::new(a.x, y, a.width, 1), base);

            let right = a.x + a.width - 1;
            let (adds, dels, faint_counts) = match &row.item {
                Item::Dir { adds, dels, .. } => (*adds, *dels, true),
                Item::File { idx, .. } => (files[*idx].adds, files[*idx].dels, false),
            };
            let counts = counts_text(adds, dels);
            let cw: u16 = counts.iter().map(|(s, _)| s.width() as u16).sum();
            let mut x = right.saturating_sub(cw);
            let lim = x.saturating_sub(1);
            for (s, green) in &counts {
                let c = match (faint_counts, green) {
                    (true, _) => p.faint,
                    (false, true) => p.green,
                    (false, false) => p.red,
                };
                put(buf, &mut x, y, right, s, base.fg(c));
            }

            let mut x = a.x + 1;
            put(buf, &mut x, y, lim, &row.guide, base.fg(p.rule));
            match &row.item {
                Item::Dir { label, collapsed, all_staged, .. } => {
                    put(buf, &mut x, y, lim, if *collapsed { "▸ " } else { "▾ " }, base.fg(p.faint));
                    put(buf, &mut x, y, lim, label, base.fg(p.muted));
                    put(buf, &mut x, y, lim, "/", base.fg(p.faint));
                    if *all_staged {
                        put(buf, &mut x, y, lim, " ✓", base.fg(p.green));
                    }
                }
                Item::File { idx, name } => {
                    let f = &files[*idx];
                    let active = self.active.as_deref() == Some(f.path.as_str());
                    if active {
                        buf.set_string(a.x, y, "▎", base.fg(p.blue));
                    }
                    let sc = match f.status {
                        'A' => p.green,
                        'D' => p.red,
                        'M' => p.yellow,
                        _ => p.blue,
                    };
                    // The checkbox: staged = reviewed.
                    let (mark, mark_fg) = match f.staged {
                        Staged::Fully => ("✓", p.green),
                        Staged::Partly => ("◐", p.yellow),
                        Staged::No => ("○", p.faint),
                    };
                    put(buf, &mut x, y, lim, mark, base.fg(mark_fg).add_modifier(Modifier::BOLD));
                    x += 1;
                    put(buf, &mut x, y, lim, &f.status.to_string(), base.fg(sc).add_modifier(Modifier::BOLD));
                    x += 1;
                    let mut name_style = base;
                    if f.staged == Staged::Fully {
                        name_style = name_style.fg(p.muted);
                    }
                    if active {
                        name_style = name_style.add_modifier(Modifier::BOLD);
                    }
                    if f.status == 'D' {
                        name_style = name_style.fg(p.muted).add_modifier(Modifier::CROSSED_OUT);
                    }
                    put(buf, &mut x, y, lim, name, name_style);
                    if f.binary {
                        put(buf, &mut x, y, lim, "  binary", base.fg(p.faint));
                    }
                }
            }
        }
    }

    fn draw_splitter(&self, buf: &mut Buffer, area: Rect) {
        let p = &self.pal;
        let color = if self.dragging { p.blue } else { p.rule };
        if self.areas.side {
            // A vertical line between the tree and the diff.
            let t = self.areas.tree;
            for y in t.y..t.y + t.height {
                buf.set_string(self.areas.splitter, y, "│", Style::new().fg(color));
            }
        } else {
            // The hr, inset one column on each side.
            hline(buf, area.x + 1, self.areas.splitter, area.width.saturating_sub(2), color);
        }

        // File title: path on the left, position in the diff on the right.
        let t = self.areas.title;
        let y = t.y;
        let right = t.x + t.width - 1;
        let mut lim = right;
        // Only once the active file's diff is wrapped (a new one may still be loading).
        if let Some(d) = self.diff.as_ref().filter(|d| Some(&d.path) == self.active.as_ref())
            && self.wrap.total() > 0
        {
            let total = d.rows.len();
            let visual = self.wrap.total();
            let (top, _) = self.wrap.locate(self.diff_scroll.min(visual.saturating_sub(1)));
            let bottom_v = (self.diff_scroll + self.areas.diff.height as usize).min(visual).saturating_sub(1);
            let (bottom, _) = self.wrap.locate(bottom_v);
            let first = (top + 1).min(total);
            let last = (bottom + 1).min(total);
            let mut info = format!("{first}–{last} of {total}");
            if self.full_context {
                info = format!("whole file · {info}");
            }
            let w = info.width() as u16;
            let mut x = right.saturating_sub(w);
            lim = x.saturating_sub(1);
            put(buf, &mut x, y, right, &info, Style::new().fg(p.faint));
        }
        if let Some(path) = &self.active {
            let (dir, name) = match path.rsplit_once('/') {
                Some((d, n)) => (format!("{d}/"), n),
                None => (String::new(), path.as_str()),
            };
            let mut x = t.x + 1;
            put(buf, &mut x, y, lim, &dir, Style::new().fg(p.muted));
            put(buf, &mut x, y, lim, name, Style::new().add_modifier(Modifier::BOLD));
        }
    }

    fn draw_diff(&self, buf: &mut Buffer) {
        let p = &self.pal;
        let a = self.areas.diff;
        let Some(d) = self.diff.as_ref().filter(|d| Some(&d.path) == self.active.as_ref()) else {
            center(buf, a, "Loading…", p.faint);
            return;
        };
        let g = Geom::new(d, a);
        let gw = g.num_w as usize;
        let text_x = a.x + g.text_off;
        let right = text_x + g.text_w;

        for line in 0..a.height as usize {
            let v = self.diff_scroll + line;
            if v >= self.wrap.total() {
                break;
            }
            let (r, sub) = self.wrap.locate(v);
            let row = &d.rows[r];
            let y = a.y + line as u16;
            let (row_bg, gutter_bg, emph_bg, sign, sign_fg) = match row.kind {
                RowKind::Add => (Some(p.add_bg), Some(p.add_gutter), Some(p.add_emph), "+", p.green),
                RowKind::Del => (Some(p.del_bg), Some(p.del_gutter), Some(p.del_emph), "−", p.red),
                RowKind::Hunk => (Some(p.hunk_bg), Some(p.hunk_bg), None, " ", p.faint),
                _ => (None, None, None, " ", p.faint),
            };
            let base = row_bg.map_or(Style::new(), |c| Style::new().bg(c));
            buf.set_style(Rect::new(a.x, y, a.width, 1), base);
            let gutter = gutter_bg.map_or(Style::new(), |c| Style::new().bg(c));
            buf.set_style(Rect::new(a.x, y, g.gutter_w, 1), gutter);

            // Number and sign only on the first visual line; continuations stay clean.
            if sub == 0 {
                let num = match row.kind {
                    RowKind::Del => row.old,
                    _ => row.new,
                };
                let num_txt = match (row.kind, num) {
                    (RowKind::Hunk, _) => format!("{:>gw$}", "⋯"),
                    (_, Some(n)) => format!("{n:>gw$}"),
                    _ => String::new(),
                };
                let num_fg = if matches!(row.kind, RowKind::Add | RowKind::Del) { p.muted } else { p.faint };
                buf.set_stringn(a.x + 1, y, &num_txt, gw, gutter.fg(num_fg));
                buf.set_string(a.x + g.gutter_w, y, sign, base.fg(sign_fg).add_modifier(Modifier::BOLD));
            }

            match row.kind {
                RowKind::Hunk => {
                    let (hdr, func) = split_hunk(&row.segs[0].text);
                    let mut x = text_x;
                    put(buf, &mut x, y, right, hdr, base.fg(p.faint));
                    put(buf, &mut x, y, right, func, base.fg(p.muted));
                }
                RowKind::Meta => {
                    let mut x = text_x;
                    let st = base.fg(p.faint).add_modifier(Modifier::ITALIC);
                    put(buf, &mut x, y, right, &row.segs[0].text, st);
                }
                _ => {
                    let w = wrap_row(row, g.text_w as usize);
                    let from = w.starts[sub];
                    let to = w.starts.get(sub + 1).copied().unwrap_or(usize::MAX);
                    let mut x = text_x + if sub > 0 { w.indent as u16 } else { 0 };
                    let mut at = 0; // char index at the start of the segment
                    for seg in &row.segs {
                        let n = seg.text.chars().count();
                        let (lo, hi) = (from.max(at), to.min(at + n));
                        if lo < hi {
                            let piece: String = seg.text.chars().skip(lo - at).take(hi - lo).collect();
                            let mut st = base.fg(seg.fg.map_or(p.fg, Color::Indexed));
                            if seg.bold {
                                st = st.add_modifier(Modifier::BOLD);
                            }
                            if seg.emph
                                && let Some(bg) = emph_bg
                            {
                                st = st.bg(bg);
                            }
                            put(buf, &mut x, y, right, &piece, st);
                        }
                        at += n;
                        if at >= to {
                            break;
                        }
                    }
                }
            }
        }
    }

    /// The bottom row: key hints, the commit box, progress, or a message.
    /// Returns where the text cursor goes when the commit box is open.
    fn draw_footer(&self, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
        let p = &self.pal;
        let y = area.y + area.height - 1;
        let mut x = area.x + 1;
        let right = area.x + area.width;

        if let Some(input) = &self.input {
            return Some(self.draw_input_box(buf, area, y, input));
        }
        if let Some(c) = &self.confirm {
            let red = Style::new().fg(p.red);
            put(buf, &mut x, y, right, &format!("{} ", c.question), red.add_modifier(Modifier::BOLD));
            if let Some(w) = &c.warning {
                put(buf, &mut x, y, right, &format!("{w} "), red);
            }
            put(buf, &mut x, y, right, "y", Style::new().fg(p.muted));
            put(buf, &mut x, y, right, &format!(" {}   ", c.verb), Style::new().fg(p.faint));
            put(buf, &mut x, y, right, "any key", Style::new().fg(p.muted));
            put(buf, &mut x, y, right, " cancel", Style::new().fg(p.faint));
            return None;
        }
        if let Some(busy) = &self.busy {
            put(buf, &mut x, y, right, &format!("{} ", self.spinner()), Style::new().fg(p.blue));
            put(buf, &mut x, y, right, busy, Style::new().fg(p.muted));
            return None;
        }
        if let Some(f) = &self.flash
            && Instant::now() < f.until
        {
            put(buf, &mut x, y, right, &f.text, Style::new().fg(if f.ok { p.green } else { p.red }));
            return None;
        }
        let hints: &[(&str, &str)] = match self.view {
            View::Changes => &[
                ("↑↓", "navigate"),
                ("←→", "folders"),
                ("space", "stage/fold"),
                ("c", "commit"),
                ("d", "discard"),
                ("x", "whole file"),
                ("2", "branches"),
                ("q", "quit"),
            ],
            View::History => &[("↑↓", "move"), ("esc", "back"), ("1", "changes"), ("q", "quit")],
            View::Branches => &[
                ("↑↓", "move"),
                ("space", "checkout"),
                ("p", "pull"),
                ("P", "push"),
                ("m", "pull into current"),
                ("n", "new branch"),
                ("d", "delete"),
                ("f", "fetch"),
                ("⏎", "history"),
                ("1", "changes"),
                ("q", "quit"),
            ],
        };
        for (key, what) in hints {
            put(buf, &mut x, y, right, key, Style::new().fg(p.muted));
            put(buf, &mut x, y, right, &format!(" {what}   "), Style::new().fg(p.faint));
        }
        None
    }

    fn draw_input_box(&self, buf: &mut Buffer, area: Rect, y: u16, input: &InputBox) -> (u16, u16) {
        let p = &self.pal;
        let right = area.x + area.width - 1;
        let msg = input.text.as_str();
        let (label, placeholder, action) = match &input.kind {
            InputKind::Commit => ("Commit ", "Commit message".to_string(), "commit"),
            InputKind::NewBranch { from, .. } => ("New branch ", format!("name (from {from})"), "create"),
        };

        let hints: &[(&str, &str)] = &[("⏎", action), ("esc", "cancel")];
        let hints_w: u16 = hints
            .iter()
            .map(|(k, w)| (k.width() + if w.is_empty() { 0 } else { w.width() + 1 } + 2) as u16)
            .sum();
        let mut x = right.saturating_sub(hints_w);
        let field_end = x.saturating_sub(1);
        for (key, what) in hints {
            put(buf, &mut x, y, right, key, Style::new().fg(p.muted));
            if !what.is_empty() {
                put(buf, &mut x, y, right, &format!(" {what}"), Style::new().fg(p.faint));
            }
            x += 2;
        }

        let mut x = area.x + 1;
        put(buf, &mut x, y, field_end, label, Style::new().add_modifier(Modifier::BOLD));
        let field_start = x;
        let field_w = field_end.saturating_sub(field_start);
        buf.set_style(Rect::new(field_start, y, field_w, 1), Style::new().bg(p.pill_bg));

        let inner = field_w.saturating_sub(2) as usize;
        let mut tx = field_start + 1;
        if msg.is_empty() {
            put(buf, &mut tx, y, field_end, &placeholder, Style::new().fg(p.faint).bg(p.pill_bg));
            return (field_start + 1, y);
        }
        // Show the end of the message when it doesn't fit.
        let mut shown = msg;
        while shown.width() > inner.saturating_sub(1) && !shown.is_empty() {
            let mut chars = shown.chars();
            chars.next();
            shown = chars.as_str();
        }
        put(buf, &mut tx, y, field_end, shown, Style::new().bg(p.pill_bg));
        (tx, y)
    }
}

/// The text box in the footer.
struct InputBox {
    kind: InputKind,
    text: String,
}

enum InputKind {
    Commit,
    /// A new branch starting from `from` (an `origin/x` row when `from_remote`).
    NewBranch { from: String, from_remote: bool },
}

/// Something destructive waiting for `y` in the footer: discarding changes
/// (`d` on Changes) or deleting a branch (`d`/`D` on Branches).
struct Confirm {
    /// "Discard changes to app/?", "Delete branch old-idea?"
    question: String,
    /// What can't be undone, if anything.
    warning: Option<String>,
    /// The word next to `y`.
    verb: &'static str,
    /// The footer while it runs.
    busy: String,
    /// The branch row that shows the spinner, if any.
    branch: Option<(String, &'static str)>,
    job: Job,
}

/// A message shown in the footer for a few seconds.
struct Flash {
    text: String,
    ok: bool,
    until: Instant,
}

impl Flash {
    fn new(text: String, ok: bool) -> Self {
        let secs = if ok { 5 } else { 8 };
        Self { text, ok, until: Instant::now() + Duration::from_secs(secs) }
    }
}

/// The last folder of a path (`~/code/app-review` → `app-review`).
fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |f| f.to_string_lossy().into_owned())
}

/// A path with the home folder written as `~`.
fn tilde(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// "just now", "5 min ago", "3 hours ago", "2 days ago"…
fn relative_time(secs: i64) -> String {
    let (n, unit) = match secs.max(0) {
        s if s < 60 => return "just now".into(),
        s if s < 3600 => (s / 60, "min"),
        s if s < 86_400 => (s / 3600, "hour"),
        s if s < 7 * 86_400 => (s / 86_400, "day"),
        s if s < 30 * 86_400 => (s / (7 * 86_400), "week"),
        s if s < 365 * 86_400 => (s / (30 * 86_400), "month"),
        s => (s / (365 * 86_400), "year"),
    };
    let plural = if n == 1 || unit == "min" { "" } else { "s" };
    format!("{n} {unit}{plural} ago")
}

/// Ctrl+W / Option+Backspace: delete the last word.
fn delete_word(s: &mut String) {
    while s.ends_with(' ') {
        s.pop();
    }
    while s.chars().last().is_some_and(|c| c != ' ') {
        s.pop();
    }
}

/// Diff columns: line number, sign, and text.
struct Geom {
    num_w: u16,
    gutter_w: u16,
    text_off: u16,
    text_w: u16,
}

impl Geom {
    fn new(d: &FileDiff, area: Rect) -> Self {
        let num_w = digits(d.max_lineno).max(2) as u16;
        let gutter_w = num_w + 2;
        let text_off = gutter_w + 2;
        // One free column on the right, so text doesn't touch the edge.
        let text_w = area.width.saturating_sub(text_off + 1).max(1);
        Self { num_w, gutter_w, text_off, text_w }
    }
}

// ---------------------------------------------------------------- helpers

fn put(buf: &mut Buffer, x: &mut u16, y: u16, max_x: u16, s: &str, style: Style) {
    if *x >= max_x || s.is_empty() {
        return;
    }
    let (nx, _) = buf.set_stringn(*x, y, s, (max_x - *x) as usize, style);
    *x = nx;
}

fn hline(buf: &mut Buffer, x: u16, y: u16, w: u16, color: Color) {
    buf.set_string(x, y, "─".repeat(w as usize), Style::new().fg(color));
}

fn center(buf: &mut Buffer, area: Rect, text: &str, color: Color) {
    let w = (text.width() as u16).min(area.width);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + area.height / 2;
    buf.set_stringn(x, y, text, w as usize, Style::new().fg(color));
}

fn counts_text(adds: usize, dels: usize) -> Vec<(String, bool)> {
    let mut v = Vec::new();
    if adds > 0 {
        v.push((format!("+{adds}"), true));
    }
    if dels > 0 {
        let sep = if adds > 0 { " " } else { "" };
        v.push((format!("{sep}−{dels}"), false));
    }
    v
}

fn digits(n: u32) -> usize {
    n.max(1).to_string().len()
}

/// `@@ -1,3 +1,4 @@ fn foo` → (`@@ -1,3 +1,4 @@`, ` fn foo`).
fn split_hunk(h: &str) -> (&str, &str) {
    match h.get(2..).and_then(|r| r.find("@@")) {
        Some(i) => h.split_at(i + 4),
        None => (h, ""),
    }
}
