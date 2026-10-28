//! Strict reader of the handshake payload grammar, mirrored by `pwa/src/remote/payload.ts`
//! (shared accept/reject vectors: `tests/vectors/payload-v1.json`).
//!
//! The payload is one flat JSON object of valid UTF-8 (no BOM). Values are strings or integers
//! only; integers are bare digits (no sign, fraction or exponent, no leading zeros except `0`)
//! up to 2^53 − 1. Strings must be valid Unicode (a lone surrogate escape is rejected). Any
//! duplicate key, known or unknown, compared after unescaping, is rejected.

/// Integers stay exactly representable as JavaScript numbers.
const MAX_INTEGER: u64 = (1 << 53) - 1;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Value {
    Str(String),
    Int(u64),
}

/// The entries of one payload object, in order, with unique keys.
#[derive(Debug)]
pub(super) struct Fields(Vec<(String, Value)>);

impl Fields {
    /// Removes and returns the value of `key`.
    pub(super) fn take(&mut self, key: &str) -> Option<Value> {
        let index = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.swap_remove(index).1)
    }
}

/// Parses a payload, or `None` if it is not in the grammar.
pub(super) fn parse(bytes: &[u8]) -> Option<Fields> {
    std::str::from_utf8(bytes).ok()?;
    let mut scanner = Scanner { bytes, pos: 0 };
    let fields = scanner.object()?;
    scanner.skip_ws();
    (scanner.pos == bytes.len()).then_some(fields)
}

struct Scanner<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Option<()> {
        (self.next()? == byte).then_some(())
    }

    fn object(&mut self) -> Option<Fields> {
        self.skip_ws();
        self.expect(b'{')?;
        let mut entries: Vec<(String, Value)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Some(Fields(entries));
        }
        loop {
            self.skip_ws();
            self.expect(b'"')?;
            let key = self.string()?;
            if entries.iter().any(|(k, _)| *k == key) {
                return None;
            }
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let value = match self.peek()? {
                b'"' => {
                    self.pos += 1;
                    Value::Str(self.string()?)
                }
                b'0'..=b'9' => Value::Int(self.integer()?),
                _ => return None,
            };
            entries.push((key, value));
            self.skip_ws();
            match self.next()? {
                b',' => {}
                b'}' => return Some(Fields(entries)),
                _ => return None,
            }
        }
    }

    /// After the opening quote; the input is known to be valid UTF-8.
    fn string(&mut self) -> Option<String> {
        let mut out = String::new();
        loop {
            let start = self.pos;
            while !matches!(self.peek()?, b'"' | b'\\' | 0..=0x1f) {
                self.pos += 1;
            }
            // Splits only at ASCII bytes, so the run is valid UTF-8.
            out.push_str(std::str::from_utf8(&self.bytes[start..self.pos]).ok()?);
            match self.next()? {
                b'"' => return Some(out),
                b'\\' => out.push(self.escape()?),
                _ => return None,
            }
        }
    }

    fn escape(&mut self) -> Option<char> {
        Some(match self.next()? {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let unit = self.hex4()?;
                match unit {
                    0xd800..=0xdbff => {
                        self.expect(b'\\')?;
                        self.expect(b'u')?;
                        let low = self.hex4()?;
                        if !(0xdc00..=0xdfff).contains(&low) {
                            return None;
                        }
                        char::from_u32(0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00))?
                    }
                    _ => char::from_u32(unit)?,
                }
            }
            _ => return None,
        })
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut unit = 0;
        for _ in 0..4 {
            unit = unit << 4 | char::from(self.next()?).to_digit(16)?;
        }
        Some(unit)
    }

    fn integer(&mut self) -> Option<u64> {
        if self.peek() == Some(b'0') {
            self.pos += 1;
            return (!matches!(self.peek(), Some(b'0'..=b'9'))).then_some(0);
        }
        let mut value: u64 = 0;
        while let Some(digit @ b'0'..=b'9') = self.peek() {
            self.pos += 1;
            value = value * 10 + u64::from(digit - b'0');
            if value > MAX_INTEGER {
                return None;
            }
        }
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar() {
        let fields =
            parse(b" {\"a\" : \"x\\u00e9\\ud83d\\ude00\", \"b\":0,\"c\":9007199254740991}\r\n");
        let mut fields = fields.unwrap();
        assert_eq!(
            fields.take("a"),
            Some(Value::Str("x\u{e9}\u{1f600}".into()))
        );
        assert_eq!(fields.take("b"), Some(Value::Int(0)));
        assert_eq!(fields.take("c"), Some(Value::Int(MAX_INTEGER)));
        assert!(parse(b"{}").is_some());
        for bad in [
            &b"{\"a\":1,\"a\":2}"[..],
            b"{\"a\":1,\"\\u0061\":2}",
            b"{\"a\":01}",
            b"{\"a\":9007199254740992}",
            b"{\"a\":\"\\udc00\"}",
            b"{\"a\":\"\\ud800x\"}",
            b"{\"a\":\"\x01\"}",
            b"{\"a\":1,}",
            b"{,}",
            b"{\"a\":1}x",
            b"",
        ] {
            assert!(parse(bad).is_none(), "{}", String::from_utf8_lossy(bad));
        }
    }
}
