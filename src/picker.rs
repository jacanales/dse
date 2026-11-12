//! Interactive group picker (S14): open a list, choose a group, work with it.

use std::io;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Paragraph};

use crate::app::Target;
use crate::ui::{ACCENT, MUTED, SURFACE, TEXT};

/// What the picker is doing after a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickFlow {
    Continue,
    Open,
    Cancel,
}

/// A list of targets the user can walk with the arrows.
#[derive(Debug, Clone)]
pub struct Picker {
    targets: Vec<Target>,
    selection: usize,
}

impl Picker {
    pub fn new(targets: Vec<Target>) -> Self {
        Self {
            targets,
            selection: 0,
        }
    }

    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    pub fn selection(&self) -> usize {
        self.selection
    }

    pub fn selected(&self) -> Option<&Target> {
        self.targets.get(self.selection)
    }

    /// Consume the picker and hand over the row under the cursor.
    pub fn into_selected(self) -> Option<Target> {
        self.targets.into_iter().nth(self.selection)
    }

    /// Move by `delta` rows, wrapping around both ends.
    pub fn move_by(&mut self, delta: isize) {
        if self.targets.is_empty() {
            return;
        }
        let len = self.targets.len() as isize;
        self.selection = (self.selection as isize + delta).rem_euclid(len) as usize;
    }
}

pub fn handle_key(picker: &mut Picker, key: KeyEvent) -> PickFlow {
    match (key.modifiers, key.code) {
        (KeyModifiers::NONE, KeyCode::Char('k')) | (_, KeyCode::Up) => picker.move_by(-1),
        (KeyModifiers::NONE, KeyCode::Char('j')) | (_, KeyCode::Down) => picker.move_by(1),
        (_, KeyCode::Enter) => return PickFlow::Open,
        (KeyModifiers::NONE, KeyCode::Char('q')) | (_, KeyCode::Esc) => return PickFlow::Cancel,
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => return PickFlow::Cancel,
        _ => {}
    }
    PickFlow::Continue
}

pub fn draw(frame: &mut Frame, picker: &Picker) {
    let [title, body, preview, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " dse ",
                Style::new()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " choose a group ",
                Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ])),
        title,
    );

    let items: Vec<ListItem> = picker
        .targets()
        .iter()
        .map(|target| {
            let hosts = target.hosts.len();
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<18}", target.label), Style::new().fg(TEXT)),
                Span::styled(
                    format!("{hosts:>3} host{}", if hosts == 1 { "" } else { "s" }),
                    Style::new().fg(MUTED),
                ),
            ]))
        })
        .collect();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Line::from(Span::styled(" groups ", Style::new().fg(MUTED))));
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(SURFACE))
        .highlight_symbol("▸ ");
    let mut state = ListState::default().with_selected(Some(picker.selection()));
    frame.render_stateful_widget(list, body, &mut state);

    // Preview of the highlighted group, so the choice is informed.
    let hosts = picker
        .selected()
        .map(|target| target.hosts.join(", "))
        .unwrap_or_default();
    let room = preview.width.saturating_sub(4) as usize;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("  {}", crate::ui::truncate(&hosts, room)),
            Style::new().fg(MUTED),
        ))),
        preview,
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑↓ move · Enter open · q cancel ",
            Style::new().fg(MUTED),
        ))),
        hints,
    );
}

/// Show the picker on the real terminal and return the chosen target.
pub async fn run(targets: Vec<Target>) -> io::Result<Option<Target>> {
    if targets.is_empty() {
        return Ok(None);
    }
    let mut terminal = ratatui::try_init()?;
    let outcome = pick_loop(&mut terminal, Picker::new(targets)).await;
    ratatui::try_restore()?;
    outcome
}

async fn pick_loop(
    terminal: &mut DefaultTerminal,
    mut picker: Picker,
) -> io::Result<Option<Target>> {
    let mut keys = crate::tui::spawn_key_reader();
    loop {
        terminal.draw(|frame| draw(frame, &picker))?;
        let Some(key) = keys.recv().await else {
            return Ok(None);
        };
        match handle_key(&mut picker, key) {
            PickFlow::Continue => {}
            PickFlow::Open => return Ok(picker.into_selected()),
            PickFlow::Cancel => return Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn picker() -> Picker {
        Picker::new(vec![
            Target {
                label: "all".into(),
                hosts: vec!["a".into(), "b".into(), "c".into()],
            },
            Target {
                label: "prox".into(),
                hosts: vec!["p".into()],
            },
            Target {
                label: "pve".into(),
                hosts: vec!["x".into(), "y".into()],
            },
        ])
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut picker = picker();
        assert_eq!(picker.selection(), 0);
        picker.move_by(-1);
        assert_eq!(picker.selection(), 2, "moving up from the first row wraps");
        picker.move_by(1);
        assert_eq!(picker.selection(), 0, "moving down from the last row wraps");
        picker.move_by(1);
        assert_eq!(
            picker.selected().map(|target| target.label.as_str()),
            Some("prox")
        );
    }

    #[test]
    fn keys_move_open_and_cancel() {
        let mut picker = picker();
        assert_eq!(
            handle_key(&mut picker, press(KeyCode::Down)),
            PickFlow::Continue
        );
        assert_eq!(picker.selection(), 1);
        handle_key(&mut picker, press(KeyCode::Char('j')));
        assert_eq!(picker.selection(), 2, "j also moves down");
        handle_key(&mut picker, press(KeyCode::Char('k')));
        assert_eq!(picker.selection(), 1, "k also moves up");

        assert_eq!(
            handle_key(&mut picker, press(KeyCode::Enter)),
            PickFlow::Open
        );
        assert_eq!(
            handle_key(&mut picker, press(KeyCode::Esc)),
            PickFlow::Cancel
        );
        assert_eq!(
            handle_key(&mut picker, press(KeyCode::Char('q'))),
            PickFlow::Cancel
        );
    }

    #[test]
    fn the_chosen_target_is_the_one_under_the_cursor() {
        let mut picker = picker();
        picker.move_by(2);
        let chosen = picker.into_selected().expect("a target is selected");
        assert_eq!(chosen.label, "pve");
        assert_eq!(chosen.hosts, vec!["x", "y"]);
    }

    #[test]
    fn a_long_preview_is_truncated_with_an_ellipsis() {
        let picker = Picker::new(vec![Target {
            label: "pve".into(),
            hosts: vec!["pve00.example.net".into(); 12],
        }]);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 8)).expect("terminal");
        terminal.draw(|frame| draw(frame, &picker)).expect("draws");
        let buffer = terminal.backend().buffer().clone();
        let screen: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(
            screen.contains('…'),
            "the preview must be ellipsised, not cut mid-word:\n{screen}"
        );
    }

    #[test]
    fn renders_every_group_with_its_host_count() {
        let mut picker = picker();
        picker.move_by(1);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 12)).expect("terminal");
        terminal.draw(|frame| draw(frame, &picker)).expect("draws");
        let buffer = terminal.backend().buffer().clone();
        let screen: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        for expected in [
            "choose a group",
            "all",
            "prox",
            "pve",
            "3 hosts",
            "1 host",
            "Enter open",
            "q cancel",
        ] {
            assert!(
                screen.contains(expected),
                "missing {expected:?} in:\n{screen}"
            );
        }
    }
}
