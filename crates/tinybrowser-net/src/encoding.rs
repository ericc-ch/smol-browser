//! Charset detection and decoding for HTTP response bodies.
//!
//! Issue #113: tinybrowser used to call `String::from_utf8_lossy` on every
//! response body, which silently corrupts every non-UTF-8 page (GBK, Big5,
//! Shift-JIS, Windows-125x, EUC-KR, ISO-8859-x). Picking the right decoder
//! is required for scraping non-Latin sites at all.
//!
//! Detection order, mirroring real browsers (HTML5 spec § 8.2.2.4):
//!   1. `Content-Type: text/html; charset=...` from the HTTP response header.
//!   2. `<meta charset="...">` or `<meta http-equiv="Content-Type" content="text/html; charset=...">`
//!      sniffed from the first 1024 bytes of the body.
//!   3. Default UTF-8.
//!
//! For non-HTML resources (JS, CSS, JSON), only steps 1 and 3 apply.

use encoding_rs::{DecoderResult, EncoderResult, Encoding, UTF_8};

/// Options for [`TextDecoder`]. Both flags default to `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextDecoderOptions {
    pub fatal: bool,
    pub ignore_bom: bool,
}

/// WHATWG `TextDecoder`: decode bytes with an explicit encoding label.
pub struct TextDecoder {
    encoding: &'static Encoding,
    options: TextDecoderOptions,
}

impl TextDecoder {
    /// `None` when `label` is not a known encoding.
    pub fn new(label: &str, options: TextDecoderOptions) -> Option<Self> {
        Some(Self {
            encoding: Encoding::for_label(label.as_bytes())?,
            options,
        })
    }

    /// WHATWG canonical name from encoding_rs (e.g. `"UTF-8"`, `"Shift_JIS"`).
    pub fn encoding_name(&self) -> &'static str {
        self.encoding.name()
    }

    /// `None` when `fatal` and the input is not valid in this encoding.
    /// Non-fatal decoding replaces errors with U+FFFD.
    pub fn decode(&self, bytes: &[u8]) -> Option<String> {
        let mut dec = if self.options.ignore_bom {
            self.encoding.new_decoder_without_bom_handling()
        } else {
            self.encoding.new_decoder()
        };
        if self.options.fatal {
            let mut out = String::with_capacity(bytes.len() + 1);
            let (res, _) = dec.decode_to_string_without_replacement(bytes, &mut out, true);
            match res {
                DecoderResult::InputEmpty => Some(out),
                _ => None,
            }
        } else {
            let mut out = String::with_capacity(bytes.len() * 2 + 1);
            let _ = dec.decode_to_string(bytes, &mut out, true);
            Some(out)
        }
    }
}

/// HTML document text plus the encoding used, for `document.characterSet`
/// and the WHATWG URL encoding override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedHtml {
    pub text: String,
    pub encoding_name: &'static str,
}

/// Decode an HTML response body. `content_type` is the raw header value if
/// present (e.g. `text/html; charset=gbk`). Sniffs `<meta charset>` in the
/// first 1KB when the header does not name a charset.
pub fn decode_html(bytes: &[u8], content_type: Option<&str>) -> DecodedHtml {
    let (encoding, _) = detect_encoding(bytes, content_type);
    let (cow, _, _) = encoding.decode(bytes);
    DecodedHtml {
        text: cow.into_owned(),
        encoding_name: encoding.name(),
    }
}

/// Decode a non-HTML resource (JS, CSS, JSON, plain text). Honors only the
/// `Content-Type` charset; does not sniff HTML meta tags.
pub fn decode_text(bytes: &[u8], content_type: Option<&str>) -> String {
    let encoding = content_type
        .and_then(charset_from_content_type)
        .and_then(|name| Encoding::for_label(name.as_bytes()))
        .unwrap_or(UTF_8);
    let (cow, _, _) = encoding.decode(bytes);
    cow.into_owned()
}

const PCT_HEX: &[u8; 16] = b"0123456789ABCDEF";

fn push_pct(out: &mut String, b: u8) {
    out.push('%');
    out.push(PCT_HEX[(b >> 4) as usize] as char);
    out.push(PCT_HEX[(b & 0x0F) as usize] as char);
}

/// Append an ASCII `byte` to a URL query string, percent-encoding it when it is
/// in the WHATWG query percent-encode set (C0 controls, space, `"`, `#`, `<`,
/// `>`, 0x7F), plus `'` for special schemes (the special-query set). ASCII
/// delimiters like `=` and `&` are left literal, so structured queries survive.
fn push_query_ascii(out: &mut String, b: u8, special: bool) {
    let must_encode =
        b <= 0x20 || b == 0x7F || matches!(b, 0x22 | 0x23 | 0x3C | 0x3E) || (special && b == 0x27);
    if must_encode {
        push_pct(out, b);
    } else {
        out.push(b as char);
    }
}

/// Encode a run of non-ASCII code points to the target charset and percent-
/// encode EVERY resulting byte. The bytes serialize a non-ASCII character, so
/// all of them are escaped (this is what the WPT legacy-mb encode-href tests
/// expect, e.g. Big5 `一` -> `%A4%40` even though the 0x40 trail byte is ASCII).
/// Unmappable code points become the literal `%26%23<decimal>%3B` sequence (a
/// percent-encoded `&#NNN;` numeric character reference), per the URL spec.
fn encode_run_pct(out: &mut String, run: &str, enc: &'static Encoding) {
    let mut encoder = enc.new_encoder();
    let mut input = run;
    let mut buf = [0u8; 256];
    loop {
        let (result, read, written) =
            encoder.encode_from_utf8_without_replacement(input, &mut buf, true);
        for &b in &buf[..written] {
            push_pct(out, b);
        }
        input = &input[read..];
        match result {
            EncoderResult::InputEmpty => break,
            EncoderResult::OutputFull => continue,
            EncoderResult::Unmappable(c) => {
                out.push_str("%26%23");
                out.push_str(&(c as u32).to_string());
                out.push_str("%3B");
            }
        }
    }
}

/// WHATWG URL "percent-encode after encoding" for the query component, using a
/// non-UTF-8 document encoding override (`label`). `query` is the already
/// UTF-8-percent-decoded query string. ASCII code points use the (special-)
/// query percent-encode set so real query delimiters (`=`, `&`) stay literal;
/// runs of non-ASCII code points are encoded to the target charset with every
/// byte percent-encoded. Returns None when the label is unknown.
pub fn url_encode_query(query: &str, label: &str, special: bool) -> Option<String> {
    let enc = Encoding::for_label(label.as_bytes())?;
    let mut out = String::with_capacity(query.len() * 3);
    let mut run_start: Option<usize> = None;
    for (idx, c) in query.char_indices() {
        if c.is_ascii() {
            if let Some(s) = run_start.take() {
                encode_run_pct(&mut out, &query[s..idx], enc);
            }
            push_query_ascii(&mut out, c as u8, special);
        } else if run_start.is_none() {
            run_start = Some(idx);
        }
    }
    if let Some(s) = run_start {
        encode_run_pct(&mut out, &query[s..], enc);
    }
    Some(out)
}

/// Resolve the encoding to use for an HTML response, mirroring the HTML5
/// detection order. Returns the encoding and a tag describing where it was
/// picked from (for logging / tests).
pub fn detect_encoding<'a>(
    bytes: &'a [u8],
    content_type_header: Option<&str>,
) -> (&'static Encoding, &'static str) {
    if let Some(charset) = content_type_header.and_then(charset_from_content_type) {
        if let Some(enc) = Encoding::for_label(charset.as_bytes()) {
            return (enc, "content-type");
        }
    }
    if let Some(enc) = sniff_meta_charset(bytes) {
        return (enc, "meta-charset");
    }
    (UTF_8, "default-utf8")
}

/// Pull the `charset=` parameter out of a Content-Type header value.
fn charset_from_content_type(header: &str) -> Option<String> {
    for part in header.split(';') {
        let trimmed = part.trim();
        if let Some(rest) = trimmed
            .strip_prefix("charset=")
            .or_else(|| trimmed.strip_prefix("Charset="))
        {
            // Strip surrounding quotes if present.
            let value = rest.trim_matches(|c: char| c == '"' || c == '\'').trim();
            if !value.is_empty() {
                return Some(value.to_ascii_lowercase());
            }
        }
        // Some servers send `Content-Type: text/html; CHARSET = gbk`.
        let lower = trimmed.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("charset") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let value = rest.trim().trim_matches(|c: char| c == '"' || c == '\'');
                if !value.is_empty() {
                    return Some(value.to_ascii_lowercase());
                }
            }
        }
    }
    None
}

/// Return an exact attribute value from a lowercased `<meta ...>` tag.
///
/// This deliberately tokenizes attribute names instead of searching for a
/// substring: `data-charset` and a description containing `charset=...` are
/// not character encoding declarations.
fn meta_attribute<'a>(tag: &'a str, target: &str) -> Option<&'a str> {
    let mut rest = tag.strip_prefix("<meta")?;
    if rest
        .chars()
        .next()
        .is_some_and(|c| !c.is_ascii_whitespace() && !matches!(c, '>' | '/'))
    {
        return None;
    }

    while !rest.is_empty() {
        rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '/');
        if rest.is_empty() || rest.starts_with('>') {
            break;
        }

        let name_end = rest
            .find(|c: char| c.is_ascii_whitespace() || matches!(c, '=' | '>' | '/'))
            .unwrap_or(rest.len());
        if name_end == 0 {
            rest = &rest[rest.chars().next()?.len_utf8()..];
            continue;
        }

        let name = &rest[..name_end];
        rest = rest[name_end..].trim_start();

        let mut value = "";
        if let Some(after_equals) = rest.strip_prefix('=') {
            let after_equals = after_equals.trim_start();
            if let Some(quote) = after_equals
                .chars()
                .next()
                .filter(|c| matches!(c, '"' | '\''))
            {
                let quoted = &after_equals[quote.len_utf8()..];
                if let Some(end) = quoted.find(quote) {
                    value = &quoted[..end];
                    rest = &quoted[end + quote.len_utf8()..];
                } else {
                    value = quoted;
                    rest = "";
                }
            } else {
                let end = after_equals
                    .find(|c: char| c.is_ascii_whitespace() || matches!(c, '>' | '/'))
                    .unwrap_or(after_equals.len());
                value = &after_equals[..end];
                rest = &after_equals[end..];
            }
        }

        if name.eq_ignore_ascii_case(target) {
            return Some(value);
        }
    }

    None
}

/// Scan the first 1024 bytes for a `<meta charset="...">` or
/// `<meta http-equiv="Content-Type" content="...; charset=...">` declaration.
/// We only look at ASCII bytes; valid meta-charset declarations are always
/// ASCII regardless of the page's actual encoding.
fn sniff_meta_charset(bytes: &[u8]) -> Option<&'static Encoding> {
    let prefix_len = bytes.len().min(1024);
    let prefix = &bytes[..prefix_len];
    // Lossy is fine: any meta charset attribute is ASCII, even on a non-UTF-8 page.
    let s = String::from_utf8_lossy(prefix).to_ascii_lowercase();

    // Look for any `<meta ... charset=...>` pattern in the first 1KB. We
    // intentionally accept both the modern shorthand (`<meta charset=gbk>`)
    // and the legacy http-equiv form (`<meta http-equiv="content-type" content="text/html; charset=gbk">`).
    let mut pos = 0;
    while let Some(meta_start) = s[pos..].find("<meta") {
        let abs = pos + meta_start;
        // Find the closing `>` for this meta tag.
        let end = s[abs..].find('>').map(|e| abs + e).unwrap_or(s.len());
        let tag = &s[abs..end];

        if let Some(enc) = meta_attribute(tag, "charset")
            .filter(|value| !value.is_empty())
            .and_then(|value| Encoding::for_label(value.as_bytes()))
        {
            return Some(enc);
        }

        let is_legacy_declaration = meta_attribute(tag, "http-equiv")
            .is_some_and(|value| value.eq_ignore_ascii_case("content-type"));
        if is_legacy_declaration {
            if let Some(enc) = meta_attribute(tag, "content")
                .and_then(charset_from_content_type)
                .and_then(|value| Encoding::for_label(value.as_bytes()))
            {
                return Some(enc);
            }
        }

        pos = end + 1;
        if pos >= s.len() {
            break;
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_charset_wins() {
        let bytes = b"<html><head><meta charset=\"utf-8\"></head><body></body></html>";
        let (enc, source) = detect_encoding(bytes, Some("text/html; charset=gbk"));
        assert_eq!(enc.name(), "GBK");
        assert_eq!(source, "content-type");
    }

    #[test]
    fn content_type_quoted_charset_is_parsed() {
        let (enc, _) = detect_encoding(b"", Some("text/html; charset=\"Shift_JIS\""));
        assert_eq!(enc.name(), "Shift_JIS");
    }

    #[test]
    fn meta_charset_used_when_header_missing() {
        let bytes = b"<!doctype html><html><head><meta charset=\"big5\"></head></html>";
        let (enc, source) = detect_encoding(bytes, None);
        assert_eq!(enc.name(), "Big5");
        assert_eq!(source, "meta-charset");
    }

    #[test]
    fn meta_http_equiv_charset_is_recognized() {
        let bytes = b"<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=EUC-KR\"></head></html>";
        let (enc, _) = detect_encoding(bytes, None);
        assert_eq!(enc.name(), "EUC-KR");
    }

    #[test]
    fn unrelated_meta_attributes_do_not_declare_a_charset() {
        for bytes in [
            &b"<meta name=\"description\" content=\"charset=gbk\">"[..],
            &b"<meta data-charset=\"gbk\">"[..],
            &b"<metadata charset=\"gbk\">"[..],
        ] {
            let (enc, source) = detect_encoding(bytes, None);
            assert_eq!(enc.name(), "UTF-8");
            assert_eq!(source, "default-utf8");
        }
    }

    #[test]
    fn no_charset_anywhere_falls_back_to_utf8() {
        let bytes = b"<html><body>hello</body></html>";
        let (enc, source) = detect_encoding(bytes, None);
        assert_eq!(enc.name(), "UTF-8");
        assert_eq!(source, "default-utf8");
    }

    #[test]
    fn decode_html_gbk_bytes_roundtrip() {
        // "你好" (ni hao) encoded as GBK = C4 E3 BA C3
        let bytes: &[u8] = &[0xC4, 0xE3, 0xBA, 0xC3];
        let decoded = decode_html(bytes, Some("text/html; charset=gbk"));
        assert_eq!(decoded.text, "你好");
        assert_eq!(decoded.encoding_name, "GBK");
    }

    #[test]
    fn decode_text_skips_meta_sniff() {
        // A JS body that happens to contain a string `<meta charset="gbk">`
        // must NOT be decoded as GBK — non-HTML resources only honor the
        // HTTP header.
        let bytes = br#"var x = '<meta charset="gbk">'; // not the real charset"#;
        let s = decode_text(bytes, Some("application/javascript"));
        assert!(s.contains("<meta charset="));
    }

    #[test]
    fn text_decoder_unknown_label_is_none() {
        assert!(TextDecoder::new("not-an-encoding", TextDecoderOptions::default()).is_none());
    }

    #[test]
    fn text_decoder_fatal_rejects_invalid_bytes() {
        let dec = TextDecoder::new(
            "utf-8",
            TextDecoderOptions {
                fatal: true,
                ignore_bom: false,
            },
        )
        .unwrap();
        assert!(dec.decode(&[0x80]).is_none());
    }

    #[test]
    fn url_encode_query_eucjp_high_bytes() {
        // U+8108 (脈) is EUC-JP CC AE; both bytes are above 0x7E so both encode.
        assert_eq!(
            url_encode_query("\u{8108}", "euc-jp", true).unwrap(),
            "%CC%AE"
        );
    }

    #[test]
    fn url_encode_query_unmappable_becomes_ncr() {
        // A code point not in shift_jis becomes the percent-encoded &#NNN;.
        // U+3402 is a CJK ext-A han char not in shift_jis.
        let got = url_encode_query("\u{3402}", "shift_jis", true).unwrap();
        assert_eq!(got, "%26%2313314%3B");
    }

    #[test]
    fn url_encode_query_big5_low_trail_byte_is_escaped() {
        // U+4E00 (一) is Big5 A4 40; the 0x40 trail byte is ASCII '@' but must
        // still be percent-encoded because it serializes a non-ASCII char.
        assert_eq!(
            url_encode_query("\u{4e00}", "big5", true).unwrap(),
            "%A4%40"
        );
    }

    #[test]
    fn url_encode_query_keeps_ascii_structure() {
        // ASCII delimiters in a real query stay literal (standard query set):
        // only the non-ASCII value is re-encoded to the target charset.
        assert_eq!(
            url_encode_query("a=\u{8108}&b=c", "euc-jp", true).unwrap(),
            "a=%CC%AE&b=c"
        );
    }

    #[test]
    fn meta_sniff_only_scans_first_1kb() {
        let mut bytes = vec![b' '; 2048];
        bytes.extend_from_slice(b"<meta charset=\"gbk\">");
        let (enc, _) = detect_encoding(&bytes, None);
        // Beyond 1KB: ignored, fall back to UTF-8.
        assert_eq!(enc.name(), "UTF-8");
    }
}
