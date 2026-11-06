use std::collections::HashSet;

#[derive(Debug)]
pub(crate) struct ValidationError {
    pub offset: usize,
    pub message: &'static str,
}

pub(crate) fn validate(bytes: &[u8], max_depth: usize) -> Result<(), ValidationError> {
    if let Err(error) = std::str::from_utf8(bytes) {
        return Err(ValidationError {
            offset: error.valid_up_to(),
            message: "JSON must be valid UTF-8",
        });
    }
    let mut parser = Parser {
        bytes,
        offset: 0,
        max_depth,
    };
    parser.value(0)?;
    parser.whitespace();
    if parser.offset != bytes.len() {
        return Err(parser.error("unexpected content after JSON value"));
    }
    Ok(())
}

struct Parser<'a> {
    bytes: &'a [u8],
    offset: usize,
    max_depth: usize,
}

impl Parser<'_> {
    fn error(&self, message: &'static str) -> ValidationError {
        ValidationError {
            offset: self.offset,
            message,
        }
    }

    fn whitespace(&mut self) {
        while self
            .peek()
            .is_some_and(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.offset += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Result<(), ValidationError> {
        self.whitespace();
        match self.peek() {
            Some(b'{' | b'[') => {
                if depth >= self.max_depth {
                    return Err(self.error("JSON nesting limit exceeded"));
                }
                if self.consume(b'{') {
                    self.object(depth + 1)
                } else {
                    self.offset += 1;
                    self.array(depth + 1)
                }
            }
            Some(b'"') => self.string().map(|_| ()),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            _ => Err(self.error("expected a JSON value")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<(), ValidationError> {
        let mut keys = HashSet::new();
        self.whitespace();
        if self.consume(b'}') {
            return Ok(());
        }
        loop {
            self.whitespace();
            let start = self.offset;
            let key = self.string()?;
            if !keys.insert(key) {
                return Err(ValidationError {
                    offset: start,
                    message: "duplicate object key",
                });
            }
            self.whitespace();
            if !self.consume(b':') {
                return Err(self.error("expected ':' after object key"));
            }
            self.value(depth)?;
            self.whitespace();
            if self.consume(b'}') {
                return Ok(());
            }
            if !self.consume(b',') {
                return Err(self.error("expected ',' or '}'"));
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<(), ValidationError> {
        self.whitespace();
        if self.consume(b']') {
            return Ok(());
        }
        loop {
            self.value(depth)?;
            self.whitespace();
            if self.consume(b']') {
                return Ok(());
            }
            if !self.consume(b',') {
                return Err(self.error("expected ',' or ']'"));
            }
        }
    }

    fn string(&mut self) -> Result<String, ValidationError> {
        let start = self.offset;
        if !self.consume(b'"') {
            return Err(self.error("expected a JSON string"));
        }
        loop {
            match self.peek() {
                Some(b'"') => {
                    self.offset += 1;
                    return serde_json::from_slice(&self.bytes[start..self.offset]).map_err(|_| {
                        ValidationError {
                            offset: start,
                            message: "invalid JSON string or Unicode escape",
                        }
                    });
                }
                Some(b'\\') => {
                    self.offset += 1;
                    if self.peek().is_none() {
                        return Err(self.error("unterminated JSON escape"));
                    }
                    self.offset += 1;
                }
                Some(0..=0x1f) => {
                    return Err(self.error("unescaped control character in JSON string"));
                }
                Some(_) => self.offset += 1,
                None => return Err(self.error("unterminated JSON string")),
            }
        }
    }

    fn number(&mut self) -> Result<(), ValidationError> {
        self.consume(b'-');
        if !self.consume(b'0') {
            if !self.peek().is_some_and(|b| matches!(b, b'1'..=b'9')) {
                return Err(self.error("invalid JSON number"));
            }
            self.digits();
        }
        if self.consume(b'.') && self.digits() == 0 {
            return Err(self.error("expected digits after decimal point"));
        }
        if self.consume(b'e') || self.consume(b'E') {
            if !self.consume(b'+') {
                self.consume(b'-');
            }
            if self.digits() == 0 {
                return Err(self.error("expected exponent digits"));
            }
        }
        Ok(())
    }

    fn digits(&mut self) -> usize {
        let start = self.offset;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.offset += 1;
        }
        self.offset - start
    }

    fn literal(&mut self, literal: &[u8]) -> Result<(), ValidationError> {
        if self.bytes[self.offset..].starts_with(literal) {
            self.offset += literal.len();
            Ok(())
        } else {
            Err(self.error("invalid JSON literal"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_numbers_without_losing_precision_or_restricting_exponents() {
        for input in [
            "1234567890123456789012345678901234567890",
            "1e999999",
            "-0.000000000000000000001",
            r#" {"n": 9007199254740993, "s": "\ud83d\ude00"} "#,
            "[true, false, null]",
        ] {
            validate(input.as_bytes(), 64).unwrap();
        }
    }

    #[test]
    fn rejects_invalid_json_and_decoded_duplicate_keys() {
        for input in [
            "",
            " ",
            "+1",
            "01",
            "-01",
            "1.",
            ".1",
            "1e",
            "1e+",
            "NaN",
            "Infinity",
            "[1,]",
            r#"{"a":1,}"#,
            "{}{}",
            r#""\x""#,
            r#""\ud800""#,
            r#"{"a":1,"\u0061":2}"#,
            r#"{"x":{"b":1,"b":2}}"#,
        ] {
            assert!(validate(input.as_bytes(), 64).is_err(), "{input}");
        }
        assert!(validate(&[b'"', 0xff, b'"'], 64).is_err());
        validate(br#"{"a":1,"b":{"a":2}}"#, 64).unwrap();
    }

    #[test]
    fn nesting_limit_counts_containers_including_empty_ones() {
        validate(b"1", 1).unwrap();
        validate(b"[]", 1).unwrap();
        validate(b"[{}]", 2).unwrap();
        assert!(validate(b"[{}]", 1).is_err());
        let nested = format!("{}0{}", "[".repeat(128), "]".repeat(128));
        validate(nested.as_bytes(), 128).unwrap();
        assert!(validate(nested.as_bytes(), 127).is_err());
    }
}
