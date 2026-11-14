use std::borrow::Cow;

use super::constants::MAX_LIST_NUMBER;

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub struct EmphasisScope(u8);

impl EmphasisScope {
    pub const BOLD: Self = Self(1 << 0);
    pub const ITALIC: Self = Self(1 << 1);
    pub const STRIKE: Self = Self(1 << 2);

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    #[must_use]
    pub const fn insert(mut self, other: Self) -> Self {
        self.0 |= other.0;
        self
    }
}

impl From<&str> for EmphasisScope {
    fn from(marker: &str) -> Self {
        match marker {
            // TODO: support "_"
            "*" => Self::ITALIC,
            "~~" => Self::STRIKE,
            _ => Self::BOLD,
        }
    }
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Default, Clone, Copy)]
pub struct FormatOpts {
    pub include_node: bool,
    pub skip_escape: bool,
    pub inline: bool,
    /// Whether to skip Markdown formatting syntax and render plain text.
    pub skip_md: bool,
    /// Enables formatting rules specific to table cells:
    /// 1. Prevents standard line breaks.
    /// 2. Replaces line breaks with `<br>` tags.
    pub table_cell: bool,
    /// Enables formatting rules for list item
    pub list_item: bool,
    /// Inside a link's text or a heading, which cannot span a blank line or
    /// hold a block marker: blocks are joined with spaces.
    pub link_body: bool,
    /// Whether the content is written inside a heading, which ends at its line end.
    pub heading: bool,
    pub emphasis_scope: EmphasisScope,
}

impl FormatOpts {
    pub fn new() -> Self {
        Self::default()
    }

    pub const fn include_node(mut self) -> Self {
        self.include_node = true;
        self
    }

    pub const fn skip_escape(mut self) -> Self {
        self.skip_escape = true;
        self
    }

    pub const fn table_cell(mut self) -> Self {
        self.table_cell = true;
        self
    }

    pub const fn list_item(mut self) -> Self {
        self.list_item = true;
        self
    }

    pub const fn link_body(mut self) -> Self {
        self.link_body = true;
        self
    }

    pub const fn inline(mut self) -> Self {
        self.inline = true;
        self
    }
    pub const fn skip_md(mut self) -> Self {
        self.skip_md = true;
        self
    }

    pub const fn emphasis_scope(mut self, scope: EmphasisScope) -> Self {
        self.emphasis_scope = self.emphasis_scope.insert(scope);
        self
    }
}

#[derive(Debug, Clone, Copy)]
/// Represents the kind of list being serialized.
pub enum ListKind {
    Ul,
    Ol(u32),
}

/// Context for a list being serialized.
pub struct ListContext {
    pub opts: FormatOpts,
    pub kind: ListKind,
    /// Writes `*` or `)` markers, so the list doesn't continue the one right before it.
    pub alt_marker: bool,
}

impl ListContext {
    pub fn new(opts: FormatOpts, kind: ListKind) -> Self {
        let mut list_opts = opts;
        // a list written as a node must not make its items write theirs
        list_opts.include_node = false;
        Self {
            opts: list_opts,
            kind,
            alt_marker: false,
        }
    }

    const fn ul_prefix(&self) -> &'static str {
        if self.alt_marker {
            "* "
        } else if self.opts.table_cell {
            "+ "
        } else {
            "- "
        }
    }

    pub fn prefix(&self) -> Cow<'static, str> {
        match self.kind {
            ListKind::Ul => Cow::Borrowed(self.ul_prefix()),
            ListKind::Ol(n) if self.alt_marker => Cow::Owned(format!("{n}) ")),
            ListKind::Ol(n) => Cow::Owned(format!("{n}. ")),
        }
    }

    pub fn advance_ol_number(&mut self) {
        if let ListKind::Ol(n) = self.kind {
            let next_num = n.saturating_add(1).min(MAX_LIST_NUMBER);
            self.kind = ListKind::Ol(next_num);
        }
    }
}
