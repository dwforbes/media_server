use quick_xml::events::{BytesRef, Event};
use quick_xml::Reader;

use crate::didl::xml_escape;

/// Pull the text of the first element named `name` (any namespace) out of a
/// SOAP request body. The reader hands character and entity references
/// over as events of their own, so the element's text is reassembled up
/// to its end tag, then trimmed. Anything malformed yields None.
pub fn param(body: &str, name: &str) -> Option<String> {
    let mut reader = Reader::from_str(body);
    let mut inside = false;
    let mut text = String::new();
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) => {
                inside = e.local_name().as_ref() == name.as_bytes();
                text.clear();
            }
            Event::Empty(e) if e.local_name().as_ref() == name.as_bytes() => {
                return Some(String::new())
            }
            Event::Text(t) if inside => text.push_str(&t.xml10_content().ok()?),
            Event::CData(c) if inside => text.push_str(&c.decode().ok()?),
            Event::GeneralRef(r) if inside => text.push_str(&resolve_ref(&r)?),
            Event::End(_) if inside => return Some(text.trim().to_string()),
            Event::Eof => return None,
            _ => {}
        }
    }
}

/// A `&…;` reference as text: numeric character references and the five
/// predefined entities resolve. Anything else would need a DTD, which
/// this parser never reads (no entity expansion, by design), so it stays
/// literal.
fn resolve_ref(r: &BytesRef) -> Option<String> {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return Some(c.to_string());
    }
    let name = r.decode().ok()?;
    Some(match &*name {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        other => format!("&{other};"),
    })
}

/// The action name from a SOAPACTION header value like
/// "urn:schemas-upnp-org:service:ContentDirectory:1#Browse".
pub fn action_from_header(value: &str) -> Option<String> {
    let trimmed = value.trim_matches('"');
    Some(trimmed.rsplit_once('#')?.1.to_string())
}

/// Wrap an action response body in a SOAP envelope.
/// `args` are (name, already-escaped-value) pairs.
pub fn envelope(service: &str, action: &str, args: &[(&str, String)]) -> String {
    let mut inner = String::new();
    for (name, value) in args {
        inner.push_str(&format!("<{name}>{value}</{name}>"));
    }
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action}Response xmlns:u="{service}">{inner}</u:{action}Response></s:Body></s:Envelope>"#
    )
}

/// A UPnP SOAP fault (used with HTTP status 500).
pub fn fault(code: u32, description: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>{code}</errorCode><errorDescription>{}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#,
        xml_escape(description)
    )
}

#[cfg(test)]
mod tests {
    use super::param;

    #[test]
    fn param_reassembles_text_around_references() {
        let body = r#"<s:Envelope xmlns:s="x"><s:Body><u:Search xmlns:u="y">            <ContainerID>0</ContainerID>            <SearchCriteria> dc:title contains "Tom &amp; Jerry" &#x26; &#38; more </SearchCriteria>            <Filter><![CDATA[a<b]]></Filter><Empty/><Blank></Blank>            <Custom>&nope;</Custom></u:Search></s:Body></s:Envelope>"#;
        assert_eq!(
            param(body, "SearchCriteria").as_deref(),
            Some(r#"dc:title contains "Tom & Jerry" & & more"#)
        );
        assert_eq!(param(body, "Filter").as_deref(), Some("a<b"));
        assert_eq!(param(body, "Empty").as_deref(), Some(""));
        assert_eq!(param(body, "Blank").as_deref(), Some(""));
        assert_eq!(param(body, "Custom").as_deref(), Some("&nope;"));
        assert_eq!(param(body, "Missing"), None);
        assert_eq!(param("<broken", "Filter"), None);
        assert_eq!(param("", "Filter"), None);
    }
}
