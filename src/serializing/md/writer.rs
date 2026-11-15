//! The Markdown output buffer. Separators, line prefixes and leads (a container's text on
//! its first line) are pending state, written only when the next content arrives, so an
//! element that writes nothing leaves nothing behind. Emphasis and link text capture
//! their content in a buffer of their own and decide from it as they close.

use super::constants::MAX_SPARE;
use super::ctx::FormatOpts;
use super::elements::emphasis::{self, is_flanking_safe};
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
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
enum Space {
    #[default]
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

/// The space and hard breaks pending at one edge of a capture.
#[derive(Clone, Copy, Default)]
pub(super) struct Edge {
    space: Space,
    hard: u8,
    joined: bool,
}

impl Edge {
    /// Whether a block edge in link text is in it, and no break.
    pub(super) fn joins(self) -> bool {
        self.joined && self.hard == 0
    }

    pub(super) fn breaks(self) -> u8 {
        self.hard
    }

    /// Writes its breaks as `<br>` tags.
    pub(super) fn push_breaks(self, out: &mut String) {
        for _ in 0..self.hard {
            out.push_str("<br>");
        }
    }

    /// Its space alone, its breaks written elsewhere.
    pub(super) fn space_only(self) -> Self {
        Self { hard: 0, ..self }
    }

    pub(super) fn is_empty(self) -> bool {
        self.space == Space::None && self.hard == 0
    }

    /// Writes it as kept text: a `<br>` per break, then a word space, or a block's space
    /// after a break.
    pub(super) fn push_kept(self, out: &mut String) {
        for _ in 0..self.hard {
            out.push_str("<br>");
        }
        if self.space == Space::Word || self.hard > 0 && self.space == Space::Join {
            out.push(' ');
        }
    }
}

/// What a capture wrote, and what it still owed at its edges.
pub(super) struct Captured {
    pub(super) text: String,
    pub(super) lead: Edge,
    pub(super) trail: Edge,
    /// it holds a block: one written inside, or one it started or ended with
    pub(super) block: bool,
    /// how its first content started a block, and how its last block ended
    pub(super) first: Option<(Start, Sep)>,
    pub(super) last: Option<(End, Sep)>,
    /// a run at its end whose closer was safe only because the capture ended there
    pub(super) doubtful: Option<Box<Doubtful>>,
}

/// A run whose closer was safe only because its capture ended there: written once the
/// character after it is known, with delimiters or as html. Edge spaces end `text`.
pub(super) struct Doubtful {
    delim: &'static str,
    text: String,
}

impl Doubtful {
    /// Writes it after `out`: as html when what follows can't follow its closer.
    pub(super) fn write(self, out: &mut String, html: bool) {
        let body = self.text.trim_end_matches(is_edge_space);
        let (open, close) = emphasis::tags(self.delim, html);
        out.push_str(open);
        out.push_str(body);
        out.push_str(close);
        out.push_str(&self.text[body.len()..]);
    }

    /// `part` followed by it with delimiters, to decide the run around both.
    pub(super) fn after(&self, part: &str) -> String {
        let mut s = String::with_capacity(part.len() + self.len());
        s.push_str(part);
        self.write_delimited(&mut s);
        s
    }

    /// Its length written with delimiters.
    pub(super) fn len(&self) -> usize {
        self.text.len() + 2 * self.delim.len()
    }

    fn write_delimited(&self, out: &mut String) {
        let body = self.text.trim_end_matches(is_edge_space);
        out.push_str(self.delim);
        out.push_str(body);
        out.push_str(self.delim);
        out.push_str(&self.text[body.len()..]);
    }
}

/// The outer rendering while a capture writes into a buffer of its own; `lead` and
/// `first` are the capture's own, set by its first content.
struct Cap {
    out: String,
    prefix: String,
    frames: Vec<Frame>,
    cur: Cur,
    held: Option<Held>,
    blocks: u32,
    lead: Edge,
    first: Option<(Start, Sep)>,
    /// the capture is written back where it opened (emphasis), inside the member there
    in_place: bool,
}

/// A finished emphasis run, written once the character after it is known: the runs
/// merged into it before, the run merged last, and a doubtful run ending that one.
/// Edge spaces at the end of `last` go after its closer.
struct Held {
    delim: &'static str,
    before: String,
    last: String,
    doubtful: Option<Box<Doubtful>>,
}

impl Held {
    /// Writes its doubtful run into `last`, as html if `next` can't follow its closer.
    fn settle_doubtful(&mut self, html: bool) {
        if let Some(d) = self.doubtful.take() {
            d.write(&mut self.last, html);
        }
    }

    /// The last char of its text before trailing `c`s.
    fn last_char(&self, c: char) -> Option<char> {
        let last = self.last.trim_end_matches(c);
        if last.is_empty() {
            self.before.trim_end_matches(c).chars().next_back()
        } else {
            last.chars().next_back()
        }
    }

    /// Whether its run closes, given whether what follows is whitespace or punctuation.
    fn closes(&self, safe_after: bool) -> bool {
        let body = self.last.trim_end_matches(is_edge_space);
        let safe = safe_after || body.len() < self.last.len();
        if !self.before.is_empty() && body.trim_end_matches(['*', '~']).is_empty() {
            let mut whole = self.before.clone();
            whole.push_str(body);
            return emphasis::closes(&whole, self.delim, safe);
        }
        emphasis::closes(body, self.delim, safe)
    }
}

/// The state of the line being written.
#[derive(Clone, Copy)]
struct Cur {
    mode: Mode,
    line_open: bool,
    /// where the line's content starts, past its prefix and leads
    content_start: usize,
    line_escape: bool,
    space: Space,
    hard: u8,
    /// a block edge in link text passed since the last content
    joined: Joined,
}

/// Whether a block edge in link text passed since the last content.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Joined {
    No,
    /// yes, and the pending space is only joins at capture edges, which main drops
    Edge,
    Yes,
}

impl Cur {
    fn new(mode: Mode, line_open: bool) -> Self {
        Self {
            mode,
            line_open,
            content_start: 0,
            line_escape: matches!(mode, Mode::Block | Mode::Cell),
            space: Space::None,
            hard: 0,
            joined: Joined::No,
        }
    }

    /// The pending space is no longer only capture-edge joins.
    fn not_edge_only(&mut self) {
        if self.joined == Joined::Edge {
            self.joined = Joined::Yes;
        }
    }
}

pub(super) struct MdWriter {
    out: String,
    prefix: String,
    frames: Vec<Frame>,
    cur: Cur,
    held: Option<Held>,
    /// block boundaries written so far
    blocks: u32,
    caps: Vec<Cap>,
    /// an included node keeps its trailing space, breaks and separator
    keep_edges: bool,
    spare_frames: Vec<Vec<Frame>>,
    spare_bufs: Vec<String>,
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
            prefix: String::new(),
            frames: vec![root],
            cur: Cur::new(Mode::Block, include_node),
            held: None,
            blocks: 0,
            caps: Vec::new(),
            keep_edges: include_node,
            spare_frames: Vec::new(),
            spare_bufs: Vec::new(),
        }
    }

    /// The written text, with the edges an included node keeps.
    pub(super) fn finish(mut self) -> String {
        self.end_held();
        if self.keep_edges {
            self.push_hard_breaks();
            if self.cur.space == Space::Word && !self.out.ends_with([' ', '\n']) {
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

    /// Writes what follows into a buffer of its own, as a fresh document in the mode
    /// `opts` calls for, until `pop_capture` hands it back.
    pub(super) fn push_capture(&mut self, opts: FormatOpts, in_place: bool) {
        let mut frames = self.spare_frames.pop().unwrap_or_default();
        let mut root = Frame::new(Kind::Root, 0);
        // the content is written back after other text: a block that writes nothing
        // before its first content still asks a separator, handed back as `first`
        root.flow = Flow::Text;
        // written back in place, it separates its blocks as the member there does
        if in_place {
            root.rules = self
                .frames
                .last()
                .filter(|f| matches!(f.kind, Kind::Member | Kind::Root))
                .and_then(|f| f.rules);
        }
        frames.push(root);
        let out = self.spare_bufs.pop().unwrap_or_default();
        let prefix = String::new();
        self.caps.push(Cap {
            out: std::mem::replace(&mut self.out, out),
            prefix: std::mem::replace(&mut self.prefix, prefix),
            frames: std::mem::replace(&mut self.frames, frames),
            cur: std::mem::replace(&mut self.cur, Cur::new(Mode::of(opts), false)),
            held: self.held.take(),
            blocks: self.blocks,
            lead: Edge::default(),
            first: None,
            in_place,
        });
    }

    pub(super) fn pop_capture(&mut self) -> Captured {
        let doubtful = match self.held.take() {
            // emphasis decides it with its own run, once the character after that is known
            Some(h)
                if self.in_place()
                    && h.before.is_empty()
                    && h.doubtful.is_none()
                    && !h.closes(false)
                    && h.closes(true) =>
            {
                Some(Box::new(Doubtful {
                    delim: h.delim,
                    text: h.last,
                }))
            }
            Some(h) => {
                self.flush(h, true);
                None
            }
            None => None,
        };
        let cap = self.caps.pop().expect("an open capture");
        let root = &self.frames[0];
        let last = match (root.flow, root.next) {
            // an empty container after a break leaves the break to end the line
            (Flow::After(e, _), Some((_, sep))) if self.cur.hard == 0 => Some((e, sep)),
            (Flow::After(e, sep), None) if sep != Sep::None => Some((e, sep)),
            _ => None,
        };
        let trail = Edge {
            space: self.cur.space,
            hard: self.cur.hard,
            joined: self.cur.joined != Joined::No,
        };
        let block = self.cur.mode == Mode::Block
            && (self.blocks != cap.blocks || cap.first.is_some() || last.is_some());
        let text = std::mem::replace(&mut self.out, cap.out);
        let mut frames = std::mem::replace(&mut self.frames, cap.frames);
        if self.spare_frames.len() < MAX_SPARE {
            frames.clear();
            self.spare_frames.push(frames);
        }
        let prefix = std::mem::replace(&mut self.prefix, cap.prefix);
        self.recycle(prefix);
        self.cur = cap.cur;
        self.held = cap.held;
        Captured {
            text,
            lead: cap.lead,
            trail,
            block,
            first: cap.first,
            last,
            doubtful,
        }
    }

    /// Takes back a buffer for the next capture.
    pub(super) fn recycle(&mut self, mut buf: String) {
        if self.spare_bufs.len() < MAX_SPARE && buf.capacity() > 0 {
            buf.clear();
            self.spare_bufs.push(buf);
        }
    }

    /// Renders through `f` into `buf` instead of the output, as a fresh document in
    /// the mode `opts` calls for; its trailing space and breaks are dropped.
    pub(super) fn render_into(
        &mut self,
        buf: &mut String,
        opts: FormatOpts,
        f: impl FnOnce(&mut Self),
    ) {
        self.push_capture(opts, false);
        self.frames[0].flow = Flow::Fresh;
        f(self);
        let text = self.pop_capture().text;
        let old = std::mem::replace(buf, text);
        self.recycle(old);
    }

    /// Writes what a capture owed at an edge.
    pub(super) fn owe(&mut self, edge: Edge) {
        for _ in 0..edge.hard {
            self.hard_break(true);
        }
        let edge_only = if edge.space == Space::None {
            self.cur.joined == Joined::Edge
        } else {
            edge.space == Space::Join
                && (self.cur.space == Space::None || self.cur.joined == Joined::Edge)
        };
        self.cur.space = self.cur.space.max(edge.space);
        self.cur.joined = if edge_only {
            Joined::Edge
        } else if edge.joined || self.cur.joined != Joined::No {
            Joined::Yes
        } else {
            Joined::No
        };
    }

    pub(super) fn word_space(&mut self) {
        self.cur.space = self.cur.space.max(Space::Word);
        self.cur.not_edge_only();
    }

    /// A word space, unless nothing is written yet here.
    pub(super) fn space_after_text(&mut self) {
        if !self.out.is_empty() {
            self.word_space();
        }
    }

    /// A boundary that takes no space before closing punctuation: an `<option>`, a
    /// block in code.
    pub(super) fn boundary(&mut self) {
        self.cur.space = self.cur.space.max(Space::Boundary);
        self.cur.not_edge_only();
    }

    /// A block in link text, joined by a space.
    pub(super) fn join(&mut self) {
        self.cur.space = self.cur.space.max(Space::Join);
        self.cur.joined = Joined::Yes;
    }

    /// Before an emphasis element in link text, a block's space already written stays.
    pub(super) fn keep_join(&mut self) {
        if self.cur.mode == Mode::Inline && self.cur.space == Space::Join && !self.out.is_empty() {
            self.cur.space = Space::Word;
        }
    }

    /// A hard line break; `stack` adds one to those pending (`<br>`), otherwise at most
    /// one is pending (a row outside its table).
    pub(super) fn hard_break(&mut self, stack: bool) {
        self.cur.hard = if stack {
            self.cur.hard.saturating_add(1)
        } else {
            self.cur.hard.max(1)
        };
        // a space before a break adds nothing; one after it, in link text, stays
        self.cur.space = Space::None;
        self.cur.not_edge_only();
        // two breaks in captured emphasis end its paragraph, which splits the run
        if self.cur.hard >= 2 && self.cur.mode == Mode::Block && self.in_place() {
            let top = self.top();
            if matches!(top.flow, Flow::Text) && top.next.is_none() {
                top.flow = Flow::After(End::Text, Sep::Blank);
                self.cur.hard = 0;
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
        if let Some(word) = words.next() {
            self.write_word(word, escape);
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
        // escaped text never forms a rule: a line-start `-` and every `*` get a `\`
        let head = if escape && word.starts_with(['-', '*']) {
            "\\"
        } else {
            word
        };
        self.begin(head, word.chars().next(), false);
        let line = self.line_content();
        // only the first word of a text node can continue a Markdown line
        let line_start = self.cur.line_escape && line.trim_matches(' ').is_empty();
        let digits = line.trim_start_matches(' ');
        if escape
            && self.cur.line_escape
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
            self.begin(s, s.chars().next(), false);
            self.out.push_str(s);
        }
    }

    /// Writes what content starting with `first` needs before it, and returns the output
    /// to append that content to, inline and with no line break.
    pub(super) fn raw(&mut self, first: char) -> &mut String {
        let mut buf = [0; 4];
        self.markup(first.encode_utf8(&mut buf), first)
    }

    /// Like `raw`, for markup whose line starts with `head`.
    pub(super) fn markup(&mut self, head: &str, first: char) -> &mut String {
        // markup is no closing punctuation: a boundary before it is a space, except at
        // a capture's start, which hands it back as it is
        if self.cur.space == Space::Boundary && !self.out.is_empty() {
            self.cur.space = Space::Word;
        }
        self.begin(head, Some(first), false);
        &mut self.out
    }

    /// The content of the line written so far, past its prefix and leads.
    /// Drops a closed frame's part of the prefix; the parts below it never change.
    fn truncate_prefix(&mut self, base: usize) {
        debug_assert!(self.prefix.is_char_boundary(base));
        self.prefix.truncate(base);
    }

    pub(super) fn line_content(&self) -> &str {
        // `out` only grows while a line is open; a new line resets `content_start`
        debug_assert!(self.out.is_char_boundary(self.cur.content_start));
        &self.out[self.cur.content_start..]
    }

    /// Joins a run of `delim` around `body` to the held run before it, as one run, if it
    /// `opens` after the char before it there; `own` and `doubtful` are `body` in pieces,
    /// and a merge takes `doubtful`.
    pub(super) fn merge_held(
        &mut self,
        delim: &'static str,
        body: &str,
        own: &str,
        doubtful: &mut Option<Box<Doubtful>>,
        opens: impl Fn(Option<char>) -> bool,
    ) -> bool {
        let Some(h) = &mut self.held else {
            return false;
        };
        let spaced = match self.cur.space {
            Space::Join => return false,
            s => space_wanted(s, body.chars().next()),
        };
        h.settle_doubtful(!spaced && !body.chars().next().is_some_and(is_flanking_safe));
        let d = if delim == "~~" { '~' } else { '*' };
        let last = h.last_char(d);
        let before = if spaced { Some(' ') } else { last };
        // `*b **c***` ends with the italic's `*`, so a bold after a space stays apart
        if h.delim != delim
            || self.cur.hard > 0
            || h.last.ends_with(is_edge_space)
            || (delim == "*" || spaced && delim == "**") && h.last.ends_with('*')
            || !opens(before)
            || self.cur.joined == Joined::Edge && !opens(last)
        {
            return false;
        }
        if !spaced && body.starts_with('[') && h.last.ends_with('!') {
            // a `!` before link text would make it an image
            h.last.pop();
            h.last.push_str("\\!");
        }
        h.before.push_str(&h.last);
        h.last.clear();
        if spaced {
            h.last.push(' ');
        }
        h.last.push_str(own);
        h.doubtful = doubtful.take();
        self.cur.space = Space::None;
        self.cur.not_edge_only();
        true
    }

    /// Holds a run of `delim` around `text` until the next content, which decides
    /// whether its closer can close; written after `markup`.
    pub(super) fn hold(&mut self, delim: &'static str, own: &str, doubtful: Option<Box<Doubtful>>) {
        let mut last = self.spare_bufs.pop().unwrap_or_default();
        last.push_str(own);
        self.hold_buf(delim, last);
        if let Some(h) = &mut self.held {
            h.doubtful = doubtful;
        }
    }

    /// `hold` for a run whose text is all of `last`.
    pub(super) fn hold_buf(&mut self, delim: &'static str, last: String) {
        self.held = Some(Held {
            delim,
            before: String::new(),
            last,
            doubtful: None,
        });
    }

    /// Writes a run of `delim` around `text` as html tags (`html`) or delimiters;
    /// edge spaces at its end go after it.
    pub(super) fn push_run(&mut self, delim: &'static str, text: &str, html: bool) {
        self.push_pieces(delim, "", text, html);
    }

    /// `push_run` for a run whose text is `before` followed by `last`.
    fn push_pieces(&mut self, delim: &'static str, before: &str, last: &str, html: bool) {
        let body = last.trim_end_matches(is_edge_space);
        let (open, close) = emphasis::tags(delim, html);
        self.out.push_str(open);
        if !before.is_empty() {
            self.push_lines(before);
        }
        self.push_lines(body);
        self.out.push_str(close);
        self.out.push_str(&last[body.len()..]);
    }

    /// Writes the held run; `safe_after` if whitespace or punctuation follows it.
    fn flush(&mut self, mut h: Held, safe_after: bool) {
        // a doubtful run ends it, so this run's closer follows that one's
        h.settle_doubtful(false);
        if h.closes(safe_after) {
            self.push_pieces(h.delim, &h.before, &h.last, false);
        } else if !h.before.is_empty() {
            // the run merged last can't close: the runs before it close on their own
            self.push_run(h.delim, &h.before, false);
            let rest = h.last.strip_prefix(' ').map_or(h.last.as_str(), |r| {
                self.out.push(' ');
                r
            });
            self.push_run(h.delim, rest, true);
        } else {
            self.push_run(h.delim, &h.last, true);
        }
        self.recycle(h.last);
        self.recycle(h.before);
    }

    #[inline]
    fn end_held(&mut self) {
        if let Some(h) = self.held.take() {
            self.flush(h, true);
        }
    }

    /// Writes the held run before a run of `delim` starts: where their delimiters differ
    /// but share a char, they would run together, so its closer is safe only apart.
    pub(super) fn settle_held(&mut self, delim: &'static str) {
        if let Some(h) = self.held.take() {
            let apart = self.cur.space != Space::None || self.cur.hard > 0;
            let clash = h.delim != delim && h.delim.as_bytes()[0] == delim.as_bytes()[0];
            self.flush(h, apart || !clash);
        }
    }

    fn in_place(&self) -> bool {
        self.caps.last().is_some_and(|c| c.in_place)
    }

    /// Whether lines are joined by `<br>` (link text, headings), so breaks at a run's
    /// edge stay inside it.
    pub(super) fn inline_mode(&self) -> bool {
        self.cur.mode == Mode::Inline
    }

    /// Writes inline text whose lines are joined by hard breaks.
    fn push_lines(&mut self, text: &str) {
        let Some((first, rest)) = text.split_once('\n') else {
            self.out.push_str(text);
            return;
        };
        self.out.push_str(first);
        for line in rest.split('\n') {
            self.newline();
            self.push_line(line);
        }
    }

    /// Declares that the next content starts a block of kind `start`, which asks
    /// `before` of the block above it.
    pub(super) fn open_block(&mut self, start: Start, before: Sep) {
        self.end_held();
        let top = self.top();
        top.end_text();
        let before = top.next.map_or(before, |(_, b)| b.max(before));
        top.next = Some((start, before));
    }

    /// Ends the block opened by `open_block`: it ended as `end` and asks `after` of
    /// the next block. A block that wrote nothing still separates the blocks around it.
    pub(super) fn close_block(&mut self, end: End, after: Sep) {
        self.end_held();
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
        let text = text
            .trim_matches('\n')
            .trim_end_matches(|c: char| c.is_ascii_whitespace());
        if text.is_empty() {
            return;
        }
        self.open_block(start, before);
        self.blocks += 1;
        let mut lines = text.split('\n');
        let first = lines.next().unwrap_or_default();
        self.begin(first, first.chars().next(), false);
        self.out.push_str(first);
        for line in lines {
            self.newline();
            self.push_line(line);
        }
        self.close_block(end, after);
    }

    pub(super) fn push_quote(&mut self) {
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
        let mut f = self.frames.last()?;
        // captured emphasis is written back where it opened, inside the member there
        if self.frames.len() == 1 && self.cur.mode == Mode::Block {
            for c in self.caps.iter().rev().take_while(|c| c.in_place) {
                f = c.frames.last()?;
                if c.frames.len() > 1 || c.cur.mode != Mode::Block {
                    break;
                }
            }
        }
        (f.kind == Kind::Member).then_some((f.started, f.lead_first, f.lead_len))
    }

    /// Whether a lead waits for content, so a line written now would hold it.
    pub(super) fn lead_pending(&self) -> bool {
        self.frames.iter().any(|f| f.lead.is_some())
    }

    pub(super) fn push_group(&mut self, rules: &'static Rules) {
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
        self.end_held();
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
        self.end_held();
        let m = *self.frames.last().expect("a member frame");
        self.cur.hard = 0;
        let end = if m.started {
            match m.flow {
                Flow::Text => End::Text,
                Flow::After(e, _) => e,
                Flow::Fresh => End::Closed,
            }
        } else {
            // an empty member writes its bare lead and takes no lazy line
            self.top().next = Some((Start::Weak, Sep::None));
            self.begin("", None, true);
            End::Closed
        };
        self.frames.pop();
        self.truncate_prefix(m.base);
        let g = self.top();
        g.last_member = Some(end);
        g.flow = Flow::After(end, Sep::None);
        g.held_para = m.held_para;
    }

    fn top(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("the root frame")
    }

    fn open_frame(&mut self) -> Option<(Start, Sep)> {
        self.end_held();
        let top = self.top();
        top.end_text();
        top.next.take()
    }

    fn close_frame(&mut self, end: End) -> bool {
        self.end_held();
        let frame = self.frames.pop().expect("a frame");
        self.truncate_prefix(frame.base);
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

    /// Writes what content arriving now needs before it: a held run, the separator from
    /// the block above, hard breaks or a space, the line prefix and leads. `head` is the
    /// content's first line, `next` its first char; `bare` writes an empty member's lead.
    #[inline]
    fn begin(&mut self, head: &str, next: Option<char>, bare: bool) {
        let len = self.frames.len();
        let top = &mut self.frames[len - 1];
        if matches!(top.flow, Flow::Text) && top.next.is_none() && self.cur.hard == 0 {
            if self.cur.line_open && top.started && top.last_member.is_none() {
                // text going on: a held run, then at most a space before it
                if let Some(h) = self.held.take() {
                    let apart = self.cur.space != Space::None;
                    self.flush(h, apart || next.is_none_or(is_flanking_safe));
                }
                if self.cur.space != Space::None {
                    let space = std::mem::replace(&mut self.cur.space, Space::None);
                    if space_wanted(space, next)
                        && !self.out.ends_with(|c: char| c.is_ascii_whitespace())
                    {
                        self.out.push(' ');
                    }
                    self.cur.joined = Joined::No;
                }
                return;
            }
            if len == 1
                && self.held.is_none()
                && !top.started
                && !bare
                && !self.cur.line_open
                && self.cur.space == Space::None
                && self.cur.joined == Joined::No
                && !self.caps.is_empty()
            {
                // a capture's first content, with nothing pending to hand back
                top.started = true;
                self.cur.line_escape = match self.cur.mode {
                    Mode::Block => true,
                    Mode::Cell => self.out.is_empty(),
                    Mode::Inline | Mode::Code => false,
                };
                self.cur.content_start = self.out.len();
                self.cur.line_open = true;
                return;
            }
        }
        self.begin_slow(head, next, bare);
    }

    fn begin_slow(&mut self, head: &str, next: Option<char>, bare: bool) {
        let len = self.frames.len();
        // started frames are a prefix of the stack
        let u = if self.frames[len - 1].started {
            len
        } else {
            self.frames.iter().position(|f| !f.started).unwrap_or(len)
        };
        let gap = if u == 0 {
            None
        } else {
            self.resolve(u, head, bare)
        };
        if let Some(h) = self.held.take() {
            let apart = gap.is_some() || self.cur.space != Space::None || self.cur.hard > 0;
            self.flush(h, apart || next.is_none_or(is_flanking_safe));
        }
        let space = std::mem::replace(&mut self.cur.space, Space::None);
        let hard = std::mem::take(&mut self.cur.hard);
        let joined = std::mem::replace(&mut self.cur.joined, Joined::No) != Joined::No;
        // a capture's first content: what was pending is handed back with it
        if u == 0 {
            if let Some(c) = self.caps.last_mut() {
                c.lead = Edge {
                    space,
                    hard,
                    joined,
                };
                let root = &self.frames[0];
                c.first = root.next.or(match root.flow {
                    Flow::After(_, sep) if sep != Sep::None => Some((Start::Text, sep)),
                    _ => None,
                });
            }
        }
        let space = self.write_break(u, gap, hard, space);
        self.open_or_space(u, head, bare, next, space);
        self.start_frames(u);
    }

    /// Writes the gap, or the hard breaks on an open line when there is none; returns
    /// the space still wanted.
    fn write_break(&mut self, u: usize, gap: Option<Gap>, hard: u8, space: Space) -> Space {
        let Some(gap) = gap else {
            if hard > 0 && self.cur.line_open {
                self.cur.hard = hard;
                self.push_hard_breaks();
            }
            return space;
        };
        let len = self.frames.len();
        let p_end = self.frames.get(u).map_or(self.prefix.len(), |f| f.base);
        // a line break after a hard break keeps it (a row outside its table); a
        // block or member start drops it
        let direct = u == len && self.frames[len - 1].kind != Kind::Group;
        if gap == Gap::Line
            && hard > 0
            && direct
            && self.cur.line_open
            && self.cur.mode != Mode::Inline
        {
            self.trim_spaces();
            self.out.push_str("  ");
        }
        self.write_gap(gap, p_end);
        Space::None
    }

    /// Opens the line, or writes the space owed before content on an open one.
    fn open_or_space(
        &mut self,
        u: usize,
        head: &str,
        bare: bool,
        next: Option<char>,
        space: Space,
    ) {
        if !self.cur.line_open {
            self.open_line(u, head, bare);
            return;
        }
        if space_wanted(space, next) && !self.out.ends_with(|c: char| c.is_ascii_whitespace()) {
            self.out.push(' ');
        }
    }

    /// Marks the frames from `u` started by this content.
    fn start_frames(&mut self, u: usize) {
        let len = self.frames.len();
        if u > 0 && u <= len {
            let pf = &mut self.frames[u - 1];
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
        // the containers it starts are blocks written
        #[allow(clippy::cast_possible_truncation)]
        {
            self.blocks += (u.max(1)..len).len() as u32;
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
        }
        self.top().next = None;
    }

    /// The separator the last started frame (`u - 1`) asks before the content that starts
    /// the frames from `u`: one match arm per (parent, what came before it) pair.
    fn resolve(&self, u: usize, head: &str, bare: bool) -> Option<Gap> {
        let len = self.frames.len();
        let (cstart, cbefore) = self.frames[len - 1]
            .next
            .unwrap_or((Start::Text, Sep::None));
        // how the content, or the first frame it opens, starts
        let (start, before) = match self.frames.get(u).map(|f| f.kind) {
            None => (cstart, cbefore),
            Some(Kind::Quote) => (Start::Quote, Sep::Blank),
            Some(Kind::Group) => {
                let start = match self.frames.get(u + 1) {
                    Some(m) if m.kind == Kind::Member => self.lead_start(u + 1, head, bare),
                    _ => Start::Text,
                };
                (start, Sep::Blank)
            }
            Some(Kind::Member | Kind::Root) => {
                if u + 1 == len {
                    (cstart, cbefore)
                } else {
                    (Start::Text, Sep::None)
                }
            }
        };
        let p = &self.frames[u - 1];
        let gap = match (p.kind, p.rules) {
            // a member in a list: after a member, after loose content, or first
            (Kind::Group, Some(r)) if u < len => match (p.last_member, p.flow) {
                (Some(e), _) => return Some((r.member_gap)(e, start)),
                (None, Flow::After(ended, s)) => {
                    let gap = sep_gap(s.max(before).max(p.next.map_or(Sep::None, |(_, b)| b)));
                    // loose content that ended in text takes the rule for text before a member
                    if ended == End::Text && gap != Some(Gap::Blank) {
                        let start = self.lead_start(u, head, bare);
                        match (r.after_loose)(p.loose_lazy, start, self.cur.hard) {
                            Gap::Blank => Some(Gap::Blank),
                            _ => gap,
                        }
                    } else {
                        gap
                    }
                }
                (None, _) => {
                    let start = self.lead_start(u, head, bare);
                    Some((r.after_loose)(p.loose_lazy, start, self.cur.hard))
                }
            },
            // loose content in a list: after a member (a blank asked wins), or after loose
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
            // a block after a block in a list item, or in emphasis written back into one
            (Kind::Member | Kind::Root, Some(r)) => match p.flow {
                Flow::After(e, _) => Some((r.block_gap)(e, start)),
                _ => None,
            },
            // anywhere else: the larger separator the two sides ask
            _ => match p.flow {
                Flow::After(_, s) => sep_gap(s.max(before)),
                _ => None,
            },
        };
        gap.map(|g| {
            if self.cur.mode == Mode::Cell {
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
        match self.cur.mode {
            Mode::Cell => {
                if !self.out.is_empty() && !ends_with_cell_break(&self.out) {
                    self.out.push_str("<br>");
                }
                if gap == Gap::Blank {
                    self.out.push_str("<br>");
                }
            }
            Mode::Inline | Mode::Code => {
                if !self.out.is_empty() && !self.out.ends_with(' ') {
                    self.out.push(' ');
                }
                return;
            }
            Mode::Block => {
                self.blocks += 1;
                if self.cur.line_open {
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
        self.cur.line_open = false;
    }

    fn trim_spaces(&mut self) {
        let keep = self.out.trim_end_matches(' ').len();
        self.out.truncate(keep);
    }

    fn push_hard_breaks(&mut self) {
        if self.cur.hard > 0 && matches!(self.cur.mode, Mode::Block | Mode::Cell) {
            self.trim_spaces();
        }
        for i in 0..std::mem::take(&mut self.cur.hard) {
            match self.cur.mode {
                Mode::Block => {
                    if i > 0 {
                        self.out.push_str(&self.prefix);
                    }
                    self.out.push_str("  \n");
                    self.cur.line_open = false;
                }
                Mode::Cell => self.out.push_str("  <br>"),
                Mode::Inline => self.out.push_str("<br>"),
                Mode::Code => {}
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
        let mut cursor = 0;
        let mut wrote_lead = false;
        let lb = if self.cur.mode == Mode::Cell {
            "<br>"
        } else {
            "\n"
        };
        for i in u..self.frames.len() {
            let Some(m) = self.frames[i].lead else {
                continue;
            };
            let pos = self.frames[i].lead_pos;
            // the member's own spaces in the prefix, reserved for its lead when pushed
            debug_assert!(self.prefix.is_char_boundary(pos + m.len as usize));
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
        } else if self.cur.mode != Mode::Cell || wrote_lead {
            self.out.push_str(&self.prefix[cursor..]);
        }
        self.cur.line_escape = match self.cur.mode {
            Mode::Block => true,
            Mode::Cell => wrote_lead || self.out.is_empty(),
            Mode::Inline | Mode::Code => false,
        };
        self.cur.content_start = self.out.len();
        self.cur.line_open = true;
    }

    fn newline(&mut self) {
        self.out.push('\n');
        self.cur.line_open = false;
    }

    /// Writes one line of a block; a blank line gets only the prefix's marks (`>`).
    fn push_line(&mut self, line: &str) {
        if !self.cur.line_open {
            if line.is_empty() {
                push_blank_prefix(&mut self.out, &self.prefix);
                self.cur.content_start = self.out.len();
                self.cur.line_open = true;
                return;
            }
            self.out.push_str(&self.prefix);
            self.cur.content_start = self.out.len();
            self.cur.line_open = true;
        }
        self.out.push_str(line);
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

/// `CommonMark` whitespace (`Zs`, tab, form feed, carriage return), which stays outside
/// a run; a newline stays with its hard break.
pub(super) fn is_edge_space(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\n' | '\x0b' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spare_pools_are_capped() {
        let mut w = MdWriter::new(false);
        for _ in 0..100 {
            w.push_capture(FormatOpts::new(), false);
        }
        for _ in 0..100 {
            w.pop_capture();
            // an unused buffer holds nothing worth keeping
            w.recycle(String::new());
            w.recycle(String::with_capacity(1));
        }
        assert_eq!(w.spare_frames.len(), MAX_SPARE);
        assert_eq!(w.spare_bufs.len(), MAX_SPARE);
    }

    #[test]
    fn test_reader_inputs() {
        let cases = [
            // B1: a merged run after `,` closes before the next char
            (
                "<strong>b,</strong><strong><option>!</option></strong>",
                "**b,!**",
            ),
            // B2: a line-start `-` word is escaped
            ("<ul><li>--</li></ul>", "- \\--"),
            // B3: an empty block before link text adds no space
            ("<a href=\"u\"><div></div><b>x</b></a>", "[**x**](u)"),
            // N1: an inner run that the next run's text follows can't close
            ("<s><b>,</b></s><s>a</s>", "~~<strong>,</strong>a~~"),
            ("<i><b>.</b></i><i>y</i>", "*<strong>.</strong>y*"),
            // N2: a nbsp keeps `1.` text
            (
                "<b><h2>x</h2>1.&nbsp;</b>",
                "<strong>\n\n## x\n\n1.\u{a0}\n\n</strong>",
            ),
            // N3: a caption lays out the same inside a list item
            (
                "<ul><li>a<table><caption><h2>t</h2>x</caption><tr><td>c</td></tr></table></li></ul>",
                "- a\n\n  ## t\n\n  x\n\n  |   |\n  | - |\n  | c |",
            ),
            // two breaks in a caption are breaks, not a paragraph end
            (
                "<table><caption>a<br><br>b</caption><tr><td>c</td></tr></table>",
                "a  \n  \nb\n\n|   |\n| - |\n| c |",
            ),
        ];
        for (html, want) in cases {
            assert_eq!(
                crate::Document::from(html).md(None).as_ref(),
                want,
                "{html}"
            );
        }
    }

    #[test]
    fn test_doubtful_runs() {
        // multi-byte text around held and doubtful runs, edge breaks, and merges
        let cases = [
            (
                "<a href=\"u\"><s><br><b>é,</b></s><s>a</s></a>",
                "[~~<br><strong>é,</strong>a~~](u)",
            ),
            (
                "<h1><s><br><br><b>,é</b></s><s>ü</s></h1>",
                "# ~~<br><br>**,é**ü~~",
            ),
            (
                "<a href=\"u\"><s><b>,</b><br></s><s>a</s></a>",
                "[~~**,**<br>a~~](u)",
            ),
            (
                "<p><s>ü<b>,</b></s><s>a</s></p>",
                "~~ü<strong>,</strong>a~~",
            ),
            (
                "<s><i>x<b>,</b></i></s><s>a</s>",
                "~~<em>x<strong>,</strong></em>a~~",
            ),
            ("<s><b>,</b> </s><s>a</s>", "~~**,** a~~"),
            (
                "<s><b>,</b></s><s>a</s><s>ö</s>",
                "~~<strong>,</strong>aö~~",
            ),
            ("<b>é</b><b>ü!</b><b>[x](u)</b>", "**éü!\\[x\\](u)**"),
            (
                "<ul><li><s><b>日,</b></s><s>本<br></s></li></ul>",
                "- ~~<strong>日,</strong>本~~",
            ),
        ];
        for (html, want) in cases {
            assert_eq!(
                crate::Document::from(html).md(None).as_ref(),
                want,
                "{html}"
            );
        }
    }

    #[test]
    fn test_no_merge_across_edge_joins() {
        // main drops the joins at both runs' edges, so `\*` after `z` can't open a merge
        let html = "<h1>- <del><p><br>y z</p></del><del><h1>*</h1> w</del></h1>";
        let md = crate::Document::from(html).md(None);
        assert_eq!(md.as_ref(), "# - ~~<br>y z~~ ~~\\* w~~");
    }
}
