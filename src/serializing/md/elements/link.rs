use html5ever::local_name;

use crate::Element;

use super::super::ctx::FormatOpts;
use super::super::text_utils::{push_md_url, push_title};
use super::super::writer::{End, MdWriter, Sep, Start};

/// Starts a link: its text is captured. Returns the options for its children.
pub(in crate::serializing::md) fn open(w: &mut MdWriter, opts: FormatOpts) -> FormatOpts {
    // the label keeps the cell rules, so a code span in it still escapes its pipes
    let mut label_opts = FormatOpts::new().link_body();
    label_opts.table_cell = opts.table_cell;
    w.push_capture(label_opts, false);
    label_opts
}

/// Ends a link that `open` started with `opts`: writes its text and destination.
pub(in crate::serializing::md) fn close(w: &mut MdWriter, el: &Element, opts: FormatOpts) {
    let cap = w.pop_capture();
    if cap.text.is_empty() && cap.trail.breaks() == 0 {
        // nothing to show: the words around it are still apart
        if !cap.trail.is_empty() {
            w.word_space();
        }
        w.recycle(cap.text);
        return;
    }
    // a block at an edge of the text is set off by a blank line, or a space where none fits
    let blank_edge = !(opts.link_body || opts.inline || opts.list_item || opts.heading);
    if cap.lead.joins() {
        if blank_edge {
            w.open_block(Start::Text, Sep::Blank);
        } else {
            w.space_after_text();
        }
    }
    let text = w.raw('[');
    // an unescaped `!` before it would make it an image
    if text.ends_with('!') {
        text.insert(text.len() - 1, '\\');
    }
    text.push('[');
    cap.lead.push_kept(text);
    text.push_str(&cap.text);
    cap.trail.push_kept(text);
    text.push_str("](");
    push_md_url(
        text,
        el.attr_ref(local_name!("href")).expect("a link with href"),
    );
    if let Some(title) = el.attr_ref(local_name!("title")) {
        push_title(text, title);
    }
    text.push(')');
    // text after the link must not join the paragraph its last block ended
    if cap.trail.joins() {
        if blank_edge {
            w.close_block(End::Text, Sep::Blank);
        } else {
            w.word_space();
        }
    }
    w.recycle(cap.text);
}
