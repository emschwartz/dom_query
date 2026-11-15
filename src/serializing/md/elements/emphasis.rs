use html5ever::{QualName, local_name};

use crate::{Element, NodeId, NodeRef};

use super::super::block::{is_block_wrapper, is_link_block};
use super::super::ctx::{EmphasisScope, FormatOpts};
use super::super::host::MDHost;
use super::super::writer::{Doubtful, End, MdWriter, Sep, Start, is_edge_space};

/// Starts an emphasis element: its content is captured, unless an enclosing run of the
/// same type covers it. Returns the options for its children.
pub(in crate::serializing::md) fn open(
    w: &mut MdWriter,
    el: &Element,
    opts: FormatOpts,
) -> FormatOpts {
    let scope = EmphasisScope::from(marker(&el.name));
    let em_opts = opts.inline().emphasis_scope(scope);
    if !opts.emphasis_scope.contains(scope) {
        w.keep_join();
        w.push_capture(em_opts, true);
    }
    em_opts
}

/// Ends an emphasis element that `open` started with `opts`: its content is written as a
/// run, one per paragraph, or between html tags where it holds other blocks.
pub(in crate::serializing::md) fn close<H: MDHost>(
    h: &H,
    w: &mut MdWriter,
    el: &Element,
    id: NodeId,
    opts: FormatOpts,
) {
    let marker = marker(&el.name);
    if opts.emphasis_scope.contains(EmphasisScope::from(marker)) {
        return;
    }
    let mut cap = w.pop_capture();
    let mut doubtful = cap.doubtful.take();
    if w.inline_mode() {
        if cap.trail.breaks() > 0 {
            // a `<br>` follows it
            if let Some(d) = doubtful.take() {
                d.write(&mut cap.text, false);
            }
        }
        // breaks are `<br>` tags here, which stay inside the run
        let mut text = String::new();
        cap.lead.push_breaks(&mut text);
        text.push_str(&cap.text);
        cap.trail.push_breaks(&mut text);
        cap.text = text;
        cap.lead = cap.lead.space_only();
        cap.trail = cap.trail.space_only();
    }
    // a break at the start goes before the run, unless two make a paragraph break
    if cap.lead.breaks() >= 2 {
        w.close_block(End::Text, Sep::Blank);
    } else {
        w.owe(cap.lead);
    }
    if cap.text.is_empty() && doubtful.is_none() {
        if let Some((end, after)) = cap.last {
            w.close_block(end, after);
        }
    } else if cap.block && !holds_only_paragraphs(h, id) {
        // a line end follows it
        if let Some(d) = doubtful.take() {
            d.write(&mut cap.text, false);
        }
        // around blocks, each tag is an html block of its own
        let (open, close) = tags(marker, true);
        w.block(open, Start::Para, Sep::Blank, End::Html, Sep::Blank);
        w.block(&cap.text, Start::Para, Sep::Blank, End::Text, Sep::Blank);
        w.block(close, Start::Para, Sep::Blank, End::Html, Sep::Blank);
    } else {
        if let Some((start, before)) = cap.first {
            w.open_block(start, before);
        }
        // a run can't span a blank line, so each paragraph gets its own; a doubtful run
        // at the end goes on to be decided with the last
        let mut rest = cap.text.as_str();
        while let Some((part, more)) = split_paragraph(rest) {
            if write_run(w, marker, part, None) {
                w.hold(marker, part, None);
            }
            w.close_block(End::Text, Sep::Blank);
            rest = more;
        }
        if write_run(w, marker, rest, doubtful.take()) {
            if rest.len() == cap.text.len() {
                w.hold_buf(marker, std::mem::take(&mut cap.text));
            } else {
                w.hold(marker, rest, None);
            }
        }
        if let Some((end, after)) = cap.last {
            w.close_block(end, after);
        }
    }
    w.owe(cap.trail);
    w.recycle(cap.text);
}

/// `s` split at its first blank line; a char search, cheaper than a pattern one on
/// short runs.
fn split_paragraph(s: &str) -> Option<(&str, &str)> {
    let mut from = 0;
    while let Some(i) = s[from..].find('\n') {
        let at = from + i;
        if s[at + 1..].starts_with('\n') {
            return Some((&s[..at], &s[at + 2..]));
        }
        from = at + 1;
    }
    None
}

/// Writes `part` as a run of `marker`, held until the next content decides its closer,
/// or as html tags when its opener can't open. Returns true for the caller to hold all
/// of `part`, which may own its buffer.
fn write_run(
    w: &mut MdWriter,
    marker: &'static str,
    part: &str,
    doubtful: Option<Box<Doubtful>>,
) -> bool {
    let whole;
    let part = match &doubtful {
        Some(d) => {
            whole = d.after(part);
            whole.as_str()
        }
        None => part,
    };
    let rest = part.trim_start_matches(is_edge_space);
    let lead = &part[..part.len() - rest.len()];
    if rest.is_empty() {
        w.inline(lead);
        return false;
    }
    let d = delimiter(marker);
    let first = rest.trim_start_matches(d).chars().next();
    let last = rest
        .trim_end_matches(is_edge_space)
        .trim_end_matches(d)
        .chars()
        .next_back();
    // whitespace next to a delimiter can't open or close it; punctuation inside the
    // run needs whitespace or punctuation outside it
    let opens = |before: Option<char>| {
        !last.is_some_and(char::is_whitespace)
            && first.is_none_or(|f| {
                !f.is_whitespace()
                    && (f.is_alphanumeric()
                        || !lead.is_empty()
                        || before.is_none_or(is_flanking_safe))
            })
    };
    // the run's own text, with the doubtful run that ends `rest` kept apart
    let mut doubtful = doubtful;
    debug_assert!(
        doubtful
            .as_ref()
            .is_none_or(|d| rest.ends_with(&d.after("")))
    );
    let own = &rest[..rest.len() - doubtful.as_ref().map_or(0, |d| d.len())];
    if lead.is_empty() && w.merge_held(marker, rest, own, &mut doubtful, opens) {
        return false;
    }
    w.settle_held(marker);
    w.inline(lead);
    w.markup(rest, d);
    let before = w.line_content().trim_end_matches(d).chars().next_back();
    if !opens(before) {
        w.push_run(marker, rest, true);
    } else if lead.is_empty() && doubtful.is_none() {
        return true;
    } else {
        w.hold(marker, own, doubtful);
    }
    false
}

/// Whether a run of `marker` around `body` can close, given whether what follows is
/// whitespace or punctuation.
pub(in crate::serializing::md) fn closes(body: &str, marker: &str, safe_after: bool) -> bool {
    match body.trim_end_matches(delimiter(marker)).chars().next_back() {
        None => true,
        Some(l) => !l.is_whitespace() && (l.is_alphanumeric() || safe_after),
    }
}

/// The opener and closer of a run of `marker`.
pub(in crate::serializing::md) fn tags(
    marker: &'static str,
    html: bool,
) -> (&'static str, &'static str) {
    match (marker, html) {
        (m, false) => (m, m),
        ("**", true) => ("<strong>", "</strong>"),
        ("~~", true) => ("<del>", "</del>"),
        (_, true) => ("<em>", "</em>"),
    }
}

/// Whether `name` is written as an emphasis run.
pub(in crate::serializing::md) const fn is_emphasis(name: &QualName) -> bool {
    md_emphasis(name).is_some()
}

/// Whether the only blocks inside `id` are paragraphs and `<div>`-like wrappers.
fn holds_only_paragraphs<H: MDHost>(h: &H, id: NodeId) -> bool {
    NodeRef::new(id, h.tree()).descendants_it().all(|n| {
        n.qual_name_ref().is_none_or(|name| match name.local {
            local_name!("p") => true,
            local_name!("pre") => false,
            // a multi-line code span is written as a fence
            local_name!("code") => !n.text().trim().contains('\n'),
            _ => !is_link_block(&name) || is_block_wrapper(&name),
        })
    })
}

fn marker(name: &QualName) -> &'static str {
    md_emphasis(name).expect("an emphasis element")
}

const fn md_emphasis(name: &QualName) -> Option<&'static str> {
    match name.local {
        local_name!("strong") | local_name!("b") => Some("**"),
        local_name!("em") | local_name!("i") => Some("*"),
        local_name!("del") | local_name!("s") | local_name!("strike") => Some("~~"),
        _ => None,
    }
}

fn delimiter(marker: &str) -> char {
    if marker == "~~" { '~' } else { '*' }
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
pub(in crate::serializing::md) fn is_flanking_safe(c: char) -> bool {
    (c.is_whitespace() && !matches!(c, '\u{b}' | '\u{85}' | '\u{2028}' | '\u{2029}'))
        || c.is_ascii_punctuation()
        || !c.is_alphanumeric() && PUNCTUATION.iter().any(|&(lo, hi)| (lo..=hi).contains(&c))
}
