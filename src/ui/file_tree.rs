use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::text::truncate_end;
use crate::app::state::{FileTreeEntryKind, FileTreeProjection, Palette};
use crate::app::AppState;

pub(crate) fn file_tree_toggle_rect(area: Rect, collapsed: bool) -> Rect {
    if area.height == 0 {
        return Rect::default();
    }
    let x = if collapsed {
        let content_width = area.width.saturating_sub(1);
        if content_width == 0 {
            return Rect::default();
        }
        area.x + 1 + content_width / 2
    } else {
        if area.width <= 1 {
            return Rect::default();
        }
        area.x + 1
    };
    Rect::new(x, area.y + area.height.saturating_sub(1), 1, 1)
}

pub(super) fn render_file_tree(app: &AppState, frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let p = &app.palette;
    let separator_style = Style::default().fg(p.surface_dim);
    let separator_x = area.x;
    let buffer = frame.buffer_mut();
    for y in area.y..area.y + area.height {
        buffer[(separator_x, y)].set_symbol("│");
        buffer[(separator_x, y)].set_style(separator_style);
    }

    if app.file_tree_collapsed {
        render_toggle(app, frame, area, true, p);
        return;
    }

    let content = Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(1),
        area.height,
    );
    if content.width == 0 {
        render_toggle(app, frame, area, false, p);
        return;
    }

    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            " files",
            Style::default().fg(p.overlay0).add_modifier(Modifier::BOLD),
        )])),
        Rect::new(content.x, content.y, content.width, 1),
    );

    if content.height < 2 {
        render_toggle(app, frame, area, false, p);
        return;
    }

    match &app.file_tree.projection {
        FileTreeProjection::Ready { root, entries } => {
            let root_label = root
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
                .unwrap_or_else(|| root.display().to_string());
            render_line(
                frame,
                Rect::new(content.x, content.y.saturating_add(1), content.width, 1),
                &format!(" root: {root_label}"),
                Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
            );

            if entries.is_empty() && content.height > 2 {
                render_line(
                    frame,
                    Rect::new(content.x, content.y.saturating_add(2), content.width, 1),
                    " empty",
                    Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
                );
            } else {
                for (index, entry) in entries.iter().enumerate() {
                    let row = content.y.saturating_add(2 + index as u16);
                    if row >= content.y + content.height.saturating_sub(1) {
                        break;
                    }
                    let marker = match entry.kind {
                        FileTreeEntryKind::Directory => "▸ ",
                        FileTreeEntryKind::File => "  ",
                    };
                    let label = format!(" {marker}{}", entry.name);
                    render_line(
                        frame,
                        Rect::new(content.x, row, content.width, 1),
                        &label,
                        Style::default().fg(p.subtext0),
                    );
                }
            }
        }
        FileTreeProjection::Unavailable { error } => {
            render_line(
                frame,
                Rect::new(content.x, content.y.saturating_add(1), content.width, 1),
                " unavailable",
                Style::default().fg(p.yellow).add_modifier(Modifier::BOLD),
            );
            if content.height > 2 {
                render_line(
                    frame,
                    Rect::new(content.x, content.y.saturating_add(2), content.width, 1),
                    &format!(" {}", error),
                    Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
                );
            }
        }
    }

    render_toggle(app, frame, area, false, p);
}

fn render_line(frame: &mut Frame, area: Rect, text: &str, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Span::styled(truncate_end(text, area.width as usize), style)),
        area,
    );
}

fn render_toggle(app: &AppState, frame: &mut Frame, area: Rect, collapsed: bool, p: &Palette) {
    let toggle = file_tree_toggle_rect(area, collapsed);
    if toggle == Rect::default() {
        return;
    }
    let icon = if collapsed { "«" } else { "»" };
    let style = if collapsed && app.global_menu_attention_badge_visible() {
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(p.overlay0)
    };
    frame.render_widget(Paragraph::new(Span::styled(icon, style)), toggle);
}
