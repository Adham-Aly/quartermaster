//! The Quartermaster's replies and questions as Markdown: `CommonMark` plus tables and
//! strikethrough, drawn only in the 16 palette roles.
//!
//! Paragraphs wrap to the width; code blocks stay unwrapped, cut with `…` at the edge, and are
//! highlighted by `syntect`'s parser through a fixed scope map (no theme, no RGB). HTML and
//! images show as their literal text.

use crate::text::{truncate, width, wrap_styled};
use crate::theme;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::OnceLock;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// A run of text in one style.
type Run = (String, Style);

/// A Markdown message body as rows at `width` cells: each row's line and whether it continues the
/// row before it after a soft wrap (for selection).
pub(crate) fn rows(text: &str, width: usize) -> Vec<(Line<'static>, bool)> {
    let mut writer = Writer::new(width.max(1));
    let parser = Parser::new_ext(text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH);
    for event in parser {
        writer.event(event);
    }
    writer.finish()
}

/// A list being drawn: the next number for an ordered one.
struct List {
    next: Option<u64>,
    /// The width of the current item's marker: the hanging indent of its rows.
    hang: usize,
}

/// A table being collected: rows of cells, the first row the header.
#[derive(Default)]
struct Table {
    rows: Vec<Vec<Vec<Run>>>,
    head_rows: usize,
}

/// Builds the rows from pulldown-cmark's events.
struct Writer {
    width: usize,
    out: Vec<(Line<'static>, bool)>,
    /// The inline runs of the block being written.
    runs: Vec<Run>,
    strong: usize,
    emphasis: usize,
    strike: usize,
    link: Option<LinkStart>,
    heading: bool,
    quote: usize,
    lists: Vec<List>,
    /// The marker (`• `, `1. `) waiting for the next row of the current item.
    marker: Option<String>,
    code: Option<(String, String)>,
    table: Option<Table>,
    image: Option<String>,
    /// A blank row goes before the next block.
    gap: bool,
}

/// Where a link's text starts in `runs`, and where it points.
struct LinkStart {
    from: usize,
    url: String,
}

impl Writer {
    fn new(width: usize) -> Self {
        Self {
            width,
            out: Vec::new(),
            runs: Vec::new(),
            strong: 0,
            emphasis: 0,
            strike: 0,
            link: None,
            heading: false,
            quote: 0,
            lists: Vec::new(),
            marker: None,
            code: None,
            table: None,
            image: None,
            gap: false,
        }
    }

    fn finish(mut self) -> Vec<(Line<'static>, bool)> {
        self.flush();
        if self.out.is_empty() {
            self.out.push((Line::default(), false));
        }
        self.out
    }

    /// The style of inline text here.
    fn style(&self) -> Style {
        let mut style = if self.quote > 0 {
            theme::dim()
        } else {
            theme::text()
        };
        if self.strong > 0 || self.heading {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.emphasis > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.link.is_some() {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        style
    }

    fn push(&mut self, text: &str, style: Style) {
        if let Some(table) = &mut self.table {
            if let Some(cell) = table.rows.last_mut().and_then(|row| row.last_mut()) {
                cell.push((text.to_owned(), style));
            }
            return;
        }
        self.runs.push((text.to_owned(), style));
    }

    /// Starts a block: a blank row first unless it is the first, or inside a list.
    fn block(&mut self) {
        self.flush();
        if self.gap && !self.out.is_empty() {
            self.out.push((Line::default(), false));
        }
        self.gap = false;
    }

    /// Ends a block: the next one gets a blank row before it, except between list items.
    fn end_block(&mut self) {
        self.flush();
        self.gap = self.lists.is_empty();
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some((_, code)) = &mut self.code {
                    code.push_str(&text);
                } else if let Some(alt) = &mut self.image {
                    alt.push_str(&text);
                } else {
                    let style = self.style();
                    self.push(&text, style);
                }
            }
            Event::Code(text) => {
                let mut style = theme::accent();
                if self.link.is_some() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                self.push(&text, style);
            }
            // HTML shows as its literal text.
            Event::Html(text)
            | Event::InlineHtml(text)
            | Event::InlineMath(text)
            | Event::DisplayMath(text)
            | Event::FootnoteReference(text) => {
                let style = self.style();
                self.push(&text, style);
            }
            Event::SoftBreak => {
                let style = self.style();
                self.push(" ", style);
            }
            Event::HardBreak => {
                let style = self.style();
                self.push("\n", style);
            }
            Event::Rule => {
                self.block();
                let rule = "─".repeat(self.width);
                self.out.push((Line::styled(rule, theme::dim()), false));
                self.gap = true;
            }
            Event::TaskListMarker(done) => {
                let style = self.style();
                self.push(if done { "[x] " } else { "[ ] " }, style);
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.lists.is_empty() {
                    self.block();
                } else {
                    self.flush();
                }
            }
            Tag::Heading { .. } => {
                self.block();
                self.heading = true;
            }
            Tag::BlockQuote(_) => {
                self.block();
                self.quote += 1;
            }
            Tag::CodeBlock(kind) => {
                if self.lists.is_empty() {
                    self.block();
                } else {
                    self.flush();
                }
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_owned()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::HtmlBlock => self.block(),
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.block();
                } else {
                    self.flush();
                }
                self.lists.push(List {
                    next: start,
                    hang: 0,
                });
            }
            Tag::Item => {
                self.flush();
                if let Some(list) = self.lists.last_mut() {
                    let marker = match &mut list.next {
                        Some(n) => {
                            let marker = format!("{n}. ");
                            *n += 1;
                            marker
                        }
                        None => String::from("• "),
                    };
                    list.hang = width(&marker);
                    self.marker = Some(marker);
                }
            }
            Tag::Emphasis => self.emphasis += 1,
            Tag::Strong => self.strong += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => {
                self.link = Some(LinkStart {
                    from: self.runs.len(),
                    url: dest_url.into_string(),
                });
            }
            Tag::Image { .. } => self.image = Some(String::new()),
            Tag::Table(_) => {
                self.block();
                self.table = Some(Table::default());
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(row) = self.table.as_mut().and_then(|t| t.rows.last_mut()) {
                    row.push(Vec::new());
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => self.end_block(),
            TagEnd::Heading(_) => {
                self.flush();
                self.heading = false;
                self.gap = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
                self.gap = self.lists.is_empty();
            }
            TagEnd::CodeBlock => {
                if let Some((lang, code)) = self.code.take() {
                    self.code_rows(&lang, &code);
                }
                self.gap = self.lists.is_empty();
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                self.gap = self.lists.is_empty();
            }
            TagEnd::Item => {
                self.flush();
                self.marker = None;
            }
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                if let Some(link) = self.link.take() {
                    let text: String = self.runs[link.from.min(self.runs.len())..]
                        .iter()
                        .map(|(t, _)| t.as_str())
                        .collect();
                    if !link.url.is_empty() && link.url != text {
                        self.push(&format!(" ({})", link.url), theme::dim());
                    }
                }
            }
            TagEnd::Image => {
                if let Some(alt) = self.image.take() {
                    // Shown as the literal text it was written as.
                    let style = self.style();
                    self.push(&format!("![{alt}]"), style);
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.head_rows = table.rows.len();
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.table_rows(&table);
                }
                self.gap = self.lists.is_empty();
            }
            _ => {}
        }
    }

    /// The prefix of every row here: a dim `│ ` per quote level, then the list's indent.
    fn prefix(&self) -> (Vec<Run>, usize) {
        let mut runs = Vec::new();
        for _ in 0..self.quote {
            runs.push((String::from("│ "), theme::dim()));
        }
        let indent = 2 * self.lists.len().saturating_sub(1);
        if indent > 0 {
            runs.push((" ".repeat(indent), theme::text()));
        }
        let used = runs.iter().map(|(t, _)| width(t)).sum();
        (runs, used)
    }

    /// The prefix of a code block or table row: [`Self::prefix`], then under the item's text.
    fn block_prefix(&self) -> (Vec<Run>, usize) {
        let (mut runs, used) = self.prefix();
        let hang = self.lists.last().map_or(0, |list| list.hang);
        if hang > 0 {
            runs.push((" ".repeat(hang), theme::text()));
        }
        (runs, used + hang)
    }

    /// Writes the pending inline runs as word-wrapped rows.
    fn flush(&mut self) {
        if self.runs.is_empty() {
            return;
        }
        let runs = std::mem::take(&mut self.runs);
        let (prefix, used) = self.prefix();
        let marker = self.marker.take();
        let hang = self.lists.last().map_or(0, |list| list.hang);
        let max = self.width.saturating_sub(used + hang).max(1);
        let mut first = true;
        for segment in split_lines(runs) {
            for (i, row) in wrap_styled(&segment, max).into_iter().enumerate() {
                let mut spans: Vec<Span<'static>> = prefix
                    .iter()
                    .map(|(t, s)| Span::styled(t.clone(), *s))
                    .collect();
                if hang > 0 {
                    match (&marker, first) {
                        (Some(m), true) => spans.push(Span::styled(m.clone(), theme::text())),
                        _ => spans.push(Span::raw(" ".repeat(hang))),
                    }
                }
                spans.extend(row.into_iter().map(|(t, s)| Span::styled(t, s)));
                self.out.push((Line::from(spans), i > 0));
                first = false;
            }
        }
    }

    /// A code block: indented 2 past the prefix, unwrapped, cut with `…`, highlighted.
    fn code_rows(&mut self, lang: &str, code: &str) {
        let (prefix, used) = self.block_prefix();
        let max = self.width.saturating_sub(used + 2);
        let highlighted = highlight(lang, code);
        for line in highlighted {
            let mut spans: Vec<Span<'static>> = prefix
                .iter()
                .map(|(t, s)| Span::styled(t.clone(), *s))
                .collect();
            spans.push(Span::raw("  "));
            spans.extend(cut(line, max).into_iter().map(|(t, s)| Span::styled(t, s)));
            self.out.push((Line::from(spans), false));
        }
    }

    /// A table: cells padded to their column's widest, 2 spaces between, the header bold.
    fn table_rows(&mut self, table: &Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0; columns];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.iter().map(|(t, _)| width(t)).sum());
            }
        }
        let (prefix, used) = self.block_prefix();
        let max = self.width.saturating_sub(used);
        for (r, row) in table.rows.iter().enumerate() {
            let head = r < table.head_rows;
            let mut runs: Vec<Run> = Vec::new();
            for (i, w) in widths.iter().enumerate() {
                if i > 0 {
                    runs.push((String::from("  "), theme::text()));
                }
                let cell = row.get(i).cloned().unwrap_or_default();
                let cell_width: usize = cell.iter().map(|(t, _)| width(t)).sum();
                for (t, s) in cell {
                    let s = if head {
                        s.add_modifier(Modifier::BOLD)
                    } else {
                        s
                    };
                    runs.push((t, s));
                }
                // No trailing padding after the last column.
                if i + 1 < columns {
                    runs.push((" ".repeat(w - cell_width), theme::text()));
                }
            }
            let mut spans: Vec<Span<'static>> = prefix
                .iter()
                .map(|(t, s)| Span::styled(t.clone(), *s))
                .collect();
            spans.extend(cut(runs, max).into_iter().map(|(t, s)| Span::styled(t, s)));
            self.out.push((Line::from(spans), false));
        }
    }
}

/// Splits runs at hard breaks (`\n`) into one list of runs per line.
fn split_lines(runs: Vec<Run>) -> Vec<Vec<Run>> {
    let mut out = vec![Vec::new()];
    for (text, style) in runs {
        let mut parts = text.split('\n');
        if let Some(first) = parts.next()
            && !first.is_empty()
            && let Some(line) = out.last_mut()
        {
            line.push((first.to_owned(), style));
        }
        for part in parts {
            out.push(Vec::new());
            if !part.is_empty()
                && let Some(line) = out.last_mut()
            {
                line.push((part.to_owned(), style));
            }
        }
    }
    // A block of HTML ends in a newline that starts no row.
    if out.len() > 1 && out.last().is_some_and(Vec::is_empty) {
        out.pop();
    }
    out
}

/// `runs` cut to at most `max` cells, ending in `…` when cut.
fn cut(runs: Vec<Run>, max: usize) -> Vec<Run> {
    let total: usize = runs.iter().map(|(t, _)| width(t)).sum();
    if total <= max {
        return runs;
    }
    let mut out = Vec::new();
    let mut left = max;
    for (text, style) in runs {
        let w = width(&text);
        if w < left {
            left -= w;
            out.push((text, style));
            continue;
        }
        // This run holds the edge: `truncate` adds the `…`.
        out.push((truncate(&text, left), style));
        return out;
    }
    out
}

/// The syntaxes `syntect` ships, loaded once.
fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The syntax scope map, most specific first within each role.
fn scope_style(stack: &ScopeStack) -> Style {
    static MAP: OnceLock<Vec<(Scope, Style)>> = OnceLock::new();
    let map = MAP.get_or_init(|| {
        [
            ("comment", theme::dim()),
            ("string", theme::ok()),
            ("constant.numeric", theme::warn()),
            ("constant.language", theme::warn()),
            ("entity.name.function", theme::strong()),
            ("keyword", theme::accent()),
            ("storage", theme::accent()),
        ]
        .into_iter()
        .filter_map(|(name, style)| Scope::new(name).ok().map(|scope| (scope, style)))
        .collect()
    });
    // The innermost scope with a role wins.
    for scope in stack.as_slice().iter().rev() {
        if let Some((_, style)) = map.iter().find(|(role, _)| role.is_prefix_of(*scope)) {
            return *style;
        }
    }
    theme::text()
}

/// The syntax for a fence's language word, if `syntect` knows it.
fn syntax_for(lang: &str) -> Option<&'static SyntaxReference> {
    if lang.is_empty() {
        return None;
    }
    let set = syntaxes();
    set.find_syntax_by_token(lang)
}

/// `code`'s lines as styled runs: highlighted when the language is known, plain text otherwise.
fn highlight(lang: &str, code: &str) -> Vec<Vec<Run>> {
    let code = code.strip_suffix('\n').unwrap_or(code);
    let Some(syntax) = syntax_for(lang) else {
        return code
            .split('\n')
            .map(|line| vec![(expand_tabs(line), theme::text())])
            .collect();
    };
    let set = syntaxes();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::new();
    for line in code.split('\n') {
        let with_newline = format!("{line}\n");
        let ops = state.parse_line(&with_newline, set).unwrap_or_default();
        let mut runs: Vec<Run> = Vec::new();
        let mut at = 0;
        for (pos, op) in ops {
            let pos = pos.min(line.len());
            if pos > at {
                push_run(&mut runs, &line[at..pos], scope_style(&stack));
                at = pos;
            }
            // A bad op leaves the stack as it was; the colours stay readable either way.
            let _ = stack.apply(&op);
        }
        if at < line.len() {
            push_run(&mut runs, &line[at..], scope_style(&stack));
        }
        out.push(runs);
    }
    out
}

/// Appends `text` in `style`, merging with the last run when the style matches.
fn push_run(runs: &mut Vec<Run>, text: &str, style: Style) {
    let text = expand_tabs(text);
    match runs.last_mut() {
        Some((last, s)) if *s == style => last.push_str(&text),
        _ => runs.push((text, style)),
    }
}

/// Tabs as four spaces, so cells line up.
fn expand_tabs(text: &str) -> String {
    text.replace('\t', "    ")
}
