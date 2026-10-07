//! Round trip: each spec example's Markdown is rendered with pulldown-cmark,
//! the html goes through `Document::md`, and that is rendered again. Both
//! renders must show the same thing. The inputs are the spec examples,
//! vendored unmodified (CC-BY-SA 4.0):
//! - `data/commonmark-spec.txt`: <https://raw.githubusercontent.com/commonmark/commonmark-spec/0.31.2/spec.txt>
//! - `data/gfm-spec.txt`: <https://raw.githubusercontent.com/github/cmark-gfm/27d942c8b0a62d192f616e5bf3578f4b6a89e180/test/spec.txt>
#![cfg(feature = "markdown")]

use std::collections::HashSet;

use dom_query::Document;
use pulldown_cmark::{Event, Options, Parser, html};

/// Extension examples to run from the GFM spec; task list examples are tagged `disabled`.
const GFM_EXTENSIONS: &[&str] = &["table", "strikethrough", "disabled"];

/// `md` writes nested emphasis of one kind as a single run, which renders the same.
const NESTED_EMPHASIS: &str = "em em, strong strong";

const FENCE: &str = "````````````````````````````````";
const EXAMPLE: &str = "```````````````````````````````` example";

fn parser(md: &str) -> Parser<'_> {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    Parser::new_ext(md, opts)
}

fn render(md: &str) -> String {
    let mut out = String::new();
    html::push_html(&mut out, parser(md));
    out
}

/// Raw html passes through the renderer as written, so `md` can't be expected
/// to reproduce it.
fn has_raw_html(md: &str) -> bool {
    parser(md).any(|e| matches!(e, Event::Html(_) | Event::InlineHtml(_)))
}

/// Whitespace as a browser shows it: outside `<pre>` a run of it is one
/// space, and none shows at the edges of a paragraph.
fn as_shown(html: &str) -> String {
    let collapse = |s: &str| s.split_ascii_whitespace().collect::<Vec<_>>().join(" ");
    let out: String = html
        .split("<pre")
        .enumerate()
        .map(|(i, part)| match part.split_once("</pre>") {
            Some((code, rest)) if i > 0 => ["<pre", code, "</pre>", &collapse(rest)].concat(),
            _ => collapse(part),
        })
        .collect();
    out.replace("<p> ", "<p>").replace(" </p>", "</p>")
}

/// The examples of a spec.txt as (number, extension tag, markdown).
fn spec_examples(spec: &str) -> Vec<(usize, &str, String)> {
    let mut examples = Vec::new();
    let mut lines = spec.lines();
    while let Some(line) = lines.next() {
        if let Some(tag) = line.strip_prefix(EXAMPLE) {
            let md: String = lines
                .by_ref()
                .take_while(|l| *l != ".")
                .map(|l| l.to_owned() + "\n")
                .collect();
            lines.by_ref().take_while(|l| *l != FENCE).for_each(drop);
            examples.push((examples.len() + 1, tag.trim(), md.replace('→', "\t")));
        }
    }
    examples
}

/// Round-trips the examples of `spec` whose extension tag `keep` accepts.
fn check_spec(spec: &str, keep: impl Fn(&str) -> bool) {
    let mut seen = HashSet::new();
    let mut failures = Vec::new();
    for (n, tag, spec_md) in spec_examples(spec) {
        if !keep(tag) || has_raw_html(&spec_md) {
            continue;
        }
        let html = render(&spec_md);
        // Many examples render the same html; run each distinct one once.
        if html.is_empty() || !seen.insert(html.clone()) {
            continue;
        }
        let doc = Document::from(html.as_str());
        if doc.select(NESTED_EMPHASIS).exists() {
            continue;
        }
        let md = doc.md(None);
        let (want, got) = (as_shown(&html), as_shown(&render(&md)));
        if want != got {
            eprintln!("=== {n}\n{spec_md}--- md\n{md}\n--- want\n{want}\n--- got\n{got}\n");
            failures.push(n);
        }
    }
    assert!(
        failures.is_empty(),
        "{} failures: {failures:?}",
        failures.len()
    );
}

#[test]
fn commonmark_spec() {
    check_spec(include_str!("data/commonmark-spec.txt"), |_| true);
}

#[test]
fn gfm_spec_extensions() {
    check_spec(include_str!("data/gfm-spec.txt"), |tag| {
        GFM_EXTENSIONS.contains(&tag)
    });
}
