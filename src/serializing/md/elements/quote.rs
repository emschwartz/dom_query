use crate::NodeId;

use super::super::ctx::FormatOpts;
use super::super::host::MDHost;
use super::super::writer::MdWriter;

pub(in crate::serializing::md) fn write<H: MDHost>(h: &H, w: &mut MdWriter, id: NodeId) {
    w.push_quote();
    h.write_to(w, id, FormatOpts::new());
    w.pop_quote();
}
