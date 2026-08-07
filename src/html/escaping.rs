
/// Escape the five ASCII characters that are meaningful in HTML text/attribute context
/// (`& < > " '`). Applied to every dynamic string before it is written into the document, so
/// no untrusted content (function/parameter/exception names pulled from analyzed Python source)
/// can break out of its containing tag or attribute.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}
