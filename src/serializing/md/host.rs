use std::cell::Ref;
use std::ops::{Deref, DerefMut};

use html5ever::QualName;

use crate::node::TreeNode;
use crate::{NodeId, Tree};

use super::ctx::FormatOpts;
use super::writer::MdWriter;

/// A scratch buffer that returns to the host's pool when dropped.
pub(super) struct ScratchBuf<'h, H: MDHost> {
    host: &'h H,
    buf: String,
}

impl<H: MDHost> Deref for ScratchBuf<'_, H> {
    type Target = String;
    fn deref(&self) -> &String {
        &self.buf
    }
}

impl<H: MDHost> DerefMut for ScratchBuf<'_, H> {
    fn deref_mut(&mut self) -> &mut String {
        &mut self.buf
    }
}

impl<H: MDHost> Drop for ScratchBuf<'_, H> {
    fn drop(&mut self) {
        self.host.put_buf(std::mem::take(&mut self.buf));
    }
}

pub(super) trait MDHost: Sized {
    /// An empty buffer with the capacity of an earlier one; see `scratch`.
    fn take_buf(&self) -> String;
    fn put_buf(&self, buf: String);

    fn scratch(&self) -> ScratchBuf<'_, Self> {
        ScratchBuf {
            host: self,
            buf: self.take_buf(),
        }
    }

    fn tree(&self) -> &Tree;
    fn nodes(&self) -> Ref<'_, Vec<TreeNode>>;
    fn is_skipped(&self, name: &QualName) -> bool;
    /// The last list written and whether the next same-type list must use the
    /// alternate marker.
    fn last_list(&self) -> Option<(NodeId, bool)>;
    /// Records the last written list; `alt_marker` is the marker that list used,
    /// and the next same-type list gets the opposite one.
    fn set_last_list(&self, list: NodeId, alt_marker: bool);
    /// Puts back what `last_list` returned, for a rendering that is rolled back.
    fn reset_last_list(&self, last: Option<(NodeId, bool)>);

    /// Writes `id` when `opts.include_node`, otherwise its children, through `w`.
    fn write_to(&self, w: &mut MdWriter, id: NodeId, opts: FormatOpts);
}
