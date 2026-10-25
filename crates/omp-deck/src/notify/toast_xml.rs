//! Pure pieces of the Windows toast: the ToastGeneric XML and the tag/group a toast is filed
//! under. Platform independent so they are unit-tested on every host.

/// The toast group of every OmpDeck notification: "Clear all" removes the whole group.
#[cfg(windows)]
pub const GROUP: &str = "omp-deck";

use std::hash::{DefaultHasher, Hash, Hasher};

use deck_service::controller::SessionTarget;

/// The toast tag of a session's notification (a newer toast with the same tag replaces the older
/// one); the settings test notification (`None`) is `"test"`. The key is hashed: tags are limited
/// to 64 characters and a session key is not.
pub fn tag(tab: Option<&SessionTarget>) -> String {
    match tab {
        Some(key) => {
            let mut h = DefaultHasher::new();
            key.hash(&mut h);
            format!("tab-{:016x}", h.finish())
        }
        None => "test".to_owned(),
    }
}

/// Escapes `s` for XML text and double-quoted attribute values. Characters XML 1.0 cannot carry
/// at all (most C0 controls, U+FFFE/U+FFFF) are dropped: the document would fail to load.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if c < ' ' || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// The toast document: a silent ToastGeneric with the title and body lines.
pub fn document(title: &str, body: &str) -> String {
    format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text id=\"1\">{}</text>\
         <text id=\"2\">{}</text></binding></visual><audio silent=\"true\"/></toast>",
        escape(title),
        escape(body),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_per_tab_with_a_fixed_test_tag() {
        let key = |n: &str| SessionTarget {
            key: deck_service::controller::SessionKey::tmux("t", n),
            generation: 1,
        };
        assert_eq!(tag(Some(&key("7"))), tag(Some(&key("7"))));
        assert!(tag(Some(&key("7"))).starts_with("tab-"));
        assert_eq!(tag(None), "test");
        assert_ne!(tag(Some(&key("1"))), tag(Some(&key("11"))));
    }

    #[test]
    fn markup_in_user_text_is_escaped() {
        assert_eq!(
            escape(r#"a<b>&"c" 'd'"#),
            "a&lt;b&gt;&amp;&quot;c&quot; &apos;d&apos;"
        );
        assert_eq!(escape("line1\nline2\ttab"), "line1\nline2\ttab");
        assert_eq!(escape("nul\0bell\u{7}ok\u{FFFF}"), "nulbellok");
        assert_eq!(escape("ünï ✓ 日本"), "ünï ✓ 日本");
    }

    #[test]
    fn document_carries_escaped_title_and_body() {
        let doc = document("</text><evil/>", "R&D \"quoted\"");
        assert!(doc.contains("<text id=\"1\">&lt;/text&gt;&lt;evil/&gt;</text>"));
        assert!(doc.contains("<text id=\"2\">R&amp;D &quot;quoted&quot;</text>"));
        assert!(!doc.contains("<evil/>"));
        assert!(doc.starts_with("<toast><visual><binding template=\"ToastGeneric\">"));
        assert!(doc.ends_with("<audio silent=\"true\"/></toast>"));
    }
}
