use encoding_rs::{Encoding, UTF_8};

/// How far into the page to look for a `<meta>` declaration.
const SNIFF_BYTES: usize = 2048;

/// Decodes a page the way a browser would: the `Content-Type` header wins,
/// then a byte order mark, then a `<meta>` declaration, then UTF-8. Pages that
/// are not UTF-8 are common enough outside English that treating everything as
/// UTF-8 turns whole articles into replacement characters.
pub fn decode(bytes: &[u8], content_type: Option<&str>) -> String {
    let encoding = content_type
        .and_then(charset_of)
        .and_then(|label| Encoding::for_label(label.as_bytes()))
        .or_else(|| Encoding::for_bom(bytes).map(|(encoding, _)| encoding))
        .or_else(|| sniff_meta(bytes))
        .unwrap_or(UTF_8);

    encoding.decode(bytes).0.into_owned()
}

fn charset_of(content_type: &str) -> Option<String> {
    let lowered = content_type.to_ascii_lowercase();
    let (_, rest) = lowered.split_once("charset=")?;
    let value = rest
        .trim_start()
        .trim_start_matches(['"', '\''])
        .split(['"', '\'', ';', ' '])
        .next()?
        .trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Looks for `<meta charset=…>` and the older
/// `<meta http-equiv="content-type" content="…charset=…">`.
fn sniff_meta(bytes: &[u8]) -> Option<&'static Encoding> {
    let head = &bytes[..bytes.len().min(SNIFF_BYTES)];
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();

    let mut rest = text.as_str();
    while let Some(at) = rest.find("charset") {
        rest = &rest[at + "charset".len()..];
        let value = rest.trim_start();
        let Some(value) = value.strip_prefix('=') else {
            continue;
        };
        let label = value
            .trim_start()
            .trim_start_matches(['"', '\''])
            .split(['"', '\'', '>', ';', ' ', '/'])
            .next()
            .unwrap_or("")
            .trim();
        if let Some(encoding) = Encoding::for_label(label.as_bytes()) {
            return Some(encoding);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_content_type_header_wins() {
        let windows_1252 = b"caf\xe9";
        let decoded = decode(windows_1252, Some("text/html; charset=windows-1252"));
        assert_eq!(decoded, "café");
    }

    #[test]
    fn a_quoted_charset_is_understood() {
        let decoded = decode(b"caf\xe9", Some("text/html; charset=\"iso-8859-1\""));
        assert_eq!(decoded, "café");
    }

    #[test]
    fn a_meta_tag_is_used_when_the_header_says_nothing() {
        let mut page =
            b"<html><head><meta charset=\"windows-1252\"><title>x</title></head><body>caf\xe9"
                .to_vec();
        page.extend_from_slice(b"</body></html>");
        assert!(decode(&page, Some("text/html")).contains("café"));
    }

    #[test]
    fn the_older_http_equiv_form_also_works() {
        let page = b"<html><head><meta http-equiv=\"content-type\" content=\"text/html; charset=iso-8859-1\"></head><body>caf\xe9</body></html>";
        assert!(decode(page, None).contains("café"));
    }

    #[test]
    fn utf8_is_the_fallback_and_a_bom_is_not_left_in_the_text() {
        assert_eq!(decode("héllo".as_bytes(), None), "héllo");
        assert_eq!(decode(b"\xef\xbb\xbfhi", None), "hi");
    }
}
