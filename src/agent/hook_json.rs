use super::AdapterError;

const MAX_HOOK_ID_BYTES: usize = 1024;
const MAX_JSON_DEPTH: usize = 32;

pub(super) fn set_required_once(
    target: &mut String,
    seen: &mut bool,
    value: Result<String, AdapterError>,
) -> Result<(), AdapterError> {
    if *seen {
        return Err(AdapterError::MalformedInput);
    }
    *seen = true;
    *target = value?;
    Ok(())
}

pub(super) fn set_optional_once(
    target: &mut Option<String>,
    value: Result<String, AdapterError>,
) -> Result<(), AdapterError> {
    if target.is_some() {
        return Err(AdapterError::MalformedInput);
    }
    *target = Some(value?);
    Ok(())
}

pub(super) fn append_identity_part(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_be_bytes());
    output.extend_from_slice(value);
}

/// Minimal, bounded JSON reader for native Hook envelopes.
///
/// Adapters explicitly select the identity and event-shape fields they need;
/// all prompt, transcript, tool input/output, and other vendor payload fields
/// are syntactically validated and skipped without allocation.
pub(super) struct HookJsonParser<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> HookJsonParser<'a> {
    pub(super) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    pub(super) fn parse_object(
        &mut self,
        mut field: impl FnMut(&mut Self, &str) -> Result<(), AdapterError>,
    ) -> Result<(), AdapterError> {
        self.whitespace();
        self.expect(b'{')?;
        self.whitespace();
        if self.consume(b'}') {
            return Ok(());
        }
        loop {
            let key = self.parse_string(128)?;
            self.whitespace();
            self.expect(b':')?;
            self.whitespace();
            field(self, &key)?;
            self.whitespace();
            if self.consume(b'}') {
                return Ok(());
            }
            self.expect(b',')?;
            self.whitespace();
        }
    }

    pub(super) fn parse_bounded_string(&mut self) -> Result<String, AdapterError> {
        self.parse_string(MAX_HOOK_ID_BYTES)
    }

    fn parse_string(&mut self, limit: usize) -> Result<String, AdapterError> {
        self.expect(b'"')?;
        let mut output = String::new();
        let mut segment = self.position;
        while let Some(byte) = self.bytes.get(self.position).copied() {
            match byte {
                b'"' => {
                    self.push_segment(&mut output, segment, self.position, limit)?;
                    self.position += 1;
                    return Ok(output);
                }
                b'\\' => {
                    self.push_segment(&mut output, segment, self.position, limit)?;
                    self.position += 1;
                    self.push_escape(&mut output, limit)?;
                    segment = self.position;
                }
                0x00..=0x1f => return Err(AdapterError::MalformedInput),
                _ => self.position += 1,
            }
        }
        Err(AdapterError::MalformedInput)
    }

    fn push_segment(
        &self,
        output: &mut String,
        start: usize,
        end: usize,
        limit: usize,
    ) -> Result<(), AdapterError> {
        let value = std::str::from_utf8(&self.bytes[start..end])
            .map_err(|_| AdapterError::MalformedInput)?;
        if output.len().saturating_add(value.len()) > limit {
            return Err(AdapterError::MalformedInput);
        }
        output.push_str(value);
        Ok(())
    }

    fn push_escape(&mut self, output: &mut String, limit: usize) -> Result<(), AdapterError> {
        let escaped = self.next()?;
        let character = match escaped {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{0008}',
            b'f' => '\u{000c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => self.parse_unicode_escape()?,
            _ => return Err(AdapterError::MalformedInput),
        };
        if output.len().saturating_add(character.len_utf8()) > limit {
            return Err(AdapterError::MalformedInput);
        }
        output.push(character);
        Ok(())
    }

    fn parse_unicode_escape(&mut self) -> Result<char, AdapterError> {
        let first = self.parse_hex_quad()?;
        let scalar = if (0xd800..=0xdbff).contains(&first) {
            self.expect(b'\\')?;
            self.expect(b'u')?;
            let second = self.parse_hex_quad()?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(AdapterError::MalformedInput);
            }
            0x1_0000 + (u32::from(first - 0xd800) << 10) + u32::from(second - 0xdc00)
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(AdapterError::MalformedInput);
        } else {
            u32::from(first)
        };
        char::from_u32(scalar).ok_or(AdapterError::MalformedInput)
    }

    fn parse_hex_quad(&mut self) -> Result<u16, AdapterError> {
        let mut value = 0_u16;
        for _ in 0..4 {
            let nibble = match self.next()? {
                b'0'..=b'9' => self.bytes[self.position - 1] - b'0',
                b'a'..=b'f' => self.bytes[self.position - 1] - b'a' + 10,
                b'A'..=b'F' => self.bytes[self.position - 1] - b'A' + 10,
                _ => return Err(AdapterError::MalformedInput),
            };
            value = (value << 4) | u16::from(nibble);
        }
        Ok(value)
    }

    pub(super) fn skip_value(&mut self, depth: usize) -> Result<(), AdapterError> {
        if depth > MAX_JSON_DEPTH {
            return Err(AdapterError::MalformedInput);
        }
        self.whitespace();
        match self.peek().ok_or(AdapterError::MalformedInput)? {
            b'"' => self.skip_string(),
            b'{' => self.skip_composite(b'{', b'}', depth),
            b'[' => self.skip_composite(b'[', b']', depth),
            b't' => self.literal(b"true"),
            b'f' => self.literal(b"false"),
            b'n' => self.literal(b"null"),
            b'-' | b'0'..=b'9' => self.skip_number(),
            _ => Err(AdapterError::MalformedInput),
        }
    }

    fn skip_composite(&mut self, open: u8, close: u8, depth: usize) -> Result<(), AdapterError> {
        self.expect(open)?;
        self.whitespace();
        if self.consume(close) {
            return Ok(());
        }
        loop {
            if open == b'{' {
                self.skip_string()?;
                self.whitespace();
                self.expect(b':')?;
                self.whitespace();
            }
            self.skip_value(depth + 1)?;
            self.whitespace();
            if self.consume(close) {
                return Ok(());
            }
            self.expect(b',')?;
            self.whitespace();
        }
    }

    fn skip_string(&mut self) -> Result<(), AdapterError> {
        self.expect(b'"')?;
        while let Some(byte) = self.bytes.get(self.position).copied() {
            self.position += 1;
            match byte {
                b'"' => return Ok(()),
                b'\\' => match self.next()? {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                    b'u' => {
                        let _ = self.parse_unicode_escape()?;
                    }
                    _ => return Err(AdapterError::MalformedInput),
                },
                0x00..=0x1f => return Err(AdapterError::MalformedInput),
                _ => {}
            }
        }
        Err(AdapterError::MalformedInput)
    }

    fn skip_number(&mut self) -> Result<(), AdapterError> {
        let start = self.position;
        self.consume(b'-');
        if self.consume(b'0') {
            if self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(AdapterError::MalformedInput);
            }
        } else {
            self.take_digits()?;
        }
        if self.consume(b'.') {
            self.take_digits()?;
        }
        if self.peek().is_some_and(|byte| matches!(byte, b'e' | b'E')) {
            self.position += 1;
            if self.peek().is_some_and(|byte| matches!(byte, b'+' | b'-')) {
                self.position += 1;
            }
            self.take_digits()?;
        }
        if self.position == start {
            return Err(AdapterError::MalformedInput);
        }
        Ok(())
    }

    fn take_digits(&mut self) -> Result<(), AdapterError> {
        let start = self.position;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.position += 1;
        }
        (self.position > start)
            .then_some(())
            .ok_or(AdapterError::MalformedInput)
    }

    fn literal(&mut self, value: &[u8]) -> Result<(), AdapterError> {
        if self.bytes.get(self.position..self.position + value.len()) == Some(value) {
            self.position += value.len();
            Ok(())
        } else {
            Err(AdapterError::MalformedInput)
        }
    }

    pub(super) fn finish(&mut self) -> Result<(), AdapterError> {
        self.whitespace();
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(AdapterError::MalformedInput)
        }
    }

    fn whitespace(&mut self) {
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.position += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), AdapterError> {
        if self.consume(expected) {
            Ok(())
        } else {
            Err(AdapterError::MalformedInput)
        }
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn next(&mut self) -> Result<u8, AdapterError> {
        let value = self.peek().ok_or(AdapterError::MalformedInput)?;
        self.position += 1;
        Ok(value)
    }
}
