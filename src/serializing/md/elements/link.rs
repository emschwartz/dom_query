use html5ever::local_name;

use crate::{Element, NodeId};

use super::super::ctx::FormatOpts;
use super::super::host::MDHost;
use super::super::text_utils::{push_md_url, push_title};
use super::super::writer::{End, MdWriter, Sep};

pub(in crate::serializing::md) fn write<H: MDHost>(
    h: &H,
    w: &mut MdWriter,
    el: &Element,
    id: NodeId,
    href: &str,
    opts: FormatOpts,
) {
    // a block at an edge of the text is set off by a blank line, or a space where none fits
    let blank_edge = !(opts.link_body || opts.inline || opts.list_item || opts.heading);
    let mark = w.push_label(blank_edge);
    // the label keeps the cell rules, so a code span in it still escapes its pipes
    let mut label_opts = FormatOpts::new().link_body();
    label_opts.table_cell = opts.table_cell;
    h.write_to(w, id, label_opts);
    let Some(block_end) = w.pop_label(mark) else {
        return;
    };
    let text = w.out_mut();
    text.push('(');
    push_md_url(text, href);
    if let Some(title) = el.attr_ref(local_name!("title")) {
        push_title(text, title);
    }
    text.push(')');
    // text after the link must not join the paragraph its last block ended
    if block_end {
        if blank_edge {
            w.close_block(End::Text, Sep::Blank);
        } else {
            w.word_space();
        }
    }
}
