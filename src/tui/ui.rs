use super::App;
use crate::session::Role;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

const SIDEBAR_WIDTH: u16 = 26;

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let normalized = text.replace('\t', "    ");
    if normalized.is_empty() {
        return vec![String::new()];
    }
    textwrap::wrap(&normalized, width)
        .into_iter()
        .map(|line| line.into_owned())
        .collect()
}

fn truncate(text: &str, max: usize) -> String {
    let single = text.lines().next().unwrap_or("").trim();
    if single.chars().count() <= max {
        return single.to_string();
    }
    let kept: String = single.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn args_preview(args: &str, max: usize) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(args) {
        for key in ["command", "path", "pattern"] {
            if let Some(text) = value.get(key).and_then(|v| v.as_str()) {
                return truncate(text, max);
            }
        }
    }
    truncate(args, max)
}

fn status_style(status: &str) -> Style {
    match status {
        "ok" => Style::default().fg(Color::DarkGray),
        "denied" => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::Red),
    }
}

fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let width = (area.width * percent_x / 100).max(10).min(area.width);
    let height = (area.height * percent_y / 100).max(5).min(area.height);
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    )
}

fn chat_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    for message in &app.session.messages {
        match message.role {
            Role::User => {
                lines.push(Line::from(Span::styled(
                    "❯ you",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )));
                for line in wrap(&message.content, width.saturating_sub(2)) {
                    lines.push(Line::from(Span::styled(
                        format!("  {line}"),
                        Style::default().fg(Color::Cyan),
                    )));
                }
                lines.push(Line::from(""));
            }
            Role::Assistant => {
                if !message.content.is_empty() {
                    for line in wrap(&message.content, width) {
                        lines.push(Line::from(line));
                    }
                }
                for call in &message.tool_calls {
                    let status = app.tool_status.get(&call.id).copied();
                    let mut spans = vec![
                        Span::styled("⏺ ", Style::default().fg(Color::Yellow)),
                        Span::styled(
                            call.name.clone(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(" — {}", args_preview(&call.arguments, width.saturating_sub(14))),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ];
                    if let Some(status) = status {
                        spans.push(Span::styled(format!(" · {status}"), status_style(status)));
                    }
                    lines.push(Line::from(spans));
                }
                lines.push(Line::from(""));
            }
            Role::Tool => {
                let status = message
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| app.tool_status.get(id).copied())
                    .unwrap_or("ok");
                let icon = match status {
                    "ok" => "↳",
                    "denied" => "⊘",
                    _ => "✗",
                };
                lines.push(Line::from(Span::styled(
                    format!("  {icon} {status}"),
                    status_style(status),
                )));
                let content: Vec<&str> = message.content.lines().collect();
                for line in content.iter().take(8) {
                    for wrapped in wrap(line, width.saturating_sub(4)) {
                        lines.push(Line::from(Span::styled(
                            format!("    {wrapped}"),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
                if content.len() > 8 {
                    lines.push(Line::from(Span::styled(
                        format!("    … +{} lines", content.len() - 8),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                lines.push(Line::from(""));
            }
            Role::System => {}
        }
    }

    if !app.stream_text.is_empty() {
        for line in wrap(&app.stream_text, width) {
            lines.push(Line::from(line));
        }
    }

    if app.busy {
        match &app.active_tool {
            Some((name, detail)) => {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} ", app.spinner_char()),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(
                        name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" — {}", truncate(detail, width.saturating_sub(14))),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            None => {
                lines.push(Line::from(Span::styled(
                    format!("{} thinking…", app.spinner_char()),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }

    if let Some(error) = &app.error {
        lines.push(Line::from(Span::styled(
            format!("error: {error}"),
            Style::default().fg(Color::Red),
        )));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "ask something — F1 for help, ^K for sessions, ^N for a new session",
            Style::default().fg(Color::DarkGray),
        )));
    }

    lines
}

fn draw_chat(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            format!(" chat · {} ", app.session.title),
            Style::default().fg(Color::DarkGray),
        ));
    let inner_width = area.width.saturating_sub(2) as usize;
    let lines = chat_lines(app, inner_width);
    let inner_height = area.height.saturating_sub(2);
    app.chat_inner_height = inner_height.max(1);

    let total = lines.len() as u16;
    let max_scroll = total.saturating_sub(inner_height);
    let offset = max_scroll.saturating_sub(app.scroll_from_bottom);

    let paragraph = Paragraph::new(lines).block(block).scroll((offset, 0));
    frame.render_widget(paragraph, area);
}

fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " sessions ",
            Style::default().fg(Color::DarkGray),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let max_items = inner.height.saturating_sub(3) as usize;
    let selected = app
        .metas
        .iter()
        .position(|meta| meta.id == app.session.id);
    let first = match selected {
        Some(index) if index >= max_items && max_items > 0 => index + 1 - max_items,
        _ => 0,
    };

    for meta in app.metas.iter().skip(first).take(max_items) {
        let active = meta.id == app.session.id;
        let title = truncate(&meta.title, inner.width.saturating_sub(4) as usize);
        let line = if active {
            Line::from(Span::styled(
                format!("> {title}"),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ))
        } else {
            Line::from(Span::styled(
                format!("  {title}"),
                Style::default().fg(Color::DarkGray),
            ))
        };
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("  (empty)", Style::default().fg(Color::DarkGray))));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "────────────",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        " ^N new   ^K open",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

fn input_height(app: &App) -> u16 {
    let count = app.editor.lines.len().clamp(1, 6) as u16;
    count + 2
}

fn draw_input(frame: &mut Frame, app: &mut App, area: Rect) {
    let busy_style = if app.busy {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Cyan)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(busy_style)
        .title(Span::styled(
            " input · Enter send  ^J newline ",
            Style::default().fg(Color::DarkGray),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible = inner.height as usize;
    if visible == 0 {
        return;
    }
    let available = inner.width.saturating_sub(2) as usize;
    if app.editor.col < app.editor.hscroll {
        app.editor.hscroll = app.editor.col;
    } else if app.editor.col >= app.editor.hscroll + available {
        app.editor.hscroll = app.editor.col + 1 - available;
    }

    let first = if app.editor.row >= visible {
        app.editor.row + 1 - visible
    } else {
        0
    };

    for offset in 0..visible {
        let index = first + offset;
        if index >= app.editor.lines.len() {
            break;
        }
        let text: String = app.editor.lines[index].iter().collect();
        let shifted: String = text.chars().skip(app.editor.hscroll).collect();
        let prefix: &'static str = if offset == 0 && first == 0 { "> " } else { "  " };
        let line = Line::from(vec![
            Span::styled(prefix, Style::default().fg(Color::DarkGray)),
            Span::raw(shifted),
        ]);
        frame.render_widget(Paragraph::new(line), {
            Rect::new(inner.x, inner.y + offset as u16, inner.width, 1)
        });
    }

    let cursor_x = inner.x + 2 + (app.editor.col.saturating_sub(app.editor.hscroll)) as u16;
    let cursor_y = inner.y + app.editor.row.saturating_sub(first) as u16;
    if cursor_x < inner.x + inner.width && cursor_y < inner.y + inner.height {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let right = format!("{} · {}", app.config.provider.label(), app.config.model);
    let right_width = (right.chars().count() as u16).saturating_add(2);
    let parts = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(right_width.min(area.width)),
    ])
    .split(area);

    let left_text = if app.busy {
        match &app.active_tool {
            Some((name, _)) => format!(
                " {} {} · {}s",
                app.spinner_char(),
                name,
                app.started.elapsed().as_secs()
            ),
            None => format!(
                " {} thinking… · {}s",
                app.spinner_char(),
                app.started.elapsed().as_secs()
            ),
        }
    } else if let Some(notice) = &app.notice {
        format!(" {notice}")
    } else if app.error.is_some() {
        " error · see chat".to_string()
    } else {
        " ready · F1 help".to_string()
    };

    let style = if app.busy {
        Style::default().fg(Color::Yellow)
    } else if app.error.is_some() {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    frame.render_widget(Paragraph::new(Line::from(Span::styled(left_text, style))), parts[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(right, Style::default().fg(Color::DarkGray))))
            .alignment(ratatui::layout::Alignment::Right),
        parts[1],
    );
}

fn draw_permission(frame: &mut Frame, app: &App, area: Rect) {
    let Some(pending) = &app.pending else { return };
    let popup = centered(area, 72, 40);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " permission ",
            Style::default().fg(Color::Yellow),
        ));
    let lines = vec![
        Line::from("The agent wants to run:"),
        Line::from(""),
        Line::from(Span::styled(
            format!("{} — {}", pending.tool, pending.detail),
            Style::default().fg(Color::Cyan),
        )),
        Line::from(""),
        Line::from("y  allow once"),
        Line::from("a  allow always (this session)"),
        Line::from("n  deny"),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(ratatui::widgets::Wrap { trim: false }),
        popup,
    );
}

fn draw_picker(frame: &mut Frame, app: &App, area: Rect) {
    let Some(picker) = &app.picker else { return };
    let popup = centered(area, 64, 60);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " sessions ",
            Style::default().fg(Color::Cyan),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let visible = inner.height as usize;
    let first = if picker.selected >= visible && visible > 0 {
        picker.selected + 1 - visible
    } else {
        0
    };
    for (index, meta) in picker.items.iter().skip(first).take(visible).enumerate() {
        let real = first + index;
        let title = truncate(&meta.title, inner.width.saturating_sub(8) as usize);
        let stamp = format_ts(meta.updated_at);
        let label = format!("{}  ({} · {})", title, meta.count, stamp);
        if real == picker.selected {
            lines.push(Line::from(Span::styled(
                format!("> {label}"),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                format!("  {label}"),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no saved sessions",
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn format_ts(ts: u64) -> String {
    let days = (ts / 86_400) as i64;
    let secs = ts % 86_400;
    let (y, m, d) = crate::agent::civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = centered(area, 72, 78);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(" help ", Style::default().fg(Color::Cyan)));
    let lines = vec![
        Line::from(Span::styled("keys", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
        Line::from("  Enter          send message"),
        Line::from("  ^J / ^Enter    newline"),
        Line::from("  ^N             new session"),
        Line::from("  ^K             session picker"),
        Line::from("  ^C             clear input (again: quit)"),
        Line::from("  PageUp/Down    scroll chat (wheel works too)"),
        Line::from("  F1             this help"),
        Line::from(""),
        Line::from(Span::styled("permissions", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
        Line::from("  while a prompt is open: y allow · a always · n deny"),
        Line::from(""),
        Line::from(Span::styled("commands", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
        Line::from("  /help          this help"),
        Line::from("  /new           start a new session"),
        Line::from("  /clear         wipe the current conversation"),
        Line::from("  /sessions      open the session picker"),
        Line::from("  /model <id>    switch model (e.g. /model gpt-4o)"),
        Line::from("  /provider <p>  openai | anthropic | ollama | custom"),
        Line::from("  /permission    show or set bash/read/write/edit mode"),
        Line::from("  /yolo          allow every tool without asking"),
        Line::from("  /ask           restore the default ask-before-write policy"),
        Line::from("  /config        show the active configuration"),
        Line::from("  /quit          exit"),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(ratatui::widgets::Wrap { trim: false }),
        popup,
    );
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let sidebar_width = if area.width >= 72 { SIDEBAR_WIDTH } else { 0 };
    let columns = Layout::horizontal([
        Constraint::Length(sidebar_width),
        Constraint::Min(20),
    ])
    .split(area);

    if sidebar_width > 0 {
        draw_sidebar(frame, app, columns[0]);
    }

    let input_h = input_height(app);
    let rows = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(input_h),
        Constraint::Length(1),
    ])
    .split(columns[1]);

    draw_chat(frame, app, rows[0]);
    draw_input(frame, app, rows[1]);
    draw_status(frame, app, rows[2]);

    if app.pending.is_some() {
        draw_permission(frame, app, area);
    }
    if app.picker.is_some() {
        draw_picker(frame, app, area);
    }
    if app.show_help {
        draw_help(frame, area);
    }
}
