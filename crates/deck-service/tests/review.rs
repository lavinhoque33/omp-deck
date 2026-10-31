//! The shared review prompt: the exact text every client hands omp, and the spans a device's
//! line-number comments become.

use deck_service::review::{
    Comment, HEADER, Loc, MAX_PROMPT_BYTES, MAX_QUOTE_LINES, PromptError, Side, Span, build_prompt,
    span_from_diff, span_on_side,
};

fn span(side: Side, start: u32, end: u32, quote: &[&str], more: bool) -> Span {
    Span {
        side,
        start,
        end,
        quote: quote.iter().map(|s| (*s).to_owned()).collect(),
        more,
    }
}

fn at(path: &str, span: Span, body: &str) -> Comment {
    Comment {
        loc: Some(Loc {
            path: path.to_owned(),
            span,
        }),
        body: body.to_owned(),
    }
}

fn general(body: &str) -> Comment {
    Comment {
        loc: None,
        body: body.to_owned(),
    }
}

#[test]
fn small_review_has_exactly_this_text() {
    let comments = [
        at(
            "src/a.rs",
            span(Side::New, 3, 3, &["let x = 1;"], false),
            "  rename x\nto count  ",
        ),
        at(
            "src/a.rs",
            span(Side::Old, 7, 8, &["old one", "old two"], false),
            "why was this removed?",
        ),
        general("Please add tests."),
    ];
    let text = build_prompt(&comments).unwrap();
    assert_eq!(
        text,
        format!(
            "{HEADER}\n\n\
             1. src/a.rs:3\n> let x = 1;\nrename x\nto count\n\n\
             2. src/a.rs:7-8 (removed)\n> old one\n> old two\nwhy was this removed?\n\n\
             3. General\nPlease add tests."
        )
    );
}

#[test]
fn long_ranges_quote_twelve_lines_then_an_ellipsis() {
    let quote: Vec<String> = (0..MAX_QUOTE_LINES).map(|i| format!("l{i}")).collect();
    let refs: Vec<&str> = quote.iter().map(String::as_str).collect();
    let text = build_prompt(&[at("f", span(Side::New, 1, 40, &refs, true), "too long")]).unwrap();
    assert_eq!(text.matches("\n> l").count(), MAX_QUOTE_LINES);
    assert!(text.contains("\n> l11\n> …\ntoo long"));
    assert!(text.contains("f:1-40\n"));
}

#[test]
fn empty_reviews_and_blank_bodies_are_refused() {
    assert_eq!(build_prompt(&[]), Err(PromptError::Empty));
    assert_eq!(build_prompt(&[general(" \n\t")]), Err(PromptError::Empty));
    // A blank comment is skipped; the numbering stays dense.
    let text = build_prompt(&[general(" "), general("y")]).unwrap();
    assert_eq!(text, format!("{HEADER}\n\n1. General\ny"));
}

#[test]
fn a_review_over_the_cap_is_refused_with_a_clear_message() {
    let err = build_prompt(&[general(&"x".repeat(MAX_PROMPT_BYTES))]).unwrap_err();
    assert!(matches!(err, PromptError::TooLarge { bytes } if bytes > MAX_PROMPT_BYTES));
    let msg = err.to_string();
    assert!(msg.contains("too large") && msg.contains("32 KiB"), "{msg}");
    let ok = general(&"x".repeat(MAX_PROMPT_BYTES - HEADER.len() - 20));
    assert!(build_prompt(&[ok]).unwrap().len() <= MAX_PROMPT_BYTES);
}

#[test]
fn span_contains_respects_the_side() {
    let s = span(Side::Old, 4, 6, &[], false);
    assert!(s.contains(Side::Old, 5));
    assert!(!s.contains(Side::New, 5));
    assert!(!s.contains(Side::Old, 7));
    assert_eq!(s.label(), "4-6");
    assert_eq!(span(Side::New, 9, 9, &[], false).label(), "9");
}

const DIFF: &str = "diff --git a/src/a.rs b/src/a.rs
index 1..2 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,5 @@
 fn main() {
-    let x = 1;
+    let count = 1;
+    let y = 2;
     println!();
 }
@@ -20,2 +21,1 @@ fn other() {
-    gone();
 }
\\ No newline at end of file
";

#[test]
fn spans_quote_the_new_side_lines_of_the_range() {
    let s = span_from_diff(DIFF, 2, 4);
    assert_eq!(
        s,
        span(
            Side::New,
            2,
            4,
            &["    let count = 1;", "    let y = 2;", "    println!();"],
            false
        )
    );
    // Reversed ranges are normalized; context lines count.
    assert_eq!(span_from_diff(DIFF, 1, 1).quote, ["fn main() {"]);
    assert_eq!(span_from_diff(DIFF, 5, 1).start, 1);
}

#[test]
fn an_explicit_side_quotes_that_side_even_when_the_other_has_the_lines() {
    // Line 2 exists on both sides of the replacement: inference prefers the new one.
    assert_eq!(span_from_diff(DIFF, 2, 2).quote, ["    let count = 1;"]);
    let old = span_on_side(DIFF, 2, 2, Some(Side::Old));
    assert_eq!(old, span(Side::Old, 2, 2, &["    let x = 1;"], false));
    let text = build_prompt(&[at("a.rs", old, "why?")]).unwrap();
    assert!(text.contains("(removed)") && text.contains("> "));
    assert_eq!(
        span_on_side(DIFF, 2, 2, Some(Side::New)),
        span_from_diff(DIFF, 2, 2)
    );
    assert_eq!(span_on_side(DIFF, 2, 2, None), span_from_diff(DIFF, 2, 2));
}

#[test]
fn spans_fall_back_to_deleted_lines_and_cap_the_quote() {
    // Line 20 exists only on the old side (deleted).
    let s = span_from_diff(DIFF, 20, 20);
    assert_eq!(s, span(Side::Old, 20, 20, &["    gone();"], false));
    // Outside every hunk: named, not quoted.
    let far = span_from_diff(DIFF, 100, 110);
    assert_eq!(far, span(Side::New, 100, 110, &[], false));
    let mut big = String::from("@@ -0,0 +1,30 @@\n");
    for i in 0..30 {
        big.push_str(&format!("+l{i}\n"));
    }
    let s = span_from_diff(&big, 1, 30);
    assert_eq!(s.quote.len(), MAX_QUOTE_LINES);
    assert!(s.more);
    let text = build_prompt(&[at("big", s, "split this")]).unwrap();
    assert!(text.contains("big:1-30\n> l0\n"));
    assert!(text.contains("> l11\n> …\nsplit this"));
}
