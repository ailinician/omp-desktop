//! The envelope of a CDP message: the top-level fields the broker routes on
//! and the proxy's tap observes, read without copying the rest of the
//! message.

use std::borrow::Cow;

use serde::Deserialize;
use serde_json::value::RawValue;

/// The fields of a CDP message read first. Strings are `Cow` borrowed from
/// the message; JSON may escape them (CDP's names and ids never need it),
/// and only then are they copied. `params` and `result` stay raw, for
/// [`parse`] to read when a message is acted on.
#[derive(Debug, Deserialize)]
pub struct Envelope<'a> {
    pub id: Option<u64>,
    #[serde(borrow)]
    pub method: Option<Str<'a>>,
    #[serde(borrow, rename = "sessionId")]
    pub session_id: Option<Str<'a>>,
    #[serde(borrow)]
    pub params: Option<&'a RawValue>,
    #[serde(borrow)]
    pub result: Option<&'a RawValue>,
}

impl<'a> Envelope<'a> {
    pub fn parse(text: &'a str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }

    pub fn method(&self) -> Option<&str> {
        self.method.as_ref().map(Str::as_str)
    }

    pub fn session(&self) -> Option<&str> {
        self.session_id.as_ref().map(Str::as_str)
    }
}

/// A string field inside an `Option`: serde borrows a field only when its
/// type is exactly `Cow<str>`, so `Option<Cow<str>>` would always copy.
#[derive(Debug, Deserialize)]
pub struct Str<'a>(#[serde(borrow)] pub Cow<'a, str>);

impl Str<'_> {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The session a `params` or `result` object names.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionId<'a> {
    #[serde(borrow)]
    pub session_id: Cow<'a, str>,
}

/// `raw` (a `params` or `result`) as `T`, when present and of that shape.
pub fn parse<'a, T: Deserialize<'a>>(raw: Option<&'a RawValue>) -> Option<T> {
    serde_json::from_str(raw?.get()).ok()
}

/// The `sessionId` inside a `params` or `result` object.
pub fn inner_session(raw: Option<&RawValue>) -> Option<String> {
    parse::<SessionId<'_>>(raw).map(|named| named.session_id.into_owned())
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{inner_session, Envelope, Str};

    #[test]
    fn envelope_strings_borrow_from_the_message_unless_escaped() {
        let message = r#"{"id":1,"method":"Page.navigate","sessionId":"S1","params":{}}"#;
        let envelope = Envelope::parse(message).unwrap();
        assert!(matches!(
            envelope.method,
            Some(Str(Cow::Borrowed("Page.navigate")))
        ));
        assert!(matches!(
            envelope.session_id,
            Some(Str(Cow::Borrowed("S1")))
        ));
        // An escaped string cannot borrow, and still reads right.
        let escaped = Envelope::parse(r#"{"method":"Page.\u006eavigate"}"#).unwrap();
        assert_eq!(escaped.method(), Some("Page.navigate"));
        assert_eq!(escaped.session(), None);
    }

    #[test]
    fn an_inner_session_must_be_a_string() {
        let read = |params: &str| {
            let text = format!(r#"{{"method":"m","params":{params}}}"#);
            inner_session(Envelope::parse(&text).unwrap().params)
        };
        assert_eq!(read(r#"{"sessionId":"C1","x":1}"#).as_deref(), Some("C1"));
        for params in [r#"{"sessionId":null}"#, r#"{"sessionId":7}"#, "{}", "[]"] {
            assert_eq!(read(params), None, "{params}");
        }
        assert_eq!(inner_session(None), None);
    }
}
