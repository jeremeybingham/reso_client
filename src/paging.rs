// src/paging.rs

//! Server-driven paging: one page of results and the link to the next one
//!
//! A RESO Web API server decides how many records it returns per response. When there are
//! more, it says where to get them with an `@odata.nextLink` in the response body (the OData
//! standard). Some servers use a header instead: Bridge Interactive's `/replication` endpoint
//! sends a `next` header, and others send an RFC 8288 `Link: <url>; rel="next"` header.
//!
//! [`Page`] holds one response and the resolved, absolute link to the next page, whichever of
//! these the server used. Walk a result set with [`ResoClient::execute_page`] and
//! [`ResoClient::next_page`](crate::ResoClient::next_page).
//!
//! [`ResoClient::execute_page`]: crate::ResoClient::execute_page

use std::ops::Range;

use reqwest::header::HeaderMap;
use reqwest::Url;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserializer;
use serde_json::value::RawValue;
use serde_json::{Map, Value as JsonValue};
use tracing::debug;

use crate::error::{ResoError, Result};

/// One page of an OData collection response
///
/// `records` is the body's `value` array as the server sent it. `next_link` is the absolute
/// URL of the next page, or `None` on the last page. `body` keeps the rest of the response, so
/// annotations such as `@odata.context` stay available; the `value` array itself is moved into
/// `records` rather than kept twice.
///
/// # Examples
///
/// ```no_run
/// # use reso_client::{ResoClient, QueryBuilder};
/// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
/// let query = QueryBuilder::new("Property")
///     .filter("StandardStatus eq 'Active'")
///     .build()?;
///
/// let mut page = client.execute_page(&query).await?;
/// let mut total = page.records.len();
/// while let Some(next) = client.next_page(&page).await? {
///     total += next.records.len();
///     page = next;
/// }
/// println!("{} records", total);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Page {
    /// The records on this page (the body's `value` array)
    pub records: Vec<JsonValue>,

    /// Absolute URL of the next page, or `None` when this is the last page
    ///
    /// Taken from the body's `@odata.nextLink` when present, else from a `next` header, else
    /// from a `Link` header with `rel="next"`. Relative links are resolved to absolute ones.
    pub next_link: Option<String>,

    /// `@odata.count` when the query asked for it (`$count=true`)
    pub count: Option<u64>,

    /// The URL this page was fetched from
    pub url: String,

    /// The response body as the server sent it, without its `value` array
    ///
    /// Every other member (`@odata.context`, `@odata.count`, `@odata.nextLink`, vendor
    /// annotations) is kept. The `value` array has been moved into [`records`](Page::records),
    /// so the records are not held twice. A `value` that is not an array is left in place.
    pub body: JsonValue,
}

impl Page {
    /// Build a page from a response body, its headers and the URL it was fetched from
    ///
    /// The `value` array is moved out of the body into `records`, not copied (#165).
    pub(crate) fn from_response(mut body: JsonValue, headers: &HeaderMap, url: &str) -> Self {
        let records = match body.as_object_mut() {
            Some(obj) if obj.get("value").is_some_and(JsonValue::is_array) => {
                match obj.remove("value") {
                    Some(JsonValue::Array(records)) => records,
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        };
        Self::from_parts(records, body, headers, url)
    }

    /// Build a page from a response body and keep each record's JSON text as sent (#162)
    ///
    /// Records are parsed as [`from_response`](Self::from_response) parses them; in addition
    /// the returned [`RawRecords`] holds the text of each record, in the same order.
    pub(crate) fn from_raw_body(
        body: Vec<u8>,
        headers: &HeaderMap,
        url: &str,
    ) -> Result<(Self, RawRecords)> {
        let parse_error =
            |e: serde_json::Error| ResoError::Parse(format!("Failed to parse JSON: {}", e));
        let text = String::from_utf8(body)
            .map_err(|e| ResoError::Parse(format!("Failed to parse JSON: {}", e.utf8_error())))?;
        if !text.trim_start().starts_with('{') {
            // Not an object (no records): parsed as `from_response` parses it
            let body = serde_json::from_str(&text).map_err(parse_error)?;
            return Ok((
                Self::from_response(body, headers, url),
                RawRecords::default(),
            ));
        }
        let mut de = serde_json::Deserializer::from_str(&text);
        let (rest, value) = (&mut de)
            .deserialize_any(BodyVisitor)
            .map_err(parse_error)?;
        de.end().map_err(parse_error)?;

        let Some(items) = value else {
            // `value` missing or not an array: no records; parsed as `from_response` parses it
            let body = serde_json::from_str(&text).map_err(parse_error)?;
            return Ok((
                Self::from_response(body, headers, url),
                RawRecords::default(),
            ));
        };
        let mut records = Vec::with_capacity(items.len());
        let mut spans = Vec::with_capacity(items.len());
        for item in items {
            records.push(serde_json::from_str(item.get()).map_err(parse_error)?);
            let start = item.get().as_ptr() as usize - text.as_ptr() as usize;
            spans.push(start..start + item.get().len());
        }
        let page = Self::from_parts(records, JsonValue::Object(rest), headers, url);
        Ok((page, RawRecords { text, spans }))
    }

    fn from_parts(
        records: Vec<JsonValue>,
        body: JsonValue,
        headers: &HeaderMap,
        url: &str,
    ) -> Self {
        let count = body.get("@odata.count").and_then(|v| v.as_u64());
        let next_link = next_link_from(&body, headers, url);
        Self {
            records,
            next_link,
            count,
            url: url.to_string(),
            body,
        }
    }

    /// Whether the server offered another page
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::{ResoClient, QueryBuilder};
    /// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    /// let page = client.execute_page(&QueryBuilder::new("Property").build()?).await?;
    /// if page.has_more() {
    ///     println!("more at {}", page.next_link.as_deref().unwrap_or_default());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn has_more(&self) -> bool {
        self.next_link.is_some()
    }
}

/// The JSON text of each record on a page, exactly as the server sent it
///
/// Returned with the parsed records by
/// [`ReplicationRun::next_batch_raw`](crate::ReplicationRun::next_batch_raw), in the same order,
/// so a writer can copy a record's bytes instead of serializing the parsed value again. The text
/// keeps the server's member order, number formatting, string escapes and any whitespace inside
/// the record.
///
/// # Examples
///
/// ```no_run
/// # use reso_client::{ResoClient, StandardReplication};
/// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
/// let mut run = client.replicate(StandardReplication::new("Property", "ListingKey"));
/// while let Some(batch) = run.next_batch_raw().await? {
///     for (record, text) in batch.records.iter().zip(batch.raw.iter()) {
///         println!("{} -> {}", record["ListingKey"], text);
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct RawRecords {
    text: String,
    spans: Vec<Range<usize>>,
}

impl RawRecords {
    /// Number of records
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Whether there are no records
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// The text of record `i`, or `None` past the end
    pub fn get(&self, i: usize) -> Option<&str> {
        self.spans.get(i).map(|r| &self.text[r.clone()])
    }

    /// The text of each record, in order
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        self.spans.iter().map(|r| &self.text[r.clone()])
    }
}

/// Reads a response body's top-level members: the `value` array as the raw text of each element
/// (one scan, no parsing), the rest as values
///
/// The second item is `None` when there is no `value` member or it is not an array.
struct BodyVisitor;

impl<'de> Visitor<'de> for BodyVisitor {
    type Value = (Map<String, JsonValue>, Option<Vec<&'de RawValue>>);

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut rest = Map::new();
        let mut value = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "value" {
                // A repeated member replaces the earlier one, as when parsing into a Value
                value = map.next_value_seed(RawArraySeed)?;
            } else {
                rest.insert(key, map.next_value()?);
            }
        }
        Ok((rest, value))
    }
}

/// Reads an array as the raw text of each element; anything else is skipped and gives `None`
struct RawArraySeed;

impl<'de> DeserializeSeed<'de> for RawArraySeed {
    type Value = Option<Vec<&'de RawValue>>;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> std::result::Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for RawArraySeed {
    type Value = Option<Vec<&'de RawValue>>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_seq<A: SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element::<&'de RawValue>()? {
            items.push(item);
        }
        Ok(Some(items))
    }

    fn visit_map<A: MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(None)
    }

    fn visit_bool<E>(self, _: bool) -> std::result::Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_i64<E>(self, _: i64) -> std::result::Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_u64<E>(self, _: u64) -> std::result::Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_f64<E>(self, _: f64) -> std::result::Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_str<E>(self, _: &str) -> std::result::Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(None)
    }
}

/// Find the next-page link in a response and resolve it to an absolute URL
///
/// Order: the body's `@odata.nextLink`, then a `next` header (Bridge), then a `Link` header
/// with `rel="next"`. A relative link is resolved against the body's `@odata.context` when it
/// has one, else against the request URL (OData JSON Format 4.01, section 4.6). A link that
/// cannot be parsed is dropped and logged at debug level, which ends paging.
pub(crate) fn next_link_from(body: &JsonValue, headers: &HeaderMap, url: &str) -> Option<String> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());

    let raw = body
        .get("@odata.nextLink")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| header("next").map(str::to_string))
        .or_else(|| header("link").and_then(parse_link_header))?;

    let base = base_url(body, url)?;
    match base.join(raw.trim()) {
        Ok(resolved) => Some(resolved.to_string()),
        Err(e) => {
            debug!("Ignoring next link that cannot be resolved: {}", e);
            None
        }
    }
}

/// The URL relative links in a body are resolved against: `@odata.context`, else the request URL
fn base_url(body: &JsonValue, url: &str) -> Option<Url> {
    let request = Url::parse(url).ok()?;
    let context = body
        .get("@odata.context")
        .and_then(|v| v.as_str())
        .and_then(|c| request.join(c).ok());
    Some(context.unwrap_or(request))
}

/// Read the `rel="next"` target from a `Link` header value
///
/// Handles the RFC 8288 form (`<url>; rel="next"`, several links separated by commas, `rel`
/// quoted or not, several relation types in one `rel`). A value with no angle brackets is taken
/// as a bare URL, which is how some replication endpoints send it. Returns `None` when no link
/// has `rel="next"`.
///
/// # Examples
///
/// ```
/// # use reso_client::paging::parse_link_header;
/// let header = r#"<https://api.example.com/Property?$skip=100>; rel="next", <https://api.example.com/Property>; rel="first""#;
/// assert_eq!(
///     parse_link_header(header).as_deref(),
///     Some("https://api.example.com/Property?$skip=100")
/// );
/// assert_eq!(parse_link_header(r#"<https://a.example/x>; rel="prev""#), None);
/// assert_eq!(
///     parse_link_header("https://api.example.com/next").as_deref(),
///     Some("https://api.example.com/next")
/// );
/// ```
pub fn parse_link_header(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if !value.contains('<') {
        return Some(value.to_string());
    }

    let mut rest = value;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let end = after.find('>')?;
        let target = &after[..end];
        let params_end = after[end + 1..]
            .find('<')
            .map(|i| end + 1 + i)
            .unwrap_or(after.len());
        let params = &after[end + 1..params_end];
        if has_rel_next(params) {
            return Some(target.trim().to_string());
        }
        rest = &after[params_end..];
    }
    None
}

/// Whether a link's parameters (`; rel="next"; title=...`) include the `next` relation
fn has_rel_next(params: &str) -> bool {
    params.split(';').any(|param| {
        let mut parts = param.splitn(2, '=');
        let name = parts.next().unwrap_or_default().trim();
        let value = parts.next().unwrap_or_default().trim();
        name.eq_ignore_ascii_case("rel")
            && value
                .trim_end_matches(',')
                .trim()
                .trim_matches('"')
                .split_whitespace()
                .any(|rel| rel.eq_ignore_ascii_case("next"))
    })
}

/// Refuse a next link that would send the credential to a different server
///
/// The client sends its bearer token with every request, so following a link to another
/// scheme, host or port would hand the token to that server. `base_url` is the configured
/// server address.
pub(crate) fn check_same_origin(base_url: &str, link: &str) -> Result<()> {
    let base = Url::parse(base_url)
        .map_err(|e| ResoError::Config(format!("Base URL is not a valid URL: {}", e)))?;
    let target = Url::parse(link)
        .map_err(|e| ResoError::Parse(format!("Next link is not a valid URL: {}", e)))?;
    if base.origin() == target.origin() {
        Ok(())
    } else {
        Err(ResoError::Parse(format!(
            "Next link points to a different server ({}) than the configured one ({}); \
             not following it, so the credential is not sent there",
            target.origin().ascii_serialization(),
            base.origin().ascii_serialization()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use serde_json::json;

    const URL: &str = "https://api.example.com/odata/Property?$filter=City%20eq%20'Austin'";

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn body_link_absolute() {
        let body = json!({"value": [], "@odata.nextLink": "https://api.example.com/odata/Property?$skip=100"});
        assert_eq!(
            next_link_from(&body, &HeaderMap::new(), URL).as_deref(),
            Some("https://api.example.com/odata/Property?$skip=100")
        );
    }

    #[test]
    fn body_link_relative_to_request_url() {
        let body = json!({"value": [], "@odata.nextLink": "Property?$skip=100"});
        assert_eq!(
            next_link_from(&body, &HeaderMap::new(), URL).as_deref(),
            Some("https://api.example.com/odata/Property?$skip=100")
        );
    }

    #[test]
    fn body_link_relative_to_context_url() {
        let body = json!({
            "@odata.context": "https://api.example.com/odata/$metadata#Property",
            "@odata.nextLink": "Property?$skiptoken=abc",
            "value": []
        });
        let url = "https://api.example.com/odata/Property";
        assert_eq!(
            next_link_from(&body, &HeaderMap::new(), url).as_deref(),
            Some("https://api.example.com/odata/Property?$skiptoken=abc")
        );
    }

    #[test]
    fn body_link_root_relative() {
        let body = json!({"value": [], "@odata.nextLink": "/odata/Property?$skip=100"});
        assert_eq!(
            next_link_from(&body, &HeaderMap::new(), URL).as_deref(),
            Some("https://api.example.com/odata/Property?$skip=100")
        );
    }

    #[test]
    fn body_link_wins_over_headers() {
        let body = json!({"value": [], "@odata.nextLink": "https://api.example.com/odata/Property?$skip=2"});
        let h = headers(&[
            (
                "next",
                "https://api.example.com/odata/Property?from-next-header",
            ),
            (
                "link",
                "<https://api.example.com/odata/Property?from-link>; rel=\"next\"",
            ),
        ]);
        assert_eq!(
            next_link_from(&body, &h, URL).as_deref(),
            Some("https://api.example.com/odata/Property?$skip=2")
        );
    }

    #[test]
    fn next_header_then_link_header() {
        let body = json!({"value": []});
        let h = headers(&[
            (
                "next",
                "https://api.example.com/odata/Property/replication?next=1",
            ),
            (
                "link",
                "<https://api.example.com/odata/Property?from-link>; rel=\"next\"",
            ),
        ]);
        assert_eq!(
            next_link_from(&body, &h, URL).as_deref(),
            Some("https://api.example.com/odata/Property/replication?next=1")
        );

        let h = headers(&[("link", "<Property?$skip=5>; rel=\"next\"")]);
        assert_eq!(
            next_link_from(&body, &h, URL).as_deref(),
            Some("https://api.example.com/odata/Property?$skip=5")
        );
    }

    #[test]
    fn no_link_anywhere() {
        let body = json!({"value": [{"ListingKey": "1"}]});
        assert_eq!(next_link_from(&body, &HeaderMap::new(), URL), None);
    }

    #[test]
    fn unresolvable_link_ends_paging() {
        // An unclosed IPv6 host cannot be parsed, so the link is dropped rather than followed
        let body = json!({"value": [], "@odata.nextLink": "https://[::1/Property?$skip=5"});
        assert_eq!(next_link_from(&body, &HeaderMap::new(), URL), None);
    }

    #[test]
    fn link_header_forms() {
        assert_eq!(
            parse_link_header("<https://a.example/p?$skip=2>; rel=next").as_deref(),
            Some("https://a.example/p?$skip=2")
        );
        assert_eq!(
            parse_link_header(
                "<https://a.example/first>; rel=\"first\", <https://a.example/2>; rel=\"next last\""
            )
            .as_deref(),
            Some("https://a.example/2")
        );
        assert_eq!(
            parse_link_header("<https://a.example/2>; title=\"x\"; REL=\"Next\"").as_deref(),
            Some("https://a.example/2")
        );
        assert_eq!(
            parse_link_header("<https://a.example/2>; rel=next, <https://a.example/1>; rel=prev")
                .as_deref(),
            Some("https://a.example/2")
        );
        assert_eq!(
            parse_link_header("<https://a.example/2>; rel=\"prev\""),
            None
        );
        assert_eq!(parse_link_header(""), None);
    }

    #[test]
    fn malformed_link_header_is_ignored() {
        // An opening bracket with no closing one, and a link with no rel at all
        assert_eq!(
            parse_link_header("<https://a.example/2; rel=\"next\""),
            None
        );
        assert_eq!(parse_link_header("<https://a.example/2>"), None);
        let body = json!({"value": []});
        let h = headers(&[("link", "<https://a.example/2; rel=\"next\"")]);
        assert_eq!(next_link_from(&body, &h, URL), None);
    }

    #[test]
    fn page_from_response() {
        let body = json!({
            "@odata.count": 3,
            "@odata.nextLink": "https://api.example.com/odata/Property?$skip=2",
            "value": [{"ListingKey": "1"}, {"ListingKey": "2"}]
        });
        let page = Page::from_response(body, &HeaderMap::new(), URL);
        assert_eq!(page.records.len(), 2);
        assert_eq!(page.count, Some(3));
        assert!(page.has_more());
        assert_eq!(page.url, URL);
        // The records are moved out of the body, not copied; the annotations stay (#165)
        assert_eq!(page.records[0]["ListingKey"], "1");
        assert!(page.body.get("value").is_none());
        assert_eq!(page.body["@odata.count"], 3);
        assert!(page.body.get("@odata.nextLink").is_some());
    }

    #[test]
    fn page_with_non_array_value_keeps_it_in_the_body() {
        let page = Page::from_response(
            json!({"value": {"ListingKey": "1"}}),
            &HeaderMap::new(),
            URL,
        );
        assert!(page.records.is_empty());
        assert_eq!(page.body["value"]["ListingKey"], "1");
    }

    #[test]
    fn page_without_value_is_empty() {
        let page = Page::from_response(json!({}), &HeaderMap::new(), URL);
        assert!(page.records.is_empty());
        assert!(!page.has_more());
        assert_eq!(page.count, None);
    }

    #[test]
    fn same_origin_check() {
        let base = "https://api.example.com/odata";
        assert!(check_same_origin(base, "https://api.example.com/odata/Property?$skip=2").is_ok());
        assert!(check_same_origin(base, "https://api.example.com:443/other").is_ok());
        assert!(check_same_origin(base, "https://evil.example.com/odata/Property").is_err());
        assert!(check_same_origin(base, "http://api.example.com/odata/Property").is_err());
        assert!(check_same_origin(base, "https://api.example.com:8443/odata").is_err());
        assert!(check_same_origin(base, "not a url").is_err());
    }

    #[test]
    fn raw_body_keeps_record_text_and_matches_parsed_page() {
        let body = r#"{"@odata.context":"https://api.example.com/odata/$metadata#Property",
            "value": [ {"Z":1.50,"A":"café"} ,
              {"B": [1, 2],
               "K": 12345678901234567890123} ],
            "@odata.count": 2,
            "@odata.nextLink":"Property?$skip=2"}"#;
        let (page, raw) =
            Page::from_raw_body(body.as_bytes().to_vec(), &HeaderMap::new(), URL).unwrap();
        let parsed =
            Page::from_response(serde_json::from_str(body).unwrap(), &HeaderMap::new(), URL);
        assert_eq!(page.records, parsed.records);
        assert_eq!(page.body, parsed.body);
        assert_eq!(
            (page.count, page.next_link.clone()),
            (parsed.count, parsed.next_link)
        );
        assert_eq!(raw.len(), 2);
        // Member order, number text and escapes as sent
        assert_eq!(raw.get(0), Some(r#"{"Z":1.50,"A":"café"}"#));
        assert_eq!(
            raw.get(1),
            Some("{\"B\": [1, 2],\n               \"K\": 12345678901234567890123}")
        );
        assert_eq!(raw.get(2), None);
        assert_eq!(raw.iter().count(), 2);
    }

    #[test]
    fn raw_body_without_a_value_array_matches_parsed_page() {
        for body in [
            r#"{"value":{"x":1},"a":2}"#,
            r#"{"a":2}"#,
            "[1,2]",
            r#"{"value":[]}"#,
        ] {
            let (page, raw) =
                Page::from_raw_body(body.as_bytes().to_vec(), &HeaderMap::new(), URL).unwrap();
            let parsed =
                Page::from_response(serde_json::from_str(body).unwrap(), &HeaderMap::new(), URL);
            assert_eq!(
                (page.records, page.body),
                (parsed.records, parsed.body),
                "{body}"
            );
            assert!(raw.is_empty(), "{body}");
        }
    }

    #[test]
    fn raw_body_rejects_invalid_json() {
        for body in [
            &b"{\"value\":[1,"[..],
            b"{\"value\":[]} x",
            b"{\"a\":\"\xff\"}",
            b"",
        ] {
            let err = Page::from_raw_body(body.to_vec(), &HeaderMap::new(), URL).unwrap_err();
            assert!(matches!(err, ResoError::Parse(_)), "{err:?}");
        }
    }
}
