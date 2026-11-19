//! Registers and instruction operands.

use crate::expr::{self, Value};
use crate::Ctx;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegKind {
    General,
    /// ah, ch, dh, bh: cannot be combined with a REX prefix.
    High8,
    /// spl, bpl, sil, dil: need a REX prefix.
    Rex8,
    Control,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reg {
    pub num: u8,
    /// Size in bytes (1, 2, 4 or 8).
    pub size: u8,
    pub kind: RegKind,
}

const R64: [&str; 16] =
    ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];
const R32: [&str; 16] = [
    "eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d", "r12d", "r13d", "r14d", "r15d",
];
const R16: [&str; 16] =
    ["ax", "cx", "dx", "bx", "sp", "bp", "si", "di", "r8w", "r9w", "r10w", "r11w", "r12w", "r13w", "r14w", "r15w"];
const R8: [&str; 16] = [
    "al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b", "r13b", "r14b", "r15b",
];
const HIGH8: [&str; 4] = ["ah", "ch", "dh", "bh"];

pub fn register(name: &str) -> Option<Reg> {
    let name = name.to_ascii_lowercase();
    let n = name.as_str();
    let find = |table: &[&str]| table.iter().position(|r| *r == n).map(|i| i as u8);
    if let Some(num) = find(&R64) {
        return Some(Reg { num, size: 8, kind: RegKind::General });
    }
    if let Some(num) = find(&R32) {
        return Some(Reg { num, size: 4, kind: RegKind::General });
    }
    if let Some(num) = find(&R16) {
        return Some(Reg { num, size: 2, kind: RegKind::General });
    }
    if let Some(num) = find(&R8) {
        let kind = if (4..8).contains(&num) { RegKind::Rex8 } else { RegKind::General };
        return Some(Reg { num, size: 1, kind });
    }
    if let Some(i) = find(&HIGH8) {
        return Some(Reg { num: i + 4, size: 1, kind: RegKind::High8 });
    }
    // Alternative names for the low bytes of r8-r15.
    if let Some(rest) = n.strip_prefix('r').and_then(|r| r.strip_suffix('l')) {
        if let Ok(num @ 8..=15) = rest.parse::<u8>() {
            return Some(Reg { num, size: 1, kind: RegKind::General });
        }
    }
    if let Some(num) = n.strip_prefix("cr").and_then(|c| c.parse::<u8>().ok()) {
        if matches!(num, 0 | 2 | 3 | 4 | 8) {
            return Some(Reg { num, size: 8, kind: RegKind::Control });
        }
    }
    None
}

/// A memory operand. Without base and index registers the address is RIP-relative.
#[derive(Clone, Copy, Debug)]
pub struct Mem {
    pub size: Option<u8>,
    pub base: Option<Reg>,
    pub index: Option<(Reg, u8)>,
    pub disp: Value,
}

#[derive(Clone, Copy, Debug)]
pub enum Operand {
    Reg(Reg),
    Mem(Mem),
    Imm(Value),
}

/// Jump distance requested with `short` / `near`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Distance {
    Default,
    Short,
    Near,
}

pub struct Parsed {
    pub op: Operand,
    pub distance: Distance,
    /// An explicit `byte`/`word`/`dword`/`qword` before a register or immediate.
    pub size_hint: Option<u8>,
}

fn size_keyword(word: &str) -> Option<u8> {
    match word.to_ascii_lowercase().as_str() {
        "byte" => Some(1),
        "word" => Some(2),
        "dword" => Some(4),
        "qword" => Some(8),
        _ => None,
    }
}

/// Splits `text` on top-level commas (outside brackets, parentheses and quotes).
pub fn split_operands(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match quote {
            Some(q) => {
                current.push(c);
                if c == q {
                    quote = None;
                }
                continue;
            }
            None if c == '\'' || c == '"' || c == '`' => quote = Some(c),
            None if c == '[' || c == '(' => depth += 1,
            None if c == ']' || c == ')' => depth -= 1,
            None if c == ',' && depth == 0 => {
                parts.push(String::from(current.trim()));
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if !current.trim().is_empty() || !parts.is_empty() {
        parts.push(String::from(current.trim()));
    }
    parts
}

pub fn parse(text: &str, ctx: &Ctx) -> Result<Parsed, String> {
    let mut rest = text.trim();
    let mut size = None;
    let mut distance = Distance::Default;
    loop {
        let (word, after) = match rest.find(|c: char| c.is_whitespace() || c == '[') {
            Some(i) => (&rest[..i], rest[i..].trim_start()),
            None => (rest, ""),
        };
        if after.is_empty() && !rest.starts_with('[') {
            break;
        }
        if let Some(s) = size_keyword(word) {
            size = Some(s);
        } else if word.eq_ignore_ascii_case("short") {
            distance = Distance::Short;
        } else if word.eq_ignore_ascii_case("near") {
            distance = Distance::Near;
        } else if word.eq_ignore_ascii_case("ptr") || word.eq_ignore_ascii_case("strict") {
        } else {
            break;
        }
        rest = after;
    }
    if let Some(inner) = rest.strip_prefix('[') {
        let inner = inner.strip_suffix(']').ok_or("missing ] in memory operand")?;
        let mem = parse_mem(inner, size, ctx)?;
        return Ok(Parsed { op: Operand::Mem(mem), distance, size_hint: None });
    }
    if let Some(reg) = register(rest) {
        return Ok(Parsed { op: Operand::Reg(reg), distance, size_hint: size });
    }
    let value = expr::eval(rest, ctx)?;
    Ok(Parsed { op: Operand::Imm(value), distance, size_hint: size })
}

fn parse_mem(inner: &str, size: Option<u8>, ctx: &Ctx) -> Result<Mem, String> {
    let mut inner = inner.trim();
    for keyword in ["rel ", "abs ", "REL ", "ABS "] {
        if let Some(r) = inner.strip_prefix(keyword) {
            inner = r.trim_start();
        }
    }
    // Split into terms at top-level + and -, keeping each term's sign.
    let mut terms: Vec<(bool, String)> = Vec::new();
    let mut current = String::new();
    let mut negative = false;
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            '+' | '-' if depth == 0 => {
                if !current.trim().is_empty() {
                    terms.push((negative, core::mem::take(&mut current)));
                } else if c == '-' {
                    negative = !negative;
                    continue;
                } else {
                    continue;
                }
                negative = c == '-';
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if !current.trim().is_empty() {
        terms.push((negative, current));
    }

    let mut base = None;
    let mut index = None;
    let mut disp_text = String::new();
    for (neg, term) in terms {
        let term = term.trim();
        let scaled = term.split_once('*').map(|(a, b)| (a.trim(), b.trim()));
        let reg_part = match scaled {
            Some((a, b)) => register(a).map(|r| (r, b)).or_else(|| register(b).map(|r| (r, a))),
            None => register(term).map(|r| (r, "1")),
        };
        match reg_part {
            Some((reg, scale_text)) => {
                if neg {
                    return Err(String::from("a register cannot be subtracted in an address"));
                }
                if reg.size != 8 || reg.kind != RegKind::General {
                    return Err(String::from("addresses must use 64-bit registers"));
                }
                let scale = expr::eval(scale_text, ctx)?.v;
                if scaled.is_none() && base.is_none() {
                    base = Some(reg);
                } else {
                    if index.is_some() {
                        return Err(String::from("too many registers in address"));
                    }
                    if !matches!(scale, 1 | 2 | 4 | 8) {
                        return Err(String::from("scale must be 1, 2, 4 or 8"));
                    }
                    index = Some((reg, scale as u8));
                }
            }
            None => {
                disp_text.push_str(if neg { " - (" } else { " + (" });
                disp_text.push_str(term);
                disp_text.push(')');
            }
        }
    }
    // An index of rsp is impossible; a lone `rsp*1` can be used as the base instead.
    if let Some((reg, 1)) = index {
        if base.is_none() {
            base = Some(reg);
            index = None;
        }
    }
    if let Some((reg, _)) = index {
        if reg.num == 4 {
            return Err(String::from("rsp cannot be an index register"));
        }
    }
    let disp = if disp_text.is_empty() { Value { v: 0, constant: true } } else { expr::eval(&disp_text, ctx)? };
    Ok(Mem { size, base, index, disp })
}
