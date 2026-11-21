//! Display-safe names. File names are untrusted input: a name can hold terminal escape
//! sequences, bidirectional overrides that reorder what follows, zero-width characters that hide
//! text, or markup. Every name passes through [`display_name`] before any view shows it.

/// True for characters that are replaced by a visible escape: C0 and C1 controls (including ESC
/// and DEL), bidirectional embeddings, overrides and isolates, the implicit direction marks, and
/// zero-width or invisible format characters.
pub fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}'              // arabic letter mark
            | '\u{180E}'            // mongolian vowel separator
            | '\u{200B}'..='\u{200F}' // zero-width space, joiners, LRM, RLM
            | '\u{2028}'..='\u{202E}' // line and paragraph separators, embeddings, overrides
            | '\u{2060}'..='\u{206F}' // word joiner, invisible operators, isolates, deprecated
            | '\u{FEFF}'            // zero-width no-break space (BOM)
            | '\u{FFF9}'..='\u{FFFB}' // interlinear annotation
        )
}

/// The name as it should be shown: every hidden character becomes `\u{XXXX}`, so it is visible
/// and inert. Other characters, including markup characters such as `<`, are kept: views must
/// render names as text, never as markup.
pub fn display_name(name: &str) -> String {
    if !name.chars().any(is_hidden) {
        return name.to_string();
    }
    let mut out = String::with_capacity(name.len() + 8);
    for c in name.chars() {
        if is_hidden(c) {
            out.push_str(&format!("\\u{{{:04X}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// Shorten `text` to at most `max` characters, keeping the start and the end with an ellipsis in
/// the middle. `max` below 3 returns the first `max` characters.
pub fn shorten_middle(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    if max < 3 {
        return text.chars().take(max).collect();
    }
    let keep = max - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out: String = text.chars().take(head).collect();
    out.push('\u{2026}');
    out.extend(text.chars().skip(count - tail));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_are_unchanged() {
        assert_eq!(display_name("node_modules"), "node_modules");
        assert_eq!(display_name("résumé 日本"), "résumé 日本");
    }

    #[test]
    fn escape_sequences_become_visible() {
        let shown = display_name("a\u{1b}[31mred\u{7}");
        assert_eq!(shown, "a\\u{001B}[31mred\\u{0007}");
        assert!(!shown.chars().any(is_hidden));
    }

    #[test]
    fn bidi_and_zero_width_become_visible() {
        assert_eq!(display_name("abc\u{202E}gpj.exe"), "abc\\u{202E}gpj.exe");
        assert_eq!(display_name("x\u{2066}y"), "x\\u{2066}y");
        assert_eq!(display_name("x\u{200B}y"), "x\\u{200B}y");
        assert_eq!(display_name("\u{FEFF}bom"), "\\u{FEFF}bom");
    }

    #[test]
    fn markup_is_kept_as_text() {
        let name = "<img src=x onerror=alert(1)>";
        assert_eq!(display_name(name), name);
    }

    #[test]
    fn shorten_keeps_both_ends() {
        assert_eq!(shorten_middle("abcdefghij", 5), "ab\u{2026}ij");
        assert_eq!(shorten_middle("abc", 5), "abc");
        assert_eq!(shorten_middle("abcdef", 2), "ab");
    }
}
