use super::constants::{ALWAYS_ESCAPED, LINE_START_ESCAPED};
use super::opts::FormatOpts;

#[allow(clippy::cast_possible_truncation)]
pub(super) fn push_normalized_text(text: &mut String, new_text: &str, f_opts: FormatOpts) {
    if !text.ends_with(['\n', ' ']) && !new_text.is_empty() && new_text.trim_ascii().is_empty() {
        text.push(' ');
        return;
    }

    let follows_newline = text.ends_with(['\n', ' ']) || text.is_empty();
    let push_start_whitespace = (f_opts.inline || !follows_newline)
        && new_text.starts_with(|c: char| c.is_ascii_whitespace());
    let push_end_whitespace = new_text.ends_with(|c: char| c.is_ascii_whitespace());

    let mut result = String::with_capacity(new_text.len());
    let mut iter = new_text.split_ascii_whitespace();

    if let Some(first) = iter.next() {
        let escape = !f_opts.skip_escape;
        if push_start_whitespace {
            result.push(' ');
        } else if escape && first.starts_with(['.', ')']) && ends_with_marker_digits(text) {
            // `<span>1</span>. foo`: the digits already written at the line
            // start and this `.` would form an ordered-list marker
            result.push('\\');
        }
        // only the first word of a text node can continue a Markdown line
        let is_line_start = text.is_empty() || text.ends_with('\n') || f_opts.list_item;
        push_escaped_chunk(&mut result, first, escape, is_line_start);
        for word in iter {
            result.push(' ');
            push_escaped_chunk(&mut result, word, escape, false);
        }
    }

    if result.is_empty() && follows_newline {
        return;
    }

    text.push_str(&result);

    if push_end_whitespace && !text.ends_with(|c: char| c.is_ascii_whitespace()) {
        text.push(' ');
    }
}

/// True when the current line holds only indentation, optionally followed by
/// a list marker opened on this line (`- `, `+ `, `12. `).
fn at_block_start(text: &str) -> bool {
    let bytes = text.as_bytes();
    // the marker, if any, is the last word and is followed by one space
    let mut end = bytes.len();
    if bytes.last() == Some(&b' ') {
        end -= 1;
    }
    // an ordered marker has at most nine digits and its `.`/`)`, and the
    // separator byte in front of it must be in the window too
    let window = end.saturating_sub(11);
    let marker_start = match bytes[window..end]
        .iter()
        .rposition(|&b| b == b' ' || b == b'\n')
    {
        Some(i) => window + i + 1,
        None if window == 0 => 0,
        None => return false,
    };
    let marker = &text[marker_start..end];
    let is_marker =
        end < bytes.len() && (matches!(marker, "-" | "+") || is_ordered_list_marker(marker));
    if !is_marker && !marker.is_empty() {
        return false;
    }
    // everything before, back to the line start, must be indentation
    bytes[..marker_start]
        .iter()
        .rev()
        .take_while(|&&b| b != b'\n')
        .all(|&b| b == b' ')
}

/// True when the current line is a line start followed by one to nine
/// digits, which a following `.` or `)` turns into an ordered-list marker.
fn ends_with_marker_digits(text: &str) -> bool {
    let digits = text
        .bytes()
        .rev()
        .take(10)
        .take_while(u8::is_ascii_digit)
        .count();
    (1..=9).contains(&digits) && at_block_start(&text[..text.len() - digits])
}

/// An ordered-list marker like `3.` or `12)` at the beginning of a line would
/// otherwise turn the line into a list item. `CommonMark` allows at most nine
/// digits.
fn is_ordered_list_marker(chunk: &str) -> bool {
    let Some(stem) = chunk.strip_suffix(['.', ')']) else {
        return false;
    };
    (1..=9).contains(&stem.len()) && stem.as_bytes().iter().all(u8::is_ascii_digit)
}

pub(super) fn push_escaped_chunk(text: &mut String, chunk: &str, escape: bool, line_start: bool) {
    if !escape {
        text.push_str(chunk);
        return;
    }

    let list_marker = line_start && is_ordered_list_marker(chunk);
    let bytes = chunk.as_bytes();
    // a first word of only `=` could underline the previous line into a setext heading
    let setext_underline = line_start && bytes.iter().all(|&b| b == b'=');
    let mut last = 0;

    text.reserve(chunk.len());

    for (i, &b) in bytes.iter().enumerate() {
        let escaped = if ALWAYS_ESCAPED.contains(&b) {
            true
        } else if LINE_START_ESCAPED.contains(&b) {
            line_start && i == 0
        } else if b == b'!' {
            bytes.get(i + 1) == Some(&b'[')
        } else if b == b'.' || b == b')' {
            list_marker
        } else if b == b'=' {
            setext_underline && i == 0
        } else {
            false
        };

        if escaped {
            text.push_str(&chunk[last..i]);
            text.push('\\');
            last = i;
        }
    }

    text.push_str(&chunk[last..]);
}

/// Escapes an unescaped trailing `!` so the `[` or `![` written right after
/// it cannot turn the text into image syntax (`Wow![x](u)`).
pub(super) fn escape_trailing_bang(text: &mut String) {
    if text
        .strip_suffix('!')
        .is_some_and(|rest| !ends_with_escape(rest))
    {
        text.insert(text.len() - 1, '\\');
    }
}

/// True when `text` ends with an odd run of backslashes, which escapes the
/// character written next.
pub(super) fn ends_with_escape(text: &str) -> bool {
    (text.len() - text.trim_end_matches('\\').len()) % 2 == 1
}

pub(super) fn trim_trailing_space(s: &mut String) {
    s.truncate(s.trim_end_matches(' ').len());
}

/// A run of `#` that ends a heading after a space would be dropped as the
/// ATX closing sequence.
pub(super) fn escape_closing_hashes(text: &mut String) {
    let content = text.trim_end();
    let stem = content.trim_end_matches('#');
    if stem.len() < content.len() && stem.ends_with(' ') {
        text.insert(stem.len(), '\\');
    }
}

pub(super) fn add_linebreaks(text: &mut String, linebreak: &str, end: &str) {
    trim_trailing_space(text);
    while !text.ends_with(end) {
        text.push_str(linebreak);
    }
}

/// Keep only the first whitespace‑delimited token and a conservative set of characters.
pub(super) fn sanitize_attr_value(raw: &str) -> String {
    let token = raw.split_ascii_whitespace().next().unwrap_or("");
    token
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.' | '#'))
        .collect()
}

pub(super) fn push_emphasis(acc: &mut String, emphasis_content: &mut String, marker: &str) {
    if emphasis_content.trim().is_empty() {
        acc.push_str(emphasis_content);
        return;
    }

    let trim_prev_marker = try_trim_prev_emphasis_marker(acc, marker);

    if emphasis_content.starts_with(' ') {
        emphasis_content.remove(0);
        if !acc.ends_with(' ') {
            acc.push(' ');
        }
    }

    let push_end_whitespace = emphasis_content.ends_with(' ');
    if push_end_whitespace {
        emphasis_content.pop();
    }
    emphasis_content.push_str(marker);

    if push_end_whitespace {
        emphasis_content.push(' ');
    }

    if !trim_prev_marker {
        acc.push_str(marker);
    }

    acc.push_str(emphasis_content);
}

#[allow(clippy::cast_possible_truncation)]
pub(super) fn try_trim_prev_emphasis_marker(acc: &mut String, marker: &str) -> bool {
    if marker.is_empty() || acc.is_empty() {
        return false;
    }

    let has_space = acc.ends_with(' ');
    let trimmed = acc.trim_ascii_end();
    let trailing = &acc[trimmed.len()..];

    if (trailing.is_empty() || trailing == " ") && is_exact_marker_end(trimmed, marker) {
        let trim_bytes = marker.len() + usize::from(has_space);
        acc.truncate(acc.len() - trim_bytes);

        if has_space {
            acc.push(' ');
        }
        true
    } else {
        false
    }
}

#[inline]
fn is_exact_marker_end(tail: &str, marker: &str) -> bool {
    // this may change later, to support "_", "__"
    let Some(prefix) = tail.strip_suffix(marker) else {
        return false;
    };
    if marker == "*" && prefix.ends_with('*') {
        return false;
    }
    !prefix.bytes().rev().take_while(|&b| b == b'\\').count() % 2 != 0
}

/// Formats a Markdown link destination and writes it directly to the buffer.
pub(super) fn push_md_url(text: &mut String, dest: &str) {
    let mut balance: i32 = 0;
    let mut needs_wrap = false;
    let mut has_backslash = false;

    for &b in dest.as_bytes() {
        match b {
            b'(' => balance += 1,
            b')' => {
                balance -= 1;
                if balance < 0 {
                    needs_wrap = true;
                    break;
                }
            }
            b' ' | b'<' | 0..=31 | 127 => {
                needs_wrap = true;
                break;
            }
            b'\\' => has_backslash = true,
            _ => {}
        }
    }

    if needs_wrap || balance != 0 {
        text.reserve(dest.len() + 4);
        text.push('<');
        for c in dest.chars() {
            match c {
                '\\' => text.push_str("\\\\"),
                '<' => text.push_str("\\<"),
                '>' => text.push_str("\\>"),
                '\n' => text.push_str("%0A"),
                '\r' => text.push_str("%0D"),
                c => text.push(c),
            }
        }
        text.push('>');
    } else if has_backslash {
        text.push_str(&dest.replace('\\', "\\\\"));
    } else {
        text.push_str(dest);
    }
}

pub(super) fn push_title(text: &mut String, title: &str) {
    text.push_str(" \"");
    if title.contains('"') {
        let mut normalized = String::with_capacity(title.len());
        push_normalized_text(&mut normalized, title, FormatOpts::new());
        text.push_str(&normalized.replace('"', "\\\""));
    } else {
        push_normalized_text(text, title, FormatOpts::new());
    }
    text.push('"');
}

pub(super) fn max_backtick_run(text: &str) -> usize {
    let mut max = 0;
    let mut current = 0;
    for b in text.as_bytes() {
        current = if *b == b'`' { current + 1 } else { 0 };
        max = max.max(current);
    }
    max
}

pub(super) fn push_code_text(text: &mut String, code_text: &str) {
    if code_text.is_empty() {
        return;
    }

    // a code span right before would merge its closing run with this
    // opening one: `x` and `y` would give `x``y`, a single span; an escaped
    // backtick is text and merges with nothing
    if text
        .strip_suffix('`')
        .is_some_and(|rest| !ends_with_escape(rest))
    {
        text.push(' ');
    }

    let fence_len = max_backtick_run(code_text) + 1;
    let bytes = code_text.as_bytes();
    // a backtick at either edge would merge into the fence, and a parser
    // strips one space from each side of content like ` a `
    let needs_space = bytes.first() == Some(&b'`')
        || bytes.last() == Some(&b'`')
        || (bytes.first() == Some(&b' ')
            && bytes.last() == Some(&b' ')
            && bytes.iter().any(|&b| b != b' '));

    text.extend(std::iter::repeat_n('`', fence_len));
    if needs_space {
        text.push(' ');
    }
    text.push_str(code_text);
    if needs_space {
        text.push(' ');
    }
    text.extend(std::iter::repeat_n('`', fence_len));
}

pub(super) fn trim_space(s: &mut String) {
    s.truncate(s.trim_ascii_end().len());
    s.drain(..s.len() - s.trim_ascii_start().len());
}

pub(super) fn escape_table_cell_inplace(content: &mut String) {
    if content.is_empty() || !content.contains('|') {
        return;
    }

    let bytes = content.as_bytes();
    let mut result = String::with_capacity(content.len() + 4);
    let mut last = 0;

    for i in 0..bytes.len() {
        if bytes[i] == b'|' {
            // Check if '|' is preceded by an unescaped backslash
            let is_escaped = i > 0 && bytes[i - 1] == b'\\';
            if !is_escaped {
                // Append preceding slice and insert escaping backslash
                result.push_str(&content[last..i]);
                result.push('\\');
                // Set 'last' to 'i' so '|' itself will be included in the next chunk slice
                last = i;
            }
        }
    }
    // If all pipes were already escaped, avoid extra string assignment
    if result.is_empty() {
        return;
    }
    // Append the remaining tail of the string
    result.push_str(&content[last..]);
    *content = result;
}

pub(super) const fn linebreak(br: bool) -> &'static str {
    if br { "<br>" } else { "\n" }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_at_block_start() {
        for text in [
            "",
            "\n",
            "a\n",
            "    ",
            "a\n  ",
            "- ",
            "+ ",
            "12. ",
            "3) ",
            "  - ",
            "x\n    1. ",
            "999999999. ",
            "x\n999999999. ",
            "x\n  123456789) ",
        ] {
            assert!(at_block_start(text), "{text:?}");
        }
        for text in [
            "a",
            "a ",
            "-",
            "- a",
            "-  ",
            "a - ",
            "1.",
            "1.x ",
            "x1. ",
            "1000000000. ",
            "x\n1000000000. ",
            "a\n  b ",
            "中文 ",
        ] {
            assert!(!at_block_start(text), "{text:?}");
        }
    }

    #[test]
    fn test_escape_text() {
        let t = r"Some text: x `y` *z* _w_ [v] <u> #h >q -l +m !i . .5 5. |q|";
        let mut text = String::new();
        // mid-line: only characters with Markdown meaning anywhere are escaped
        push_normalized_text(&mut text, t, FormatOpts::new());
        assert_eq!(
            text,
            r"Some text: x \`y\` \*z\* \_w\_ \[v\] \<u> #h >q -l +m !i . .5 5. \|q\|"
        );

        // at the beginning of a line, block-starting characters are escaped too
        let mut text = String::new();
        for word in ["#h", ">q", "-l", "+m", "2024.", "5)", "."] {
            push_escaped_chunk(&mut text, word, true, true);
            text.push(' ');
        }
        text.pop();
        assert_eq!(text, r"\#h \>q \-l \+m 2024\. 5\) .");

        // escape: false is used for inline code content: only backticks are
        // escaped so they cannot terminate the code span
        let mut text = String::new();
        push_normalized_text(&mut text, t, FormatOpts::new().skip_escape());
        assert_eq!(
            text,
            r"Some text: x `y` *z* _w_ [v] <u> #h >q -l +m !i . .5 5. |q|"
        );
    }
}
