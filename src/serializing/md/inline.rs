use super::ctx::FormatOpts;
use super::elements::code::ends_with_code_span;
use super::elements::table::ends_with_cell_break;
use super::text_utils::{
    escape_trailing_bang, push_block_space, push_md_url, push_normalized_text, push_title,
};

/// What a code span or an image shows: opaque, it counts as a word on both ends.
const WORD: char = '\u{fffc}';

/// Inline Markdown, plus the first and last characters a reader would see.
/// Markup like `**` or `](url)` is invisible and never counts as an end.
pub(super) struct InlineWriter {
    pub(super) text: String,
    first_visible: Option<char>,
    /// A `<br>` counts as `'\n'`.
    last_visible: Option<char>,
    /// A separator came before any visible text: the parent decides it.
    separator_before: bool,
    /// A separator came after the last visible text.
    separator_after: bool,
}

/// One piece of inline output, named for what it is.
pub(super) enum Inline<'a> {
    Text(&'a str, FormatOpts),
    /// An `<svg>` or `<option>`: shows nothing, keeps words apart.
    Separator,
    LineBreak(&'a str),
    Code(&'a str),
    Image(&'a str),
    /// Emphasis content, and a writer that wraps it in markup chosen from the text before it.
    Emphasis(
        &'a mut InlineWriter,
        &'a mut dyn FnMut(&mut String, &mut String),
    ),
    /// A link label, its destination and its title, written as `[label](url "title")`.
    Link(&'a mut InlineWriter, &'a str, Option<&'a str>),
}

impl InlineWriter {
    /// Continues `text`, whose last character is the visible end so far.
    pub(super) fn new(text: String) -> Self {
        let end = visible_end(&text);
        Self {
            text,
            first_visible: end,
            last_visible: end,
            separator_before: false,
            separator_after: false,
        }
    }

    pub(super) fn push(&mut self, inline: Inline) {
        match inline {
            // waits for the next visible character; one before any belongs to the parent
            Inline::Separator if self.first_visible.is_none() => self.separator_before = true,
            Inline::Separator => self.separator_after = true,
            // text is judged by its source characters, before escaping
            Inline::Text(s, opts) => {
                let (Some(first), Some(last)) = (s.chars().next(), s.chars().next_back()) else {
                    return;
                };
                self.space_before(first, !opts.skip_md);
                push_normalized_text(&mut self.text, s, opts);
                self.shown(first, last);
            }
            // whitespace on both sides, so no separator is written around it
            Inline::LineBreak(s) => {
                self.separator_after = false;
                self.text.push_str(s);
                self.shown('\n', '\n');
            }
            // opaque words; a code span right after another would merge their backticks
            Inline::Code(span) => {
                if !self.space_before(WORD, true) && ends_with_code_span(&self.text) {
                    self.text.push(' ');
                }
                self.text.push_str(span);
                self.shown(WORD, WORD);
            }
            Inline::Image(md) => {
                self.space_before(WORD, true);
                self.text.push_str(md);
                self.shown(WORD, WORD);
            }
            // a run that writes nothing passes its separators on
            Inline::Emphasis(run, _) | Inline::Link(run, ..) if run.text.is_empty() => {
                if run.separator_before || run.separator_after {
                    self.push(Inline::Separator);
                }
            }
            // a run is judged by the visible ends inside its markup; its separators join ours
            Inline::Emphasis(run, write_markup) => {
                self.space_before_run(run);
                write_markup(&mut self.text, &mut run.text);
                self.shown_run(run);
            }
            Inline::Link(run, href, title) => {
                self.space_before_run(run);
                // an unescaped `!` before `[` would make image syntax
                escape_trailing_bang(&mut self.text);
                self.text.push('[');
                self.text.push_str(&run.text);
                self.text.push_str("](");
                push_md_url(&mut self.text, href);
                if let Some(title) = title {
                    push_title(&mut self.text, title);
                }
                self.text.push(')');
                self.shown_run(run);
            }
        }
    }

    /// Block output written straight to the text ends the inline run: it drops a separator
    /// waiting for it, and its last character is the visible end.
    pub(super) fn write_block(&mut self, write: impl FnOnce(&mut String)) {
        let len = self.text.len();
        write(&mut self.text);
        if self.text.len() != len {
            self.separator_after = false;
            self.first_visible.get_or_insert('\n');
            self.last_visible = visible_end(&self.text);
        }
    }

    /// The spacing rule: a waiting separator is a space unless whitespace or closing
    /// punctuation follows, or whitespace, an opening bracket or a `<br>` precedes.
    /// Block breaks are written straight to the text, so its end is checked too, except
    /// in code (`md` false), where a `<br>` is literal text.
    fn space_before(&mut self, next: char, md: bool) -> bool {
        let closing = |c: char| {
            c.is_whitespace() || matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
        };
        let opening = |c: char| c.is_whitespace() || matches!(c, '(' | '[' | '{');
        if !std::mem::take(&mut self.separator_after)
            || closing(next)
            || self.last_visible.is_some_and(opening)
            || (md && ends_with_cell_break(&self.text))
        {
            return false;
        }
        let len = self.text.len();
        push_block_space(&mut self.text);
        self.text.len() != len
    }

    fn space_before_run(&mut self, run: &InlineWriter) {
        if run.separator_before {
            self.push(Inline::Separator);
        }
        self.space_before(run.first_visible.unwrap_or(WORD), true);
    }

    fn shown(&mut self, first: char, last: char) {
        self.first_visible.get_or_insert(first);
        self.last_visible = Some(last);
    }

    fn shown_run(&mut self, run: &InlineWriter) {
        self.first_visible = self.first_visible.or(run.first_visible);
        self.last_visible = run.last_visible;
        self.separator_after = run.separator_after;
    }
}

fn visible_end(text: &str) -> Option<char> {
    if ends_with_cell_break(text) {
        Some('\n')
    } else {
        text.chars().next_back()
    }
}
