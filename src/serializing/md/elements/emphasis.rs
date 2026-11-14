use html5ever::{QualName, local_name};

use crate::{Element, NodeId, NodeRef};

use super::super::block::{is_block_wrapper, is_link_block};
use super::super::ctx::{EmphasisScope, FormatOpts};
use super::super::host::MDHost;
use super::super::writer::{End, MdWriter, Sep, Start};

/// Writes an emphasis run; the writer places its delimiters, or html tags where they
/// can't open or close.
pub(in crate::serializing::md) fn write<H: MDHost>(
    h: &H,
    w: &mut MdWriter,
    el: &Element,
    id: NodeId,
    opts: FormatOpts,
) {
    let Some(marker) = md_emphasis(&el.name) else {
        return;
    };
    let scope = EmphasisScope::from(marker);
    let mut em_opts = opts.inline().emphasis_scope(scope);
    // its children are written, not itself again (a list's stray child arrives with it set)
    em_opts.include_node = false;
    // inside a run of the same type it adds no delimiters
    if opts.emphasis_scope.contains(scope) {
        h.write_to(w, id, em_opts);
        return;
    }
    let last_list = h.last_list();
    let mark = w.push_emph(marker);
    h.write_to(w, id, em_opts);
    // a run can't wrap a block other than a paragraph: html tags go around it, each
    // an html block between blank lines
    if w.pop_emph() && !holds_only_paragraphs(h, id) {
        w.rollback(mark);
        h.reset_last_list(last_list);
        let (open, close) = html_tags(marker);
        w.block(open, Start::Para, Sep::Blank, End::Html, Sep::Blank);
        h.write_to(w, id, em_opts);
        w.block(close, Start::Para, Sep::Blank, End::Html, Sep::Blank);
    } else {
        w.commit(mark);
    }
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
