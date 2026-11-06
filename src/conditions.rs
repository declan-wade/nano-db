use axum::http::{HeaderMap, header};

use crate::error::ApiError;

#[derive(Clone, Default)]
pub(crate) struct Conditions {
    if_match: Option<TagSet>,
    if_none_match: Option<TagSet>,
}

#[derive(Clone)]
enum TagSet {
    Any,
    Tags(Vec<Tag>),
}

#[derive(Clone)]
struct Tag {
    weak: bool,
    opaque: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Proceed,
    NotModified,
    Failed,
}

impl Conditions {
    pub fn parse(headers: &HeaderMap) -> Result<Self, ApiError> {
        if headers.contains_key(header::IF_MODIFIED_SINCE)
            || headers.contains_key(header::IF_UNMODIFIED_SINCE)
        {
            return Err(ApiError::bad_request(
                "unsupported_precondition",
                "use ETag preconditions, not date preconditions",
            ));
        }
        Ok(Self {
            if_match: parse_set(headers, header::IF_MATCH)?,
            if_none_match: parse_set(headers, header::IF_NONE_MATCH)?,
        })
    }

    pub fn create_only() -> Self {
        Self {
            if_match: None,
            if_none_match: Some(TagSet::Any),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.if_match.is_none() && self.if_none_match.is_none()
    }

    pub fn evaluate(&self, etag: Option<&str>, read: bool) -> Decision {
        if let Some(tags) = &self.if_match
            && !tags.matches(etag, true)
        {
            return Decision::Failed;
        }
        if let Some(tags) = &self.if_none_match
            && tags.matches(etag, false)
        {
            return if read {
                Decision::NotModified
            } else {
                Decision::Failed
            };
        }
        Decision::Proceed
    }
}

impl TagSet {
    fn matches(&self, etag: Option<&str>, strong: bool) -> bool {
        let Some(etag) = etag else { return false };
        match self {
            Self::Any => true,
            Self::Tags(tags) => {
                let opaque = &etag.as_bytes()[1..etag.len() - 1];
                tags.iter()
                    .any(|tag| (!strong || !tag.weak) && tag.opaque == opaque)
            }
        }
    }
}

fn parse_set(headers: &HeaderMap, name: header::HeaderName) -> Result<Option<TagSet>, ApiError> {
    let mut input = Vec::new();
    let mut present = false;
    for value in headers.get_all(&name).iter() {
        let separator = usize::from(present);
        if input.len() + separator + value.as_bytes().len() > 8192 {
            return Err(ApiError::bad_request(
                "invalid_precondition",
                "conditional header exceeds 8192 bytes",
            ));
        }
        if separator != 0 {
            input.push(b',');
        }
        input.extend_from_slice(value.as_bytes());
        present = true;
    }
    if !present {
        return Ok(None);
    }
    let mut offset = 0;
    whitespace(&input, &mut offset);
    if input.get(offset) == Some(&b'*') {
        offset += 1;
        whitespace(&input, &mut offset);
        return if offset == input.len() {
            Ok(Some(TagSet::Any))
        } else {
            Err(invalid())
        };
    }
    let mut tags = Vec::new();
    let mut empty_members = 0;
    loop {
        whitespace(&input, &mut offset);
        if offset == input.len() {
            return Ok(Some(TagSet::Tags(tags)));
        }
        if input.get(offset) == Some(&b',') {
            empty_members += 1;
            if empty_members > 64 {
                return Err(invalid());
            }
            offset += 1;
            continue;
        }
        let weak = input[offset..].starts_with(b"W/");
        if weak {
            offset += 2;
        }
        if input.get(offset) != Some(&b'"') {
            return Err(invalid());
        }
        offset += 1;
        let start = offset;
        while let Some(&byte) = input.get(offset) {
            if byte == b'"' {
                break;
            }
            if byte != 0x21 && !(0x23..=0x7e).contains(&byte) && byte < 0x80 {
                return Err(invalid());
            }
            offset += 1;
        }
        if input.get(offset) != Some(&b'"') || tags.len() >= 64 {
            return Err(invalid());
        }
        tags.push(Tag {
            weak,
            opaque: input[start..offset].to_vec(),
        });
        offset += 1;
        whitespace(&input, &mut offset);
        if offset == input.len() {
            return Ok(Some(TagSet::Tags(tags)));
        }
        if input.get(offset) != Some(&b',') {
            return Err(invalid());
        }
        offset += 1;
    }
}

fn whitespace(input: &[u8], offset: &mut usize) {
    while input
        .get(*offset)
        .is_some_and(|b| matches!(b, b' ' | b'\t'))
    {
        *offset += 1;
    }
}

fn invalid() -> ApiError {
    ApiError::bad_request(
        "invalid_precondition",
        "invalid entity-tag list (maximum 64 tags)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_comparison_and_ordering_follow_http_semantics() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            r#"W/"tag", "other,tag""#.parse().unwrap(),
        );
        let conditions = Conditions::parse(&headers).unwrap();
        assert_eq!(
            conditions.evaluate(Some(r#""tag""#), true),
            Decision::NotModified
        );
        assert_eq!(
            conditions.evaluate(Some(r#""tag""#), false),
            Decision::Failed
        );
        headers.insert(header::IF_MATCH, r#"W/"tag""#.parse().unwrap());
        assert_eq!(
            Conditions::parse(&headers)
                .unwrap()
                .evaluate(Some(r#""tag""#), true),
            Decision::Failed
        );
        assert_eq!(
            Conditions::create_only().evaluate(None, false),
            Decision::Proceed
        );
    }

    #[test]
    fn rejects_malformed_headers_and_parses_multiple_fields() {
        for value in ["* , \"a\"", "w/\"a\"", "\"a b\"", "\"a"] {
            let mut headers = HeaderMap::new();
            headers.insert(header::IF_MATCH, value.parse().unwrap());
            assert!(Conditions::parse(&headers).is_err(), "{value}");
        }
        let mut headers = HeaderMap::new();
        headers.append(header::IF_MATCH, r#""a""#.parse().unwrap());
        headers.append(header::IF_MATCH, r#""b""#.parse().unwrap());
        assert_eq!(
            Conditions::parse(&headers)
                .unwrap()
                .evaluate(Some(r#""b""#), false),
            Decision::Proceed
        );
        headers.insert(header::IF_MATCH, r#",, "b", ,"#.parse().unwrap());
        assert_eq!(
            Conditions::parse(&headers)
                .unwrap()
                .evaluate(Some(r#""b""#), false),
            Decision::Proceed
        );
        headers.insert(header::IF_MATCH, "".parse().unwrap());
        assert_eq!(
            Conditions::parse(&headers)
                .unwrap()
                .evaluate(Some(r#""b""#), false),
            Decision::Failed
        );
    }
}
