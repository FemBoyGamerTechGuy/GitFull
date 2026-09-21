//! Minimal JSON parser — hand-written, std-only.
//!
//! Forge search/resolution needs to read forge REST API responses
//! (`search.rs`), but gitfull's dependency budget is exactly `serde` +
//! `toml`. Adding `serde_json` (or any HTTP crate) would break the posture
//! documented in docs/AUDIT.md, so the JSON subset needed for API responses
//! is parsed here instead — same spirit as the hand-written SHA-256.
//!
//! Supported (everything forge APIs emit): objects, arrays, strings with
//! the full escape set (including `\uXXXX` + surrogate pairs), integers,
//! floats, booleans, `null`. Object key order is preserved; duplicate keys
//! keep the last value (none of the forge APIs use duplicates).

use std::fmt;

use crate::error::{GitfullError, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn parse(text: &str) -> Result<Json> {
        let mut p = Parser {
            bytes: text.as_bytes(),
            pos: 0,
        };
        p.skip_ws();
        let v = p.value()?;
        p.skip_ws();
        if p.pos != p.bytes.len() {
            return Err(p.err("trailing characters after JSON value"));
        }
        Ok(v)
    }

    /// Object member lookup (first match wins).
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(members) => members
                .iter()
                .rev() // last assignment wins, like serde does
                .find(|(k, _)| k == key)
                .map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_obj(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(o) => Some(o),
            _ => None,
        }
    }

    /// Shorthand: `obj.key` as &str.
    pub fn str_of(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(|v| v.as_str())
    }

    /// Shorthand: `obj.key` as f64.
    pub fn num_of(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(|v| v.as_f64())
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Json::Null => write!(f, "null"),
            Json::Bool(b) => write!(f, "{b}"),
            Json::Num(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    write!(f, "{}", *n as i64)
                } else {
                    write!(f, "{n}")
                }
            }
            Json::Str(s) => write!(f, "{s:?}"),
            Json::Arr(a) => {
                write!(f, "[")?;
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            Json::Obj(o) => {
                write!(f, "{{")?;
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k:?}: {v}")?;
                }
                write!(f, "}}")
            }
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, msg: &str) -> GitfullError {
        GitfullError::Unsupported(format!(
            "JSON parse error at byte {}: {msg}",
            self.pos
        ))
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, b: u8) -> Result<()> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected `{}`", b as char)))
        }
    }

    fn value(&mut self) -> Result<Json> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.lit("true", Json::Bool(true)),
            Some(b'f') => self.lit("false", Json::Bool(false)),
            Some(b'n') => self.lit("null", Json::Null),
            Some(b'-') | Some(b'0'..=b'9') => self.number(),
            Some(c) => Err(self.err(&format!("unexpected `{}`", c as char))),
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn lit(&mut self, word: &str, v: Json) -> Result<Json> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(self.err(&format!("expected `{word}`")))
        }
    }

    fn object(&mut self) -> Result<Json> {
        self.expect(b'{')?;
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(members));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let val = self.value()?;
            members.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(members));
                }
                _ => return Err(self.err("expected `,` or `}`")),
            }
        }
    }

    fn array(&mut self) -> Result<Json> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(self.err("expected `,` or `]`")),
            }
        }
    }

    fn string(&mut self) -> Result<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let b = self.peek().ok_or_else(|| self.err("unterminated string"))?;
            match b {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let esc = self
                        .peek()
                        .ok_or_else(|| self.err("unterminated escape"))?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let cp = self.hex4()?;
                            let ch = if (0xD800..0xDC00).contains(&cp) {
                                // high surrogate: expect \uXXXX low surrogate
                                if self.bytes[self.pos..].starts_with(b"\\u") {
                                    self.pos += 2;
                                    let low = self.hex4()?;
                                    if (0xDC00..0xE000).contains(&low) {
                                        let c = 0x10000
                                            + ((cp - 0xD800) << 10)
                                            + (low - 0xDC00);
                                        char::from_u32(c)
                                            .ok_or_else(|| self.err("bad surrogate"))?
                                    } else {
                                        return Err(self.err("bad low surrogate"));
                                    }
                                } else {
                                    return Err(self.err("lone high surrogate"));
                                }
                            } else {
                                char::from_u32(cp)
                                    .ok_or_else(|| self.err("bad code point"))?
                            };
                            out.push(ch);
                        }
                        other => {
                            return Err(self.err(&format!("bad escape `\\{}`", other as char)))
                        }
                    }
                }
                0x00..=0x1F => return Err(self.err("raw control character in string")),
                _ => {
                    // consume one UTF-8 scalar; input is a &str so it is
                    // already valid UTF-8 — find the full char
                    let start = self.pos;
                    let len = utf8_len(b);
                    self.pos += len;
                    let s = std::str::from_utf8(&self.bytes[start..self.pos])
                        .map_err(|_| self.err("invalid UTF-8"))?;
                    out.push_str(s);
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32> {
        if self.pos + 4 > self.bytes.len() {
            return Err(self.err("truncated \\u escape"));
        }
        let s = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| self.err("bad \\u escape"))?;
        let v = u32::from_str_radix(s, 16).map_err(|_| self.err("bad \\u escape"))?;
        self.pos += 4;
        Ok(v)
    }

    fn number(&mut self) -> Result<Json> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // JSON forbids leading zeros: "0" or "0.x", never "01"
        let leading_zero = self.peek() == Some(b'0');
        if leading_zero {
            self.pos += 1;
            if matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("leading zeros are not allowed in numbers"));
            }
        } else {
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected a digit"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected a digit after `.`"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected a digit in the exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("bad number"))?;
        s.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| self.err(&format!("bad number `{s}`")))
    }
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first < 0xE0 {
        2
    } else if first < 0xF0 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars() {
        assert_eq!(Json::parse("null").unwrap(), Json::Null);
        assert_eq!(Json::parse("true").unwrap(), Json::Bool(true));
        assert_eq!(Json::parse("false").unwrap(), Json::Bool(false));
        assert_eq!(Json::parse("0").unwrap(), Json::Num(0.0));
        assert_eq!(Json::parse("-1").unwrap(), Json::Num(-1.0));
        assert_eq!(Json::parse("3.14").unwrap(), Json::Num(3.14));
        assert_eq!(Json::parse("1e3").unwrap(), Json::Num(1000.0));
        assert_eq!(Json::parse("-2.5E-2").unwrap(), Json::Num(-0.025));
        assert_eq!(Json::parse("  42 \n").unwrap(), Json::Num(42.0));
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(
            Json::parse(r#""hello""#).unwrap(),
            Json::Str("hello".into())
        );
        assert_eq!(
            Json::parse(r#""a\nb\tc\"d\\e\/f""#).unwrap(),
            Json::Str("a\nb\tc\"d\\e/f".into())
        );
        assert_eq!(
            Json::parse(r#""é 斉 🚀""#).unwrap(),
            Json::Str("é 斉 🚀".into())
        );
        // \u escapes + surrogate pair for U+1F600
        assert_eq!(
            Json::parse("\"\\u00e9\\u0041\\ud83d\\ude00\"").unwrap(),
            Json::Str("éA😀".into())
        );
        // lone high surrogate is rejected, not silently mangled
        assert!(Json::parse("\"\\ud83d\"").is_err());
    }

    #[test]
    fn containers() {
        let v = Json::parse(r#"{"a": [1, 2, {"b": null}], "c": "x"}"#).unwrap();
        assert_eq!(v.num_of("a"), None);
        let a = v.get("a").unwrap().as_arr().unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!(a[0], Json::Num(1.0));
        assert_eq!(a[2].get("b"), Some(&Json::Null));
        assert_eq!(v.str_of("c"), Some("x"));
        // nested access on non-objects is gracefully absent
        assert_eq!(a[0].get("b"), None);
        assert_eq!(v.str_of("missing"), None);
    }

    #[test]
    fn github_like_response() {
        // the shape search.rs actually consumes
        let body = r#"{
          "total_count": 2,
          "items": [
            {"full_name": "octocat/Hello-World",
             "stargazers_count": 142,
             "pushed_at": "2025-11-30T08:21:00Z"},
            {"full_name": "someone/hello",
             "stargazers_count": 7,
             "pushed_at": "2019-02-01T00:00:00Z"}
          ]
        }"#;
        let v = Json::parse(body).unwrap();
        let items = v.get("items").unwrap().as_arr().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].str_of("full_name"), Some("octocat/Hello-World"));
        assert_eq!(items[0].num_of("stargazers_count"), Some(142.0));
        assert_eq!(v.num_of("total_count"), Some(2.0));
    }

    #[test]
    fn malformed() {
        for bad in [
            "", "{", "}", "[", "[1,", "{\"a\"}", "{\"a\":}", "tru",
            "\"unterminated", "1.2.3", "{\"a\":1}x", "[01]", "nul",
            r#""\q""#, r#""\u12""#,
        ] {
            assert!(Json::parse(bad).is_err(), "expected error for {bad:?}");
        }
    }

    #[test]
    fn duplicate_keys_last_wins() {
        let v = Json::parse(r#"{"x": 1, "x": 2}"#).unwrap();
        assert_eq!(v.num_of("x"), Some(2.0));
    }
}
