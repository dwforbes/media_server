use quick_xml::events::Event;
use quick_xml::Reader;

use crate::didl::xml_escape;

/// Pull the text of the first element named `name` (any namespace) out of a
/// SOAP request body.
pub fn param(body: &str, name: &str) -> Option<String> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut inside = false;
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) => {
                inside = e.local_name().as_ref() == name.as_bytes();
            }
            Event::Text(t) if inside => return t.unescape().ok().map(|s| s.to_string()),
            Event::Empty(e) if e.local_name().as_ref() == name.as_bytes() => {
                return Some(String::new())
            }
            Event::End(_) => {
                if inside {
                    // Empty element, e.g. <Filter></Filter>.
                    return Some(String::new());
                }
            }
            Event::Eof => return None,
            _ => {}
        }
    }
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
