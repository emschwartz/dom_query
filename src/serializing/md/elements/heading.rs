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
    w.raw('#').push_str(md_prefix(&e.name).unwrap_or_default());
    let start = w.len();
    w.push_scope(opts, false);
    h.write_to(w, id, opts);
    w.pop_scope();
    let out = w.out_mut();
    if out.len() == start {
        // an empty heading keeps its `#`s
        out.pop();
    } else {
        escape_closing_hashes(out, start);
    }
    w.close_block(End::Closed, Sep::Blank);
}
