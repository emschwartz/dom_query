use std::fmt::Write;

use super::ctx::FormatOpts;
use super::elements::code::ends_with_code_span;
use super::elements::emphasis::{Edges, Emphasis, push_hard_break};
use super::elements::table::ends_with_cell_break;
use super::text_utils::{
    escape_trailing_bang, push_block_space, push_md_url, push_normalized_text, push_title,
};

/// What a code span or an image shows: opaque, it counts as a word on both ends.
const WORD: char = '\u{fffc}';
/// What a run of another kind opens with, as far as the run before can tell: its delimiter
/// may fuse with that run's closing one, so it counts as a letter.
const OTHER_RUN: char = 'a';

/// Inline Markdown, plus the first and last characters a reader would see.
/// Markup like `**` or `](url)` is invisible and never counts as an end.
pub(super) struct InlineWriter {
    pub(super) text: String,
    first_visible: Option<char>,
    /// A `<br>` counts as `'\n'`.
    last_visible: Option<char>,
    /// A separator came before any visible text: the parent decides it.
    separator_before: bool,
    /// A separator came after the last visible text.
    separator_after: bool,
    /// A finished emphasis run, written once what follows shows whether its delimiters fit.
    pending: Option<Run>,
    /// What the text ends with, for the flanking of the next opening delimiter.
    written: Written,
    /// An empty buffer for the next pending run.
    spare: String,
}

struct Run {
    kind: Emphasis,
    /// The content between the delimiters, without edge spaces or hard breaks.
    content: String,
    /// What the text ended with before the opening delimiter.
    start: Written,
    /// Edge spaces and a hard break, written after the closing delimiter.
    trail: String,
}

#[derive(Clone, Copy)]
enum Written {
    /// A line start or markup, which delimiters can sit next to.
    Edge,
    /// A source character.
    Char(char),
    /// The closing delimiter of a run of `kind`; `before` is the character before it, and
    /// `nested` whether a nested run's delimiter comes right before it.
    Closing {
        kind: Emphasis,
        before: Option<char>,
        nested: bool,
    },
}

/// One piece of inline output, named for what it is.
pub(super) enum Inline<'a> {
    Text(&'a str, FormatOpts),
    /// An `<svg>` or `<option>`: shows nothing, keeps words apart.
    Separator,
    LineBreak(&'a str),
    Code(&'a str),
    Image(&'a str),
    /// The content of an emphasis run, without its delimiters.
    Emphasis(Emphasis, &'a mut InlineWriter),
    /// A link label, its destination and its title, written as `[label](url "title")`.
    Link(&'a mut InlineWriter, &'a str, Option<&'a str>),
}

impl InlineWriter {
    /// Continues `text`, whose last character is the visible end so far.
    pub(super) fn new(text: String) -> Self {
        let end = visible_end(&text);
        let written = text
            .chars()
            .next_back()
            .map_or(Written::Edge, Written::Char);
        Self {
            text,
            first_visible: end,
            last_visible: end,
            separator_before: false,
            separator_after: false,
            pending: None,
            written,
            spare: String::new(),
        }
    }

    pub(super) fn push(&mut self, inline: Inline) {
        match inline {
            // waits for the next visible character; one before any belongs to the parent
            Inline::Separator if self.first_visible.is_none() => self.separator_before = true,
            Inline::Separator => self.separator_after = true,
            // whitespace after a pending run stays at its edge, where a run of its kind may join it
            Inline::Text(s, _)
                if self.pending.is_some() && !s.is_empty() && s.trim_ascii().is_empty() =>
            {
                if let Some(run) = &mut self.pending {
                    if !run.trail.ends_with([' ', '\n']) {
                        run.trail.push(' ');
                    }
                }
                self.separator_after = false;
                self.last_visible = Some(' ');
            }
            // text is judged by its source characters, before escaping
            Inline::Text(s, opts) => {
                let (Some(first), Some(last)) = (s.chars().next(), s.chars().next_back()) else {
                    return;
                };
                self.start_piece(first, Some(first));
                push_normalized_text(&mut self.text, s, opts);
                self.wrote(first, last, Written::Char(last));
            }
            // whitespace, so no separator is written before it
            Inline::LineBreak(s) => {
                self.start_piece('\n', Some('\n'));
                self.text.push_str(s);
                self.wrote('\n', '\n', Written::Edge);
            }
            // opaque words; a code span right after another would merge their backticks
            Inline::Code(span) => {
                if !self.start_piece(WORD, Some('`')) && ends_with_code_span(&self.text) {
                    self.text.push(' ');
                }
                self.text.push_str(span);
                self.wrote(WORD, WORD, Written::Edge);
            }
            Inline::Image(md) => {
                self.start_piece(WORD, Some('!'));
                self.text.push_str(md);
                self.wrote(WORD, WORD, Written::Edge);
            }
            // a run that writes nothing passes its separators on
            Inline::Emphasis(_, run) | Inline::Link(run, ..) if run.text.is_empty() => {
                if run.separator_before || run.separator_after {
                    self.push(Inline::Separator);
                }
            }
            Inline::Emphasis(kind, run) => {
                if run.separator_before {
                    self.push(Inline::Separator);
                }
                self.push_emphasis(kind, &run.text, run.first_visible.unwrap_or(WORD));
                self.shown_run(run);
            }
            Inline::Link(run, href, title) => {
                if run.separator_before {
                    self.push(Inline::Separator);
                }
                self.start_piece(run.first_visible.unwrap_or(WORD), Some('['));
                // an unescaped `!` before `[` would make image syntax
                escape_trailing_bang(&mut self.text);
                self.text.push('[');
                self.text.push_str(&run.text);
                self.text.push_str("](");
                push_md_url(&mut self.text, href);
                if let Some(title) = title {
                    push_title(&mut self.text, title);
                }
                self.text.push(')');
                self.shown_run(run);
                self.written = Written::Edge;
            }
        }
    }

    /// Block output written straight to the text ends the inline run: it drops a separator
    /// waiting for it, and its last character is the visible end.
    pub(super) fn write_block(&mut self, write: impl FnOnce(&mut String)) {
        self.finish();
        let len = self.text.len();
        write(&mut self.text);
        if self.text.len() != len {
            self.separator_after = false;
            self.first_visible.get_or_insert('\n');
            self.last_visible = visible_end(&self.text);
            self.written = Written::Edge;
        }
    }

    /// Writes a pending run: what follows is the end of the line or of the enclosing markup.
    pub(super) fn finish(&mut self) {
        self.settle(None);
    }

    /// Whether the last write left an emphasis run waiting, and how long it is.
    pub(super) fn pending_len(&self) -> usize {
        self.pending.as_ref().map_or(0, |run| run.content.len() + 1)
    }

    /// Starts a piece that shows `next` first and writes `source` first: writes the pending
    /// run, then the waiting separator if it is a space. Returns whether a space was written.
    fn start_piece(&mut self, next: char, source: Option<char>) -> bool {
        let owed = self.owes_space(next);
        self.write_owed_space(owed, source)
    }

    fn owes_space(&mut self, next: char) -> bool {
        std::mem::take(&mut self.separator_after) && separator_is_space(self.last_visible, next)
    }

    /// Writes the pending run, followed by `source` or by the space if one is `owed`, then
    /// the space.
    fn write_owed_space(&mut self, owed: bool, source: Option<char>) -> bool {
        self.settle(if owed { Some(' ') } else { source });
        if !owed {
            return false;
        }
        let len = self.text.len();
        push_block_space(&mut self.text);
        self.written = Written::Char(' ');
        self.text.len() != len
    }

    /// Holds the run until what follows is known: edges go outside its delimiters, and a run of
    /// the same kind right after (or past one space) joins it.
    #[inline(never)]
    fn push_emphasis(&mut self, kind: Emphasis, content: &str, first_visible: char) {
        // whitespace alone takes no delimiters
        if content.trim().is_empty() {
            if let Some(run) = &mut self.pending {
                run.trail.push_str(content);
            } else {
                self.start_piece(first_visible, content.chars().next());
                self.text.push_str(content);
                if let Some(c) = content.chars().next_back() {
                    self.written = Written::Char(c);
                }
            }
            return;
        }
        let edges = Edges::of(content);
        let owed_space = self.owes_space(first_visible);
        if let Some(run) = &mut self.pending {
            let spaced = owed_space || run.trail == " " || !edges.lead.is_empty();
            if !edges.hard_start && run.joins(kind, edges.core, spaced) {
                run.append(owed_space, &edges);
                return;
            }
        }
        // what the pending run is followed by: an edge is whitespace, and a run of the same
        // kind fuses with it anyway (`Written::fuses`)
        let same_kind = self.pending.as_ref().is_some_and(|run| run.kind == kind);
        let next = if same_kind || edges.hard_start || !edges.lead.is_empty() {
            None
        } else {
            Some(OTHER_RUN)
        };
        self.write_owed_space(owed_space, next);
        if edges.hard_start {
            self.write_block(push_hard_break);
        }
        push_edge_space(&mut self.text, edges.lead);
        if let Some(c) = edges.lead.chars().next_back() {
            self.written = Written::Char(c);
        }
        let content = std::mem::take(&mut self.spare);
        self.pending = Some(Run::new(kind, content, self.written, &edges));
    }

    /// Writes the pending run, with delimiters if they fit before `after`, else as html.
    fn settle(&mut self, after: Option<char>) {
        if self.pending.is_some() {
            self.write_pending(after);
        }
    }

    #[inline(never)]
    fn write_pending(&mut self, after: Option<char>) {
        let Some(mut run) = self.pending.take() else {
            return;
        };
        let after = run.trail.chars().next().or(after);
        let (kind, start) = (run.kind, run.start);
        if !start.fuses(kind) && kind.fits(start.before(kind), &run.content, after) {
            let (marker, delim) = (kind.marker(), kind.delimiter());
            self.text.push_str(marker);
            self.text.push_str(&run.content);
            self.text.push_str(marker);
            self.written = Written::Closing {
                kind,
                before: run.content.trim_end_matches(delim).chars().next_back(),
                nested: run.content.ends_with(delim),
            };
        } else {
            let tag = kind.tag();
            let _ = write!(self.text, "<{tag}>{}</{tag}>", run.content);
            self.written = Written::Edge;
        }
        if let Some(c) = run.trail.chars().next_back() {
            self.text.push_str(&run.trail);
            self.written = if c == '\n' {
                Written::Edge
            } else {
                Written::Char(c)
            };
        }
        run.content.clear();
        self.spare = run.content;
    }

    /// Records a written piece: its visible ends and what the text now ends with.
    fn wrote(&mut self, first: char, last: char, written: Written) {
        self.first_visible.get_or_insert(first);
        self.last_visible = Some(last);
        self.written = written;
    }

    fn shown_run(&mut self, run: &InlineWriter) {
        self.first_visible = self.first_visible.or(run.first_visible);
        self.last_visible = run.last_visible;
        self.separator_after = run.separator_after;
    }
}

impl Written {
    /// The source character an opening delimiter of `kind` would follow; a closing delimiter
    /// of the same character joins it into one delimiter run, so it looks past that.
    fn before(self, kind: Emphasis) -> Option<char> {
        match self {
            Self::Edge => None,
            Self::Char(c) => Some(c),
            Self::Closing {
                kind: k, before, ..
            } if k.delimiter() == kind.delimiter() => before,
            Self::Closing { kind: k, .. } => Some(k.delimiter()),
        }
    }

    /// Whether an opening delimiter of `kind` here would fuse with the closing one before it:
    /// `**a***b*` works, `**a****b**` and `***a*****b**` don't.
    fn fuses(self, kind: Emphasis) -> bool {
        matches!(self, Self::Closing { kind: k, nested, .. }
            if k.delimiter() == kind.delimiter() && (k == kind || nested))
    }
}

impl Run {
    /// A run of `kind` holding `edges`, in `content`, an empty buffer.
    fn new(kind: Emphasis, mut content: String, start: Written, edges: &Edges) -> Self {
        content.push_str(edges.core);
        let mut run = Self {
            kind,
            content,
            start,
            trail: String::new(),
        };
        run.set_trail(edges);
        run
    }

    /// Whether a run of `kind` starting with `core` joins this one, past one space when `spaced`.
    fn joins(&self, kind: Emphasis, core: &str, spaced: bool) -> bool {
        self.kind == kind
            && (self.trail.is_empty() || self.trail == " ")
            && kind.can_merge(&self.content, core, spaced)
    }

    /// Appends the content of a run that joins this one, after the space between them.
    fn append(&mut self, owed_space: bool, edges: &Edges) {
        let gap = std::mem::take(&mut self.trail);
        push_edge_space(&mut self.content, &gap);
        if owed_space {
            push_edge_space(&mut self.content, " ");
        }
        push_edge_space(&mut self.content, edges.lead);
        // an unescaped `!` before `[` would make image syntax
        if edges.core.starts_with('[') {
            escape_trailing_bang(&mut self.content);
        }
        self.content.push_str(edges.core);
        self.set_trail(edges);
    }

    fn set_trail(&mut self, edges: &Edges) {
        push_edge_space(&mut self.trail, edges.trail);
        self.trail.push_str(edges.hard_end);
    }
}

/// The spacing rule: a separator is a space unless whitespace or closing punctuation follows,
/// or whitespace, an opening bracket or a `<br>` (`'\n'`) precedes.
fn separator_is_space(last: Option<char>, next: char) -> bool {
    let closing = |c: char| {
        c.is_whitespace() || matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
    };
    let opening = |c: char| c.is_whitespace() || matches!(c, '(' | '[' | '{');
    !closing(next) && !last.is_some_and(opening)
}

/// Pushes `space`, collapsing ASCII spaces into one that `acc` doesn't already end with.
fn push_edge_space(acc: &mut String, space: &str) {
    for c in space.chars() {
        if c != ' ' || !acc.ends_with(' ') {
            acc.push(c);
        }
    }
}

fn visible_end(text: &str) -> Option<char> {
    if ends_with_cell_break(text) {
        Some('\n')
    } else {
        text.chars().next_back()
    }
}
