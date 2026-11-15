use html5ever::local_name;

use crate::{Element, NodeId};

use super::ctx::FormatOpts;
use super::host::MDHost;
use super::writer::MdWriter;

pub(super) mod code;
pub(super) mod emphasis;
pub(super) mod heading;
pub(super) mod image;
pub(super) mod link;
pub(super) mod list;
pub(super) mod table;

pub(super) fn write_element<H: MDHost>(
    h: &H,
    w: &mut MdWriter,
    e: &Element,
    id: NodeId,
    opts: FormatOpts,
) -> bool {
    match e.name.local {
        // lists, quotes, emphasis, tables and links are walked by `write_to` itself
        local_name!("input") if opts.list_item && !opts.link_body && !opts.table_cell => {
            list::write_task_marker(h, w, e, id);
        }
        local_name!("img") => image::write(w, e),
        local_name!("pre") | local_name!("code") => code::write(h, w, e, id, opts),
        local_name!("h1")
        | local_name!("h2")
        | local_name!("h3")
        | local_name!("h4")
        | local_name!("h5")
        | local_name!("h6")
            if !opts.link_body =>
        {
            heading::write(h, w, e, id, opts);
        }
        _ => return false,
    }
    true
}
