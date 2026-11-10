//! Small, streaming-friendly parser for the parts of Godot text resources that
//! resource navigation needs.
//!
//! This is intentionally not a full Godot variant parser. It understands
//! section headers, quoted strings (including escapes and triple-quoted
//! strings), comments, `ExtResource(...)`/`SubResource(...)` calls, and bare
//! `res://`/`uid://` tokens (needed for `.uid` sidecar files).

use crate::{Position, Range};

#[derive(Clone, Debug)]
pub(crate) enum TokenKind {
    Path(String),
    ExtResource(String),
    SubResource(String),
}

#[derive(Clone, Debug)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub range: Range,
    pub start_byte: usize,
    pub end_byte: usize,
    /// Full `ExtResource(...)`/`SubResource(...)` call, or the token itself.
    pub call_start_byte: usize,
    pub call_end_byte: usize,
}

impl Token {
    pub fn text(&self) -> &str {
        match &self.kind {
            TokenKind::Path(value) => value,
            TokenKind::ExtResource(id) | TokenKind::SubResource(id) => id,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Attribute {
    pub key: String,
    pub value: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct Section {
    pub kind: String,
    pub range: Range,
    pub attributes: Vec<Attribute>,
    pub start_byte: usize,
    pub end_byte: usize,
}

impl Section {
    pub fn attr(&self, key: &str) -> Option<&Attribute> {
        self.attributes.iter().rev().find(|attr| attr.key == key)
    }

    pub fn value(&self, key: &str) -> Option<&str> {
        self.attr(key).map(|attr| attr.value.as_str())
    }
}

pub(crate) struct ParsedDocument<'a> {
    text: &'a str,
    line_starts: Vec<usize>,
    pub tokens: Vec<Token>,
    pub sections: Vec<Section>,
}

impl<'a> ParsedDocument<'a> {
    pub fn parse(text: &'a str) -> Self {
        let line_starts = line_starts(text);
        let mut parser = Parser {
            text,
            bytes: text.as_bytes(),
            line_starts: &line_starts,
            tokens: Vec::new(),
            sections: Vec::new(),
        };
        parser.run();
        let tokens = std::mem::take(&mut parser.tokens);
        let sections = std::mem::take(&mut parser.sections);
        drop(parser);
        Self {
            text,
            line_starts,
            tokens,
            sections,
        }
    }

    pub fn token_at(&self, position: Position) -> Option<&Token> {
        let byte = self.byte_at(position)?;
        // Prefer a token containing the cursor; if the cursor is exactly at
        // the end of a token (a common place for "go to definition"), accept
        // that token when nothing else starts there.
        if let Some(token) = self.tokens.iter().find(|token| {
            (token.start_byte <= byte && byte < token.end_byte)
                || (token.call_start_byte <= byte && byte < token.call_end_byte)
        }) {
            return Some(token);
        }
        self.tokens
            .iter()
            .find(|token| byte == token.end_byte && token.start_byte < token.end_byte)
    }

    pub fn header(&self) -> Option<&Section> {
        self.sections
            .iter()
            .find(|section| section.kind == "gd_scene" || section.kind == "gd_resource")
    }

    pub fn section_containing(&self, byte: usize) -> Option<&Section> {
        self.sections
            .iter()
            .find(|section| section.start_byte <= byte && byte <= section.end_byte)
    }

    pub fn sub_sections(&self, id: &str) -> Vec<&Section> {
        self.sections
            .iter()
            .filter(|section| section.kind == "sub_resource" && section.value("id") == Some(id))
            .collect()
    }

    pub fn ext_sections(&self, id: &str) -> Vec<&Section> {
        self.sections
            .iter()
            .filter(|section| section.kind == "ext_resource" && section.value("id") == Some(id))
            .collect()
    }

    fn byte_at(&self, position: Position) -> Option<usize> {
        let line_start = *self.line_starts.get(position.line as usize)?;
        let line_end = self
            .line_starts
            .get(position.line as usize + 1)
            .copied()
            .unwrap_or(self.text.len());
        let mut units = 0u32;
        for (offset, ch) in self.text[line_start..line_end].char_indices() {
            if units == position.character {
                return Some(line_start + offset);
            }
            units += ch.len_utf16() as u32;
        }
        if units == position.character {
            Some(line_end)
        } else {
            None
        }
    }
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    line_starts: &'a [usize],
    tokens: Vec<Token>,
    sections: Vec<Section>,
}

impl<'a> Parser<'a> {
    fn run(&mut self) {
        let mut i = 0usize;
        let mut at_line_start = true;
        if self.bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
            i = 3;
        }
        while i < self.bytes.len() {
            let b = self.bytes[i];

            if b == b';' {
                while i < self.bytes.len() && self.bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            // GDShader-style line comments. `res://`/`uid://` tokens are
            // consumed whole before their slashes can be seen here.
            if b == b'/' && self.bytes.get(i + 1) == Some(&b'/') {
                while i < self.bytes.len() && self.bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if b == b'/' && self.bytes.get(i + 1) == Some(&b'*') {
                i += 2;
                while i + 1 < self.bytes.len()
                    && !(self.bytes[i] == b'*' && self.bytes[i + 1] == b'/')
                {
                    i += 1;
                }
                if i + 1 < self.bytes.len() {
                    i += 2;
                } else {
                    i = self.bytes.len();
                }
                continue;
            }
            if b == b'\n' {
                at_line_start = true;
                i += 1;
                continue;
            }
            if b == b'\r' {
                i += 1;
                continue;
            }
            if b == b' ' || b == b'\t' {
                i += 1;
                continue;
            }

            if at_line_start && b == b'[' {
                if let Some((section, _end)) = parse_section(self.text, self.line_starts, i) {
                    self.sections.push(section);
                }
                at_line_start = false;
                i += 1;
                continue;
            }
            at_line_start = false;

            if (b == b'E' || b == b'S')
                && self.ident_boundary(i)
                && let Some((token, next)) = parse_ref_call(self.text, self.line_starts, i)
            {
                self.tokens.push(token);
                i = next;
                continue;
            }

            if b == b'"'
                && let Some((token, next)) = parse_string(self.text, self.line_starts, i)
            {
                if let Some(token) = token {
                    self.tokens.push(token);
                }
                i = next;
                continue;
            }

            if self.bare_path_prefix(i) {
                let end = bare_token_end(self.bytes, i);
                if end > i {
                    let value = &self.text[i..end];
                    self.tokens.push(Token {
                        kind: TokenKind::Path(value.to_string()),
                        range: Range {
                            start: position_at(self.text, self.line_starts, i),
                            end: position_at(self.text, self.line_starts, end),
                        },
                        start_byte: i,
                        end_byte: end,
                        call_start_byte: i,
                        call_end_byte: end,
                    });
                    i = end;
                    continue;
                }
            }

            i += 1;
        }
    }

    fn ident_boundary(&self, start: usize) -> bool {
        start == 0 || !is_ident_byte(self.bytes[start - 1])
    }

    fn bare_path_prefix(&self, start: usize) -> bool {
        let rest = &self.text[start..];
        (rest.starts_with("res://") || rest.starts_with("uid://") || rest.starts_with("user://"))
            && self.ident_boundary(start)
    }
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

fn position_at(text: &str, line_starts: &[usize], byte: usize) -> Position {
    let byte = byte.min(text.len());
    let line = line_starts
        .partition_point(|&start| start <= byte)
        .saturating_sub(1);
    let line_start = line_starts[line];
    let character = text[line_start..byte].encode_utf16().count() as u32;
    Position {
        line: line as u32,
        character,
    }
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn bare_token_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() {
        match bytes[end] {
            b' ' | b'\t' | b'\r' | b'\n' | b';' | b'"' | b'\'' | b')' | b']' | b'}' | b',' => break,
            _ => end += 1,
        }
    }
    // Do not include trailing punctuation commonly found in prose.
    while end > start {
        match bytes[end - 1] {
            b'.' | b':' | b'!' | b'?' => end -= 1,
            _ => break,
        }
    }
    end
}

fn parse_ref_call(text: &str, line_starts: &[usize], start: usize) -> Option<(Token, usize)> {
    let rest = &text[start..];
    let (name, kind) = if rest.starts_with("ExtResource") {
        ("ExtResource", true)
    } else if rest.starts_with("SubResource") {
        ("SubResource", false)
    } else {
        return None;
    };
    let mut i = start + name.len();
    let bytes = text.as_bytes();
    if i < bytes.len() && is_ident_byte(bytes[i]) {
        return None;
    }
    i = skip_ws(bytes, i);
    if bytes.get(i) != Some(&b'(') {
        return None;
    }
    i = skip_ws(bytes, i + 1);
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    let (content, content_start, content_end, after) = parse_quoted_content(text, i, usize::MAX)?;
    let mut next = skip_ws(bytes, after);
    if bytes.get(next) == Some(&b')') {
        next += 1;
    }
    let range = Range {
        start: position_at(text, line_starts, content_start),
        end: position_at(text, line_starts, content_end),
    };
    let kind = if kind {
        TokenKind::ExtResource(content)
    } else {
        TokenKind::SubResource(content)
    };
    Some((
        Token {
            kind,
            range,
            start_byte: content_start,
            end_byte: content_end,
            call_start_byte: start,
            call_end_byte: next,
        },
        next,
    ))
}

fn parse_string(text: &str, line_starts: &[usize], start: usize) -> Option<(Option<Token>, usize)> {
    let bytes = text.as_bytes();
    if bytes[start] != b'"' {
        return None;
    }
    if text[start..].starts_with("\"\"\"") {
        return parse_triple_string(text, line_starts, start);
    }
    let (content, content_start, content_end, after) =
        parse_quoted_content(text, start, usize::MAX)?;
    let next = after;
    let kind = path_token_kind(&content);
    let token = kind.map(|kind| Token {
        kind,
        range: Range {
            start: position_at(text, line_starts, content_start),
            end: position_at(text, line_starts, content_end),
        },
        start_byte: content_start,
        end_byte: content_end,
        call_start_byte: start,
        call_end_byte: after,
    });
    Some((token, next))
}

fn parse_triple_string(
    text: &str,
    line_starts: &[usize],
    start: usize,
) -> Option<(Option<Token>, usize)> {
    let bytes = text.as_bytes();
    let mut i = start + 3;
    while i + 3 <= bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i..].starts_with(b"\"\"\"") {
            let content_start = start + 3;
            let content_end = i;
            let content = &text[content_start..content_end];
            let token = path_token_kind(content).map(|kind| Token {
                kind,
                range: Range {
                    start: position_at(text, line_starts, content_start),
                    end: position_at(text, line_starts, content_end),
                },
                start_byte: content_start,
                end_byte: content_end,
                call_start_byte: start,
                call_end_byte: i + 3,
            });
            return Some((token, i + 3));
        }
        i += 1;
    }
    Some((None, text.len()))
}

/// Parse a normal double-quoted string. Returns the decoded content, the byte
/// range of the raw content (without quotes), and the byte after the closing
/// quote. `limit` is an exclusive byte bound used for section headers.
fn parse_quoted_content(
    text: &str,
    start: usize,
    limit: usize,
) -> Option<(String, usize, usize, usize)> {
    let bytes = text.as_bytes();
    let limit = limit.min(bytes.len());
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let content_start = start + 1;
    let mut i = content_start;
    let mut content = String::new();
    while i < limit {
        let ch = text[i..].chars().next()?;
        match ch {
            '\\' => {
                let (decoded, next) = parse_escape(text, i, limit);
                content.push_str(&decoded);
                i = next;
            }
            '"' => {
                return Some((content, content_start, i, i + 1));
            }
            '\n' | '\r' => {
                return Some((content, content_start, i, i));
            }
            _ => {
                content.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    Some((content, content_start, i, i))
}

fn parse_escape(text: &str, start: usize, limit: usize) -> (String, usize) {
    let i = start + 1;
    if i >= limit {
        return ("\\".to_string(), i);
    }
    let ch = match text[i..].chars().next() {
        Some(ch) => ch,
        None => return ("\\".to_string(), i),
    };
    match ch {
        'n' => ("\n".to_string(), i + 1),
        'r' => ("\r".to_string(), i + 1),
        't' => ("\t".to_string(), i + 1),
        'b' => ("\u{8}".to_string(), i + 1),
        'f' => ("\u{c}".to_string(), i + 1),
        'v' => ("\u{b}".to_string(), i + 1),
        '0' => ("\0".to_string(), i + 1),
        '\\' => ("\\".to_string(), i + 1),
        '"' => ("\"".to_string(), i + 1),
        '\'' => ("'".to_string(), i + 1),
        'u' | 'U' => {
            let count = if ch == 'u' { 4 } else { 8 };
            let digits_start = i + 1;
            let digits_end = digits_start + count;
            if digits_end <= limit
                && let Some(code) = text.get(digits_start..digits_end)
                && let Ok(value) = u32::from_str_radix(code, 16)
                && let Some(ch) = char::from_u32(value)
            {
                return (ch.to_string(), digits_end);
            }
            // Unknown escape: keep the backslash and the `u`.
            ("\\u".to_string(), i + 1)
        }
        _ => {
            let len = ch.len_utf8();
            (ch.to_string(), i + len)
        }
    }
}

fn path_token_kind(content: &str) -> Option<TokenKind> {
    // Project autoloads are stored as `*res://...` / `*uid://...`; the star is
    // a project-setting marker, not part of the resource path.
    let value = content.strip_prefix('*').unwrap_or(content);
    if value.starts_with("res://") || value.starts_with("uid://") || value.starts_with("user://") {
        Some(TokenKind::Path(value.to_string()))
    } else {
        None
    }
}

fn parse_section(text: &str, line_starts: &[usize], start: usize) -> Option<(Section, usize)> {
    let bytes = text.as_bytes();
    let close = header_close(bytes, start)?;
    let mut i = start + 1;
    while i < close && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    let tag_start = i;
    while i < close && is_ident_byte(bytes[i]) {
        i += 1;
    }
    if i == tag_start {
        return None;
    }
    let kind = text[tag_start..i].to_string();
    let mut attributes = Vec::new();
    while i < close {
        while i < close && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= close {
            break;
        }
        let key_start = i;
        while i < close && (is_ident_byte(bytes[i]) || bytes[i] == b'/') {
            i += 1;
        }
        if i == key_start {
            i += 1;
            continue;
        }
        let key = text[key_start..i].to_string();
        while i < close && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if bytes.get(i) != Some(&b'=') {
            continue;
        }
        i += 1;
        while i < close && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= close {
            break;
        }
        if bytes[i] == b'"' {
            let parsed = parse_quoted_content(text, i, close)?;
            let (_decoded, content_start, content_end, after) = parsed;
            let value = &text[content_start..content_end];
            attributes.push(Attribute {
                key,
                value: unescape_raw(value),
                start_byte: content_start,
                end_byte: content_end,
            });
            i = after;
        } else {
            let value_start = i;
            while i < close && bytes[i] != b' ' && bytes[i] != b'\t' {
                i += 1;
            }
            attributes.push(Attribute {
                key,
                value: text[value_start..i].to_string(),
                start_byte: value_start,
                end_byte: i,
            });
        }
    }
    let section = Section {
        kind,
        range: Range {
            start: position_at(text, line_starts, start),
            end: position_at(text, line_starts, close + 1),
        },
        attributes,
        start_byte: start,
        end_byte: close + 1,
    };
    Some((section, close + 1))
}

fn header_close(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    let mut in_string = false;
    while i < bytes.len() {
        let byte = bytes[i];
        if in_string {
            if byte == b'\\' {
                i += 2;
                continue;
            }
            if byte == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                i += 1;
            }
            b']' => return Some(i),
            b'\n' => return None,
            _ => i += 1,
        }
    }
    None
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    i
}

fn unescape_raw(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let mut out = String::new();
    let mut i = 0;
    while i < raw.len() {
        if raw.as_bytes()[i] == b'\\' {
            let (decoded, next) = parse_escape(raw, i, raw.len());
            out.push_str(&decoded);
            i = next;
        } else {
            let ch = raw[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}
