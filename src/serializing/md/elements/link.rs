use html5ever::local_name;

use crate::Element;

use super::super::ctx::FormatOpts;
use super::super::text_utils::{push_md_url, push_title};
use super::super::writer::{End, Mark, MdWriter, Sep};

/// A link whose text `write_to` is walking.
pub(in crate::serializing::md) struct OpenLink {
    mark: Mark,
    blank_edge: bool,
}

impl OpenLink {
    pub(in crate::serializing::md) const fn mark(&self) -> &Mark {
        &self.mark
    }
}

/// Starts a link's text. Returns the options for its children.
pub(in crate::serializing::md) fn open(
    w: &mut MdWriter,
    opts: FormatOpts,
) -> (FormatOpts, OpenLink) {
    // a block at an edge of the text is set off by a blank line, or a space where none fits
    let blank_edge = !(opts.link_body || opts.inline || opts.list_item || opts.heading);
    let mark = w.push_label(blank_edge);
    // the label keeps the cell rules, so a code span in it still escapes its pipes
    let mut label_opts = FormatOpts::new().link_body();
    label_opts.table_cell = opts.table_cell;
    (label_opts, OpenLink { mark, blank_edge })
}

/// Ends the link's text and writes its destination.
pub(in crate::serializing::md) fn close(w: &mut MdWriter, el: &Element, link: OpenLink) {
    let Some(block_end) = w.pop_label(link.mark) else {
        return;
    };
    let href = el.attr_ref(local_name!("href")).expect("a link with href");
    let text = w.out_mut();
    text.push('(');
    push_md_url(text, href);
    if let Some(title) = el.attr_ref(local_name!("title")) {
        push_title(text, title);
    }
    text.push(')');
    // text after the link must not join the paragraph its last block ended
    if block_end {
        if link.blank_edge {
            w.close_block(End::Text, Sep::Blank);
        } else {
            w.word_space();
        }
    }
}
