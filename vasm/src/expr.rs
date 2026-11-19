//! Expressions: numbers, character constants, symbols, `$`, `$$`, and the usual integer
//! operators with C-like precedence.

use crate::Ctx;
use alloc::string::String;
use alloc::vec::Vec;

/// The value of an expression.
#[derive(Clone, Copy, Debug)]
pub struct Value {
    pub v: i64,
    /// True if the value is the same in both passes, so it may choose an instruction's size:
    /// it uses only numbers, `$`, `$$`, and symbols defined on earlier lines.
    pub constant: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(i64),
    Ident(String),
    Op(&'static str),
    Open,
    Close,
}

const OPS: [&str; 13] = ["<<", ">>", "+", "-", "*", "/", "%", "&", "|", "^", "~", "!", "$"];

pub fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '.' || c == '@'
}

pub fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '@' || c == '$'
}

pub fn parse_number(s: &str) -> Option<i64> {
    let s_clean: String = s.chars().filter(|&c| c != '_').collect();
    let s = s_clean.as_str();
    let lower = s.to_ascii_lowercase();
    let (digits, radix) = if let Some(h) = lower.strip_prefix("0x") {
        (String::from(h), 16)
    } else if let Some(b) = lower.strip_prefix("0b") {
        (String::from(b), 2)
    } else if let Some(o) = lower.strip_prefix("0o") {
        (String::from(o), 8)
    } else if let Some(h) = lower.strip_suffix('h') {
        (String::from(h), 16)
    } else {
        (lower.clone(), 10)
    };
    if digits.is_empty() {
        return None;
    }
    u64::from_str_radix(&digits, radix).ok().map(|v| v as i64)
}

/// Turns a quoted string body into bytes, handling escapes in backquoted strings.
pub fn unquote(body: &str, quote: char) -> Result<Vec<u8>, String> {
    if quote != '`' {
        return Ok(body.as_bytes().to_vec());
    }
    let mut out = Vec::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('0') => out.push(0),
            Some('e') => out.push(0x1B),
            Some('\\') => out.push(b'\\'),
            Some('`') => out.push(b'`'),
            Some('\'') => out.push(b'\''),
            Some('"') => out.push(b'"'),
            other => return Err(alloc::format!("unknown escape \\{}", other.unwrap_or(' '))),
        }
    }
    Ok(out)
}

fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '(' {
            toks.push(Tok::Open);
            i += 1;
        } else if c == ')' {
            toks.push(Tok::Close);
            i += 1;
        } else if c == '\'' || c == '"' || c == '`' {
            let end = chars[i + 1..].iter().position(|&d| d == c).ok_or("unterminated character constant")?;
            let body: String = chars[i + 1..i + 1 + end].iter().collect();
            let bytes = unquote(&body, c)?;
            if bytes.len() > 8 {
                return Err(String::from("character constant longer than 8 bytes"));
            }
            let mut v = 0u64;
            for (k, b) in bytes.iter().enumerate() {
                v |= (*b as u64) << (8 * k);
            }
            toks.push(Tok::Num(v as i64));
            i += end + 2;
        } else if c == '$' && chars.get(i + 1) == Some(&'$') {
            toks.push(Tok::Ident(String::from("$$")));
            i += 2;
        } else if c == '$' && !chars.get(i + 1).is_some_and(|&d| is_ident_char(d)) {
            toks.push(Tok::Ident(String::from("$")));
            i += 1;
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            toks.push(Tok::Num(parse_number(&text).ok_or_else(|| alloc::format!("bad number: {text}"))?));
        } else if is_ident_start(c) || c == '$' {
            let start = i;
            if c == '$' {
                i += 1; // `$name` escapes a name that looks like a register
            }
            while i < chars.len() && is_ident_char(chars[i]) {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            toks.push(Tok::Ident(String::from(text.trim_start_matches('$'))));
        } else {
            let rest: String = chars[i..].iter().take(2).collect();
            let op = OPS
                .iter()
                .find(|op| rest.starts_with(**op))
                .ok_or_else(|| alloc::format!("unexpected character '{c}' in expression"))?;
            toks.push(Tok::Op(op));
            i += op.len();
        }
    }
    Ok(toks)
}

struct Parser<'a, 'c> {
    toks: Vec<Tok>,
    pos: usize,
    ctx: &'a Ctx<'c>,
}

fn precedence(op: &str) -> Option<u8> {
    Some(match op {
        "|" => 1,
        "^" => 2,
        "&" => 3,
        "<<" | ">>" => 4,
        "+" | "-" => 5,
        "*" | "/" | "%" => 6,
        _ => return None,
    })
}

impl Parser<'_, '_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn unary(&mut self) -> Result<Value, String> {
        let tok = self.toks.get(self.pos).cloned().ok_or("expression ends too early")?;
        self.pos += 1;
        match tok {
            Tok::Num(v) => Ok(Value { v, constant: true }),
            Tok::Ident(name) => self.ctx.symbol(&name),
            Tok::Open => {
                let v = self.binary(0)?;
                if self.peek() != Some(&Tok::Close) {
                    return Err(String::from("missing )"));
                }
                self.pos += 1;
                Ok(v)
            }
            Tok::Op("-") => self.unary().map(|x| Value { v: x.v.wrapping_neg(), ..x }),
            Tok::Op("+") => self.unary(),
            Tok::Op("~") => self.unary().map(|x| Value { v: !x.v, ..x }),
            Tok::Op("!") => self.unary().map(|x| Value { v: (x.v == 0) as i64, ..x }),
            other => Err(alloc::format!("unexpected {other:?} in expression")),
        }
    }

    fn binary(&mut self, min: u8) -> Result<Value, String> {
        let mut left = self.unary()?;
        while let Some(Tok::Op(op)) = self.peek() {
            let op = *op;
            let Some(prec) = precedence(op) else { break };
            if prec < min {
                break;
            }
            self.pos += 1;
            let right = self.binary(prec + 1)?;
            let (a, b) = (left.v, right.v);
            let v = match op {
                "|" => a | b,
                "^" => a ^ b,
                "&" => a & b,
                "<<" => a.wrapping_shl(b as u32),
                ">>" => ((a as u64) >> (b as u32 & 63)) as i64,
                "+" => a.wrapping_add(b),
                "-" => a.wrapping_sub(b),
                "*" => a.wrapping_mul(b),
                "/" | "%" if b == 0 => {
                    if left.constant && right.constant {
                        return Err(String::from("division by zero"));
                    }
                    0
                }
                "/" => a.wrapping_div(b),
                "%" => a.wrapping_rem(b),
                _ => unreachable!(),
            };
            left = Value { v, constant: left.constant && right.constant };
        }
        Ok(left)
    }
}

pub fn eval(s: &str, ctx: &Ctx) -> Result<Value, String> {
    let toks = tokenize(s)?;
    if toks.is_empty() {
        return Err(String::from("missing value"));
    }
    let mut p = Parser { toks, pos: 0, ctx };
    let v = p.binary(0)?;
    if p.pos != p.toks.len() {
        return Err(alloc::format!("unexpected text in expression: {s}"));
    }
    Ok(v)
}
