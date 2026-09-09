use super::{
    todo_card_line, todo_failure_color, todo_label_color, todo_meta_color, todo_score_color,
    todo_warning_color, wrap_todo_detail,
};
use ratatui::{
    style::Style,
    text::{Line, Span},
};

/// Plan-level assessment lines shown once above the todo groups.
pub(super) fn push_todo_plan_details(
    lines: &mut Vec<Line<'static>>,
    plan: &crate::todo::TodoPlan,
    base_indent: &str,
    inner_width: usize,
    compact_details: bool,
) {
    let intention = plan
        .user_intention
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(state) = plan.understands_user_intent {
        let state_color = match state {
            crate::todo::IntentUnderstanding::Uncertain => todo_failure_color(),
            crate::todo::IntentUnderstanding::Partial => todo_warning_color(),
            crate::todo::IntentUnderstanding::Clear
            | crate::todo::IntentUnderstanding::Complete => todo_score_color(),
        };
        let mut spans = vec![
            Span::styled("Intent ", Style::default().fg(todo_label_color())),
            Span::styled(state.as_str().to_string(), Style::default().fg(state_color)),
            Span::styled(": ", Style::default().fg(todo_label_color())),
        ];
        if let Some(intention) = intention {
            if compact_details {
                spans.push(Span::styled(
                    intention.to_string(),
                    Style::default().fg(todo_meta_color()),
                ));
                lines.push(todo_card_line(spans, base_indent, inner_width));
            } else {
                push_todo_wrapped_detail_spans(lines, spans, intention, base_indent, inner_width);
            }
        } else {
            lines.push(todo_card_line(spans, base_indent, inner_width));
        }
    } else if let Some(intention) = intention {
        push_todo_detail(
            lines,
            "Intent",
            intention,
            base_indent,
            inner_width,
            compact_details,
        );
    }
}

fn push_todo_detail(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    value: &str,
    base_indent: &str,
    inner_width: usize,
    compact: bool,
) {
    if !compact {
        push_todo_wrapped_detail(lines, label, value, base_indent, inner_width);
        return;
    }

    let prefix = format!("  {} · ", label);
    lines.push(todo_card_line(
        vec![
            Span::styled(prefix, Style::default().fg(todo_label_color())),
            Span::styled(value.to_string(), Style::default().fg(todo_meta_color())),
        ],
        base_indent,
        inner_width,
    ));
}

/// Wrap one labeled detail line to the card width.
fn push_todo_wrapped_detail(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    value: &str,
    base_indent: &str,
    inner_width: usize,
) {
    push_todo_wrapped_detail_spans(
        lines,
        vec![Span::styled(
            format!("  {} · ", label),
            Style::default().fg(todo_label_color()),
        )],
        value,
        base_indent,
        inner_width,
    );
}

fn push_todo_wrapped_detail_spans(
    lines: &mut Vec<Line<'static>>,
    prefix: Vec<Span<'static>>,
    value: &str,
    base_indent: &str,
    inner_width: usize,
) {
    let prefix_width = Line::from(prefix.clone()).width();
    let available = inner_width.saturating_sub(prefix_width).max(1);
    for (index, chunk) in wrap_todo_detail(value, available).into_iter().enumerate() {
        let mut spans = if index == 0 {
            prefix.clone()
        } else {
            vec![Span::styled(
                " ".repeat(prefix_width),
                Style::default().fg(todo_label_color()),
            )]
        };
        spans.push(Span::styled(chunk, Style::default().fg(todo_meta_color())));
        lines.push(todo_card_line(spans, base_indent, inner_width));
    }
}
