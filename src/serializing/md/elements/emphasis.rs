use std::fmt::Write;

use html5ever::{QualName, local_name};

use crate::{Element, NodeId, NodeRef};

use super::super::block::{is_block_wrapper, is_link_block};
use super::super::ctx::{EmphasisScope, FormatOpts};
use super::super::host::MDHost;
use super::super::inline::{Inline, InlineWriter};
use super::super::serializer::add_block_end;
use super::super::text_iter::Paragraphs;
use super::super::text_utils::trim_trailing_space;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::serializing::md) enum Emphasis {
    Bold,
    Italic,
    Strike,
}

impl Emphasis {
    const fn of(name: &QualName) -> Option<Self> {
        match name.local {
            local_name!("strong") | local_name!("b") => Some(Self::Bold),
            local_name!("em") | local_name!("i") => Some(Self::Italic),
            local_name!("del") | local_name!("s") | local_name!("strike") => Some(Self::Strike),
            _ => None,
        }
    }

    pub(in crate::serializing::md) const fn marker(self) -> &'static str {
        match self {
            Self::Bold => "**",
            Self::Italic => "*",
            Self::Strike => "~~",
        }
    }

    pub(in crate::serializing::md) const fn delimiter(self) -> char {
        match self {
            Self::Strike => '~',
            _ => '*',
        }
    }

    pub(in crate::serializing::md) const fn tag(self) -> &'static str {
        match self {
            Self::Bold => "strong",
            Self::Italic => "em",
            Self::Strike => "del",
        }
    }

    const fn scope(self) -> EmphasisScope {
        match self {
            Self::Bold => EmphasisScope::BOLD,
            Self::Italic => EmphasisScope::ITALIC,
            Self::Strike => EmphasisScope::STRIKE,
        }
    }

    /// Whether delimiters fit around `content` between the source characters `before` and
    /// `after` (`None` is a line edge or markup), looking past nested delimiters at its edges.
    pub(in crate::serializing::md) fn fits(
        self,
        before: Option<char>,
        content: &str,
        after: Option<char>,
    ) -> bool {
        let delim = self.delimiter();
        if fuses_ambiguously(content, delim) {
            return false;
        }
        let first = content.trim_start_matches(delim).chars().next();
        let last = content.trim_end_matches(delim).chars().next_back();
        let (Some(first), Some(last)) = (first, last) else {
            return true;
        };
        !first.is_whitespace()
            && !last.is_whitespace()
            && opens(before, first)
            && closes(last, after)
    }

    /// Whether a run of this kind starting with `right` can merge into one ending with `left`,
    /// past one space when `spaced`, without making the delimiters between them ambiguous.
    pub(in crate::serializing::md) fn can_merge(
        self,
        left: &str,
        right: &str,
        spaced: bool,
    ) -> bool {
        let star = matches!(self, Self::Italic) || (spaced && matches!(self, Self::Bold));
        let escapes = left.bytes().rev().take_while(|&b| b == b'\\').count();
        !(star && left.ends_with('*')) && escapes % 2 == 0 && (spaced || junction_fits(left, right))
    }
}

/// Whether a delimiter can close between `last` and `after`.
fn closes(last: char, after: Option<char>) -> bool {
    last.is_alphanumeric() || after.is_none_or(is_flanking_safe)
}

/// Whether a delimiter can open between `before` and `first`.
fn opens(before: Option<char>, first: char) -> bool {
    first.is_alphanumeric() || before.is_none_or(is_flanking_safe)
}

/// Whether a delimiter opening `content` would fuse with a nested one into a run that a
/// delimiter inside a word (`x**y`) closes early, by `CommonMark`'s rule of three.
fn fuses_ambiguously(content: &str, delim: char) -> bool {
    let (bytes, delim) = (content.as_bytes(), delim as u8);
    bytes.first() == Some(&delim)
        && bytes.windows(2).enumerate().any(|(i, w)| {
            // a delimiter run starts at `i + 1`, right after a letter, and a letter ends it
            matches!(w, &[a, b] if a.is_ascii_alphanumeric() && b == delim)
                && bytes
                    .iter()
                    .skip(i + 1)
                    .find(|&&b| b != delim)
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
}

/// Whether the delimiters of runs nested at the end of `left` and the start of `right`
/// still close and open once the two touch.
fn junction_fits(left: &str, right: &str) -> bool {
    debug_assert!(!left.is_empty() && !right.is_empty(), "only run cores meet");
    let (Some(l), Some(r)) = (left.chars().next_back(), right.chars().next()) else {
        return true;
    };
    let is_delimiter = |c: char| matches!(c, '*' | '~');
    match (is_delimiter(l), is_delimiter(r)) {
        (true, true) => false,
        (true, false) => left
            .trim_end_matches(l)
            .chars()
            .next_back()
            .is_none_or(|last| closes(last, Some(r))),
        (false, true) => right
            .trim_start_matches(r)
            .chars()
            .next()
            .is_none_or(|first| opens(Some(l), first)),
        (false, false) => true,
    }
}

/// A run's content split for writing: edge spaces (`lead`) and a hard break at the start go
/// before the delimiters, `core` between them, and `trail` then `hard_end` after them.
pub(in crate::serializing::md) struct Edges<'a> {
    pub(in crate::serializing::md) hard_start: bool,
    pub(in crate::serializing::md) lead: &'a str,
    pub(in crate::serializing::md) core: &'a str,
    pub(in crate::serializing::md) trail: &'a str,
    pub(in crate::serializing::md) hard_end: &'a str,
}

impl<'a> Edges<'a> {
    pub(in crate::serializing::md) fn of(content: &'a str) -> Self {
        let hard_start = starts_with_hard_break(content);
        let rest = if hard_start {
            content.get(3..).unwrap_or_default()
        } else {
            content
        };
        let core = rest.trim_start_matches(is_edge_space);
        // spaces before a hard break stay with it
        let lead = if core.starts_with('\n') {
            ""
        } else {
            rest.strip_suffix(core).unwrap_or_default()
        };
        let rest = rest.strip_prefix(lead).unwrap_or(rest);
        let (rest, hard_end) = rest
            .strip_suffix("  \n")
            .map_or((rest, ""), |r| (r, "  \n"));
        let core = rest.trim_end_matches(is_edge_space);
        let trail = rest.strip_prefix(core).unwrap_or_default();
        Self {
            hard_start,
            lead,
            core,
            trail,
            hard_end,
        }
    }
}

/// Ends the line with a hard break, unless a blank line already ends it.
pub(in crate::serializing::md) fn push_hard_break(text: &mut String) {
    if !text.ends_with("\n\n") {
        trim_trailing_space(text);
        text.push_str("  \n");
    }
}

pub(in crate::serializing::md) fn write<H: MDHost>(
    h: &H,
    out: &mut InlineWriter,
    el: &Element,
    id: NodeId,
    opts: FormatOpts,
) {
    let Some(kind) = Emphasis::of(&el.name) else {
        return;
    };
    let em_opts = opts.include_node().inline().emphasis_scope(kind.scope());
    // inside the same emphasis, its delimiters would close the outer run
    if opts.emphasis_scope.contains(kind.scope()) {
        return h.write_children(out, id, em_opts);
    }
    let mut run = InlineWriter::new(h.take_buf());
    h.write_children(&mut run, id, em_opts);
    run.finish();

    let content = &run.text;
    // a run can't span a blank line, so each paragraph inside gets its own;
    // `<br><br>` writes a blank line of spaces
    if (content.contains("\n\n") || content.contains("\n  \n")) && holds_only_paragraphs(h, id) {
        let mut body = content.as_str();
        if starts_with_hard_break(body) {
            body = &body[3..];
            out.write_block(push_hard_break);
        }
        let mut parts = Paragraphs::new(body).peekable();
        while let Some(part) = parts.next() {
            let last = parts.peek().is_none();
            // a hard break before the blank line is dropped, not left inside the run
            let part = if last {
                part
            } else {
                part.trim_end_matches(' ')
            };
            if holds_block(part) {
                out.write_block(|text| push_html_block(text, part, kind, opts.offset));
            } else {
                out.push(Inline::Emphasis(
                    kind,
                    &mut InlineWriter::new(part.to_owned()),
                ));
            }
            if !last {
                out.write_block(|text| add_block_end(text, false));
            }
        }
    } else if holds_block(content) {
        out.write_block(|text| push_html_block(text, content, kind, opts.offset));
    } else {
        out.push(Inline::Emphasis(kind, &mut run));
    }
    h.put_buf(run.text);
}

/// A hard break at the start goes before the run, unless a blank line follows it.
fn starts_with_hard_break(content: &str) -> bool {
    content
        .strip_prefix("  \n")
        .is_some_and(|rest| !rest.trim_start_matches(' ').starts_with('\n'))
}

/// A newline that isn't a hard break comes from a block, which no run can wrap.
fn holds_block(content: &str) -> bool {
    content
        .match_indices('\n')
        .any(|(i, _)| !content[..i].ends_with("  "))
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

/// `CommonMark` whitespace (`Zs`, tab, form feed, carriage return), which is moved outside
/// the run; a newline stays with its hard break.
fn is_edge_space(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\n' | '\x0b' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// Common non-ASCII punctuation blocks; anything else (a letter, a mark, a
/// format character such as a soft hyphen) falls back to html.
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

/// Whitespace or punctuation. The spec's Unicode whitespace is `Zs`, tab, LF, FF and CR, so
/// unlike `char::is_whitespace` it leaves out U+000B, U+0085, U+2028 and U+2029.
fn is_flanking_safe(c: char) -> bool {
    (c.is_whitespace() && !matches!(c, '\u{b}' | '\u{85}' | '\u{2028}' | '\u{2029}'))
        || c.is_ascii_punctuation()
        || !c.is_alphanumeric() && PUNCTUATION.iter().any(|&(lo, hi)| (lo..=hi).contains(&c))
}

/// Around blocks, each tag is an html block of its own between blank lines.
fn push_html_block(acc: &mut String, content: &str, kind: Emphasis, offset: usize) {
    if !acc.is_empty() {
        add_block_end(acc, false);
    }
    let tag = kind.tag();
    let body = content.trim_matches(|c| c == ' ' || c == '\n');
    // the block starts at the surrounding list's offset; later lines may be
    // deeper, so their indent can't be used here
    let pad = " ".repeat(offset);
    let _ = write!(acc, "<{tag}>\n\n{pad}{body}\n\n</{tag}>\n\n");
}
