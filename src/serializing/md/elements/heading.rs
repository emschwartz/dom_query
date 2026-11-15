use crate::{Element, NodeId};

use super::super::ctx::FormatOpts;
use super::super::heading::{escape_closing_hashes, md_prefix};
use super::super::host::MDHost;
use super::super::writer::{End, MdWriter, Sep, Start};

/// Writes a heading on one line: an ATX heading ends at the line end.
pub(in crate::serializing::md) fn write<H: MDHost>(
    h: &H,
    w: &mut MdWriter,
    e: &Element,
    id: NodeId,
    opts: FormatOpts,
) {
    // a table cell already joins blocks with `<br>`
    let mut opts = if opts.table_cell {
        opts
    } else {
        opts.link_body()
    };
    opts.include_node = false;
    opts.heading = true;
    w.open_block(Start::Block, Sep::Blank);
    w.push_capture(opts, false);
    h.write_to(w, id, opts);
    let cap = w.pop_capture();
    let prefix = md_prefix(&e.name).unwrap_or_default();
    let out = w.raw('#');
    if cap.text.is_empty() {
        out.push_str(prefix.trim_end());
    } else {
        out.push_str(prefix);
        // a break at the start is kept; a space is not
        for _ in 0..cap.lead.breaks() {
            out.push_str("<br>");
        }
        let start = out.len();
        out.push_str(&cap.text);
        escape_closing_hashes(out, start);
    }
    w.recycle(cap.text);
    w.close_block(End::Closed, Sep::Blank);
}
