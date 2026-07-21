//! A minimal terminal UI for the coding agent (ratatui + crossterm).
//!
//! It is deliberately simple: a scrolling transcript, an input line, and a
//! synchronous turn — when you submit a message the agent runs to completion,
//! prompting inline (`[y/N]`) before each mutating tool. Live token streaming is
//! intentionally left out to keep the example readable; the runtime supports it
//! through `RuntimeRunContext::stream_sink`.

use std::io::{self, Stdout};

use awaken_agent_contract::agent::message::{Message, Role};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{ExecutableCommand, cursor};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::{Approval, CodingSession};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Run the TUI loop until the user quits. `runtime` is the Tokio runtime used to
/// drive each (async) turn synchronously.
pub fn run(
    runtime: tokio::runtime::Runtime,
    session: CodingSession,
    model: &str,
) -> io::Result<()> {
    let mut terminal = setup()?;
    let mut input = String::new();
    let mut log: Vec<Line> = vec![Line::from(Span::styled(
        format!("coding agent ready — model: {model}. Type a request, Enter to send, Esc to quit."),
        Style::default().fg(Color::DarkGray),
    ))];

    loop {
        draw(&mut terminal, &log, &input, model)?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc => break,
            KeyCode::Char(c) => input.push(c),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Enter if !input.trim().is_empty() => {
                let prompt = std::mem::take(&mut input);
                log.push(line(Role::User, &prompt));
                draw(&mut terminal, &log, "", model)?;

                // Run the turn, prompting inline before each mutating tool.
                let result = runtime.block_on(session.turn(&prompt, |ticket| {
                    let tool = ticket.call_id.clone().unwrap_or_default();
                    approve_prompt(&mut terminal, &log, model, &tool).unwrap_or(Approval::Deny)
                }));
                match result {
                    Ok(messages) => log.extend(messages.iter().filter_map(render)),
                    Err(err) => log.push(line(Role::Tool, &format!("error: {err}"))),
                }
            }
            _ => {}
        }
    }

    teardown(&mut terminal)
}

/// Draw a one-shot approval prompt and block for a y/n keypress.
fn approve_prompt(
    terminal: &mut Term,
    log: &[Line],
    model: &str,
    tool: &str,
) -> io::Result<Approval> {
    let mut shown = log.to_vec();
    shown.push(Line::from(Span::styled(
        format!("allow tool `{tool}`? [y/N]"),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    draw(terminal, &shown, "", model)?;
    loop {
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(Approval::Allow),
                _ => return Ok(Approval::Deny),
            }
        }
    }
}

fn draw(terminal: &mut Term, log: &[Line], input: &str, model: &str) -> io::Result<()> {
    terminal.draw(|frame| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(frame.area());

        // Keep the latest lines visible.
        let height = chunks[0].height.saturating_sub(2) as usize;
        let start = log.len().saturating_sub(height);
        let transcript = Paragraph::new(log[start..].to_vec())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" coding agent · {model} ")),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(transcript, chunks[0]);

        let prompt = Paragraph::new(format!("› {input}"))
            .block(Block::default().borders(Borders::ALL).title(" message "));
        frame.render_widget(prompt, chunks[1]);
    })?;
    Ok(())
}

/// Render a committed message as a colored line, skipping empty assistant turns.
fn render(message: &Message) -> Option<Line<'static>> {
    let text = message.text_content();
    if text.trim().is_empty() {
        return None;
    }
    Some(line(message.role, &text))
}

fn line(role: Role, text: &str) -> Line<'static> {
    let (tag, color) = match role {
        Role::User => ("you", Color::Cyan),
        Role::Assistant => ("agent", Color::Green),
        Role::Tool => ("tool", Color::Magenta),
        Role::System => ("system", Color::DarkGray),
    };
    Line::from(vec![
        Span::styled(
            format!("{tag}: "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(text.replace('\n', " ⏎ ").to_string()),
    ])
}

fn setup() -> io::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(cursor::Hide)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn teardown(terminal: &mut Term) -> io::Result<()> {
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.backend_mut().execute(cursor::Show)?;
    terminal.show_cursor()
}
