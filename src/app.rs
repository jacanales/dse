//! TUI state machine: the embedded sub-terminal plus per-host progress (S2, S3).
//!
//! Rendering lives in `crate::ui`; this module owns state and key handling so the
//! transitions stay testable without a terminal.

use std::collections::VecDeque;
use std::time::Instant;

use crate::exec::{HostState, RunEvent, RunHandle, RunSpec, RunSummary, Stream};
use crate::ssh::SshSettings;

/// Per-host output kept in memory; older lines roll off.
pub const MAX_LINES_PER_HOST: usize = 500;
pub const HISTORY_LIMIT: usize = 50;

/// One target the user can cycle through with Tab: a group or an explicit host list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub label: String,
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    Prompt,
    Detail,
    /// zsh-style reverse search over the command history (Ctrl-R).
    Search(SearchState),
}

/// State of an in-progress reverse search; the prompt is untouched while it lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    pub query: Vec<char>,
    pub cursor: usize,
    /// Index into `App::history` of the current match, newest-first on entry.
    pub match_idx: Option<usize>,
    previous_input: Vec<char>,
    previous_cursor: usize,
}

/// A line of remote output tagged with the stream it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    pub stream: Stream,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostView {
    pub name: String,
    pub state: HostState,
    pub elapsed_ms: Option<u128>,
    pub output: VecDeque<OutputLine>,
}

impl HostView {
    fn new(name: String) -> Self {
        Self {
            name,
            state: HostState::Pending,
            elapsed_ms: None,
            output: VecDeque::new(),
        }
    }

    fn push_line(&mut self, stream: Stream, text: String) {
        if self.output.len() >= MAX_LINES_PER_HOST {
            self.output.pop_front();
        }
        self.output.push_back(OutputLine { stream, text });
    }

    /// Last line of output, used for the compact preview in the host list.
    pub fn preview(&self) -> Option<&str> {
        self.output.back().map(|line| line.text.as_str())
    }

    /// Short right-aligned status text.
    pub fn status_label(&self) -> String {
        match &self.state {
            HostState::Pending => "queued".to_string(),
            HostState::Running => "running".to_string(),
            HostState::Succeeded => "ok".to_string(),
            HostState::Failed(Some(code)) => format!("exit {code}"),
            HostState::Failed(None) => "signal".to_string(),
            HostState::Errored(_) => "error".to_string(),
            HostState::TimedOut(_) => "timeout".to_string(),
            HostState::Cancelled => "cancelled".to_string(),
        }
    }
}

pub struct App {
    pub targets: Vec<Target>,
    pub current: usize,
    pub hosts: Vec<HostView>,
    /// Command being typed in the sub-terminal.
    pub input: Vec<char>,
    pub cursor: usize,
    pub history: Vec<String>,
    pub history_pos: Option<usize>,
    /// Line being typed before the user started walking the history.
    draft: Option<Vec<char>>,
    pub focus: Focus,
    pub selection: usize,
    pub detail_scroll: usize,
    pub tick: u64,
    pub running: bool,
    pub started: Option<Instant>,
    pub summary: Option<RunSummary>,
    pub notice: String,
    run: Option<RunHandle>,
    ssh: SshSettings,
    concurrency: usize,
}

impl App {
    pub fn new(targets: Vec<Target>, initial: usize, ssh: SshSettings, concurrency: usize) -> Self {
        let current = initial.min(targets.len().saturating_sub(1));
        let hosts = targets
            .get(current)
            .map(|target| target.hosts.iter().cloned().map(HostView::new).collect())
            .unwrap_or_default();
        Self {
            targets,
            current,
            hosts,
            input: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            history_pos: None,
            draft: None,
            focus: Focus::Prompt,
            selection: 0,
            detail_scroll: 0,
            tick: 0,
            running: false,
            started: None,
            summary: None,
            notice: "type a command and press Enter to fan it out".to_string(),
            run: None,
            ssh,
            concurrency,
        }
    }

    pub fn target(&self) -> Option<&Target> {
        self.targets.get(self.current)
    }

    pub fn input_string(&self) -> String {
        self.input.iter().collect()
    }

    /// Completed hosts/ total hosts, used by the gauge.
    pub fn progress(&self) -> (usize, usize) {
        let done = self
            .hosts
            .iter()
            .filter(|host| matches!(host.state, HostState::Succeeded) || host.state.is_failure())
            .count();
        (done, self.hosts.len())
    }

    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut ok = 0;
        let mut failed = 0;
        let mut running = 0;
        let mut pending = 0;
        for host in &self.hosts {
            match host.state {
                HostState::Succeeded => ok += 1,
                HostState::Running => running += 1,
                HostState::Pending => pending += 1,
                _ => failed += 1,
            }
        }
        (ok, failed, running, pending)
    }

    /// Braille spinner frame; animated by the tick.
    pub fn spinner(&self) -> char {
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        FRAMES[(self.tick as usize) % FRAMES.len()]
    }

    pub fn elapsed_ms(&self) -> u128 {
        self.started
            .map(|started| started.elapsed().as_millis())
            .unwrap_or(0)
    }

    /// Cycle to the next target; only allowed while nothing is running.
    pub fn cycle_target(&mut self) {
        if self.running || self.targets.is_empty() {
            return;
        }
        self.current = (self.current + 1) % self.targets.len();
        self.focus = Focus::Prompt;
        self.reset_hosts();
    }

    fn reset_hosts(&mut self) {
        self.hosts = self
            .target()
            .map(|target| target.hosts.iter().cloned().map(HostView::new).collect())
            .unwrap_or_default();
        self.selection = 0;
        self.detail_scroll = 0;
        self.summary = None;
        self.notice = "type a command and press Enter to fan it out".to_string();
    }

    /// Start a fan-out with the typed command, wired to the long-lived event channel.
    pub fn start_run(&mut self, events: tokio::sync::mpsc::UnboundedSender<RunEvent>) -> bool {
        let command = self.input_string().trim().to_string();
        let Some(target) = self.target() else {
            return false;
        };
        let hosts = target.hosts.clone();
        if command.is_empty() || self.running || hosts.is_empty() {
            return false;
        }
        self.record_history(&command);
        self.history_pos = None;
        self.reset_hosts();
        self.running = true;
        self.started = Some(Instant::now());
        self.input.clear();
        self.cursor = 0;
        self.notice = format!("running on {} hosts", hosts.len());

        let spec = RunSpec {
            hosts,
            command,
            concurrency: self.concurrency,
            ssh: self.ssh.clone(),
            timeout: None,
        };
        self.run = Some(crate::exec::spawn_run(spec, events));
        true
    }

    /// Feed one engine event into the view.
    pub fn apply(&mut self, event: RunEvent) {
        match event {
            RunEvent::Started { idx } => {
                if let Some(host) = self.hosts.get_mut(idx) {
                    host.state = HostState::Running;
                }
            }
            RunEvent::Output { idx, stream, line } => {
                if let Some(host) = self.hosts.get_mut(idx) {
                    host.push_line(stream, line);
                }
            }
            RunEvent::Finished {
                idx,
                state,
                elapsed_ms,
            } => {
                if let Some(host) = self.hosts.get_mut(idx) {
                    host.state = state;
                    host.elapsed_ms = Some(elapsed_ms);
                }
                if self
                    .hosts
                    .iter()
                    .all(|host| !matches!(host.state, HostState::Pending | HostState::Running))
                {
                    self.finish();
                }
            }
        }
    }

    /// Mark the run as finished and build the summary from the host views.
    fn finish(&mut self) {
        if !self.running {
            return;
        }
        self.running = false;
        self.run = None;
        let elapsed_ms = self.elapsed_ms();
        let failures: Vec<_> = self
            .hosts
            .iter()
            .enumerate()
            .filter(|(_, host)| host.state.is_failure())
            .map(|(idx, host)| crate::exec::Failure {
                idx,
                host: host.name.clone(),
                state: host.state.clone(),
            })
            .collect();
        let succeeded = self
            .hosts
            .iter()
            .filter(|h| h.state == HostState::Succeeded)
            .count();
        let summary = RunSummary {
            total: self.hosts.len(),
            succeeded,
            failures,
            elapsed_ms,
        };
        self.notice = if summary.is_success() {
            format!(
                "done · {} hosts in {:.1}s",
                summary.total,
                summary.elapsed_ms as f64 / 1000.0
            )
        } else {
            format!(
                "{} of {} hosts failed · {:.1}s",
                summary.failures.len(),
                summary.total,
                summary.elapsed_ms as f64 / 1000.0
            )
        };
        self.summary = Some(summary);
    }

    pub fn cancel_run(&mut self) {
        if let Some(run) = self.run.take() {
            run.cancel();
        }
        if self.running {
            self.running = false;
            for host in &mut self.hosts {
                if matches!(host.state, HostState::Pending | HostState::Running) {
                    host.state = HostState::Cancelled;
                }
            }
            self.finish_cancelled();
        }
    }

    fn finish_cancelled(&mut self) {
        self.notice = "run cancelled".to_string();
        let elapsed_ms = self.elapsed_ms();
        let succeeded = self
            .hosts
            .iter()
            .filter(|h| h.state == HostState::Succeeded)
            .count();
        let failures: Vec<_> = self
            .hosts
            .iter()
            .enumerate()
            .filter(|(_, host)| host.state.is_failure())
            .map(|(idx, host)| crate::exec::Failure {
                idx,
                host: host.name.clone(),
                state: host.state.clone(),
            })
            .collect();
        self.summary = Some(RunSummary {
            total: self.hosts.len(),
            succeeded,
            failures,
            elapsed_ms,
        });
    }

    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    // --- sub-terminal editing -------------------------------------------------

    pub fn insert_char(&mut self, ch: char) {
        self.input.insert(self.cursor, ch);
        self.cursor += 1;
        self.history_pos = None;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.input.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let target = self.cursor as isize + delta;
        self.cursor = target.clamp(0, self.input.len() as isize) as usize;
    }

    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_end(&mut self) {
        self.cursor = self.input.len();
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_pos = None;
    }

    /// Recall history: `back` walks towards older entries.
    /// Remember a command, merging consecutive repeats like zsh's HIST_IGNORE_DUPS.
    pub fn record_history(&mut self, command: &str) {
        if self.history.last().map(String::as_str) == Some(command) {
            return;
        }
        self.history.push(command.to_string());
        if self.history.len() > HISTORY_LIMIT {
            self.history.remove(0);
        }
    }

    /// Walk the history with ↑/↓. Leaving history restores the unfinished line.
    pub fn recall_history(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.history_pos, back) {
            (None, true) => {
                self.draft = Some(self.input.clone());
                Some(self.history.len() - 1)
            }
            (None, false) => None,
            (Some(0), true) => Some(0),
            (Some(pos), true) => Some(pos - 1),
            (Some(pos), false) if pos + 1 < self.history.len() => Some(pos + 1),
            (Some(_), false) => None,
        };
        self.history_pos = next;
        match next {
            Some(pos) => {
                self.input = self.history[pos].chars().collect();
                self.cursor = self.input.len();
            }
            None => {
                // Back at the live line: restore the draft when walking past it.
                let draft = self.draft.take().unwrap_or_default();
                self.cursor = draft.len();
                self.input = draft;
            }
        }
    }

    // --- reverse search (Ctrl-R) ---------------------------------------------

    pub fn is_searching(&self) -> bool {
        matches!(self.focus, Focus::Search(_))
    }

    pub fn search_state(&self) -> Option<&SearchState> {
        match &self.focus {
            Focus::Search(state) => Some(state),
            _ => None,
        }
    }

    pub fn search_query(&self) -> Option<String> {
        self.search_state()
            .map(|state| state.query.iter().collect())
    }

    /// Most recent command containing the query.
    pub fn search_match_text(&self) -> Option<String> {
        let index = self.search_state()?.match_idx?;
        self.history.get(index).cloned()
    }

    pub fn begin_search(&mut self) -> bool {
        if self.history.is_empty() {
            self.notice = "no command history yet".to_string();
            return false;
        }
        self.focus = Focus::Search(SearchState {
            query: Vec::new(),
            cursor: 0,
            match_idx: None,
            previous_input: self.input.clone(),
            previous_cursor: self.cursor,
        });
        self.refresh_search();
        true
    }

    pub fn search_insert(&mut self, ch: char) {
        match &mut self.focus {
            Focus::Search(state) => {
                state.query.insert(state.cursor, ch);
                state.cursor += 1;
            }
            _ => return,
        }
        self.refresh_search();
    }

    pub fn search_backspace(&mut self) {
        let empty = match &mut self.focus {
            Focus::Search(state) => {
                if state.cursor > 0 {
                    state.cursor -= 1;
                    state.query.remove(state.cursor);
                }
                state.query.is_empty()
            }
            _ => return,
        };
        if empty {
            self.cancel_search();
        } else {
            self.refresh_search();
        }
    }

    pub fn search_move_cursor(&mut self, delta: isize) {
        if let Focus::Search(state) = &mut self.focus {
            let target = state.cursor as isize + delta;
            state.cursor = target.clamp(0, state.query.len() as isize) as usize;
        }
    }

    /// Step to the next older (`older`) or newer match; stops at the ends.
    pub fn search_cycle(&mut self, older: bool) {
        let Some(query) = self.search_query() else {
            return;
        };
        let Some(current) = self.search_state().and_then(|state| state.match_idx) else {
            return;
        };
        let found = if older {
            search_backwards(&self.history, &query, Some(current))
        } else {
            search_forwards(&self.history, &query, current)
        };
        if let (Some(index), Focus::Search(state)) = (found, &mut self.focus) {
            state.match_idx = Some(index);
        }
    }

    /// Put the match in the prompt (never runs it) so it can be edited first.
    pub fn accept_search(&mut self) {
        let Focus::Search(state) = std::mem::replace(&mut self.focus, Focus::Prompt) else {
            return;
        };
        match state
            .match_idx
            .and_then(|index| self.history.get(index))
            .cloned()
        {
            Some(text) => {
                self.cursor = text.chars().count();
                self.input = text.chars().collect();
                self.history_pos = None;
            }
            None => {
                self.input = state.previous_input;
                self.cursor = state.previous_cursor;
            }
        }
    }

    pub fn cancel_search(&mut self) {
        let Focus::Search(state) = std::mem::replace(&mut self.focus, Focus::Prompt) else {
            return;
        };
        self.input = state.previous_input;
        self.cursor = state.previous_cursor;
    }

    /// Recompute the match from the current query, always newest-first.
    fn refresh_search(&mut self) {
        let Some(query) = self.search_query() else {
            return;
        };
        let found = search_backwards(&self.history, &query, None);
        if let Focus::Search(state) = &mut self.focus {
            state.match_idx = found;
        }
    }

    pub fn select_delta(&mut self, delta: isize) {
        if self.hosts.is_empty() {
            return;
        }
        let target = self.selection as isize + delta;
        self.selection = target.clamp(0, self.hosts.len() as isize - 1) as usize;
    }

    pub fn open_detail(&mut self) {
        if self.hosts.is_empty() {
            return;
        }
        self.focus = Focus::Detail;
        self.detail_scroll = 0;
    }

    pub fn close_detail(&mut self) {
        self.focus = Focus::Prompt;
    }

    pub fn scroll_detail(&mut self, delta: isize) {
        let target = self.detail_scroll as isize + delta;
        self.detail_scroll = target.max(0) as usize;
    }

    pub fn selected(&self) -> Option<&HostView> {
        self.hosts.get(self.selection)
    }
}

/// Case-insensitive substring match, like zsh's default reverse search.
fn matches_query(entry: &str, query: &str) -> bool {
    query.is_empty() || entry.to_lowercase().contains(&query.to_lowercase())
}

/// Newest entry before `before` (exclusive) that matches.
fn search_backwards(history: &[String], query: &str, before: Option<usize>) -> Option<usize> {
    let end = before.unwrap_or(history.len()).min(history.len());
    (0..end)
        .rev()
        .find(|index| matches_query(&history[*index], query))
}

/// Next older-side entry after `after` (exclusive) that matches.
fn search_forwards(history: &[String], query: &str, after: usize) -> Option<usize> {
    ((after + 1)..history.len()).find(|index| matches_query(&history[*index], query))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let targets = vec![
            Target {
                label: "pve".to_string(),
                hosts: vec!["pve00".into(), "pve01".into()],
            },
            Target {
                label: "prox".to_string(),
                hosts: vec!["prox00".into()],
            },
        ];
        App::new(targets, 0, SshSettings::default(), 4)
    }

    #[test]
    fn typing_edits_at_the_cursor() {
        let mut app = app();
        for ch in "apt update".chars() {
            app.insert_char(ch);
        }
        assert_eq!(app.input_string(), "apt update");
        app.cursor_home();
        app.insert_char('#');
        assert_eq!(app.input_string(), "#apt update");
        app.cursor_end();
        app.backspace();
        assert_eq!(app.input_string(), "#apt updat");
        app.delete();
        assert_eq!(app.input_string(), "#apt updat");
    }

    #[test]
    fn tab_cycles_targets_and_resets_hosts() {
        let mut app = app();
        app.open_detail();
        app.cycle_target();
        assert_eq!(app.target().expect("target").label, "prox");
        assert_eq!(app.hosts.len(), 1);
        assert_eq!(app.focus, Focus::Prompt);
        app.cycle_target();
        assert_eq!(app.target().expect("target").label, "pve");
    }

    #[test]
    fn events_move_hosts_through_states_and_summarize() {
        let mut app = app();
        app.input = "apt update".chars().collect();
        app.started = Some(Instant::now());
        app.running = true;

        app.apply(RunEvent::Started { idx: 0 });
        app.apply(RunEvent::Output {
            idx: 0,
            stream: Stream::Stdout,
            line: "Reading package lists".into(),
        });
        app.apply(RunEvent::Finished {
            idx: 0,
            state: HostState::Succeeded,
            elapsed_ms: 1200,
        });
        assert!(app.running, "one host still pending");

        app.apply(RunEvent::Started { idx: 1 });
        app.apply(RunEvent::Output {
            idx: 1,
            stream: Stream::Stderr,
            line: "E: boom".into(),
        });
        app.apply(RunEvent::Finished {
            idx: 1,
            state: HostState::Failed(Some(100)),
            elapsed_ms: 900,
        });

        assert!(!app.running);
        let summary = app.summary.clone().expect("summary");
        assert_eq!(summary.total, 2);
        assert_eq!(summary.succeeded, 1);
        assert_eq!(summary.failures[0].host, "pve01");
        assert_eq!(app.progress(), (2, 2));
        assert_eq!(app.counts(), (1, 1, 0, 0));
        assert_eq!(app.hosts[0].preview(), Some("Reading package lists"));
        assert!(app.notice.starts_with("1 of 2 hosts failed"));
    }

    #[test]
    fn cancel_marks_unfinished_hosts_cancelled() {
        let mut app = app();
        app.running = true;
        app.started = Some(Instant::now());
        app.apply(RunEvent::Started { idx: 0 });
        app.cancel_run();
        assert!(!app.running);
        assert_eq!(app.hosts[0].state, HostState::Cancelled);
        assert_eq!(app.hosts[1].state, HostState::Cancelled);
        assert_eq!(app.summary.expect("summary").failures.len(), 2);
        assert_eq!(app.notice, "run cancelled");
    }

    #[test]
    fn history_walks_back_and_forth() {
        let mut app = app();
        app.history = vec!["uptime".into(), "apt update".into()];
        app.recall_history(true);
        assert_eq!(app.input_string(), "apt update");
        app.recall_history(true);
        assert_eq!(app.input_string(), "uptime");
        app.recall_history(false);
        assert_eq!(app.input_string(), "apt update");
        app.recall_history(false);
        assert_eq!(app.input_string(), "");
    }

    #[test]
    fn walking_history_keeps_the_unfinished_line() {
        let mut app = app();
        app.history = vec!["uptime".into(), "apt update".into()];
        for ch in "df -h".chars() {
            app.insert_char(ch);
        }

        app.recall_history(true);
        assert_eq!(app.input_string(), "apt update");
        app.recall_history(true);
        assert_eq!(app.input_string(), "uptime");
        app.recall_history(false);
        app.recall_history(false);
        assert_eq!(
            app.input_string(),
            "df -h",
            "leaving history restores the unfinished line"
        );
    }

    #[test]
    fn repeated_commands_are_recorded_once_in_a_row() {
        let mut app = app();
        app.record_history("uptime");
        app.record_history("uptime");
        app.record_history("apt update");
        app.record_history("uptime");
        assert_eq!(app.history, vec!["uptime", "apt update", "uptime"]);

        for index in 0..(HISTORY_LIMIT + 5) {
            app.record_history(&format!("cmd {index}"));
        }
        assert_eq!(app.history.len(), HISTORY_LIMIT);
        assert_eq!(app.history.last().map(String::as_str), Some("cmd 54"));
    }

    #[test]
    fn reverse_search_finds_the_most_recent_match_first() {
        let mut app = app();
        app.history = vec![
            "apt update".into(),
            "uptime".into(),
            "apt install nginx".into(),
        ];
        assert!(app.begin_search());

        for ch in "apt".chars() {
            app.search_insert(ch);
        }
        assert_eq!(
            app.search_match_text().as_deref(),
            Some("apt install nginx")
        );

        app.search_cycle(true);
        assert_eq!(app.search_match_text().as_deref(), Some("apt update"));
        app.search_cycle(true);
        assert_eq!(
            app.search_match_text().as_deref(),
            Some("apt update"),
            "stops at the oldest match"
        );

        app.search_cycle(false);
        assert_eq!(
            app.search_match_text().as_deref(),
            Some("apt install nginx")
        );
        app.search_cycle(false);
        assert_eq!(
            app.search_match_text().as_deref(),
            Some("apt install nginx"),
            "stops at the newest match"
        );
    }

    #[test]
    fn reverse_search_ignores_case_and_accepting_fills_the_prompt() {
        let mut app = app();
        app.history = vec!["APT UPDATE".into(), "uptime".into()];
        app.begin_search();
        for ch in "apt".chars() {
            app.search_insert(ch);
        }
        assert_eq!(app.search_match_text().as_deref(), Some("APT UPDATE"));

        app.search_insert('x');
        assert_eq!(app.search_match_text(), None, "no match keeps the query");

        app.search_backspace();
        assert_eq!(app.search_match_text().as_deref(), Some("APT UPDATE"));

        app.accept_search();
        assert_eq!(app.focus, Focus::Prompt);
        assert_eq!(app.input_string(), "APT UPDATE");
        assert_eq!(app.cursor, "APT UPDATE".len(), "cursor lands at the end");
    }

    #[test]
    fn cancelling_and_accepting_without_a_match_restore_the_previous_line() {
        let mut app = app();
        app.history = vec!["uptime".into()];
        for ch in "df -h".chars() {
            app.insert_char(ch);
        }

        app.begin_search();
        app.search_insert('z');
        app.cancel_search();
        assert_eq!(app.focus, Focus::Prompt);
        assert_eq!(app.input_string(), "df -h");

        app.begin_search();
        app.search_insert('z');
        app.accept_search();
        assert_eq!(
            app.input_string(),
            "df -h",
            "no match means nothing to reuse"
        );
        assert_eq!(app.cursor, "df -h".len());
    }

    #[test]
    fn search_without_history_explains_itself() {
        let mut app = app();
        assert!(!app.begin_search());
        assert_eq!(app.focus, Focus::Prompt);
        assert_eq!(app.notice, "no command history yet");
    }

    /// End-to-end path of S2: typing a command in the sub-terminal runs it on
    /// every host of the selected group and lands the output in the view.
    #[tokio::test]
    async fn typed_command_fans_out_to_every_host_in_the_target() {
        let dir = tempfile::tempdir().expect("temp dir");
        let stub = dir.path().join("ssh");
        std::fs::write(
            &stub,
            "#!/bin/sh\nhost=\"$5\"\ncmd=\"$6\"\necho \"ran $cmd on $host\"\nexit 0\n",
        )
        .expect("stub written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let ssh = SshSettings {
            bin: stub.display().to_string(),
            connect_timeout_secs: 1,
            options: Vec::new(),
        };
        let targets = vec![Target {
            label: "pve".to_string(),
            hosts: vec!["pve00".into(), "pve01".into(), "pve02".into()],
        }];
        let mut app = App::new(targets, 0, ssh, 2);
        for ch in "apt update".chars() {
            app.insert_char(ch);
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(app.start_run(tx), "run starts");
        assert!(app.running);
        assert!(
            app.input.is_empty(),
            "prompt is cleared for the next command"
        );
        while let Some(event) = rx.recv().await {
            app.apply(event);
        }

        assert!(!app.running);
        let summary = app.summary.clone().expect("summary");
        assert_eq!(summary.succeeded, 3);
        let previews: Vec<String> = app
            .hosts
            .iter()
            .map(|host| host.preview().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            previews,
            vec![
                "ran apt update on pve00",
                "ran apt update on pve01",
                "ran apt update on pve02"
            ]
        );
        assert_eq!(app.history, vec!["apt update".to_string()]);
    }

    #[test]
    fn output_lines_are_capped() {
        let mut app = app();
        for i in 0..(MAX_LINES_PER_HOST + 10) {
            app.apply(RunEvent::Output {
                idx: 0,
                stream: Stream::Stdout,
                line: format!("line {i}"),
            });
        }
        assert_eq!(app.hosts[0].output.len(), MAX_LINES_PER_HOST);
        assert_eq!(
            app.hosts[0].preview(),
            Some(format!("line {}", MAX_LINES_PER_HOST + 9).as_str())
        );
    }
}
