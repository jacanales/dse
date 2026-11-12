//! Rendering for the fan-out view (S3): one row per host, a live gauge and an
//! embedded prompt. Pure function of `App`, so it renders in tests too.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Gauge, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, Focus, HostView, SearchState};
use crate::exec::{HostState, Stream};

pub const ACCENT: Color = Color::Rgb(0x6e, 0xe7, 0xf9);
pub const SUCCESS: Color = Color::Rgb(0x34, 0xd3, 0x99);
pub const DANGER: Color = Color::Rgb(0xf8, 0x71, 0x71);
pub const WARN: Color = Color::Rgb(0xfb, 0xbf, 0x24);
pub const MUTED: Color = Color::Rgb(0x6b, 0x72, 0x80);
pub const TEXT: Color = Color::Rgb(0xe5, 0xe7, 0xeb);
pub const SURFACE: Color = Color::Rgb(0x11, 0x18, 0x27);
pub const TRACK: Color = Color::Rgb(0x1f, 0x29, 0x37);

pub fn draw(frame: &mut Frame, app: &App) {
    let [header, body, prompt, hints] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    match &app.focus {
        Focus::Detail => draw_detail(frame, app, body),
        // Searching keeps the host list in view; the prompt shows the search.
        _ => draw_hosts(frame, app, body),
    }
    draw_prompt(frame, app, prompt);
    draw_hints(frame, app, hints);
}

fn rounded(color: Color) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let label = app
        .target()
        .map(|target| target.label.clone())
        .unwrap_or_else(|| "no target".to_string());
    let title = Line::from(vec![
        Span::styled(
            " dse ",
            Style::new()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {label} "),
            Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("· {} hosts ", app.hosts.len()),
            Style::new().fg(MUTED),
        ),
    ]);
    let block = rounded(MUTED).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (done, total) = app.progress();
    let (ok, failed, running, pending) = app.counts();
    let ratio = if total == 0 {
        0.0
    } else {
        done as f64 / total as f64
    };
    let color = if failed > 0 {
        DANGER
    } else if app.running {
        ACCENT
    } else if total > 0 && done == total {
        SUCCESS
    } else {
        MUTED
    };
    let running_label = if running > 0 {
        format!("{} {running} running", app.spinner())
    } else {
        format!("{running} running")
    };
    let label = format!(
        " {done}/{total}  {ok} ok · {failed} failed · {running_label} · {pending} queued    {:.1}s ",
        app.elapsed_ms() as f64 / 1000.0
    );
    let gauge = Gauge::default()
        .ratio(ratio.clamp(0.0, 1.0))
        .label(label)
        .gauge_style(Style::new().fg(color).bg(TRACK));
    frame.render_widget(gauge, inner);
}

fn draw_hosts(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .hosts
        .iter()
        .map(|host| ListItem::new(host_line(host, app)))
        .collect();
    let title = Line::from(Span::styled(
        format!(" {} ", app.notice),
        Style::new().fg(
            if app
                .summary
                .as_ref()
                .is_some_and(|summary| !summary.is_success())
            {
                DANGER
            } else {
                MUTED
            },
        ),
    ));
    let block = rounded(MUTED).title(title);
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(SURFACE));

    if app.hosts.is_empty() {
        let placeholder = Paragraph::new(Line::from(Span::styled(
            "  no hosts in this target",
            Style::new().fg(MUTED),
        )))
        .block(rounded(MUTED).title(Line::from(Span::styled(" hosts ", Style::new().fg(MUTED)))));
        frame.render_widget(placeholder, area);
        return;
    }

    let mut state = ListState::default().with_selected(Some(app.selection));
    frame.render_stateful_widget(list, area, &mut state);
}

fn host_line(host: &HostView, app: &App) -> Line<'static> {
    let (glyph, glyph_style) = glyph_for(&host.state, app);
    let duration = match host.elapsed_ms {
        Some(ms) => format!("{:.1}s", ms as f64 / 1000.0),
        None if host.state == HostState::Running => "…".to_string(),
        None => "-".to_string(),
    };
    let mut spans = vec![
        Span::styled(format!(" {glyph} "), glyph_style),
        Span::styled(
            format!("{:<26}", truncate(&host.name, 26)),
            Style::new().fg(TEXT),
        ),
        Span::styled(
            format!("{:>9} ", host.status_label()),
            Style::new().fg(status_color(&host.state)),
        ),
        Span::styled(format!("{duration:>6}  "), Style::new().fg(MUTED)),
    ];
    if let HostState::Errored(message) = &host.state {
        spans.push(Span::styled(truncate(message, 60), Style::new().fg(WARN)));
    } else if let Some(preview) = host.preview() {
        let style = if preview_is_error(host) {
            Style::new().fg(DANGER)
        } else {
            Style::new().fg(MUTED)
        };
        spans.push(Span::styled(truncate(preview.trim(), 72), style));
    }
    Line::from(spans)
}

fn preview_is_error(host: &HostView) -> bool {
    host.output
        .back()
        .map(|line| line.stream == Stream::Stderr)
        .unwrap_or(false)
}

fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let Some(host) = app.selected() else {
        return;
    };
    let title = Line::from(vec![
        Span::styled(
            format!(" {} ", host.name),
            Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("· {} ", host.status_label()),
            Style::new().fg(status_color(&host.state)),
        ),
        Span::styled("· Esc back · ↑↓ scroll ", Style::new().fg(MUTED)),
    ]);
    let block = rounded(ACCENT).title(title);
    let inner_height = area.height.saturating_sub(2) as usize;
    let total = host.output.len();
    let max_scroll = total.saturating_sub(inner_height);
    let scroll = app.detail_scroll.min(max_scroll);
    let lines: Vec<Line> = host
        .output
        .iter()
        .skip(scroll)
        .take(inner_height)
        .map(|line| match line.stream {
            Stream::Stdout => Line::from(Span::styled(line.text.clone(), Style::new().fg(TEXT))),
            Stream::Stderr => Line::from(Span::styled(line.text.clone(), Style::new().fg(DANGER))),
        })
        .collect();

    let body = if lines.is_empty() {
        vec![Line::from(Span::styled(
            "waiting for output…",
            Style::new().fg(MUTED),
        ))]
    } else {
        lines
    };
    frame.render_widget(
        Paragraph::new(body).block(block).wrap(Wrap { trim: false }),
        area,
    );
}

/// zsh-style reverse search line: `(reverse-i-search)'query': match`.
fn draw_search(frame: &mut Frame, app: &App, area: Rect, search: &SearchState) {
    let block = rounded(WARN).title(Line::from(Span::styled(
        " history search ",
        Style::new().fg(MUTED),
    )));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let query: String = search.query.iter().collect();
    let mut spans = vec![
        Span::styled("(reverse-i-search)", Style::new().fg(MUTED)),
        Span::styled(
            format!("'{query}'"),
            Style::new().fg(WARN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(": ", Style::new().fg(MUTED)),
    ];
    match app.search_match_text() {
        Some(text) => spans.push(Span::styled(text, Style::new().fg(TEXT))),
        None => spans.push(Span::styled("no match", Style::new().fg(DANGER))),
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);

    let caret = ("(reverse-i-search)'".chars().count() + search.cursor)
        .min(inner.width.saturating_sub(1) as usize);
    frame.set_cursor_position(Position::new(inner.x + caret as u16, inner.y));
}

fn draw_prompt(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(search) = app.search_state() {
        draw_search(frame, app, area, search);
        return;
    }
    let focused = matches!(app.focus, Focus::Prompt);
    let border = if focused { ACCENT } else { MUTED };
    let block = rounded(border).title(Line::from(Span::styled(
        " command ",
        Style::new().fg(MUTED),
    )));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let available = inner.width.saturating_sub(2) as usize;
    let text = app.input_string();
    let (visible, cursor_x) = window(&text, app.cursor, available);
    let arrow_style = if app.running {
        Style::new().fg(MUTED)
    } else {
        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
    };
    let line = Line::from(vec![
        Span::styled("❯ ", arrow_style),
        Span::styled(visible, Style::new().fg(TEXT)),
    ]);
    frame.render_widget(Paragraph::new(line), inner);
    if focused && !app.running {
        frame.set_cursor_position(Position::new(inner.x + 2 + cursor_x as u16, inner.y));
    }
}

fn draw_hints(frame: &mut Frame, app: &App, area: Rect) {
    // The counter needs ~22 columns; the keys get the rest.
    let [left, right] =
        Layout::horizontal([Constraint::Min(20), Constraint::Length(24)]).areas(area);
    let keys = match &app.focus {
        Focus::Search(_) => " type to filter · ^R/↑ older · ↓ newer · Enter use · Esc cancel ",
        Focus::Prompt if app.running => " ^C cancel · ↑↓ history · Tab group · Enter view ",
        Focus::Prompt => " Enter run · ↑↓ history · ^R search · Tab group · q quit ",
        Focus::Detail => " ↑↓ scroll · Esc back · ^C cancel · q quit ",
    };
    let counter = format!(
        " host {}/{} · {:.1}s ",
        app.selection + 1,
        app.hosts.len().max(1),
        app.elapsed_ms() as f64 / 1000.0
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(keys, Style::new().fg(MUTED)))),
        left,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(counter, Style::new().fg(MUTED))))
            .alignment(ratatui::layout::Alignment::Right),
        right,
    );
}

fn glyph_for(state: &HostState, app: &App) -> (String, Style) {
    match state {
        HostState::Pending => ("·".to_string(), Style::new().fg(MUTED)),
        HostState::Running => (app.spinner().to_string(), Style::new().fg(ACCENT)),
        HostState::Succeeded => ("✔".to_string(), Style::new().fg(SUCCESS)),
        HostState::Failed(_) => (
            "✖".to_string(),
            Style::new().fg(DANGER).add_modifier(Modifier::BOLD),
        ),
        HostState::Errored(_) => (
            "!".to_string(),
            Style::new().fg(WARN).add_modifier(Modifier::BOLD),
        ),
        HostState::TimedOut(_) => (
            "⏳".to_string(),
            Style::new().fg(DANGER).add_modifier(Modifier::BOLD),
        ),
        HostState::Cancelled => ("×".to_string(), Style::new().fg(MUTED)),
    }
}

fn status_color(state: &HostState) -> Color {
    match state {
        HostState::Pending | HostState::Cancelled => MUTED,
        HostState::Running => ACCENT,
        HostState::Succeeded => SUCCESS,
        HostState::Failed(_) | HostState::TimedOut(_) => DANGER,
        HostState::Errored(_) => WARN,
    }
}

pub(crate) fn truncate(text: &str, width: usize) -> String {
    let mut out: String = text.chars().take(width).collect();
    if text.chars().count() > width {
        out.pop();
        out.push('…');
    }
    out
}

/// Keep the cursor visible: returns the visible slice and the cursor column.
fn window(text: &str, cursor: usize, width: usize) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    if width == 0 {
        return (String::new(), 0);
    }
    let start = cursor.saturating_sub(width);
    let visible: String = chars.iter().skip(start).take(width).collect();
    (visible, cursor - start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, Target};
    use crate::exec::RunEvent;
    use crate::ssh::SshSettings;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(96, 18)).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draws");
        let buffer = terminal.backend().buffer().clone();
        buffer.content().iter().map(|cell| cell.symbol()).collect()
    }

    fn app_with_progress() -> App {
        let targets = vec![
            Target {
                label: "pve".to_string(),
                hosts: vec!["pve00.jacd.local".into(), "pve01.jacd.local".into()],
            },
            Target {
                label: "prox".to_string(),
                hosts: vec!["prox00.jacd.local".into()],
            },
        ];
        let mut app = App::new(targets, 0, SshSettings::default(), 4);
        app.apply(RunEvent::Started { idx: 0 });
        app.apply(RunEvent::Output {
            idx: 0,
            stream: Stream::Stdout,
            line: "Reading package lists…".into(),
        });
        app.apply(RunEvent::Finished {
            idx: 0,
            state: HostState::Succeeded,
            elapsed_ms: 1400,
        });
        app.apply(RunEvent::Started { idx: 1 });
        app.apply(RunEvent::Output {
            idx: 1,
            stream: Stream::Stderr,
            line: "E: Unable to lock".into(),
        });
        app.apply(RunEvent::Finished {
            idx: 1,
            state: HostState::Failed(Some(100)),
            elapsed_ms: 900,
        });
        app
    }

    #[test]
    fn renders_targets_hosts_and_progress() {
        let app = app_with_progress();
        let screen = rendered(&app);
        for expected in [
            "dse",
            "pve",
            "pve00.jacd.local",
            "pve01.jacd.local",
            "2/2",
            "1 ok",
            "1 failed",
            "exit 100",
            "E: Unable to lock",
        ] {
            assert!(
                screen.contains(expected),
                "missing {expected:?} in:\n{screen}"
            );
        }
    }

    #[test]
    fn renders_prompt_and_keys() {
        let mut app = app_with_progress();
        app.input = "apt update".chars().collect();
        app.cursor = app.input.len();
        let screen = rendered(&app);
        assert!(screen.contains("command"), "{screen}");
        assert!(screen.contains("apt update"), "{screen}");
        assert!(screen.contains("Enter run"), "{screen}");
    }

    #[test]
    fn search_line_shows_the_query_and_the_match() {
        let mut app = app_with_progress();
        app.history = vec!["apt update".into(), "apt install nginx".into()];
        app.begin_search();
        for ch in "inst".chars() {
            app.search_insert(ch);
        }
        let screen = rendered(&app);
        assert!(screen.contains("reverse-i-search"), "{screen}");
        assert!(screen.contains("'inst'"), "{screen}");
        assert!(screen.contains("apt install nginx"), "{screen}");
        assert!(screen.contains("Esc cancel"), "{screen}");
    }

    #[test]
    fn detail_view_shows_per_host_output() {
        let mut app = app_with_progress();
        app.open_detail();
        let screen = rendered(&app);
        assert!(screen.contains("Reading package lists…"), "{screen}");
        assert!(screen.contains("Esc back"), "{screen}");
    }

    /// Dev aid: dump the rendered screen with
    /// `cargo test -- --ignored --nocapture print_preview`.
    #[test]
    #[ignore]
    fn print_preview() {
        let mut app = app_with_progress();
        app.input = "apt update".chars().collect();
        app.cursor = app.input.len();
        let screen: Vec<char> = rendered(&app).chars().collect();
        for chunk in screen.chunks(96) {
            println!("{}", chunk.iter().collect::<String>());
        }

        println!("\n--- reverse search ---");
        app.history = vec!["apt update".into(), "apt install nginx".into()];
        app.begin_search();
        for ch in "inst".chars() {
            app.search_insert(ch);
        }
        let screen: Vec<char> = rendered(&app).chars().collect();
        for chunk in screen.chunks(96) {
            println!("{}", chunk.iter().collect::<String>());
        }
    }

    #[test]
    fn window_keeps_cursor_visible() {
        let (visible, cursor) = window("0123456789", 10, 4);
        assert_eq!(visible, "6789");
        assert_eq!(cursor, 4);
    }
}
