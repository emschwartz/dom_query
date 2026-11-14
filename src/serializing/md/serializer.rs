use std::cell::{Cell, Ref, RefCell};

use html5ever::{QualName, local_name};
use tendril::StrTendril;

use crate::{Element, NodeId, Tree};

use crate::node::{NodeData, NodeRef, TreeNode, child_nodes};

use super::block::{is_link_block, is_list, is_md_block, is_spaced_inline};
use super::constants::{DEFAULT_SKIP_TAGS, MAX_NESTING};
use super::ctx::FormatOpts;
use super::elements::{self, emphasis, link, list, table};
use super::host::MDHost;
use super::writer::{End, MdWriter, Sep, Start};

pub struct MDSerializer<'a> {
    pub(super) root_node: &'a NodeRef<'a>,
    pub(super) nodes: Ref<'a, Vec<TreeNode>>,
    pub(super) skip_tags: &'a [&'a str],
    last_list: Cell<Option<(NodeId, bool)>>,
    depth: Cell<usize>,
    // at most one per nesting level, freed with the serializer
    bufs: RefCell<Vec<String>>,
}

/// A step of the walk in `write_to`; containers close with their own step, not a call.
/// `Restore`, `MemberEnd` and `QuoteEnd` also bring back the options saved on entry.
enum Op<'a> {
    Open(NodeId),
    Close(&'a QualName),
    ListChild(NodeId),
    MemberEnd,
    ListEnd,
    QuoteEnd,
    EmphEnd(NodeId),
    EmphHtmlEnd(&'a QualName),
    TextTableEnd,
    LinkEnd(NodeId),
    Restore,
}

/// Where the walk stood when an emphasis run opened: the length of `ops` with its end op,
/// and the number of open links.
struct RunAt {
    ops: usize,
    links: usize,
}

/// What `write_to`'s open containers keep while it walks their children, innermost last.
#[derive(Default)]
struct Open {
    /// the options to restore when a container that changed them ends
    saved: Vec<FormatOpts>,
    lists: Vec<list::OpenList>,
    /// none for a run inside one of its type
    runs: Vec<Option<(emphasis::OpenEmph, RunAt)>>,
    links: Vec<link::OpenLink>,
}

impl Open {
    /// Whether a run may have to turn to html tags. A block marks every span opened before
    /// it, so no run holds one unless the outermost does.
    #[inline]
    fn may_turn(&self, w: &MdWriter) -> bool {
        match self.runs.first() {
            None => false,
            Some(Some((run, _))) => w.span_blocky(run.span()),
            Some(None) => true,
        }
    }
}

impl<'a> MDSerializer<'a> {
    pub fn new(root_node: &'a NodeRef, skip_tags: Option<&'a [&'a str]>) -> Self {
        let skip_tags = skip_tags.unwrap_or(&DEFAULT_SKIP_TAGS);
        let nodes = root_node.tree.nodes.borrow();
        MDSerializer {
            root_node,
            nodes,
            skip_tags,
            last_list: Cell::new(None),
            depth: Cell::new(0),
            bufs: RefCell::default(),
        }
    }

    pub fn serialize(&self, include_node: bool) -> StrTendril {
        let opts = FormatOpts {
            include_node,
            ..Default::default()
        };
        let mut w = MdWriter::new(include_node);
        self.write_to(&mut w, self.root_node.id, opts);
        w.finish().into()
    }

    /// Whether an open run turned to html tags, which drops the op being walked. Checked as
    /// containers open, so a run turns before any run inside it opens: walks never nest.
    #[inline(never)]
    fn run_turned<'n>(
        &'n self,
        w: &mut MdWriter,
        ops: &mut Vec<Op<'n>>,
        st: &mut Open,
        opts: &mut FormatOpts,
    ) -> bool {
        // an empty link inside the run rolls back, and with it a block it resolved
        let links = st.links.len();
        let blocky = st.runs.iter().any(|r| {
            r.as_ref()
                .is_some_and(|(run, at)| at.links == links && w.span_blocky(run.span()))
        });
        blocky && self.turn_html(w, ops, st, opts)
    }

    /// Turns the outermost open run that must be html tags so: its walk so far is undone
    /// and its children walked again after the opening tag.
    #[cold]
    #[inline(never)]
    fn turn_html<'n>(
        &'n self,
        w: &mut MdWriter,
        ops: &mut Vec<Op<'n>>,
        st: &mut Open,
        opts: &mut FormatOpts,
    ) -> bool {
        let links = st.links.len();
        let Some(i) = st.runs.iter_mut().position(|r| {
            r.as_mut().is_some_and(|(run, at)| {
                let Op::EmphEnd(id) = ops[at.ops - 1] else {
                    unreachable!("a run's end op");
                };
                at.links == links && run.needs_html(self, w, id)
            })
        }) else {
            return false;
        };
        let (run, at) = st.runs[i].take().expect("an open run");
        let Op::EmphEnd(id) = ops[at.ops - 1] else {
            unreachable!("a run's end op");
        };
        // the runs and links inside it hold marks taken after its own
        let inner_runs = st.runs[i + 1..].iter().flatten().map(|(r, _)| r.mark());
        let inner_links = st.links[at.links..].iter().map(link::OpenLink::mark);
        emphasis::reopen_html(self, w, run, inner_runs.chain(inner_links));
        st.runs.truncate(i);
        // the containers opened inside it end unwritten: what their end ops would restore
        // is restored, so the options end as the run's own
        while ops.len() > at.ops {
            let ends = match ops.pop().expect("an op") {
                Op::Open(_) | Op::Close(_) | Op::ListChild(_) => continue,
                Op::MemberEnd | Op::Restore => false,
                Op::ListEnd => {
                    st.lists.pop();
                    self.depth.set(self.depth.get() - 1);
                    continue;
                }
                Op::LinkEnd(_) => {
                    st.links.pop();
                    true
                }
                Op::QuoteEnd | Op::EmphEnd(_) | Op::EmphHtmlEnd(_) | Op::TextTableEnd => true,
            };
            if ends {
                self.depth.set(self.depth.get() - 1);
            }
            *opts = st.saved.pop().expect("saved options");
        }
        // and its own end op
        ops.truncate(at.ops - 1);
        let e = self.nodes[id.value()].as_element().expect("an element");
        ops.push(Op::EmphHtmlEnd(&e.name));
        ops.extend(child_nodes(Ref::clone(&self.nodes), &id, true).map(Op::Open));
        true
    }

    fn write_element(&self, w: &mut MdWriter, e: &Element, id: NodeId, opts: FormatOpts) -> bool {
        let depth = self.depth.get();
        // these handlers never dispatch to other handlers, so they add one frame at most
        let leaf = matches!(
            e.name.local,
            local_name!("img") | local_name!("input") | local_name!("pre") | local_name!("code")
        );
        if depth >= MAX_NESTING && !leaf {
            return false;
        }
        self.depth.set(depth + 1);
        let handled = elements::write_element(self, w, e, id, opts);
        self.depth.set(depth);
        handled
    }
}

impl MDHost for MDSerializer<'_> {
    fn take_buf(&self) -> String {
        self.bufs.borrow_mut().pop().unwrap_or_default()
    }

    fn put_buf(&self, mut buf: String) {
        let mut pool = self.bufs.borrow_mut();
        // only pathological nesting fills this; past that, free instead of retaining
        if pool.len() >= 64 {
            return;
        }
        buf.clear();
        pool.push(buf);
    }

    fn tree(&self) -> &Tree {
        self.root_node.tree
    }

    fn nodes(&self) -> Ref<'_, Vec<TreeNode>> {
        Ref::clone(&self.nodes)
    }

    fn is_skipped(&self, name: &QualName) -> bool {
        self.skip_tags.contains(&name.local.as_ref())
    }

    /// The last list written and whether the next same-type list must use the
    /// alternate marker.
    fn last_list(&self) -> Option<(NodeId, bool)> {
        self.last_list.get()
    }

    /// Records the last written list; `alt_marker` is the marker that list used,
    /// and the next same-type list gets the opposite one.
    fn set_last_list(&self, list: NodeId, alt_marker: bool) {
        self.last_list.set(Some((list, !alt_marker)));
    }

    fn reset_last_list(&self, last: Option<(NodeId, bool)>) {
        self.last_list.set(last);
    }

    #[allow(clippy::too_many_lines)]
    fn write_to(&self, w: &mut MdWriter, root_id: NodeId, opts: FormatOpts) {
        let children = |id: &NodeId| child_nodes(Ref::clone(&self.nodes), id, true).map(Op::Open);
        let mut ops = if opts.include_node {
            vec![Op::Open(root_id)]
        } else {
            children(&root_id).collect()
        };
        // the options of the node being walked; a container that changes them for its
        // children saves the old ones and its end op restores them
        let mut opts = opts;
        let mut st = Open::default();

        // a run that must be html tags turns so as soon as that is known: its walk so far
        // is undone and redone once, rather than finished first (each level doubled it)
        while let Some(op) = ops.pop() {
            match op {
                Op::Open(id) => {
                    let node = &self.nodes[id.value()];
                    match &node.data {
                        NodeData::Text { contents } => w.text(contents, opts),
                        NodeData::Element(e) => {
                            if self.is_skipped(&e.name) {
                                continue;
                            }
                            let plain = open_element(w, &e.name, id, &self.nodes, opts);
                            // past the cap a list or quote is plain content: each level
                            // prefixes every line, so output would grow with depth squared
                            let under_cap = self.depth.get() < MAX_NESTING;
                            let nest = under_cap && !opts.link_body;
                            if plain {
                                // its children, with no handler
                            } else if nest && e.name.local == local_name!("table") {
                                if st.may_turn(w)
                                    && self.run_turned(w, &mut ops, &mut st, &mut opts)
                                {
                                    continue;
                                }
                                if table::write(self, w, id) {
                                    continue;
                                }
                                // a table written as text is still a block, also inside emphasis
                                w.open_block(Start::Text, Sep::Blank);
                                self.depth.set(self.depth.get() + 1);
                                ops.push(Op::TextTableEnd);
                                ops.extend(children(&id));
                                st.saved.push(opts);
                                opts = FormatOpts::new();
                                continue;
                            } else if under_cap
                                && e.name.local == local_name!("a")
                                && e.attr_ref(local_name!("href")).is_some()
                            {
                                if st.may_turn(w)
                                    && self.run_turned(w, &mut ops, &mut st, &mut opts)
                                {
                                    continue;
                                }
                                // with no href the contents are written in place, like any inline
                                let (label_opts, open) = link::open(w, opts);
                                st.links.push(open);
                                self.depth.set(self.depth.get() + 1);
                                ops.push(Op::LinkEnd(id));
                                ops.extend(children(&id));
                                st.saved.push(opts);
                                opts = label_opts;
                                continue;
                            } else if under_cap && emphasis::is_emphasis(&e.name) {
                                if st.may_turn(w)
                                    && self.run_turned(w, &mut ops, &mut st, &mut opts)
                                {
                                    continue;
                                }
                                let (em_opts, run) = emphasis::open(self, w, e, opts);
                                self.depth.set(self.depth.get() + 1);
                                ops.push(Op::EmphEnd(id));
                                st.saved.push(opts);
                                st.runs.push(run.map(|run| {
                                    let at = RunAt {
                                        ops: ops.len(),
                                        links: st.links.len(),
                                    };
                                    (run, at)
                                }));
                                ops.extend(children(&id));
                                opts = em_opts;
                                continue;
                            } else if nest && is_list(&e.name) {
                                if st.may_turn(w)
                                    && self.run_turned(w, &mut ops, &mut st, &mut opts)
                                {
                                    continue;
                                }
                                // their markers cannot go in link text
                                st.lists.push(list::open(self, w, e, id, opts));
                                self.depth.set(self.depth.get() + 1);
                                ops.push(Op::ListEnd);
                                let kids = child_nodes(Ref::clone(&self.nodes), &id, true);
                                ops.extend(kids.map(Op::ListChild));
                                continue;
                            } else if nest
                                && e.name.local == local_name!("blockquote")
                                && !opts.table_cell
                            {
                                if st.may_turn(w)
                                    && self.run_turned(w, &mut ops, &mut st, &mut opts)
                                {
                                    continue;
                                }
                                // a cell holds no `>` lines: its quote is written as a plain block
                                w.push_quote();
                                self.depth.set(self.depth.get() + 1);
                                ops.push(Op::QuoteEnd);
                                ops.extend(children(&id));
                                st.saved.push(opts);
                                opts = FormatOpts::new();
                                continue;
                            } else if (e.name.local == local_name!("hr") && !opts.link_body)
                                || self.write_element(w, e, id, opts)
                            {
                                continue;
                            } else if !opts.link_body
                                && is_md_block(&e.name)
                                && declares_block(&e.name)
                            {
                                // past the nesting limit: written as plain content
                                w.open_block(Start::Text, Sep::Blank);
                            }
                            ops.push(Op::Close(&e.name));
                            ops.extend(children(&id));
                        }
                        _ => {}
                    }
                }
                Op::Close(name) => close_element(w, name, opts),
                Op::ListChild(id) => {
                    let open = st.lists.last_mut().expect("an open list");
                    let child = list::child(w, open, &NodeRef::new(id, self.root_node.tree));
                    st.saved.push(opts);
                    match child {
                        list::ListChild::Item(o) => {
                            ops.push(Op::MemberEnd);
                            ops.extend(children(&id));
                            opts = o;
                        }
                        list::ListChild::Stray(o) => {
                            ops.push(Op::Restore);
                            ops.push(Op::Open(id));
                            opts = o;
                        }
                    }
                }
                Op::MemberEnd => {
                    w.pop_member();
                    opts = st.saved.pop().expect("saved options");
                }
                Op::ListEnd => {
                    let open = st.lists.pop().expect("an open list");
                    list::close(self, w, &open);
                    self.depth.set(self.depth.get() - 1);
                }
                Op::QuoteEnd => {
                    w.pop_quote();
                    self.depth.set(self.depth.get() - 1);
                    opts = st.saved.pop().expect("saved options");
                }
                Op::EmphEnd(id) => {
                    if st.may_turn(w) {
                        // back on the stack while checking, so a turn unwinds this run too
                        ops.push(Op::EmphEnd(id));
                        if self.run_turned(w, &mut ops, &mut st, &mut opts) {
                            continue;
                        }
                        ops.pop();
                    }
                    if let Some((run, _)) = st.runs.pop().expect("an open run") {
                        emphasis::close(w, run);
                    }
                    self.depth.set(self.depth.get() - 1);
                    opts = st.saved.pop().expect("saved options");
                }
                Op::EmphHtmlEnd(name) => {
                    emphasis::close_html(w, name);
                    self.depth.set(self.depth.get() - 1);
                    opts = st.saved.pop().expect("saved options");
                }
                Op::TextTableEnd => {
                    w.close_block(End::Text, Sep::Blank);
                    self.depth.set(self.depth.get() - 1);
                    opts = st.saved.pop().expect("saved options");
                }
                Op::LinkEnd(id) => {
                    let e = self.nodes[id.value()].as_element().expect("an element");
                    link::close(w, e, st.links.pop().expect("an open link"));
                    self.depth.set(self.depth.get() - 1);
                    opts = st.saved.pop().expect("saved options");
                }
                Op::Restore => opts = st.saved.pop().expect("saved options"),
            }
        }
    }
}

/// Starts an element written by `write_to`: the boundary before it and, for a rule, the
/// rule itself. Returns whether its children are written as plain content (md skipped).
fn open_element(
    w: &mut MdWriter,
    name: &QualName,
    id: NodeId,
    nodes: &[TreeNode],
    opts: FormatOpts,
) -> bool {
    // code (md skipped) joins its blocks with a space
    if opts.skip_md {
        if is_link_block(name) {
            w.boundary();
        }
        return true;
    }
    // an option is written apart
    if is_spaced_inline(name) {
        w.boundary();
    }
    if opts.link_body {
        if is_link_block(name) {
            w.join();
        }
        return false;
    }
    match name.local {
        local_name!("hr") => {
            // a rule right in an item is written so that no marker can underline it
            let in_item = nodes[id.value()].parent.is_some_and(|p| {
                nodes[p.value()]
                    .as_element()
                    .is_some_and(|e| e.name.local == local_name!("li"))
            });
            if opts.table_cell {
                // a cell has no line for a rule: it is text on a line of its own, `***` on
                // a marker line; right in a list item only the break is kept (as main does)
                let rule = if in_item && w.top_member().is_some() {
                    ""
                } else if w.lead_pending() {
                    "***"
                } else {
                    "---"
                };
                w.open_block(Start::Text, Sep::Line);
                if !rule.is_empty() {
                    w.inline(rule);
                }
                w.close_block(End::Text, Sep::Line);
            } else {
                // `---` under a marker line would underline it, or read as a list
                // `---` right under a text line would underline it: it starts as a paragraph
                // does, after a blank line
                let (rule, start) = if in_item || w.lead_pending() {
                    ("***", Start::Block)
                } else {
                    ("---", Start::Para)
                };
                w.block(rule, start, Sep::Blank, End::Closed, Sep::Blank);
            }
        }
        // handled elements declare their own blocks
        _ if declares_block(name) => {}
        local_name!("p") => w.open_block(Start::Para, Sep::Blank),
        // an item outside a list starts a line, as its end does
        local_name!("li") => w.open_block(Start::Text, Sep::Line),
        _ if is_md_block(name) => w.open_block(Start::Text, Sep::Blank),
        _ => {}
    }
    false
}

/// A block whose handler declares its start and end to the writer.
fn declares_block(name: &QualName) -> bool {
    matches!(
        name.local,
        local_name!("ul")
            | local_name!("ol")
            | local_name!("blockquote")
            | local_name!("table")
            | local_name!("h1")
            | local_name!("h2")
            | local_name!("h3")
            | local_name!("h4")
            | local_name!("h5")
            | local_name!("h6")
    )
}

/// Ends an element written by `write_to`.
fn close_element(w: &mut MdWriter, name: &QualName, opts: FormatOpts) {
    if opts.skip_md {
        if is_link_block(name) {
            w.boundary();
        } else if name.local == local_name!("br") {
            // a `<br>` is a space when Markdown output is disabled
            w.word_space();
        }
        return;
    }
    if is_spaced_inline(name) {
        w.boundary();
    }
    if opts.link_body && is_link_block(name) {
        w.join();
        return;
    }
    match name.local {
        local_name!("br") => w.hard_break(true),
        // fallback for `tr` and `caption` elements outside their standard context
        local_name!("tr") | local_name!("caption") => w.hard_break(false),
        local_name!("li") => {
            w.hard_break(false);
            w.close_block(End::Text, Sep::Line);
        }
        // a table written as text keeps its cells apart
        local_name!("td") | local_name!("th") => w.word_space(),
        local_name!("p") => w.close_block(End::Para, Sep::Blank),
        _ if is_md_block(name) => w.close_block(End::Text, Sep::Blank),
        _ => {}
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::Document;

    #[test]
    fn test_scratch_reuses_buffer() {
        let doc = Document::from("<p>x</p>");
        let root = doc.root();
        let ser = MDSerializer::new(&root, None);
        {
            let mut buf = ser.scratch();
            buf.push_str("abc");
        }
        let buf = ser.scratch();
        assert!(buf.is_empty());
        assert!(buf.capacity() >= 3);
    }

    #[test]
    fn test_scratch_pool_is_capped() {
        let html = format!(
            "{}x{}",
            "<blockquote>".repeat(100),
            "</blockquote>".repeat(100)
        );
        let doc = Document::from(html);
        let root = doc.root();
        let ser = MDSerializer::new(&root, None);
        ser.serialize(false);
        assert_eq!(ser.bufs.borrow().len(), 64);
    }
}
