use html5ever::{LocalName, local_name};

use crate::{Element, NodeId, NodeRef};

use super::super::block::{is_list, is_md_block};
use super::super::constants::{LIST_OFFSET_BASE, MAX_LIST_NUMBER};
use super::super::ctx::{FormatOpts, ListContext, ListKind};
use super::super::host::MDHost;
use super::super::writer::{End, Gap, MdWriter, Rules, Start};
use super::image::image_src;

/// A list being written: its group frame is open while `write_to` walks its children.
pub(in crate::serializing::md) struct OpenList {
    ctx: ListContext,
    id: NodeId,
    offset: usize,
    has_items: bool,
}

/// How a child of a list is written.
pub(in crate::serializing::md) enum ListChild {
    /// an item: its member is open; its children are written with these options
    Item(FormatOpts),
    /// content directly in the list, written as a node with these options
    Stray(FormatOpts),
}

/// Opens a list's group; `child` and `close` do the rest as `write_to` walks it.
pub(in crate::serializing::md) fn open<H: MDHost>(
    h: &H,
    w: &mut MdWriter,
    el: &Element,
    id: NodeId,
    opts: FormatOpts,
) -> OpenList {
    let kind = if el.name.local == local_name!("ol") {
        let start = el
            .attr("start")
            .and_then(|v| parse_list_number(&v))
            .unwrap_or(1);
        ListKind::Ol(start)
    } else {
        ListKind::Ul
    };

    let mut ctx = ListContext::new(opts, kind);
    // a list right after one of the same type takes the other marker, or the two
    // would merge
    let list = NodeRef::new(id, h.tree());
    let has_items = list.children_it(false).any(|c| c.has_name("li"));
    let use_alt_marker = !opts.table_cell && has_items && should_use_alt_marker(h, &el.name.local);
    ctx.alt_marker = use_alt_marker;

    // a list inside an item sits past its content column, unless it starts the item
    // or follows a list that did; one in an inline wrapper never starts it
    let direct = list
        .parent()
        .is_none_or(|p| p.has_name("li") || node_is_md_block(&p));
    let offset = match w.top_member() {
        Some((started, lead_first, len)) if started && !lead_first || !direct => {
            LIST_OFFSET_BASE.saturating_sub(len)
        }
        _ => 0,
    };
    w.push_group(&RULES);
    OpenList {
        ctx,
        id,
        offset,
        has_items,
    }
}

/// Starts a child of the list: an item opens its member, closed by `w.pop_member()`.
pub(in crate::serializing::md) fn child(
    w: &mut MdWriter,
    list: &mut OpenList,
    child: &NodeRef,
) -> ListChild {
    let ctx = &mut list.ctx;
    if !child.has_name("li") {
        // content directly in the list is written at its column; it keeps the cell
        // rules, like the link label does
        let mut opts = FormatOpts::new().include_node();
        opts.table_cell = ctx.opts.table_cell;
        return ListChild::Stray(opts);
    }
    if matches!(ctx.kind, ListKind::Ol(_)) {
        // `<li value>` renumbers this item and the ones after it
        if let Some(v) = list_number(child, "value") {
            ctx.kind = ListKind::Ol(v);
        }
    }
    let marker = ctx.prefix();
    // with content, a bullet or `1.` item can interrupt a paragraph (`CommonMark` §5.2)
    let start = if matches!(ctx.kind, ListKind::Ul | ListKind::Ol(1)) {
        Start::Block
    } else {
        Start::Weak
    };
    ctx.advance_ol_number();
    w.push_member(&marker, list.offset, start);
    ListChild::Item(ctx.opts.list_item())
}

pub(in crate::serializing::md) fn close<H: MDHost>(h: &H, w: &mut MdWriter, list: &OpenList) {
    if list.has_items {
        h.set_last_list(list.id, list.ctx.alt_marker);
    }
    w.pop_group();
}

static RULES: Rules = Rules {
    block_gap,
    member_gap: item_gap,
    loose_gap: |e, para| item_gap(if para { End::Para } else { e }, Start::Text),
    continues: |e, para| !para && matches!(e, End::Text | End::Quote | End::Lazy | End::Row),
    after_loose,
    group_end,
    breaks_line: is_rule_like,
};

/// The separator between two blocks of an item (`CommonMark` §4.8, §5.1).
fn block_gap(prev: End, next: Start) -> Gap {
    match (prev, next) {
        // the blank lines around a paragraph keep its item loose
        (End::Para, _) | (_, Start::Para) => Gap::Blank,
        // text would continue the quote's paragraph lazily
        (End::Quote, Start::Text) => Gap::QuoteEnd,
        // a heading, fence or rule ends at its own line end, a `>` line or block start
        // interrupts a paragraph, and any item start ends a quote, table or nested item
        (End::Closed, _)
        | (End::Text, Start::Quote | Start::Block)
        | (End::Quote, Start::Block | Start::Weak)
        | (End::Lazy | End::Row, Start::Quote | Start::Block | Start::Weak) => Gap::Line,
        // raw HTML runs on to a blank line, two quotes in a row would merge, and text
        // would continue the paragraph, table or nested item above it
        _ => Gap::Blank,
    }
}

/// Between items: a line break, unless a paragraph at either side keeps the list loose.
fn item_gap(prev: End, next: Start) -> Gap {
    if prev == End::Para || next == Start::Para {
        Gap::Blank
    } else {
        Gap::Line
    }
}

/// Before an item after stray text: a lazy line takes the marker as the next item's;
/// stray text that began the list, or two hard breaks, end with a blank line, and
/// otherwise only an item that can interrupt a paragraph may follow on the next line.
fn after_loose(lazy: Option<bool>, start: Start, hard: u8) -> Gap {
    match lazy {
        _ if hard >= 2 => Gap::Blank,
        Some(true) => Gap::Line,
        Some(false) if start == Start::Block => Gap::Line,
        _ => Gap::Blank,
    }
}

/// A nested item's last line takes text lazily, unless it ends at its own line end; a
/// table row doesn't join a line outside its item.
fn group_end(last_item: Option<End>, own: End) -> End {
    match last_item {
        Some(End::Closed | End::Row) => End::Closed,
        Some(_) => End::Lazy,
        None => own,
    }
}

/// Markers and text that would read as a thematic break (`- - -`, `* ***`).
fn is_rule_like(markers: &mut dyn Iterator<Item = &str>, head: &str) -> bool {
    let (mut dashes, mut stars) = (0, 0);
    // the head first: it usually holds a word and settles it
    for b in head.bytes().chain(markers.flat_map(str::bytes)) {
        match b {
            b' ' => {}
            b'-' => dashes += 1,
            b'*' => stars += 1,
            _ => return false,
        }
    }
    (dashes == 0 || stars == 0) && dashes + stars >= 3
}

/// Whether the last written list has the same tag, so this list needs the
/// alternate marker to avoid merging with it.
fn should_use_alt_marker<H: MDHost>(h: &H, name: &LocalName) -> bool {
    let Some((last_list_id, alt)) = h.last_list() else {
        return false;
    };
    let last_list = NodeRef::new(last_list_id, h.tree());
    alt && last_list
        .qual_name_ref()
        .is_some_and(|list_name| &list_name.local == name)
}

pub(in crate::serializing::md) fn write_task_marker<H: MDHost>(
    h: &H,
    w: &mut MdWriter,
    el: &Element,
    id: NodeId,
) {
    let is_checkbox = el
        .attr("type")
        .is_some_and(|t| t.eq_ignore_ascii_case("checkbox"));
    if is_checkbox && starts_list_item(h, NodeRef::new(id, h.tree())) {
        w.inline(if el.has_attr("checked") { "[x]" } else { "[ ]" });
        w.word_space();
    }
}

/// Whether `node` is the first thing in its `<li>`, directly or through a
/// `<p>`, `<label>`, `<span>` or `<div>`, and inline content follows it.
fn starts_list_item<H: MDHost>(h: &H, mut node: NodeRef) -> bool {
    let mut has_content = false;
    loop {
        let mut prev = node.prev_sibling();
        while let Some(p) = prev {
            if !is_blank(&p) {
                return false;
            }
            prev = p.prev_sibling();
        }
        let mut next = node.next_sibling();
        while !has_content {
            let Some(n) = next else { break };
            // a skipped element is dropped with its subtree, so it is not a boundary
            if n.qual_name_ref().is_some_and(|name| h.is_skipped(&name)) {
                next = n.next_sibling();
                continue;
            }
            // content after a block starts on another line; the marker would stand alone
            if n.has_name("pre") || node_is_md_block(&n) {
                return false;
            }
            has_content = has_visible_text(h, &n);
            next = n.next_sibling();
        }
        match node.parent() {
            Some(parent) if parent.has_name("li") => return has_content,
            Some(parent)
                if ["p", "label", "span", "div"]
                    .iter()
                    .any(|n| parent.has_name(n)) =>
            {
                node = parent;
            }
            _ => return false,
        }
    }
}

fn node_is_md_block(node: &NodeRef) -> bool {
    node.qual_name_ref().is_some_and(|name| is_md_block(&name))
}

/// The first node of `node`'s subtree, in document order or its reverse, that
/// matches; only the nodes `descend` matches are looked into.
fn find_node<'a>(
    node: &NodeRef<'a>,
    rev: bool,
    descend: impl Fn(&NodeRef) -> bool,
    matches: impl Fn(&NodeRef) -> bool,
) -> Option<NodeRef<'a>> {
    let mut next = Some(*node);
    while let Some(n) = next {
        if matches(&n) {
            return Some(n);
        }
        next = match (descend(&n), rev) {
            (false, _) => None,
            (true, false) => n.first_child(),
            (true, true) => n.last_child(),
        };
        // past the subtree's end, climb to the next sibling short of `node`
        let mut up = n;
        while next.is_none() && up.id != node.id {
            next = if rev {
                up.prev_sibling()
            } else {
                up.next_sibling()
            };
            up = up.parent()?;
        }
    }
    None
}

/// A comment or a text node holding only whitespace: neither shows content.
fn is_blank(node: &NodeRef) -> bool {
    node.is_comment() || node.is_ascii_empty()
}

/// Whether the subtree renders text; skipped elements and nested lists render none.
fn has_visible_text<H: MDHost>(h: &H, node: &NodeRef) -> bool {
    let rendered = |n: &NodeRef| {
        n.qual_name_ref()
            .is_some_and(|name| !h.is_skipped(&name) && !is_list(&name))
    };
    let text = find_node(
        node,
        false,
        |n| rendered(n) && !n.has_name("img"),
        |n| {
            n.is_nonempty_text()
                || (n.has_name("img")
                    && rendered(n)
                    && n.element_ref().and_then(|el| image_src(&el)).is_some())
        },
    );
    text.is_some()
}

fn list_number(node: &NodeRef, key: &str) -> Option<u32> {
    node.attr(key).and_then(|v| parse_list_number(&v))
}

/// Parses an `ol start` or `li value` attribute the way HTML does: leading
/// whitespace, an optional sign, then digits, ignoring anything after them
/// (`" 5abc"` is 5). A Markdown list number cannot be negative, so negative
/// values become 0, and values above the nine-digit limit are clamped.
fn parse_list_number(value: &str) -> Option<u32> {
    // Trim leading ASCII whitespace according to WHATWG HTML standard
    let value = value.trim_start_matches(|c: char| c.is_ascii_whitespace());
    // Parse an optional sign prefix
    let (negative, value) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    // Extract the contiguous sequence of ASCII digits
    let digits_len = value.bytes().take_while(u8::is_ascii_digit).count();
    if digits_len == 0 {
        return None;
    }
    // md doesn't support negative list markers, clamp to zero
    if negative {
        return Some(0);
    }

    // strip leading zeros from the digit slice
    let meaningful = value[..digits_len].trim_start_matches('0');
    match meaningful.len() {
        // the input was "0" or multiple zeros ("000")
        0 => Some(0),
        // 1..=9 digits always fit u32
        1..=9 => meaningful.parse().ok(),
        // clamp to the CommonMark limit
        _ => Some(MAX_LIST_NUMBER),
    }
}
