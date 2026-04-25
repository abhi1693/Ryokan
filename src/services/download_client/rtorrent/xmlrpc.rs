//! XML-RPC wire codec for the rtorrent client. Self-contained ~360
//! LoC: encoder for the small subset rtorrent's API needs (string,
//! i4/i8, boolean, array, struct), a decoder that handles the same
//! shapes plus fault-message extraction, and a tiny pull parser.
//!
//! Split from `mod.rs` during the v1.5 refactor so the trait impl
//! and the codec stay independently navigable. The codec is driven
//! exclusively by `RtorrentClient::send` over the parent module's
//! HTTP layer; nothing else in the codebase touches XML-RPC.

// ---------------------------------------------------------------------------
// Minimal XML-RPC wire format. Ryokan uses string, i4/i8, boolean,
// array, and struct (decode only, for fault handling). Rolling this
// by hand avoids pulling in dxr/xmlrpc + their four transitive
// proc-macro crates for a narrow one-file use case.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(super) enum XmlValue {
    String(String),
    Int(i64),
    Bool(bool),
    Array(Vec<XmlValue>),
    /// Raw binary payload, encoded as XML-RPC `<base64>` on the
    /// wire. Needed for `load.raw_start_verbose` / `load.raw_verbose`
    /// which accept an entire `.torrent` file as a base64 blob —
    /// these are the canonical ways to hand a tiny synthetic
    /// `.torrent` to rtorrent without serving it over HTTP or
    /// sharing a filesystem mount with the rtorrent container.
    /// Test-only for now (used by `live_smoke_narrowed` and friends);
    /// gated behind `#[cfg(test)]` both here and in the encoder
    /// match arm so the production binary doesn't ship a speculative
    /// feature. Promote to unconditional if a UI feature later needs
    /// to load `.torrent` bytes directly.
    #[cfg(test)]
    Base64(Vec<u8>),
}

impl XmlValue {
    pub(super) fn as_string(&self) -> Option<&str> {
        match self {
            XmlValue::String(s) => Some(s),
            _ => None,
        }
    }
    pub(super) fn as_int(&self) -> Option<i64> {
        match self {
            XmlValue::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub(super) fn into_array(self) -> Option<Vec<XmlValue>> {
        match self {
            XmlValue::Array(a) => Some(a),
            _ => None,
        }
    }
}

pub(super) fn encode_request(method: &str, params: &[XmlValue]) -> String {
    let mut s = String::with_capacity(256);
    s.push_str("<?xml version=\"1.0\"?><methodCall><methodName>");
    s.push_str(&xml_text_escape(method));
    s.push_str("</methodName><params>");
    for p in params {
        s.push_str("<param>");
        encode_value(p, &mut s);
        s.push_str("</param>");
    }
    s.push_str("</params></methodCall>");
    s
}

pub(super) fn encode_value(v: &XmlValue, out: &mut String) {
    out.push_str("<value>");
    match v {
        XmlValue::String(s) => {
            out.push_str("<string>");
            out.push_str(&xml_text_escape(s));
            out.push_str("</string>");
        }
        XmlValue::Int(i) => {
            out.push_str("<i8>");
            out.push_str(&i.to_string());
            out.push_str("</i8>");
        }
        XmlValue::Bool(b) => {
            out.push_str("<boolean>");
            out.push_str(if *b { "1" } else { "0" });
            out.push_str("</boolean>");
        }
        XmlValue::Array(a) => {
            out.push_str("<array><data>");
            for inner in a {
                encode_value(inner, out);
            }
            out.push_str("</data></array>");
        }
        #[cfg(test)]
        XmlValue::Base64(bytes) => {
            use base64::{Engine, engine::general_purpose};
            out.push_str("<base64>");
            out.push_str(&general_purpose::STANDARD.encode(bytes));
            out.push_str("</base64>");
        }
    }
    out.push_str("</value>");
}

pub(super) fn xml_text_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Backslash + quote escape for values embedded inside an rtorrent
/// inline command string like `d.custom1.set="value"`. We aren't
/// encoding an XML attribute — we're embedding inside a param string
/// that rtorrent's own parser then re-parses. The quoting convention
/// rtorrent's cmd parser accepts is double-quoted + backslash-escape.
pub(super) fn xml_attr_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

/// Parse an XML-RPC response body. Returns the single `<param>` value
/// on success, or a decoded fault string on `<fault>`.
pub(super) fn decode_response(xml: &str) -> Result<XmlValue, String> {
    let mut p = Parser::new(xml);
    p.expect_open("methodResponse")?;
    let tag = p.peek_open().ok_or("malformed XML-RPC response")?;
    match tag {
        "params" => {
            p.expect_open("params")?;
            p.expect_open("param")?;
            let v = decode_value(&mut p)?;
            p.expect_close("param")?;
            // Only single-param responses from rtorrent; skip any
            // trailing params defensively.
            while p.peek_open() == Some("param") {
                p.expect_open("param")?;
                let _ = decode_value(&mut p)?;
                p.expect_close("param")?;
            }
            p.expect_close("params")?;
            p.expect_close("methodResponse")?;
            Ok(v)
        }
        "fault" => {
            p.expect_open("fault")?;
            let v = decode_value(&mut p)?;
            p.expect_close("fault")?;
            p.expect_close("methodResponse")?;
            // Fault values are structs of {faultCode, faultString}.
            // We decode the string out of the raw XML, since the
            // struct decoder isn't needed elsewhere — cheap and
            // sufficient.
            let msg = fault_message(xml).unwrap_or_else(|| "(no fault message)".to_string());
            let _ = v;
            Err(format!("rtorrent fault: {msg}"))
        }
        other => Err(format!("unexpected XML-RPC response tag: {other}")),
    }
}

pub(super) fn fault_message(xml: &str) -> Option<String> {
    // Find `<name>faultString</name>` then the next <string>...</string>.
    let needle = "<name>faultString</name>";
    let idx = xml.find(needle)?;
    let rest = &xml[idx + needle.len()..];
    let s_start = rest.find("<string>")?;
    let from = &rest[s_start + "<string>".len()..];
    let s_end = from.find("</string>")?;
    Some(xml_text_unescape(&from[..s_end]))
}

pub(super) fn xml_text_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

pub(super) fn decode_value(p: &mut Parser) -> Result<XmlValue, String> {
    p.expect_open("value")?;
    // Implicit string: `<value>bare text</value>` is legal per XML-RPC
    // spec. rtorrent sometimes emits this for empty strings.
    let inner_tag = p.peek_open();
    let v = match inner_tag {
        Some("string") => {
            p.expect_open("string")?;
            let s = p.read_text_until("</string>")?;
            XmlValue::String(xml_text_unescape(s))
        }
        Some("i4") | Some("int") => {
            let tag = inner_tag.unwrap();
            p.consume_open_tag(tag)?;
            let s = p.read_text_until(&format!("</{tag}>"))?;
            let i = s
                .trim()
                .parse::<i64>()
                .map_err(|e| format!("XML-RPC int parse: {e}"))?;
            XmlValue::Int(i)
        }
        Some("i8") => {
            p.expect_open("i8")?;
            let s = p.read_text_until("</i8>")?;
            let i = s
                .trim()
                .parse::<i64>()
                .map_err(|e| format!("XML-RPC i8 parse: {e}"))?;
            XmlValue::Int(i)
        }
        Some("boolean") => {
            p.expect_open("boolean")?;
            let s = p.read_text_until("</boolean>")?;
            XmlValue::Bool(s.trim() != "0")
        }
        Some("array") => {
            p.expect_open("array")?;
            p.expect_open("data")?;
            let mut items = Vec::new();
            while p.peek_open() == Some("value") {
                items.push(decode_value(p)?);
            }
            p.expect_close("data")?;
            p.expect_close("array")?;
            XmlValue::Array(items)
        }
        Some("struct") => {
            // We don't use struct values anywhere except fault, and
            // fault decoding goes through fault_message() not here.
            // Skip through to the closing </struct>.
            p.expect_open("struct")?;
            p.skip_to_close("struct")?;
            XmlValue::String(String::new())
        }
        _ => {
            // Implicit-string case: raw text until </value>.
            // `read_text_until` consumes the marker, so the closing
            // `</value>` has already been swallowed — no additional
            // expect_close call below.
            let s = p.read_text_until("</value>")?;
            return Ok(XmlValue::String(xml_text_unescape(s.trim())));
        }
    };
    p.expect_close("value")?;
    Ok(v)
}

/// Cursor-based pull parser. Just enough shape-awareness to walk
/// well-formed XML-RPC responses. Not a general XML parser — assumes
/// ASCII tag names, no CDATA sections, no namespaces, no processing
/// instructions beyond the `<?xml?>` prolog. All of those hold for
/// rtorrent 0.9.x's XML-RPC responses.
pub(super) struct Parser<'a> {
    buf: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        // Skip `<?xml?>` prolog if present.
        let mut pos = 0;
        let trimmed = s.trim_start();
        pos += s.len() - trimmed.len();
        if trimmed.starts_with("<?xml")
            && let Some(end) = trimmed.find("?>")
        {
            pos += end + 2;
        }
        Self { buf: s, pos }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.buf.len() {
            let c = self.buf.as_bytes()[self.pos];
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek_open(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let rest = self.buf.get(self.pos..)?;
        if !rest.starts_with('<') || rest.starts_with("</") {
            return None;
        }
        let end = rest.find('>')?;
        let tag_inner = &rest[1..end];
        // Attributes not expected; take the tag name up to the first
        // whitespace.
        let name_end = tag_inner
            .find(|c: char| c.is_whitespace())
            .unwrap_or(tag_inner.len());
        Some(&tag_inner[..name_end])
    }

    fn consume_open_tag(&mut self, tag: &str) -> Result<(), String> {
        self.skip_ws();
        let expected = format!("<{tag}>");
        if self.buf[self.pos..].starts_with(&expected) {
            self.pos += expected.len();
            Ok(())
        } else {
            Err(format!(
                "XML parse: expected <{tag}> at position {}",
                self.pos
            ))
        }
    }

    fn expect_open(&mut self, tag: &str) -> Result<(), String> {
        self.consume_open_tag(tag)
    }

    fn expect_close(&mut self, tag: &str) -> Result<(), String> {
        self.skip_ws();
        let expected = format!("</{tag}>");
        if self.buf[self.pos..].starts_with(&expected) {
            self.pos += expected.len();
            Ok(())
        } else {
            Err(format!(
                "XML parse: expected </{tag}> at position {}",
                self.pos
            ))
        }
    }

    fn read_text_until(&mut self, end_marker: &str) -> Result<&'a str, String> {
        let rest = &self.buf[self.pos..];
        let idx = rest
            .find(end_marker)
            .ok_or_else(|| format!("XML parse: no {end_marker} found"))?;
        let text = &rest[..idx];
        self.pos += idx + end_marker.len();
        Ok(text)
    }

    fn skip_to_close(&mut self, tag: &str) -> Result<(), String> {
        let close = format!("</{tag}>");
        let rest = &self.buf[self.pos..];
        let idx = rest
            .find(&close)
            .ok_or_else(|| format!("XML parse: no {close} found"))?;
        self.pos += idx + close.len();
        Ok(())
    }
}
