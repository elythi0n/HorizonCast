//! UPnP AV / DLNA helpers: device-description parsing and AVTransport SOAP control.
//!
//! The parsing and SOAP/DIDL builders are pure and unit-tested. The two network
//! functions ([`fetch_description`], [`send_soap_action`]) are thin wrappers over an
//! HTTP client, exercised against real renderers.

use std::time::Duration;

use hc_core::{Error, Result};

/// AVTransport service type we control.
pub const AV_TRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";

const HTTP_TIMEOUT: Duration = Duration::from_secs(3);

/// The subset of a UPnP device description we need.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeviceDescription {
    /// Human-readable device name.
    pub friendly_name: Option<String>,
    /// `controlURL` of the AVTransport service (may be relative to the description URL).
    pub av_transport_control_url: Option<String>,
}

/// Parse a UPnP device description XML document. Lenient: returns whatever fields it
/// can find and never panics on malformed input.
#[must_use]
pub fn parse_device_description(xml: &str) -> DeviceDescription {
    use quick_xml::XmlVersion;
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    #[derive(Clone, Copy, PartialEq)]
    enum Field {
        None,
        Friendly,
        ServiceType,
        ControlUrl,
    }

    let mut reader = Reader::from_str(xml);
    let mut desc = DeviceDescription::default();
    let mut field = Field::None;
    let mut in_service = false;
    let mut svc_type = String::new();
    let mut svc_ctrl = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"service" => {
                    in_service = true;
                    svc_type.clear();
                    svc_ctrl.clear();
                }
                b"friendlyName" => field = Field::Friendly,
                b"serviceType" if in_service => field = Field::ServiceType,
                b"controlURL" if in_service => field = Field::ControlUrl,
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if field != Field::None
                    && let Ok(txt) = e.xml_content(XmlVersion::Implicit1_0)
                {
                    match field {
                        Field::Friendly => {
                            if desc.friendly_name.is_none() && !txt.trim().is_empty() {
                                desc.friendly_name = Some(txt.trim().to_string());
                            }
                        }
                        Field::ServiceType => svc_type.push_str(txt.trim()),
                        Field::ControlUrl => svc_ctrl.push_str(txt.trim()),
                        Field::None => {}
                    }
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"service" => {
                    if in_service
                        && svc_type.contains("AVTransport")
                        && desc.av_transport_control_url.is_none()
                        && !svc_ctrl.is_empty()
                    {
                        desc.av_transport_control_url = Some(svc_ctrl.clone());
                    }
                    in_service = false;
                }
                b"friendlyName" | b"serviceType" | b"controlURL" => field = Field::None,
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break, // be lenient with malformed XML
            _ => {}
        }
        buf.clear();
    }

    desc
}

/// Resolve a possibly-relative `controlURL` against the description's `location` URL.
#[must_use]
pub fn resolve_url(location: &str, control_url: &str) -> Option<String> {
    if control_url.starts_with("http://") || control_url.starts_with("https://") {
        return Some(control_url.to_string());
    }
    let scheme_end = location.find("://")? + 3;
    let after = location.get(scheme_end..)?;
    let authority_len = after.find('/').unwrap_or(after.len());
    let base = location.get(..scheme_end + authority_len)?;
    if control_url.starts_with('/') {
        Some(format!("{base}{control_url}"))
    } else {
        Some(format!("{base}/{control_url}"))
    }
}

/// Escape a string for safe embedding in XML text/attribute content.
#[must_use]
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Build a minimal DIDL-Lite metadata document describing one video item.
///
/// `mime` (e.g. `video/mp4`) is embedded in the `<res protocolInfo>` attribute, which
/// many renderers — notably Samsung — require in order to accept the item.
#[must_use]
pub fn didl_lite_video(title: &str, uri: &str, mime: &str) -> String {
    let protocol_info = format!(
        "http-get:*:{mime}:{}",
        crate::media_server::DLNA_CONTENT_FEATURES
    );
    didl_with_protocol_info(title, uri, &protocol_info)
}

/// DIDL-Lite for a *live* (open-ended, non-seekable) video source — e.g. a screen-mirror
/// stream. Uses the live DLNA flags (`OP=00`) so the renderer treats it as a stream.
#[must_use]
pub fn didl_lite_live_video(title: &str, uri: &str, mime: &str) -> String {
    let protocol_info = format!(
        "http-get:*:{mime}:{}",
        crate::live_stream::DLNA_LIVE_FEATURES
    );
    didl_with_protocol_info(title, uri, &protocol_info)
}

fn didl_with_protocol_info(title: &str, uri: &str, protocol_info: &str) -> String {
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
         <item id=\"0\" parentID=\"-1\" restricted=\"1\">\
         <dc:title>{title}</dc:title>\
         <upnp:class>object.item.videoItem</upnp:class>\
         <res protocolInfo=\"{proto}\">{uri}</res>\
         </item></DIDL-Lite>",
        title = xml_escape(title),
        proto = xml_escape(protocol_info),
        uri = xml_escape(uri),
    )
}

fn soap_envelope(action: &str, inner: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{AV_TRANSPORT}\">{inner}</u:{action}></s:Body>\
         </s:Envelope>"
    )
}

/// SOAP body for `SetAVTransportURI` (tells the renderer which media to load).
#[must_use]
pub fn build_set_av_transport_uri(uri: &str, didl_metadata: &str) -> String {
    let inner = format!(
        "<InstanceID>0</InstanceID>\
         <CurrentURI>{}</CurrentURI>\
         <CurrentURIMetaData>{}</CurrentURIMetaData>",
        xml_escape(uri),
        xml_escape(didl_metadata),
    );
    soap_envelope("SetAVTransportURI", &inner)
}

/// SOAP body for `Play`.
#[must_use]
pub fn build_play() -> String {
    soap_envelope("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
}

/// SOAP body for `Stop`.
#[must_use]
pub fn build_stop() -> String {
    soap_envelope("Stop", "<InstanceID>0</InstanceID>")
}

/// The full `SOAPACTION` header value for an AVTransport `action`.
#[must_use]
pub fn soap_action_header(action: &str) -> String {
    format!("\"{AV_TRANSPORT}#{action}\"")
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| Error::Other(e.into()))
}

/// Fetch a UPnP device-description document over HTTP.
pub async fn fetch_description(url: &str) -> Result<String> {
    let resp = http_client()?
        .get(url)
        .send()
        .await
        .map_err(|e| Error::DeviceUnreachable(e.to_string()))?;
    resp.text()
        .await
        .map_err(|e| Error::Protocol(format!("reading description: {e}")))
}

/// POST a SOAP `body` for `action` to an AVTransport `control_url`, returning the
/// response body. Errors map to [`Error::Sink`] / [`Error::DeviceUnreachable`].
pub async fn send_soap_action(control_url: &str, action: &str, body: String) -> Result<String> {
    let resp = http_client()?
        .post(control_url)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header("SOAPACTION", soap_action_header(action))
        .body(body)
        .send()
        .await
        .map_err(|e| Error::DeviceUnreachable(e.to_string()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| Error::Protocol(format!("reading SOAP response: {e}")))?;
    if status.is_success() {
        Ok(text)
    } else {
        Err(Error::Sink(format!(
            "{action} failed: HTTP {status}: {text}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMSUNG_DESC: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>[TV] Samsung Q80</friendlyName>
    <manufacturer>Samsung Electronics</manufacturer>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <controlURL>/upnp/control/RenderingControl1</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <controlURL>/upnp/control/AVTransport1</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn parses_friendly_name_and_av_transport_control_url() {
        let d = parse_device_description(SAMSUNG_DESC);
        assert_eq!(d.friendly_name.as_deref(), Some("[TV] Samsung Q80"));
        assert_eq!(
            d.av_transport_control_url.as_deref(),
            Some("/upnp/control/AVTransport1")
        );
    }

    #[test]
    fn picks_av_transport_not_rendering_control() {
        // RenderingControl appears first; we must skip it and pick AVTransport.
        let d = parse_device_description(SAMSUNG_DESC);
        assert_eq!(
            d.av_transport_control_url.as_deref(),
            Some("/upnp/control/AVTransport1")
        );
    }

    #[test]
    fn malformed_xml_does_not_panic() {
        let d = parse_device_description("<root><device><friendlyName>X");
        assert_eq!(d.friendly_name.as_deref(), Some("X"));
    }

    #[test]
    fn empty_xml_yields_empty_description() {
        assert_eq!(parse_device_description(""), DeviceDescription::default());
    }

    #[test]
    fn resolves_relative_control_url() {
        let base = "http://192.168.1.50:9197/dmr/desc.xml";
        assert_eq!(
            resolve_url(base, "/upnp/control/AVTransport1").as_deref(),
            Some("http://192.168.1.50:9197/upnp/control/AVTransport1")
        );
    }

    #[test]
    fn resolves_relative_without_leading_slash() {
        let base = "http://10.0.0.2:80/desc.xml";
        assert_eq!(
            resolve_url(base, "ctrl/AV").as_deref(),
            Some("http://10.0.0.2:80/ctrl/AV")
        );
    }

    #[test]
    fn absolute_control_url_passthrough() {
        assert_eq!(
            resolve_url("http://x/desc.xml", "http://other:99/c").as_deref(),
            Some("http://other:99/c")
        );
    }

    #[test]
    fn xml_escape_handles_all_specials() {
        assert_eq!(
            xml_escape(r#"a&b<c>d"e'f"#),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn didl_escapes_title_and_uri_and_has_protocol_info() {
        let didl = didl_lite_video("Tom & Jerry", "http://h/v.mp4?a=1&b=2", "video/mp4");
        assert!(didl.contains("Tom &amp; Jerry"));
        assert!(didl.contains("a=1&amp;b=2"));
        assert!(didl.contains("object.item.videoItem"));
        // Samsung-required protocolInfo with the matching MIME type.
        assert!(didl.contains("protocolInfo="));
        assert!(didl.contains("http-get:*:video/mp4:"));
    }

    #[test]
    fn set_av_transport_uri_contains_uri_and_metadata() {
        let didl = didl_lite_video("Clip", "http://h/v.mp4", "video/mp4");
        let body = build_set_av_transport_uri("http://h/v.mp4", &didl);
        assert!(body.contains("SetAVTransportURI"));
        assert!(body.contains(AV_TRANSPORT));
        assert!(body.contains("<CurrentURI>http://h/v.mp4</CurrentURI>"));
        // DIDL is nested, so its angle brackets must be escaped inside the envelope.
        assert!(body.contains("&lt;DIDL-Lite"));
    }

    #[test]
    fn play_and_stop_bodies_are_well_formed() {
        assert!(build_play().contains("<u:Play"));
        assert!(build_play().contains("<Speed>1</Speed>"));
        assert!(build_stop().contains("<u:Stop"));
    }

    #[test]
    fn soap_action_header_format() {
        assert_eq!(
            soap_action_header("Play"),
            "\"urn:schemas-upnp-org:service:AVTransport:1#Play\""
        );
    }
}
