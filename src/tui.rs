//! Terminal driver: alternate screen, key reader and the async loop that binds
//! the sub-terminal to the fan-out engine (S2, S3).

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc::{UnboundedSender, channel, unbounded_channel};
use tokio::time::MissedTickBehavior;

use crate::app::{App, Focus};
use crate::exec::RunEvent;
use crate::ui;

/// Run the interactive view until the user quits.
pub async fn run(mut app: App) -> io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    let (run_tx, mut run_rx) = unbounded_channel::<RunEvent>();
    let mut keys = spawn_key_reader();
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let result = loop {
        if let Err(err) = terminal.draw(|frame| ui::draw(frame, &app)) {
            break Err(err);
        }
        let quit = tokio::select! {
            key = keys.recv() => match key {
                Some(key) => handle_key(&mut app, key, &run_tx),
                None => true,
            },
            event = run_rx.recv() => {
                if let Some(event) = event {
                    app.apply(event);
                }
                false
            }
            _ = ticker.tick() => {
                app.tick();
                false
            }
        };
        if quit {
            break Ok(());
        }
    };

    // Never leave ssh children running behind a restored terminal.
    app.cancel_run();
    ratatui::try_restore()?;
    result
}

/// Read keys on a blocking thread; terminal input is not async on every platform.
fn spawn_key_reader() -> tokio::sync::mpsc::Receiver<KeyEvent> {
    let (tx, rx) = channel(64);
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(80)) {
                Ok(true) => match event::read() {
                    Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        if tx.blocking_send(key).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                },
                Ok(false) => {}
                Err(_) => return,
            }
        }
    });
    rx
}

fn handle_key(app: &mut App, key: KeyEvent, run_tx: &UnboundedSender<RunEvent>) -> bool {
    // The reverse search owns the keyboard while it is open.
    if app.is_searching() {
        return handle_search_key(app, key);
    }
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            if app.running {
                app.cancel_run();
            } else if !app.input.is_empty() {
                app.clear_input();
            } else {
                return true;
            }
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => return true,
        (KeyModifiers::CONTROL, KeyCode::Char('r')) => {
            app.begin_search();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('p')) => app.select_delta(-1),
        (KeyModifiers::CONTROL, KeyCode::Char('n')) => app.select_delta(1),
        (KeyModifiers::CONTROL, KeyCode::Char('a')) => app.cursor_home(),
        (KeyModifiers::CONTROL, KeyCode::Char('e')) => app.cursor_end(),
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => app.clear_input(),
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(ch)) => {
            if app.focus == Focus::Prompt {
                if ch == 'q' && app.input.is_empty() && !app.running {
                    return true;
                }
                app.insert_char(ch);
            }
        }
        (_, KeyCode::Backspace) => {
            if app.focus == Focus::Prompt {
                app.backspace();
            } else {
                app.close_detail();
            }
        }
        (_, KeyCode::Delete) => {
            if app.focus == Focus::Prompt {
                app.delete();
            }
        }
        (_, KeyCode::Left) => {
            if app.focus == Focus::Prompt {
                app.move_cursor(-1);
            }
        }
        (_, KeyCode::Right) => {
            if app.focus == Focus::Prompt {
                app.move_cursor(1);
            }
        }
        (_, KeyCode::Home) => app.cursor_home(),
        (_, KeyCode::End) => app.cursor_end(),
        (_, KeyCode::Tab) => app.cycle_target(),
        (_, KeyCode::Up) => match &app.focus {
            // Terminal semantics: the arrows walk the command history.
            Focus::Prompt => app.recall_history(true),
            _ => app.scroll_detail(-1),
        },
        (_, KeyCode::Down) => match &app.focus {
            Focus::Prompt => app.recall_history(false),
            _ => app.scroll_detail(1),
        },
        (_, KeyCode::PageUp) => app.scroll_detail(-10),
        (_, KeyCode::PageDown) => app.scroll_detail(10),
        (_, KeyCode::Esc) => match &app.focus {
            Focus::Detail => app.close_detail(),
            _ => app.clear_input(),
        },
        (_, KeyCode::Enter) => {
            if app.focus == Focus::Detail {
                app.close_detail();
            } else if app.input.iter().any(|ch| !ch.is_whitespace()) {
                app.start_run(run_tx.clone());
            } else {
                app.open_detail();
            }
        }
        _ => {}
    }
    false
}

/// Keys while the reverse search is open: type to refine, `Ctrl-R`/`↑` step to
/// older matches, `Enter` accepts into the prompt, `Esc` cancels.
fn handle_search_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => app.cancel_search(),
        (KeyModifiers::CONTROL, KeyCode::Char('r')) => app.search_cycle(true),
        (KeyModifiers::CONTROL, KeyCode::Char('s')) => app.search_cycle(false),
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(ch)) => app.search_insert(ch),
        (_, KeyCode::Backspace) => app.search_backspace(),
        (_, KeyCode::Left) => app.search_move_cursor(-1),
        (_, KeyCode::Right) => app.search_move_cursor(1),
        (_, KeyCode::Up) => app.search_cycle(true),
        (_, KeyCode::Down) => app.search_cycle(false),
        (_, KeyCode::Enter) => app.accept_search(),
        (_, KeyCode::Esc) => app.cancel_search(),
        _ => {}
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, SearchState, Target};
    use crate::ssh::SshSettings;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use tokio::sync::mpsc::unbounded_channel;

    fn app() -> App {
        let targets = vec![Target {
            label: "pve".into(),
            hosts: vec!["pve00".into(), "pve01".into()],
        }];
        App::new(targets, 0, SshSettings::default(), 4)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press_ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            handle_key(app, press(KeyCode::Char(ch)), &unbounded_channel().0);
        }
    }

    fn assert_press_only(key: KeyEvent) {
        assert_eq!(key.kind, KeyEventKind::Press);
    }

    #[test]
    fn ctrl_r_opens_the_reverse_search() {
        let mut app = app();
        app.history = vec!["uptime".into()];
        assert_press_only(press_ctrl('r'));

        handle_key(&mut app, press_ctrl('r'), &unbounded_channel().0);
        assert!(app.is_searching(), "Ctrl-R must open the reverse search");

        type_text(&mut app, "upt");
        assert_eq!(app.search_query().as_deref(), Some("upt"));
        assert_eq!(app.search_match_text().as_deref(), Some("uptime"));

        handle_key(&mut app, press(KeyCode::Enter), &unbounded_channel().0);
        assert!(!app.is_searching(), "Enter leaves the search");
        assert_eq!(app.input_string(), "uptime");
    }

    #[test]
    fn arrows_walk_history_and_ctrl_p_n_move_the_host_selection() {
        let mut app = app();
        app.history = vec!["one".into(), "two".into()];

        handle_key(&mut app, press(KeyCode::Up), &unbounded_channel().0);
        assert_eq!(app.input_string(), "two", "↑ walks the history");
        handle_key(&mut app, press(KeyCode::Up), &unbounded_channel().0);
        assert_eq!(app.input_string(), "one");
        handle_key(&mut app, press(KeyCode::Down), &unbounded_channel().0);
        assert_eq!(app.input_string(), "two");
        handle_key(&mut app, press(KeyCode::Down), &unbounded_channel().0);
        assert_eq!(app.input_string(), "");

        handle_key(&mut app, press_ctrl('n'), &unbounded_channel().0);
        assert_eq!(app.selection, 1, "Ctrl-N moves to the next host");
        handle_key(&mut app, press_ctrl('p'), &unbounded_channel().0);
        assert_eq!(app.selection, 0, "Ctrl-P moves to the previous host");
    }

    #[test]
    fn search_keys_filter_cycle_and_cancel() {
        let mut app = app();
        app.history = vec!["apt update".into(), "uptime".into(), "apt install".into()];
        handle_key(&mut app, press_ctrl('r'), &unbounded_channel().0);
        type_text(&mut app, "apt");
        assert_eq!(app.search_match_text().as_deref(), Some("apt install"));

        handle_key(&mut app, press(KeyCode::Up), &unbounded_channel().0);
        assert_eq!(app.search_match_text().as_deref(), Some("apt update"));
        handle_key(&mut app, press(KeyCode::Down), &unbounded_channel().0);
        assert_eq!(app.search_match_text().as_deref(), Some("apt install"));
        handle_key(&mut app, press_ctrl('r'), &unbounded_channel().0);
        assert_eq!(
            app.search_match_text().as_deref(),
            Some("apt update"),
            "Ctrl-R steps to the older match"
        );

        handle_key(&mut app, press(KeyCode::Esc), &unbounded_channel().0);
        assert!(!app.is_searching());
        assert_eq!(app.input_string(), "", "Esc restores the pre-search line");
        assert!(matches!(app.focus, Focus::Prompt));
        let _: Option<&SearchState> = app.search_state();
    }
}
