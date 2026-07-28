//! Smart selection matching for click-expanded terminal ranges.

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use regex::Regex;
use scribe_common::config::{
    SmartSelectionAction, SmartSelectionActionKind, SmartSelectionConfig,
    SmartSelectionParameterMode, SmartSelectionPrecision,
};

use crate::selection::{SelectionPoint, read_cell_char, read_cell_flags};

#[derive(Debug, Clone)]
pub struct SmartSelectionCandidate {
    pub range_start: SelectionPoint,
    pub range_end: SelectionPoint,
    pub text: String,
    pub captures: Vec<Option<String>>,
    pub rule_name: String,
    pub precision: SmartSelectionPrecision,
    pub actions: Vec<SmartSelectionAction>,
}

impl SmartSelectionCandidate {
    #[must_use]
    pub fn resolved_actions(
        &self,
        context: &ActionExpansionContext<'_>,
    ) -> Vec<ResolvedSmartSelectionAction> {
        self.actions
            .iter()
            .map(|action| {
                let parameter = expand_action_parameter(action, self, context);
                ResolvedSmartSelectionAction {
                    label: format!("{}: {}", self.rule_name, action_kind_label(action.kind)),
                    kind: action.kind,
                    parameter,
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ActionExpansionContext<'a> {
    pub cwd: Option<&'a str>,
    pub user: &'a str,
    pub host: &'a str,
}

#[derive(Debug, Clone)]
pub struct ResolvedSmartSelectionAction {
    pub label: String,
    pub kind: SmartSelectionActionKind,
    pub parameter: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleValidationError {
    pub rule_id: String,
    pub rule_name: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct CompiledSmartSelection {
    rules: Vec<CompiledRule>,
    pub errors: Vec<RuleValidationError>,
}

impl CompiledSmartSelection {
    #[must_use]
    pub fn compile(config: &SmartSelectionConfig) -> Self {
        let mut rules = Vec::new();
        let mut errors = Vec::new();

        for rule in &config.rules {
            if !rule.enabled {
                continue;
            }

            if rule.regex.is_empty() {
                errors.push(RuleValidationError {
                    rule_id: rule.id.clone(),
                    rule_name: rule.name.clone(),
                    message: String::from("Regular expression cannot be empty."),
                });
                continue;
            }

            match Regex::new(&rule.regex) {
                Ok(regex) => rules.push(CompiledRule {
                    regex,
                    name: rule.name.clone(),
                    precision: rule.precision,
                    actions: rule.actions.clone(),
                }),
                Err(error) => errors.push(RuleValidationError {
                    rule_id: rule.id.clone(),
                    rule_name: rule.name.clone(),
                    message: error.to_string(),
                }),
            }
        }

        Self { rules, errors }
    }

    #[must_use]
    pub fn candidate_at(
        &self,
        term: &Term<VoidListener>,
        point: SelectionPoint,
    ) -> Option<SmartSelectionCandidate> {
        let logical_line = LogicalLine::collect(term, point)?;
        let cursor_index = logical_line.cursor_char_index(point)?;

        let mut selected: Option<SmartSelectionCandidate> = None;
        for rule in &self.rules {
            let Some(candidate) = rule.best_candidate(&logical_line, cursor_index) else {
                continue;
            };

            if selected.as_ref().is_none_or(|current| candidate_beats(&candidate, current)) {
                selected = Some(candidate);
            }
        }

        selected
    }

    #[must_use]
    pub fn action_candidates_at(
        &self,
        term: &Term<VoidListener>,
        point: SelectionPoint,
    ) -> Vec<SmartSelectionCandidate> {
        let Some(logical_line) = LogicalLine::collect(term, point) else {
            return Vec::new();
        };
        let Some(cursor_index) = logical_line.cursor_char_index(point) else {
            return Vec::new();
        };

        self.rules
            .iter()
            .filter_map(|rule| rule.best_candidate(&logical_line, cursor_index))
            .filter(|candidate| !candidate.actions.is_empty())
            .collect()
    }
}

#[derive(Debug, Clone)]
struct CompiledRule {
    regex: Regex,
    name: String,
    precision: SmartSelectionPrecision,
    actions: Vec<SmartSelectionAction>,
}

impl CompiledRule {
    fn best_candidate(
        &self,
        logical_line: &LogicalLine,
        cursor_index: usize,
    ) -> Option<SmartSelectionCandidate> {
        let mut best: Option<SmartSelectionCandidate> = None;

        for captures in self.regex.captures_iter(&logical_line.text) {
            let Some(matched) = captures.get(0) else {
                continue;
            };

            let Some(start_index) = logical_line.char_index_for_byte(matched.start()) else {
                continue;
            };
            let Some(end_index) = logical_line.char_index_for_byte(matched.end()) else {
                continue;
            };

            if start_index == end_index || cursor_index < start_index || cursor_index >= end_index {
                continue;
            }

            let Some(range_start) = logical_line.cells.get(start_index).copied() else {
                continue;
            };
            let Some(range_end) = logical_line.cells.get(end_index.saturating_sub(1)).copied()
            else {
                continue;
            };

            let capture_values = captures
                .iter()
                .map(|capture| capture.map(|value| value.as_str().to_owned()))
                .collect();

            let candidate = SmartSelectionCandidate {
                range_start,
                range_end,
                text: matched.as_str().to_owned(),
                captures: capture_values,
                rule_name: self.name.clone(),
                precision: self.precision,
                actions: self.actions.clone(),
            };

            if best.as_ref().is_none_or(|current| candidate_beats(&candidate, current)) {
                best = Some(candidate);
            }
        }

        best
    }
}

fn candidate_beats(candidate: &SmartSelectionCandidate, current: &SmartSelectionCandidate) -> bool {
    candidate.precision.rank() > current.precision.rank()
        || (candidate.precision == current.precision
            && candidate.text.chars().count() > current.text.chars().count())
}

fn expand_action_parameter(
    action: &SmartSelectionAction,
    candidate: &SmartSelectionCandidate,
    context: &ActionExpansionContext<'_>,
) -> String {
    let template = if action.parameter.is_empty() { r"\0" } else { &action.parameter };
    match action.parameter_mode {
        SmartSelectionParameterMode::Legacy => {
            expand_legacy_parameter(template, candidate, context)
        }
        SmartSelectionParameterMode::Interpolated => {
            expand_interpolated_parameter(template, candidate, context)
        }
    }
}

fn expand_legacy_parameter(
    template: &str,
    candidate: &SmartSelectionCandidate,
    context: &ActionExpansionContext<'_>,
) -> String {
    let mut out = String::new();
    let mut chars = template.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }

        let Some(escaped) = chars.next() else {
            out.push('\\');
            break;
        };

        match escaped {
            '0'..='9' => {
                if let Some(index) = escaped.to_digit(10).and_then(|d| usize::try_from(d).ok()) {
                    append_capture(&mut out, candidate, index);
                }
            }
            'd' => {
                if let Some(cwd) = context.cwd {
                    out.push_str(cwd);
                }
            }
            'u' => out.push_str(context.user),
            'h' => out.push_str(context.host),
            'n' => out.push('\n'),
            '\\' => out.push('\\'),
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

fn append_capture(out: &mut String, candidate: &SmartSelectionCandidate, index: usize) {
    let Some(Some(value)) = candidate.captures.get(index) else {
        return;
    };
    out.push_str(value);
}

fn expand_interpolated_parameter(
    template: &str,
    candidate: &SmartSelectionCandidate,
    context: &ActionExpansionContext<'_>,
) -> String {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' || chars.peek().copied() != Some('(') {
            out.push(ch);
            continue;
        }
        if chars.next().is_none() {
            out.push(ch);
            continue;
        }

        let mut expression = String::new();
        let mut closed = false;
        for expr_ch in chars.by_ref() {
            if expr_ch == ')' {
                closed = true;
                break;
            }
            expression.push(expr_ch);
        }

        if closed {
            if let Some(value) = resolve_interpolated_expression(&expression, candidate, context) {
                out.push_str(&value);
            } else {
                out.push_str(r"\(");
                out.push_str(&expression);
                out.push(')');
            }
        } else {
            out.push_str(r"\(");
            out.push_str(&expression);
        }
    }
    out
}

fn resolve_interpolated_expression(
    expression: &str,
    candidate: &SmartSelectionCandidate,
    context: &ActionExpansionContext<'_>,
) -> Option<String> {
    match expression {
        "path" => context.cwd.map(str::to_owned),
        "user" => Some(context.user.to_owned()),
        "host" => Some(context.host.to_owned()),
        _ => match_index_expression(expression).and_then(|index| {
            candidate.captures.get(index).and_then(|value| value.as_ref()).cloned()
        }),
    }
}

fn match_index_expression(expression: &str) -> Option<usize> {
    let inner = expression.strip_prefix("matches[")?.strip_suffix(']')?;
    inner.parse::<usize>().ok()
}

fn action_kind_label(kind: SmartSelectionActionKind) -> &'static str {
    match kind {
        SmartSelectionActionKind::OpenFile => "Open File",
        SmartSelectionActionKind::OpenUrl => "Open URL",
        SmartSelectionActionKind::RunCommand => "Run Command",
        SmartSelectionActionKind::RunCoprocess => "Run Coprocess",
        SmartSelectionActionKind::SendText => "Send Text",
        SmartSelectionActionKind::RunCommandInWindow => "Run Command in Window",
        SmartSelectionActionKind::Copy => "Copy",
    }
}

#[derive(Debug, Clone)]
struct LogicalLine {
    text: String,
    cells: Vec<SelectionPoint>,
    byte_offsets: Vec<usize>,
}

impl LogicalLine {
    fn collect(term: &Term<VoidListener>, point: SelectionPoint) -> Option<Self> {
        let topmost = term.grid().topmost_line().0;
        let bottommost = term.grid().bottommost_line().0;
        if point.row < topmost || point.row > bottommost {
            return None;
        }

        let cols = term.grid().columns();
        if cols == 0 {
            return None;
        }

        let last_col = Column(cols.saturating_sub(1));
        let mut first = point.row;
        while first > topmost {
            let above = first - 1;
            if read_cell_flags(term, Line(above), last_col).contains(Flags::WRAPLINE) {
                first = above;
            } else {
                break;
            }
        }

        let mut last = point.row;
        while last < bottommost {
            if read_cell_flags(term, Line(last), last_col).contains(Flags::WRAPLINE) {
                last += 1;
            } else {
                break;
            }
        }

        let mut text = String::new();
        let mut cells = Vec::new();
        let mut byte_offsets = Vec::new();
        let mut row = first;
        while row <= last {
            let mut col = 0usize;
            while col < cols {
                byte_offsets.push(text.len());
                text.push(read_cell_char(term, Line(row), Column(col)));
                cells.push(SelectionPoint { row, col });
                col = col.saturating_add(1);
            }
            row = row.saturating_add(1);
        }
        byte_offsets.push(text.len());

        Some(Self { text, cells, byte_offsets })
    }

    fn cursor_char_index(&self, point: SelectionPoint) -> Option<usize> {
        self.cells.iter().position(|cell| cell.row == point.row && cell.col == point.col)
    }

    fn char_index_for_byte(&self, byte_offset: usize) -> Option<usize> {
        self.byte_offsets.binary_search(&byte_offset).ok()
    }
}
