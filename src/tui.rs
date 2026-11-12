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
        (KeyModifiers::CONTROL, KeyCode::Char('p')) => app.recall_history(true),
        (KeyModifiers::CONTROL, KeyCode::Char('n')) => app.recall_history(false),
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
        (_, KeyCode::Up) => match app.focus {
            Focus::Prompt => app.select_delta(-1),
            Focus::Detail => app.scroll_detail(-1),
        },
        (_, KeyCode::Down) => match app.focus {
            Focus::Prompt => app.select_delta(1),
            Focus::Detail => app.scroll_detail(1),
        },
        (_, KeyCode::PageUp) => app.scroll_detail(-10),
        (_, KeyCode::PageDown) => app.scroll_detail(10),
        (_, KeyCode::Esc) => match app.focus {
            Focus::Detail => app.close_detail(),
            Focus::Prompt => app.clear_input(),
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
