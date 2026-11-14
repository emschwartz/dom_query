//! The Markdown output buffer. Separators, line prefixes, leads (a container's text on
//! its first line) and emphasis delimiters are pending state, written only when the next content arrives, so an
//! element that writes nothing leaves nothing behind.

use super::ctx::FormatOpts;
use super::elements::table::ends_with_cell_break;
use super::text_utils::push_escaped_chunk;

/// How a block starts, seen from the line above it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Start {
    Text,
    /// a `<p>`, set off by blank lines
    Para,
    Quote,
    /// a heading, fence or rule, or a lead that can interrupt a paragraph
    Block,
    /// a lead that cannot interrupt a paragraph
    Weak,
}

/// How a block ends, seen from the line below it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum End {
    /// a paragraph at this column: text after it continues it
    Text,
    Para,
    /// a `>` line at this column: text after it would join the quote lazily
    Quote,
    /// a line that text after it joins, but that any block start ends
    Lazy,
    /// a table row: like `Lazy`, but a line outside its container doesn't join it
    Row,
    /// a heading, fence or rule, which ends at its own line end
    Closed,
    /// raw HTML, which runs on to the next blank line
    Html,
}

/// The separator a block asks of its neighbors, where no group rules apply.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Sep {
    None,
    Line,
    Blank,
}

/// The separator between two blocks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Gap {
    Line,
    Blank,
    /// an empty `>` line, which ends the quote's paragraph without a blank line
    QuoteEnd,
}

/// How a group of members separates its children, given by the module that opens it.
/// A member is a frame with a lead written on its first line; loose content is
/// anything in the group outside a member.
pub(super) struct Rules {
    /// between two blocks inside a member
    pub(super) block_gap: fn(End, Start) -> Gap,
    /// between two members
    pub(super) member_gap: fn(End, Start) -> Gap,
    /// before loose content after a member that ended as `End` and held a paragraph or not
    pub(super) loose_gap: fn(End, bool) -> Gap,
    /// whether that loose content continues the member's last line
    pub(super) continues: fn(End, bool) -> bool,
    /// before a member that starts as `Start` after loose content, which continued the
    /// member before it, or (`None`) started the group; the hard breaks pending
    pub(super) after_loose: fn(Option<bool>, Start, u8) -> Gap,
    /// how the group ends for its parent: from its last member's end, else its own
    pub(super) group_end: fn(Option<End>, End) -> End,
    /// whether a line holding these leads and then this content must break after the leads
    pub(super) breaks_line: fn(&mut dyn Iterator<Item = &str>, &str) -> bool,
}

fn sep_gap(sep: Sep) -> Option<Gap> {
    match sep {
        Sep::None => None,
        Sep::Line => Some(Gap::Line),
        Sep::Blank => Some(Gap::Blank),
    }
}

/// A word boundary waiting for the next content, weakest first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Space {
    None,
    /// an `<option>`, or a block in code: none before closing punctuation
    Boundary,
    /// a block in link text: dropped at the end
    Join,
    /// whitespace in text
    Word,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Block,
    /// lines are joined by `<br>`, and only lines with a lead are indented
    Cell,
    /// link text or a heading: one line, blocks joined by spaces
    Inline,
    /// code (md skipped): blocks joined by spaces
    Code,
}

impl Mode {
    fn of(opts: FormatOpts) -> Self {
        if opts.skip_md {
            Self::Code
        } else if opts.link_body {
            Self::Inline
        } else if opts.table_cell {
            Self::Cell
        } else {
            Self::Block
        }
    }
}

#[derive(Clone, Copy)]
enum Flow {
    Fresh,
    /// inline text that the next text continues
    Text,
    /// a block that ended as `End` and asks `Sep` of the next one
    After(End, Sep),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Root,
    Quote,
    /// holds members and loose content
    Group,
    /// writes a lead on its first line
    Member,
}

#[derive(Clone, Copy)]
struct Lead {
    buf: [u8; 12],
    len: u8,
}

impl Lead {
    fn new(s: &str) -> Self {
        let mut buf = [0; 12];
        let len = s.len().min(12);
        buf[..len].copy_from_slice(&s.as_bytes()[..len]);
        #[allow(clippy::cast_possible_truncation)]
        Self {
            buf,
            len: len as u8,
        }
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or_default()
    }
}

/// A container: the document, a quote, a group or a member.
#[derive(Clone, Copy)]
struct Frame {
    kind: Kind,
    /// the prefix length before this frame's own part
    base: usize,
    started: bool,
    flow: Flow,
    /// how the next content starts, declared by the block that opened around it
    next: Option<(Start, Sep)>,
    /// the parent's `next`, given back if this frame writes nothing
    outer_next: Option<(Start, Sep)>,
    rules: Option<&'static Rules>,
    /// member: the lead, until the first content writes it at `lead_pos`
    lead: Option<Lead>,
    lead_pos: usize,
    lead_len: usize,
    /// member: how it starts when its lead line holds content
    lead_start: Start,
    /// member: its first line also held a deeper member's lead
    lead_first: bool,
    /// group: how the member just written ended, until loose content follows it
    last_member: Option<End>,
    /// group: whether its loose content continues the member before it; `None` when
    /// it started the group
    loose_lazy: Option<bool>,
    /// a paragraph ended in it (member), or in its last member (group)
    held_para: bool,
}

impl Frame {
    fn new(kind: Kind, base: usize) -> Self {
        Self {
            kind,
            base,
            started: false,
            flow: Flow::Fresh,
            next: None,
            outer_next: None,
            rules: None,
            lead: None,
            lead_pos: 0,
            lead_len: 0,
            lead_start: Start::Text,
            lead_first: false,
            last_member: None,
            loose_lazy: None,
            held_para: false,
        }
    }

    fn end_text(&mut self) {
        if matches!(self.flow, Flow::Text) {
            self.flow = Flow::After(End::Text, Sep::None);
        }
    }
}

/// An inline run: emphasis (`delim` set), link text or a scope with its own mode.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy)]
struct Span {
    delim: &'static str,
    /// written: where the opener starts, and whether it is the html tag
    open: Option<(usize, bool)>,
    /// where the content starts, past the opener
    start: usize,
    /// the block count when the span opened; a block after it makes it `blocky`
    blocks: u32,
    blocky: bool,
    /// the state the span replaced, given back when it closes
    mode: Mode,
    line_escape: bool,
    content_start: usize,
    floor: usize,
    keep_lead: bool,
    keep_edges: bool,
    label: Option<Label>,
}

#[derive(Clone, Copy)]
struct Label {
    pos: usize,
    /// the content start of the line, and the mode, outside the text
    line_start: usize,
    mode: Mode,
    fresh: bool,
    joined: bool,
    blank_edge: bool,
}

/// An emphasis run whose closing delimiter waits for the next character.
#[derive(Clone, Copy)]
struct Closer {
    delim: &'static str,
    /// the opener's position and whether it is the html tag
    pos: usize,
    html: bool,
    /// the run's content start, and where the closer goes (before trailing edge spaces)
    start: usize,
    at: usize,
}

/// What follows a pending closer.
#[derive(Clone, Copy)]
enum Next {
    /// a line end, whitespace or the end of the text
    End,
    /// another run's opener
    Opener(&'static str),
    Char(char),
}

/// A change before the end of the output, undone on rollback.
struct Edit {
    pos: usize,
    old: &'static str,
    new_len: usize,
}

/// The state a rollback restores.
#[allow(clippy::struct_excessive_bools)]
pub(super) struct Mark {
    len: usize,
    from: usize,
    frames_at: usize,
    spans_at: usize,
    closers_at: usize,
    edits_at: usize,
    line_open: bool,
    line_begin: usize,
    content_start: usize,
    line_escape: bool,
    space: Space,
    hard: u8,
    owed: usize,
    label: Option<Label>,
    mode: Mode,
    floor: usize,
    keep_lead: bool,
    keep_edges: bool,
}

/// The state of an outer rendering while `render_into` writes elsewhere.
#[allow(clippy::struct_excessive_bools)]
struct Saved {
    out: String,
    prefix: String,
    frames: Vec<Frame>,
    spans: Vec<Span>,
    closers: Vec<Closer>,
    mode: Mode,
    line_open: bool,
    line_begin: usize,
    content_start: usize,
    line_escape: bool,
    space: Space,
    hard: u8,
    keep_lead: bool,
    keep_edges: bool,
    include_node: bool,
    floor: usize,
    owed: usize,
    label: Option<Label>,
    live: usize,
    edits_base: usize,
}

/// Buffers kept for the next `render_into`.
type Spare = (Vec<Frame>, Vec<Span>, Vec<Closer>, String);

#[allow(clippy::struct_excessive_bools)]
pub(super) struct MdWriter {
    out: String,
    mode: Mode,
    prefix: String,
    frames: Vec<Frame>,
    spans: Vec<Span>,
    closers: Vec<Closer>,
    line_open: bool,
    line_begin: usize,
    content_start: usize,
    line_escape: bool,
    space: Space,
    hard: u8,
    /// a space before the first content of a scope is kept (code)
    keep_lead: bool,
    /// trailing spaces and breaks are kept (code, link text, an included node)
    keep_edges: bool,
    include_node: bool,
    /// trailing trims stop here
    floor: usize,
    /// emphasis spans whose opener waits for content
    owed: usize,
    /// the open link text
    label: Option<Label>,
    /// block boundaries seen so far
    blocks: u32,
    undo_frames: Vec<Frame>,
    undo_spans: Vec<Span>,
    undo_closers: Vec<Closer>,
    edits: Vec<Edit>,
    live: usize,
    edits_base: usize,
    spare: Vec<Spare>,
}

impl MdWriter {
    pub(super) fn new(include_node: bool) -> Self {
        let mut root = Frame::new(Kind::Root, 0);
        // an included node keeps the separator before its first block
        if include_node {
            root.started = true;
            root.flow = Flow::Text;
        }
        Self {
            out: String::new(),
            mode: Mode::Block,
            prefix: String::new(),
            frames: vec![root],
            spans: Vec::new(),
            closers: Vec::new(),
            line_open: include_node,
            line_begin: 0,
            content_start: 0,
            line_escape: true,
            space: Space::None,
            hard: 0,
            keep_lead: false,
            keep_edges: include_node,
            include_node,
            floor: 0,
            owed: 0,
            label: None,
            blocks: 0,
            undo_frames: Vec::new(),
            undo_spans: Vec::new(),
            undo_closers: Vec::new(),
            edits: Vec::new(),
            live: 0,
            edits_base: 0,
            spare: Vec::new(),
        }
    }

    /// The written text, with the edges an included node keeps.
    pub(super) fn finish(mut self) -> String {
        self.settle_all(Next::End);
        if self.keep_edges {
            self.push_hard_breaks();
            if self.space == Space::Word && !self.out.ends_with([' ', '\n']) {
                self.out.push(' ');
            }
            if let Flow::After(_, sep) = self.frames[0].flow {
                match sep {
                    Sep::None => {}
                    Sep::Line => self.out.push('\n'),
                    Sep::Blank => self.out.push_str("\n\n"),
                }
            }
        }
        self.out
    }

    /// Renders through `f` into `buf` instead of the output, as a fresh document in
    /// the mode `opts` calls for; the writer's own state is kept aside meanwhile.
    pub(super) fn render_into(
        &mut self,
        buf: &mut String,
        opts: FormatOpts,
        f: impl FnOnce(&mut Self),
    ) {
        let (mut frames, spans, closers, prefix) = self.spare.pop().unwrap_or_default();
        frames.push(Frame::new(Kind::Root, 0));
        let mode = Mode::of(opts);
        let saved = Saved {
            out: std::mem::replace(&mut self.out, std::mem::take(buf)),
            prefix: std::mem::replace(&mut self.prefix, prefix),
            frames: std::mem::replace(&mut self.frames, frames),
            spans: std::mem::replace(&mut self.spans, spans),
            closers: std::mem::replace(&mut self.closers, closers),
            mode: std::mem::replace(&mut self.mode, mode),
            line_open: std::mem::replace(&mut self.line_open, !self.out.is_empty()),
            line_begin: std::mem::take(&mut self.line_begin),
            content_start: std::mem::replace(&mut self.content_start, self.out.len()),
            line_escape: std::mem::replace(
                &mut self.line_escape,
                matches!(mode, Mode::Block | Mode::Cell),
            ),
            space: std::mem::replace(&mut self.space, Space::None),
            hard: std::mem::take(&mut self.hard),
            keep_lead: std::mem::take(&mut self.keep_lead),
            keep_edges: std::mem::take(&mut self.keep_edges),
            include_node: std::mem::take(&mut self.include_node),
            floor: std::mem::take(&mut self.floor),
            owed: std::mem::take(&mut self.owed),
            label: self.label.take(),
            live: std::mem::take(&mut self.live),
            edits_base: std::mem::replace(&mut self.edits_base, self.edits.len()),
        };
        f(self);
        self.settle_all(Next::End);
        *buf = std::mem::replace(&mut self.out, saved.out);
        let mut frames = std::mem::replace(&mut self.frames, saved.frames);
        let mut spans = std::mem::replace(&mut self.spans, saved.spans);
        let mut closers = std::mem::replace(&mut self.closers, saved.closers);
        let mut prefix = std::mem::replace(&mut self.prefix, saved.prefix);
        frames.clear();
        spans.clear();
        closers.clear();
        prefix.clear();
        self.spare.push((frames, spans, closers, prefix));
        self.edits.truncate(self.edits_base);
        self.mode = saved.mode;
        self.line_open = saved.line_open;
        self.line_begin = saved.line_begin;
        self.content_start = saved.content_start;
        self.line_escape = saved.line_escape;
        self.space = saved.space;
        self.hard = saved.hard;
        self.keep_lead = saved.keep_lead;
        self.keep_edges = saved.keep_edges;
        self.include_node = saved.include_node;
        self.floor = saved.floor;
        self.owed = saved.owed;
        self.label = saved.label;
        self.live = saved.live;
        self.edits_base = saved.edits_base;
    }

    pub(super) fn word_space(&mut self) {
        self.space = self.space.max(Space::Word);
    }

    /// A boundary that takes no space before closing punctuation: an `<option>`, a
    /// block in code.
    pub(super) fn boundary(&mut self) {
        self.space = self.space.max(Space::Boundary);
    }

    /// A block in link text, joined by a space.
    pub(super) fn join(&mut self) {
        self.space = self.space.max(Space::Join);
        if let Some(l) = &mut self.label {
            l.joined = true;
        }
    }

    /// A block ends here in link text, with no space after it.
    pub(super) fn label_edge(&mut self) {
        if let Some(l) = &mut self.label {
            l.joined = true;
        }
    }

    /// A hard line break; `stack` adds one to those pending (`<br>`), otherwise at most
    /// one is pending (a row outside its table).
    pub(super) fn hard_break(&mut self, stack: bool) {
        self.hard = if stack {
            self.hard.saturating_add(1)
        } else {
            self.hard.max(1)
        };
        // a space before a break adds nothing; one after it, in link text, stays
        self.space = Space::None;
        // two breaks in emphasis end its paragraph, which splits the run
        if self.hard >= 2
            && self.mode == Mode::Block
            && self.spans.iter().any(|s| !s.delim.is_empty())
        {
            let top = self.top();
            if matches!(top.flow, Flow::Text) && top.next.is_none() {
                top.flow = Flow::After(End::Text, Sep::Blank);
                self.hard = 0;
            }
        }
    }

    /// Writes text, its whitespace collapsed to word boundaries.
    pub(super) fn text(&mut self, contents: &str, opts: FormatOpts) {
        let escape = !opts.skip_escape;
        if contents.starts_with(|c: char| c.is_ascii_whitespace()) {
            self.word_space();
        }
        let mut words = contents.split_ascii_whitespace();
        let mut first = true;
        for word in words.by_ref() {
            if !first {
                self.word_space();
            }
            first = false;
            let mut word = word;
            if self.owed > 0 {
                // edge spaces stay outside the run, where its delimiter can open
                let rest = word.trim_start_matches(is_edge_space);
                if rest.len() < word.len() {
                    let lead = &word[..word.len() - rest.len()];
                    self.begin(lead, lead.chars().next(), false, false);
                    self.out.push_str(lead);
                    if rest.is_empty() {
                        continue;
                    }
                    word = rest;
                }
            }
            self.write_word(word, escape);
            if self.owed == 0 {
                break;
            }
        }
        for word in words {
            self.out.push(' ');
            push_escaped_chunk(&mut self.out, word, escape, false);
        }
        if contents.ends_with(|c: char| c.is_ascii_whitespace()) {
            self.word_space();
        }
    }

    fn write_word(&mut self, word: &str, escape: bool) {
        self.begin(word, word.chars().next(), false, true);
        let line = &self.out[self.content_start..];
        // only the first word of a text node can continue a Markdown line
        let line_start = self.line_escape && line.trim_matches(' ').is_empty();
        let digits = line.trim_start_matches(' ');
        if escape
            && self.line_escape
            && matches!(word, "." | ")")
            && (1..=9).contains(&digits.len())
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            // `<span>1</span>. foo`: the digits and this `.` would form a list marker
            self.out.push('\\');
        }
        push_escaped_chunk(&mut self.out, word, escape, line_start);
    }

    /// Writes `s`, inline content with no line break.
    pub(super) fn inline(&mut self, s: &str) {
        if !s.is_empty() {
            self.begin(s, s.chars().next(), false, true);
            self.out.push_str(s);
        }
    }

    /// Writes what content starting with `first` needs before it, and returns the output
    /// to append that content to, inline and with no line break.
    pub(super) fn raw(&mut self, first: char) -> &mut String {
        // markup is no closing punctuation: a boundary before it is a space
        if self.space == Space::Boundary {
            self.space = Space::Word;
        }
        let mut buf = [0; 4];
        self.begin(first.encode_utf8(&mut buf), Some(first), false, true);
        &mut self.out
    }

    /// The output, for a handler that rewrites what it just wrote; nothing is pending.
    pub(super) fn out_mut(&mut self) -> &mut String {
        &mut self.out
    }

    pub(super) fn len(&self) -> usize {
        self.out.len()
    }

    /// Escapes a `!` that ends the output, before a `[` that would make it an image.
    pub(super) fn escape_bang(&mut self) {
        let len = self.out.len();
        if len > self.floor && self.out.ends_with('!') {
            self.edit(len - 1, "!", "\\!");
        }
    }

    /// Declares that the next content starts a block of kind `start`, which asks
    /// `before` of the block above it.
    pub(super) fn open_block(&mut self, start: Start, before: Sep) {
        self.blocks += 1;
        let top = self.top();
        top.end_text();
        let before = top.next.map_or(before, |(_, b)| b.max(before));
        top.next = Some((start, before));
    }

    /// Ends the block opened by `open_block`: it ended as `end` and asks `after` of
    /// the next block. A block that wrote nothing still separates the blocks around it.
    pub(super) fn close_block(&mut self, end: End, after: Sep) {
        self.blocks += 1;
        let top = self.top();
        if let Some((_, before)) = top.next {
            top.next = Some((Start::Text, before.max(after)));
            return;
        }
        top.flow = match top.flow {
            Flow::Fresh => Flow::Fresh,
            Flow::Text => Flow::After(end, after),
            Flow::After(e, s) => Flow::After(e, s.max(after)),
        };
        if end == End::Para {
            top.held_para = true;
        }
    }

    /// Writes `text`, lines of a block that starts as `start` and ends as `end`;
    /// `before` and `after` are the separators it asks of its neighbors.
    pub(super) fn block(&mut self, text: &str, start: Start, before: Sep, end: End, after: Sep) {
        let text = text.trim_matches('\n').trim_end();
        if text.is_empty() {
            return;
        }
        self.open_block(start, before);
        let mut lines = text.split('\n');
        let first = lines.next().unwrap_or_default();
        self.begin(first, first.chars().next(), false, true);
        self.out.push_str(first);
        if text.contains('\n') {
            self.mark_blocky();
        }
        for line in lines {
            self.newline();
            self.push_line(line);
        }
        self.close_block(end, after);
    }

    /// Opens an emphasis run delimited by `delim`; its opener waits for the first content.
    pub(super) fn push_emph(&mut self, delim: &'static str) -> Mark {
        let mark = self.mark();
        // a block before this element in link text left its space written
        if self.mode == Mode::Inline && self.space == Space::Join {
            self.space = Space::Word;
        }
        let span = self.span(delim);
        self.spans.push(span);
        self.owed += 1;
        mark
    }

    /// The open span at the top, for `span_blocky`.
    pub(super) fn top_span(&self) -> usize {
        self.spans.len() - 1
    }

    /// Whether the open span `i` holds a block.
    pub(super) fn span_blocky(&self, i: usize) -> bool {
        self.spans[i].blocky
    }

    /// Closes the run; returns whether it held a block.
    pub(super) fn pop_emph(&mut self) -> bool {
        let span = self.spans.pop().expect("an emphasis span");
        match span.open {
            None => self.owed -= 1,
            Some((pos, html)) => {
                let at = self.edge_start(span.start);
                self.closers.push(Closer {
                    delim: span.delim,
                    pos,
                    html,
                    start: span.start,
                    at,
                });
            }
        }
        span.blocky
    }

    /// Opens link text after writing `[`; `blank_edge` if a block at either edge of the
    /// text is set off by a blank line, not a space.
    pub(super) fn push_label(&mut self, blank_edge: bool) -> Mark {
        let mark = self.mark();
        self.raw('[');
        self.escape_bang();
        self.out.push('[');
        let pos = self.out.len() - 1;
        let mut span = self.span("");
        span.label = self.label.replace(Label {
            pos,
            line_start: self.content_start,
            mode: self.mode,
            fresh: true,
            joined: false,
            blank_edge,
        });
        self.spans.push(span);
        self.enter_mode(Mode::Inline);
        self.keep_lead = true;
        self.keep_edges = true;
        mark
    }

    /// Closes link text with `]`; returns `None` if it wrote nothing (and is rolled
    /// back), else whether it ended with a block.
    pub(super) fn pop_label(&mut self, mark: Mark) -> Option<bool> {
        let span = *self.spans.last().expect("a label span");
        let label = self.label.expect("a label");
        if self.out.len() == span.start && self.hard == 0 {
            let space = self.space > Space::None;
            self.rollback(mark);
            if space {
                self.word_space();
            }
            return None;
        }
        let next = if self.space == Space::Word {
            Next::End
        } else {
            Next::Char(']')
        };
        self.settle_all(next);
        let block_end = label.joined && self.hard == 0;
        self.push_hard_breaks();
        if self.space == Space::Word && !self.out.ends_with(' ') {
            self.out.push(' ');
        }
        self.space = Space::None;
        self.out.push(']');
        self.leave_span();
        self.label = span.label;
        self.commit(mark);
        Some(block_end)
    }

    /// Writes nothing; the next content runs in the mode `opts` calls for until
    /// `pop_scope`. `keep_edges` keeps a space or break at either end.
    pub(super) fn push_scope(&mut self, opts: FormatOpts, keep_edges: bool) {
        let span = self.span("");
        self.spans.push(span);
        self.enter_mode(Mode::of(opts));
        self.keep_lead = keep_edges;
        self.keep_edges = keep_edges;
    }

    pub(super) fn pop_scope(&mut self) {
        self.settle_all(Next::End);
        if self.keep_edges {
            self.push_hard_breaks();
            let at_start = self.out.len() == self.floor;
            if self.space == Space::Word && (at_start || !self.out.ends_with(' ')) {
                self.out.push(' ');
            }
        }
        self.space = Space::None;
        self.hard = 0;
        self.leave_span();
    }

    fn span(&self, delim: &'static str) -> Span {
        Span {
            delim,
            open: None,
            start: self.out.len(),
            blocks: self.blocks,
            blocky: false,
            mode: self.mode,
            line_escape: self.line_escape,
            content_start: self.content_start,
            floor: self.floor,
            keep_lead: self.keep_lead,
            keep_edges: self.keep_edges,
            label: None,
        }
    }

    fn enter_mode(&mut self, mode: Mode) {
        let len = self.out.len();
        if let Some(s) = self.spans.last_mut() {
            s.open = Some((len, false));
            s.start = len;
        }
        self.mode = mode;
        self.floor = len;
        self.content_start = len;
        self.line_escape = matches!(mode, Mode::Block | Mode::Cell);
    }

    fn leave_span(&mut self) {
        let span = self.spans.pop().expect("a span");
        self.mode = span.mode;
        self.line_escape = span.line_escape;
        self.content_start = span.content_start;
        self.floor = span.floor;
        self.keep_lead = span.keep_lead;
        self.keep_edges = span.keep_edges;
    }

    /// Where trailing edge spaces from `start` begin; a closer goes before them.
    fn edge_start(&self, start: usize) -> usize {
        let tail = &self.out[start.min(self.out.len())..];
        self.out.len() - (tail.len() - tail.trim_end_matches(is_edge_space).len())
    }

    /// Takes a snapshot that `rollback` restores; `commit` drops it.
    pub(super) fn mark(&mut self) -> Mark {
        let from = self
            .frames
            .iter()
            .position(|f| !f.started)
            .unwrap_or(self.frames.len())
            .saturating_sub(1);
        let mark = Mark {
            len: self.out.len(),
            from,
            frames_at: self.undo_frames.len(),
            spans_at: self.undo_spans.len(),
            closers_at: self.undo_closers.len(),
            edits_at: self.edits.len(),
            line_open: self.line_open,
            line_begin: self.line_begin,
            content_start: self.content_start,
            line_escape: self.line_escape,
            space: self.space,
            hard: self.hard,
            owed: self.owed,
            label: self.label,
            mode: self.mode,
            floor: self.floor,
            keep_lead: self.keep_lead,
            keep_edges: self.keep_edges,
        };
        self.undo_frames.extend_from_slice(&self.frames[from..]);
        self.undo_spans.extend_from_slice(&self.spans);
        self.undo_closers.extend_from_slice(&self.closers);
        self.live += 1;
        mark
    }

    pub(super) fn commit(&mut self, m: Mark) {
        self.undo_frames.truncate(m.frames_at);
        self.undo_spans.truncate(m.spans_at);
        self.undo_closers.truncate(m.closers_at);
        self.live -= 1;
        if self.live == 0 {
            self.edits.truncate(self.edits_base);
        }
    }

    pub(super) fn rollback(&mut self, m: Mark) {
        while self.edits.len() > m.edits_at {
            let e = self.edits.pop().expect("an edit");
            self.out.replace_range(e.pos..e.pos + e.new_len, e.old);
        }
        self.out.truncate(m.len);
        self.frames.truncate(m.from);
        self.frames
            .extend_from_slice(&self.undo_frames[m.frames_at..]);
        self.spans.clear();
        self.spans.extend_from_slice(&self.undo_spans[m.spans_at..]);
        self.closers.clear();
        self.closers
            .extend_from_slice(&self.undo_closers[m.closers_at..]);
        self.line_open = m.line_open;
        self.line_begin = m.line_begin;
        self.content_start = m.content_start;
        self.line_escape = m.line_escape;
        self.space = m.space;
        self.hard = m.hard;
        self.owed = m.owed;
        self.label = m.label;
        self.mode = m.mode;
        self.floor = m.floor;
        self.keep_lead = m.keep_lead;
        self.keep_edges = m.keep_edges;
        self.commit(m);
    }

    /// Rolls back to `m` while marks taken after it (`inner`) and containers opened after
    /// it are still open; those are dropped.
    pub(super) fn rollback_nested<'m>(&mut self, m: Mark, inner: impl Iterator<Item = &'m Mark>) {
        let mut n = 0;
        // the earliest inner mark's copies start where `m`'s end
        if let Some(first) = inner.inspect(|_| n += 1).min_by_key(|i| i.spans_at) {
            self.undo_frames.truncate(first.frames_at);
            self.undo_spans.truncate(first.spans_at);
            self.undo_closers.truncate(first.closers_at);
        }
        self.live -= n;
        // the first frame opened after the mark starts where the mark's prefix ended
        let frames = m.from + self.undo_frames.len() - m.frames_at;
        if let Some(f) = self.frames.get(frames) {
            self.prefix.truncate(f.base);
        }
        self.rollback(m);
    }

    /// Replaces `old` at `pos` with `new`, moving the positions after it.
    fn edit(&mut self, pos: usize, old: &'static str, new: &str) {
        self.out.replace_range(pos..pos + old.len(), new);
        if self.live > 0 {
            self.edits.push(Edit {
                pos,
                old,
                new_len: new.len(),
            });
        }
        let delta = new.len() - old.len();
        let shift = |p: &mut usize| {
            if *p > pos {
                *p += delta;
            }
        };
        for c in &mut self.closers {
            if c.at >= pos {
                c.at += delta;
            }
            shift(&mut c.pos);
            shift(&mut c.start);
        }
        for s in &mut self.spans {
            if let Some((p, _)) = &mut s.open {
                shift(p);
            }
            shift(&mut s.start);
            shift(&mut s.content_start);
            shift(&mut s.floor);
        }
        if let Some(l) = &mut self.label {
            shift(&mut l.pos);
            shift(&mut l.line_start);
        }
        shift(&mut self.content_start);
        shift(&mut self.line_begin);
        shift(&mut self.floor);
    }

    pub(super) fn push_quote(&mut self) {
        self.blocks += 1;
        let base = self.prefix.len();
        let outer_next = self.open_frame();
        let mut frame = Frame::new(Kind::Quote, base);
        frame.outer_next = outer_next;
        self.frames.push(frame);
        self.prefix.push_str("> ");
    }

    pub(super) fn pop_quote(&mut self) {
        self.close_frame(End::Quote);
    }

    /// The member at the top: whether it started, whether its first line held a deeper
    /// lead, and its lead's length.
    pub(super) fn top_member(&self) -> Option<(bool, bool, usize)> {
        let f = self.frames.last()?;
        (f.kind == Kind::Member).then_some((f.started, f.lead_first, f.lead_len))
    }

    /// Whether a lead waits for content, so a line written now would hold it.
    pub(super) fn lead_pending(&self) -> bool {
        self.frames.iter().any(|f| f.lead.is_some())
    }

    pub(super) fn push_group(&mut self, rules: &'static Rules) {
        self.blocks += 1;
        let base = self.prefix.len();
        let outer_next = self.open_frame();
        let mut frame = Frame::new(Kind::Group, base);
        frame.outer_next = outer_next;
        frame.rules = Some(rules);
        self.frames.push(frame);
    }

    /// Closes a group; returns whether it wrote anything.
    pub(super) fn pop_group(&mut self) -> bool {
        let g = *self.frames.last().expect("a group frame");
        let own = match g.flow {
            Flow::After(e, _) => e,
            _ => End::Text,
        };
        let end = g.rules.map_or(own, |r| (r.group_end)(g.last_member, own));
        self.close_frame(end)
    }

    /// Opens a member whose `lead` is written `indent` columns past the group's column
    /// with its first content, or alone if it has none; with content it starts as `start`.
    pub(super) fn push_member(&mut self, lead: &str, indent: usize, start: Start) {
        self.blocks += 1;
        let base = self.prefix.len();
        let mut frame = Frame::new(Kind::Member, base);
        frame.rules = self.frames.last().and_then(|f| f.rules);
        frame.lead = Some(Lead::new(lead));
        frame.lead_pos = base + indent;
        frame.lead_len = lead.len();
        frame.lead_start = start;
        self.frames.push(frame);
        self.prefix
            .extend(std::iter::repeat_n(' ', indent + lead.len()));
    }

    pub(super) fn pop_member(&mut self) {
        self.blocks += 1;
        let m = *self.frames.last().expect("a member frame");
        self.hard = 0;
        let end = if m.started {
            match m.flow {
                Flow::Text => End::Text,
                Flow::After(e, _) => e,
                Flow::Fresh => End::Closed,
            }
        } else {
            // an empty member writes its bare lead and takes no lazy line
            self.top().next = Some((Start::Weak, Sep::None));
            self.begin("", None, true, true);
            End::Closed
        };
        self.frames.pop();
        self.prefix.truncate(m.base);
        let g = self.top();
        g.last_member = Some(end);
        g.flow = Flow::After(end, Sep::None);
        g.held_para = m.held_para;
    }

    fn top(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("the root frame")
    }

    fn open_frame(&mut self) -> Option<(Start, Sep)> {
        let top = self.top();
        top.end_text();
        top.next.take()
    }

    fn close_frame(&mut self, end: End) -> bool {
        self.blocks += 1;
        let frame = self.frames.pop().expect("a frame");
        self.prefix.truncate(frame.base);
        let parent = self.top();
        if frame.started {
            parent.flow = Flow::After(end, Sep::Blank);
        } else if matches!(parent.flow, Flow::After(..)) {
            // a container that wrote nothing still separates the content around it
            parent.next = Some((
                frame.outer_next.map_or(Start::Text, |(s, _)| s),
                outer_sep(frame.outer_next, Sep::Blank),
            ));
        } else {
            parent.next = frame.outer_next;
        }
        frame.started
    }

    #[inline]
    fn mark_blocky(&mut self) {
        if self.mode == Mode::Block && !self.spans.is_empty() {
            let blocks = self.blocks;
            for s in &mut self.spans {
                if blocks > s.blocks {
                    s.blocky = true;
                }
            }
        }
    }

    /// Writes what content arriving now needs before it: closers, the separator from
    /// the block above, hard breaks or a space, the line prefix, leads and openers.
    /// `head` is the content's first line, `next` its first char; `bare` writes an empty
    /// member's lead; `open` writes the owed emphasis openers.
    #[inline]
    fn begin(&mut self, head: &str, next: Option<char>, bare: bool, open: bool) {
        let len = self.frames.len();
        let top = &self.frames[len - 1];
        if self.line_open
            && top.started
            && matches!(top.flow, Flow::Text)
            && top.next.is_none()
            && self.space == Space::None
            && self.hard == 0
            && self.closers.is_empty()
            && (self.owed == 0 || !open)
        {
            if let Some(l) = &mut self.label {
                l.fresh = false;
                l.joined = false;
            }
            return;
        }
        self.begin_slow(head, next, bare, open);
    }

    fn begin_slow(&mut self, head: &str, next: Option<char>, bare: bool, open: bool) {
        let len = self.frames.len();
        // started frames are a prefix of the stack
        let u = if self.frames[len - 1].started {
            len
        } else {
            self.frames.iter().position(|f| !f.started).unwrap_or(len)
        };
        let inline = !self.spans.is_empty() || !self.closers.is_empty();
        if let Some(l) = self.label {
            if open && l.fresh && l.joined && self.hard == 0 {
                self.label_block_start(l);
            }
        }
        let gap = if u == 0 {
            None
        } else {
            self.resolve(u, head, bare)
        };
        if inline && (gap.is_some() || u < len || self.frames[len - 1].next.is_some()) {
            self.mark_blocky();
        }

        let mut space = std::mem::replace(&mut self.space, Space::None);
        let hard = std::mem::take(&mut self.hard);
        let (first_owed, open, merged) = if inline {
            self.settle_runs(gap.is_some(), hard, space, next, head, open)
        } else {
            (0, false, false)
        };

        if gap.is_some() && matches!(self.mode, Mode::Inline | Mode::Code) {
            space = space.max(Space::Join);
        } else if let Some(gap) = gap {
            let p_end = self.frames.get(u).map_or(self.prefix.len(), |f| f.base);
            // a line break after a hard break keeps it (a row outside its table); a
            // block or member start drops it
            let direct = u == len && self.frames[len - 1].kind != Kind::Group;
            if gap == Gap::Line && hard > 0 && direct && self.line_open && self.mode != Mode::Inline
            {
                self.trim_spaces();
                self.out.push_str("  ");
            }
            self.write_gap(gap, p_end);
            space = Space::None;
        } else if hard > 0 && (self.line_open || self.keep_lead && self.out.len() == self.floor) {
            self.hard = hard;
            self.push_hard_breaks();
        }

        if self.line_open {
            let ws = self.out.ends_with(|c: char| c.is_ascii_whitespace())
                || self.mode == Mode::Cell && ends_with_cell_break(&self.out);
            // at a scope's start only a kept leading space is written
            let wanted = if self.out.len() == self.floor && !self.spans.is_empty() {
                self.keep_lead && space == Space::Word
            } else {
                space_wanted(space, next) && !ws
            };
            if wanted {
                self.out.push(' ');
            }
        } else {
            self.open_line(u, head, bare);
        }

        if open {
            self.write_openers(first_owed, merged, head);
        }
        if let Some(l) = &mut self.label {
            if open || bare || !head.is_empty() {
                l.fresh = false;
                l.joined = false;
            }
        }

        // the frames from `u` start with this content
        if u > 0 && u <= len {
            let p = u - 1;
            let pf = &mut self.frames[p];
            pf.next = None;
            if u == len {
                if let (Kind::Group, Some(r)) = (pf.kind, pf.rules) {
                    if let Some(e) = pf.last_member.take() {
                        pf.loose_lazy = Some((r.continues)(e, pf.held_para));
                    }
                }
                pf.flow = Flow::Text;
            } else if pf.kind == Kind::Group {
                pf.last_member = None;
            }
        }
        let mut deeper_lead = false;
        for i in (u..len).rev() {
            let f = &mut self.frames[i];
            let lead_first = f.kind == Kind::Member && deeper_lead;
            deeper_lead |= f.lead.is_some();
            f.started = true;
            f.flow = Flow::Text;
            f.lead = None;
            f.lead_first |= lead_first;
            if f.kind == Kind::Group {
                f.loose_lazy = None;
            }
        }
        self.top().next = None;
    }

    /// Settles the pending closers before the content, and ends the runs open at a
    /// blank line; returns the first owed run, whether to open the owed runs, and
    /// whether the first merged with the run before it.
    fn settle_runs(
        &mut self,
        gap: bool,
        hard: u8,
        space: Space,
        next: Option<char>,
        head: &str,
        open: bool,
    ) -> (usize, bool, bool) {
        let mut first_owed = self.spans.len() - self.owed;
        let mut merged = false;
        if !self.closers.is_empty() {
            let owed = open && self.owed > 0;
            let following = if gap || hard > 0 || self.line_open && space_wanted(space, next) {
                Next::End
            } else if owed {
                Next::Opener(self.spans[first_owed].delim)
            } else {
                next.map_or(Next::End, Next::Char)
            };
            while self.closers.len() > 1 {
                let c = self.closers.remove(0);
                self.settle(c, following);
            }
            let c = self.closers[0];
            self.closers.clear();
            if owed
                && !gap
                && hard == 0
                && (space == Space::None || space_wanted(space, next))
                && self.can_merge(c, first_owed, head, space != Space::None)
            {
                let s = &mut self.spans[first_owed];
                s.open = Some((c.pos, false));
                s.start = c.start;
                merged = true;
            } else {
                self.settle(c, following);
            }
        }
        // a blank line ends the runs open in it; each paragraph gets its own
        if gap && self.mode == Mode::Block {
            for i in (0..self.spans.len()).rev() {
                let s = self.spans[i];
                if let (Some((pos, html)), false) = (s.open, s.delim.is_empty()) {
                    let at = self.edge_start(s.start);
                    let start = s.start;
                    let delim = s.delim;
                    let c = Closer {
                        delim,
                        pos,
                        html,
                        start,
                        at,
                    };
                    self.settle(c, Next::End);
                    self.spans[i].open = None;
                    self.owed += 1;
                }
            }
            first_owed = self.spans.len() - self.owed;
        }
        (first_owed, open && self.owed > 0, merged)
    }

    /// Link text that starts with a block is set off from the text before it.
    fn label_block_start(&mut self, l: Label) {
        let pos = l.pos;
        let before = &self.out[..pos];
        if pos <= l.line_start {
            return;
        }
        if !l.blank_edge {
            if !before.ends_with(|c: char| c.is_ascii_whitespace()) {
                self.edit(pos, "", " ");
            }
        } else if l.mode == Mode::Cell {
            if !ends_with_cell_break(before) {
                self.edit(pos, "", "<br>");
            }
        } else {
            let mut sep = String::from("\n");
            push_blank_prefix(&mut sep, &self.prefix);
            sep.push('\n');
            sep.push_str(&self.prefix);
            // a space before the break would be left at the line end
            if before.ends_with(' ') {
                self.edit(pos - 1, " ", &sep);
            } else {
                self.edit(pos, "", &sep);
            }
        }
    }

    /// Whether the owed run at `i` merges with the pending closer `c` of the run before.
    fn can_merge(&self, c: Closer, i: usize, head: &str, spaced: bool) -> bool {
        let s = &self.spans[i];
        if c.delim != s.delim || c.html || c.at != self.out.len() {
            return false;
        }
        let prefix = &self.out[c.start.min(self.out.len())..];
        // `*b **c***` ends with the italic's `*`, so a bold after a space stays apart
        if (s.delim == "*" || spaced && s.delim == "**") && prefix.ends_with('*') {
            return false;
        }
        if prefix.bytes().rev().take_while(|&b| b == b'\\').count() % 2 != 0 {
            return false;
        }
        let first = self.first_after(i, head);
        let before = self.before_char(i, delimiter(s.delim));
        opens(first, before)
    }

    /// The first char after the opener of owed span `i`, past inner openers' copies of
    /// its delimiter; a delimiter char in the text counts, escaped or not it is punctuation.
    fn first_after(&self, i: usize, head: &str) -> Option<char> {
        let d = delimiter(self.spans[i].delim);
        for s in &self.spans[i + 1..] {
            if let Some(c) = opener(s.delim, s.html()).chars().find(|&c| c != d) {
                return Some(c);
            }
        }
        head.chars().next()
    }

    /// The char before an opener of span `i`, past its own delimiter chars.
    fn before_char(&self, i: usize, d: char) -> Option<char> {
        let lower = if i > 0 {
            self.spans[i - 1].start.max(self.content_start)
        } else {
            self.content_start
        };
        let lower = lower.min(self.out.len());
        self.out[lower..].trim_end_matches(d).chars().next_back()
    }

    fn write_openers(&mut self, first: usize, merged: bool, head: &str) {
        // inner openers: the text before them is their enclosing run's, which is empty
        for i in (first + usize::from(merged)..self.spans.len()).rev() {
            if i > first {
                let f = self.first_after(i, head);
                self.spans[i].open = Some((0, !opens(f, None)));
            }
        }
        let mut wrote = false;
        for i in first..self.spans.len() {
            if i == first && merged {
                continue;
            }
            let s = self.spans[i];
            let html = if i == first {
                let f = self.first_after(i, head);
                !opens(f, self.before_char(i, delimiter(s.delim)))
            } else {
                s.html()
            };
            let pos = self.out.len();
            self.out.push_str(opener(s.delim, html));
            let span = &mut self.spans[i];
            span.open = Some((pos, html));
            span.start = self.out.len();
            wrote = true;
        }
        self.owed = 0;
        if wrote {
            // a run's text is escaped as at a line start
            self.content_start = self.out.len();
            self.line_escape = matches!(self.mode, Mode::Block | Mode::Cell);
        }
    }

    fn settle_all(&mut self, next: Next) {
        while !self.closers.is_empty() {
            let c = self.closers.remove(0);
            self.settle(c, next);
        }
    }

    /// Writes the closer `c`, followed by `next`: its delimiter where it can close the
    /// run, else the run's html tags.
    fn settle(&mut self, c: Closer, next: Next) {
        let closes = c.html || {
            let d = delimiter(c.delim);
            let start = c.start.min(c.at);
            match self.out[start..c.at]
                .trim_end_matches(d)
                .chars()
                .next_back()
            {
                None => true,
                Some(l) if l.is_whitespace() => false,
                Some(l) => {
                    l.is_alphanumeric()
                        || c.at < self.out.len()
                        || match next {
                            Next::End => true,
                            Next::Opener(m) => m == c.delim,
                            Next::Char(ch) => is_flanking_safe(ch),
                        }
                }
            }
        };
        let close = closer(c.delim, !closes || c.html);
        let mut at = c.at;
        if !closes {
            let tag = opener(c.delim, true);
            self.edit(c.pos, c.delim, tag);
            at += tag.len() - c.delim.len();
        }
        if at == self.out.len() {
            self.out.push_str(close);
            for o in &mut self.closers {
                if o.at == at {
                    o.at += close.len();
                }
            }
        } else {
            self.edit(at, "", close);
        }
    }

    /// The separator the last started frame (`u - 1`) asks before the content that
    /// starts the frames from `u`.
    fn resolve(&self, u: usize, head: &str, bare: bool) -> Option<Gap> {
        let len = self.frames.len();
        let (cstart, cbefore) = self.frames[len - 1]
            .next
            .unwrap_or((Start::Text, Sep::None));
        // how the content, or the first frame it opens, starts
        let (start, before) = match self.frames.get(u).map(|f| (f.kind, f.outer_next)) {
            None => (cstart, cbefore),
            Some((Kind::Quote, outer)) => (Start::Quote, outer_sep(outer, Sep::Blank)),
            Some((Kind::Group, outer)) => {
                let start = match self.frames.get(u + 1) {
                    Some(m) if m.kind == Kind::Member => self.lead_start(u + 1, head, bare),
                    _ => Start::Text,
                };
                (start, outer_sep(outer, Sep::Blank))
            }
            Some((Kind::Member | Kind::Root, _)) => {
                if u + 1 == len {
                    (cstart, cbefore)
                } else {
                    (Start::Text, Sep::None)
                }
            }
        };
        let p = &self.frames[u - 1];
        let gap = match (p.kind, p.rules) {
            (Kind::Group, Some(r)) if u < len => match (p.last_member, p.flow) {
                (Some(e), _) => return Some((r.member_gap)(e, start)),
                (None, Flow::After(ended, s)) => {
                    let gap = sep_gap(s.max(before).max(p.next.map_or(Sep::None, |(_, b)| b)));
                    // loose content that ended in text takes the rule for text before a member
                    if ended == End::Text && gap != Some(Gap::Blank) {
                        let start = self.lead_start(u, head, bare);
                        match (r.after_loose)(p.loose_lazy, start, self.hard) {
                            Gap::Blank => Some(Gap::Blank),
                            _ => gap,
                        }
                    } else {
                        gap
                    }
                }
                (None, _) => {
                    let start = self.lead_start(u, head, bare);
                    Some((r.after_loose)(p.loose_lazy, start, self.hard))
                }
            },
            (Kind::Group, Some(r)) => match (p.last_member, p.flow) {
                (Some(e), _) => {
                    let gap = (r.loose_gap)(e, p.held_para);
                    Some(if before == Sep::Blank {
                        Gap::Blank
                    } else {
                        gap
                    })
                }
                (None, Flow::After(_, s)) => sep_gap(s.max(before)),
                _ => None,
            },
            (Kind::Member, Some(r)) => match p.flow {
                Flow::After(e, _) => Some((r.block_gap)(e, start)),
                _ => None,
            },
            _ => match p.flow {
                Flow::After(_, s) => sep_gap(s.max(before)),
                _ => None,
            },
        };
        gap.map(|g| {
            if self.mode == Mode::Cell {
                Gap::Line
            } else {
                g
            }
        })
    }

    /// How member `i` starts: as declared, unless its lead stands alone on its line.
    fn lead_start(&self, i: usize, head: &str, bare: bool) -> Start {
        if self.lead_alone(i, head, bare) {
            Start::Weak
        } else {
            self.frames[i].lead_start
        }
    }

    fn write_gap(&mut self, gap: Gap, p_end: usize) {
        match self.mode {
            Mode::Cell => {
                if self.out.len() > self.floor && !ends_with_cell_break(&self.out) {
                    self.out.push_str("<br>");
                }
                if gap == Gap::Blank {
                    self.out.push_str("<br>");
                }
            }
            Mode::Inline | Mode::Code => {
                if self.out.len() > self.floor && !self.out.ends_with(' ') {
                    self.out.push(' ');
                }
                return;
            }
            Mode::Block => {
                self.mark_blocky();
                if self.line_open {
                    self.out.push('\n');
                }
                let prefix = &self.prefix[..p_end];
                match gap {
                    Gap::Line => {}
                    Gap::Blank => {
                        push_blank_prefix(&mut self.out, prefix);
                        self.out.push('\n');
                    }
                    Gap::QuoteEnd => {
                        self.out.push_str(prefix);
                        self.out.push_str(">\n");
                    }
                }
            }
        }
        self.line_open = false;
    }

    fn trim_spaces(&mut self) {
        let floor = self.floor.min(self.out.len());
        let keep = floor + self.out[floor..].trim_end_matches(' ').len();
        self.out.truncate(keep);
    }

    fn push_hard_breaks(&mut self) {
        if self.hard > 0 && matches!(self.mode, Mode::Block | Mode::Cell) {
            self.trim_spaces();
        }
        for i in 0..std::mem::take(&mut self.hard) {
            match self.mode {
                Mode::Block => {
                    if i > 0 {
                        self.out.push_str(&self.prefix);
                    }
                    self.out.push_str("  \n");
                    self.line_open = false;
                }
                Mode::Cell => self.out.push_str("  <br>"),
                Mode::Inline => self.out.push_str("<br>"),
                Mode::Code => {
                    if !self.out.ends_with(' ') {
                        self.out.push(' ');
                    }
                }
            }
        }
    }

    /// Whether the lead of member `i` stands alone on its line: for an empty member,
    /// before content indented past it, or where the group's rules break the line.
    fn lead_alone(&self, i: usize, head: &str, bare: bool) -> bool {
        let f = &self.frames[i];
        let Some(m) = f.lead else { return false };
        let later = self.frames[i + 1..].iter().filter_map(|f| f.lead);
        if bare && later.clone().next().is_none() {
            return true;
        }
        let rest_start = f.lead_pos + m.len as usize;
        let next_pos = self.frames[i + 1..]
            .iter()
            .find(|f| f.lead.is_some())
            .map_or(self.prefix.len(), |f| f.lead_pos);
        if self.prefix[rest_start..next_pos].starts_with(' ') {
            return true;
        }
        let Some(r) = f.rules else { return false };
        let mut leads = self.frames[i..]
            .iter()
            .filter_map(|f| f.lead.as_ref().map(Lead::as_str));
        (r.breaks_line)(&mut leads, head)
    }

    fn open_line(&mut self, u: usize, head: &str, bare: bool) {
        self.line_begin = self.out.len();
        let mut cursor = 0;
        let mut wrote_lead = false;
        let lb = if self.mode == Mode::Cell {
            "<br>"
        } else {
            "\n"
        };
        for i in u..self.frames.len() {
            let Some(m) = self.frames[i].lead else {
                continue;
            };
            let pos = self.frames[i].lead_pos;
            let alone = self.lead_alone(i, head, bare);
            let is_last = self.frames[i + 1..].iter().all(|f| f.lead.is_none());
            self.out.push_str(&self.prefix[cursor..pos]);
            cursor = pos + m.len as usize;
            if alone {
                self.out.push_str(m.as_str().trim_end());
                if bare && is_last {
                    break;
                }
                self.out.push_str(lb);
                self.out.push_str(&self.prefix[..cursor]);
            } else {
                self.out.push_str(m.as_str());
            }
            wrote_lead = true;
        }
        if bare {
            self.trim_spaces();
        } else if self.mode != Mode::Cell || wrote_lead {
            self.out.push_str(&self.prefix[cursor..]);
        }
        self.line_escape = match self.mode {
            Mode::Block => true,
            Mode::Cell => wrote_lead || self.out.is_empty(),
            Mode::Inline | Mode::Code => false,
        };
        self.content_start = self.out.len();
        self.line_open = true;
    }

    fn newline(&mut self) {
        self.out.push('\n');
        self.line_open = false;
    }

    /// Writes one line of a block; a blank line gets only the prefix's marks (`>`).
    fn push_line(&mut self, line: &str) {
        if !self.line_open {
            self.line_begin = self.out.len();
            if line.is_empty() {
                push_blank_prefix(&mut self.out, &self.prefix);
                self.content_start = self.out.len();
                self.line_open = true;
                return;
            }
            self.out.push_str(&self.prefix);
            self.content_start = self.out.len();
            self.line_open = true;
        }
        self.out.push_str(line);
    }
}

impl Span {
    fn html(&self) -> bool {
        self.open.is_some_and(|(_, h)| h)
    }
}

/// A blank line inside the prefix: its trailing indent dropped, a `>` keeps its space.
fn push_blank_prefix(out: &mut String, prefix: &str) {
    let trimmed = prefix.trim_end();
    out.push_str(trimmed);
    if trimmed.ends_with('>') {
        out.push(' ');
    }
}

fn outer_sep(outer: Option<(Start, Sep)>, sep: Sep) -> Sep {
    outer.map_or(sep, |(_, s)| s.max(sep))
}

fn space_wanted(space: Space, next: Option<char>) -> bool {
    match space {
        Space::None => false,
        Space::Boundary => !next.is_some_and(is_closing_punct),
        Space::Join | Space::Word => true,
    }
}

fn is_closing_punct(c: char) -> bool {
    c.is_whitespace() || matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
}

/// Whether a run opens with `first` after it and `before` before it: punctuation
/// inside the run needs whitespace or punctuation outside it.
fn opens(first: Option<char>, before: Option<char>) -> bool {
    first.is_none_or(|f| {
        !f.is_whitespace() && (f.is_alphanumeric() || before.is_none_or(is_flanking_safe))
    })
}

fn opener(delim: &'static str, html: bool) -> &'static str {
    match (delim, html) {
        (m, false) => m,
        ("**", true) => "<strong>",
        ("~~", true) => "<del>",
        (_, true) => "<em>",
    }
}

fn closer(delim: &'static str, html: bool) -> &'static str {
    match (delim, html) {
        (m, false) => m,
        ("**", true) => "</strong>",
        ("~~", true) => "</del>",
        (_, true) => "</em>",
    }
}

fn delimiter(delim: &str) -> char {
    if delim == "~~" { '~' } else { '*' }
}

/// `CommonMark` whitespace (`Zs`, tab, form feed, carriage return), which stays outside
/// a run; a newline stays with its hard break.
pub(super) fn is_edge_space(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\n' | '\x0b' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// Common non-ASCII punctuation blocks; anything else (a letter, a mark, a format
/// character such as a soft hyphen) is no flanking punctuation.
const PUNCTUATION: &[(char, char)] = &[
    ('\u{a1}', '\u{ac}'),
    ('\u{ae}', '\u{bf}'),
    ('\u{2010}', '\u{2027}'),
    ('\u{2030}', '\u{205e}'),
    ('\u{3001}', '\u{3003}'),
    ('\u{3008}', '\u{3011}'),
    ('\u{ff01}', '\u{ff0f}'),
    ('\u{ff1a}', '\u{ff20}'),
    ('\u{ff3b}', '\u{ff40}'),
    ('\u{ff5b}', '\u{ff65}'),
];

/// Whitespace or punctuation. The spec's Unicode whitespace is `Zs`, tab, LF, FF and CR,
/// so unlike `char::is_whitespace` it leaves out U+000B, U+0085, U+2028 and U+2029.
fn is_flanking_safe(c: char) -> bool {
    (c.is_whitespace() && !matches!(c, '\u{b}' | '\u{85}' | '\u{2028}' | '\u{2029}'))
        || c.is_ascii_punctuation()
        || !c.is_alphanumeric() && PUNCTUATION.iter().any(|&(lo, hi)| (lo..=hi).contains(&c))
}
