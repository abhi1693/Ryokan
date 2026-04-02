use ammonia::Builder;
use std::collections::HashSet;

fn escape_html(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

pub fn sanitize_rich_description(raw: &str, treat_as_html: bool) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let normalized = trimmed.replace("\r\n", "\n").replace('\r', "\n");
    let fragment = if treat_as_html {
        normalized
    } else {
        escape_html(&normalized).replace("\n", "<br>\n")
    };

    let tags: HashSet<&str> = [
        "br", "p", "b", "strong", "i", "em", "u", "ul", "ol", "li", "blockquote",
    ]
    .into_iter()
    .collect();

    Builder::default()
        .tags(tags)
        .clean(&fragment)
        .to_string()
}
