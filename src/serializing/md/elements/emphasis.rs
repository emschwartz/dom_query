use html5ever::{QualName, local_name};

use crate::{Element, NodeId, NodeRef};

use super::super::block::{is_block_wrapper, is_link_block};
use super::super::ctx::{EmphasisScope, FormatOpts};
use super::super::host::MDHost;
use super::super::writer::{End, Mark, MdWriter, Sep, Start};

/// An emphasis run open while `write_to` walks its children.
pub(in crate::serializing::md) struct OpenEmph {
    mark: Mark,
    last_list: Option<(NodeId, bool)>,
    marker: &'static str,
    span: usize,
    only_paragraphs: Option<bool>,
}

/// Starts an emphasis run; the writer places its delimiters, or html tags where they
/// can't open or close. Returns the options for its children, and the run unless an
/// enclosing run of the same type already covers it.
pub(in crate::serializing::md) fn open<H: MDHost>(
    h: &H,
    w: &mut MdWriter,
    el: &Element,
    opts: FormatOpts,
) -> (FormatOpts, Option<OpenEmph>) {
    let marker = md_emphasis(&el.name).expect("an emphasis element");
    let scope = EmphasisScope::from(marker);
    let mut em_opts = opts.inline().emphasis_scope(scope);
    // its children are written, not itself again (a list's stray child arrives with it set)
    em_opts.include_node = false;
    // inside a run of the same type it adds no delimiters
    if opts.emphasis_scope.contains(scope) {
        return (em_opts, None);
    }
    let last_list = h.last_list();
    let mark = w.push_emph(marker);
    let run = OpenEmph {
        mark,
        last_list,
        marker,
        span: w.top_span(),
        only_paragraphs: None,
    };
    (em_opts, Some(run))
}

impl OpenEmph {
    pub(in crate::serializing::md) const fn mark(&self) -> &Mark {
        &self.mark
    }

    pub(in crate::serializing::md) const fn span(&self) -> usize {
        self.span
    }

    /// Whether the run of `id` must be html tags: it holds a block other than a paragraph.
    /// Known as soon as a block is written in it, so the caller need not finish the run first.
    pub(in crate::serializing::md) fn needs_html<H: MDHost>(
        &mut self,
        h: &H,
        w: &MdWriter,
        id: NodeId,
    ) -> bool {
        w.span_blocky(self.span)
            && !*self
                .only_paragraphs
                .get_or_insert_with(|| holds_only_paragraphs(h, id))
    }
}

/// Ends a run written with delimiters.
pub(in crate::serializing::md) fn close(w: &mut MdWriter, run: OpenEmph) {
    w.pop_emph();
    w.commit(run.mark);
}

/// Rolls the run back to an opening html tag, written as its own html block; the caller
/// writes the children again and then `close_html`.
pub(in crate::serializing::md) fn reopen_html<'m, H: MDHost>(
    h: &H,
    w: &mut MdWriter,
    run: OpenEmph,
    inner: impl Iterator<Item = &'m Mark>,
) {
    w.rollback_nested(run.mark, inner);
    h.reset_last_list(run.last_list);
    w.block(
        html_tags(run.marker).0,
        Start::Para,
        Sep::Blank,
        End::Html,
        Sep::Blank,
    );
}

/// Ends the html-tag form of a run, written as its own html block.
pub(in crate::serializing::md) fn close_html(w: &mut MdWriter, name: &QualName) {
    let close = html_tags(md_emphasis(name).expect("an emphasis element")).1;
    w.block(close, Start::Para, Sep::Blank, End::Html, Sep::Blank);
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

fn html_tags(marker: &str) -> (&'static str, &'static str) {
    match marker {
        "**" => ("<strong>", "</strong>"),
        "~~" => ("<del>", "</del>"),
        _ => ("<em>", "</em>"),
    }
}

const fn md_emphasis(name: &QualName) -> Option<&'static str> {
    match name.local {
        local_name!("strong") | local_name!("b") => Some("**"),
        local_name!("em") | local_name!("i") => Some("*"),
        local_name!("del") | local_name!("s") | local_name!("strike") => Some("~~"),
        _ => None,
    }
}
