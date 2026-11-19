//! vasm: a small x86-64 assembler for a NASM-like Intel syntax subset. Used by the kernel (the
//! `asm` command and running `.asm` files) and by the xtask (prebuilt example programs).
//!
//! Supported: labels (`name:`, local `.name`, and `name db ...` without a colon), `equ`,
//! `db/dw/dd/dq`, `resb/resw/resd/resq`, `times`, `align`/`alignb`, expressions with `$` and
//! `$$`, the common integer instructions, and `[base + index*scale + disp]` / `[label]`
//! (RIP-relative) memory operands. Programs are flat images loaded at a fixed origin.
//!
//! Assembly runs three passes over the source: the first finds label addresses, the second
//! settles `equ` values that refer forward, the third emits the bytes. Instruction sizes never
//! depend on values that can change between passes, so addresses are stable.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod encode;
mod expr;
mod operand;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use expr::Value;

/// Where the OS loads programs; `.vex` files and `.asm` sources are assembled for this address.
pub const PROGRAM_BASE: u64 = 0x400_0000;

/// `.vex` executable header: magic, entry offset, image size, zero-fill size (all u32 LE).
pub const VEX_MAGIC: &[u8; 4] = b"VEX1";
pub const VEX_HEADER_SIZE: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

pub struct Program {
    pub image: Vec<u8>,
    /// Offset of the entry point (`_start` or `start`, else the first byte) in `image`.
    pub entry: u32,
    /// Zeroed memory reserved after the image.
    pub zero_fill: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VexHeader {
    pub entry: u32,
    pub image_size: u32,
    pub zero_fill: u32,
}

pub fn to_vex(program: &Program) -> Vec<u8> {
    let mut out = Vec::with_capacity(VEX_HEADER_SIZE + program.image.len());
    out.extend_from_slice(VEX_MAGIC);
    out.extend_from_slice(&program.entry.to_le_bytes());
    out.extend_from_slice(&(program.image.len() as u32).to_le_bytes());
    out.extend_from_slice(&program.zero_fill.to_le_bytes());
    out.extend_from_slice(&program.image);
    out
}

pub fn parse_vex(data: &[u8]) -> Result<(VexHeader, &[u8]), &'static str> {
    if data.len() < VEX_HEADER_SIZE || &data[..4] != VEX_MAGIC {
        return Err("not a .vex executable (bad header)");
    }
    let field = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
    let header = VexHeader { entry: field(4), image_size: field(8), zero_fill: field(12) };
    let image = &data[VEX_HEADER_SIZE..];
    if image.len() != header.image_size as usize {
        return Err("corrupt .vex executable (image size does not match the header)");
    }
    if header.entry as usize >= image.len().max(1) {
        return Err("corrupt .vex executable (entry point outside the image)");
    }
    Ok((header, image))
}

#[derive(Clone, Copy)]
struct Sym {
    value: i64,
    line: usize,
}

/// What expressions can see while one line is assembled.
pub(crate) struct Ctx<'a> {
    symbols: &'a BTreeMap<String, Sym>,
    line: usize,
    here: u64,
    origin: u64,
    final_pass: bool,
    scope: &'a str,
}

impl Ctx<'_> {
    pub fn symbol(&self, name: &str) -> Result<Value, String> {
        match name {
            "$" => return Ok(Value { v: self.here as i64, constant: true }),
            "$$" => return Ok(Value { v: self.origin as i64, constant: true }),
            _ => {}
        }
        let full = full_name(self.scope, name);
        match self.symbols.get(&full) {
            Some(sym) => Ok(Value { v: sym.value, constant: sym.line < self.line }),
            None if !self.final_pass => Ok(Value { v: 0, constant: false }),
            None if operand::register(name).is_some() => Err(alloc::format!("register {name} cannot be used here")),
            None => Err(alloc::format!("undefined symbol: {name}")),
        }
    }
}

fn full_name(scope: &str, name: &str) -> String {
    if name.starts_with('.') {
        alloc::format!("{scope}{name}")
    } else {
        String::from(name)
    }
}

const DATA: [(&str, usize); 4] = [("db", 1), ("dw", 2), ("dd", 4), ("dq", 8)];
const RESERVE: [(&str, usize); 4] = [("resb", 1), ("resw", 2), ("resd", 4), ("resq", 8)];
const IGNORED: [&str; 7] = ["bits", "use64", "default", "section", "segment", "global", "cpu"];

struct Pass {
    symbols: BTreeMap<String, Sym>,
    origin: u64,
    final_pass: bool,
    first_pass: bool,
    out: Vec<u8>,
    scope: String,
    line: usize,
}

/// Removes a `;` comment, ignoring semicolons inside quotes.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' || c == '`' => quote = Some(c),
            None if c == ';' => return &line[..i],
            None => {}
        }
    }
    line
}

/// Splits off the first whitespace-separated word.
fn split_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(expr::is_ident_start) && chars.all(expr::is_ident_char)
}

fn find<'t>(table: &'t [(&'t str, usize)], word: &str) -> Option<usize> {
    table.iter().find(|(name, _)| name.eq_ignore_ascii_case(word)).map(|&(_, n)| n)
}

impl Pass {
    fn here(&self) -> u64 {
        self.origin + self.out.len() as u64
    }

    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            symbols: &self.symbols,
            line: self.line,
            here: self.here(),
            origin: self.origin,
            final_pass: self.final_pass,
            scope: &self.scope,
        }
    }

    fn eval(&self, text: &str) -> Result<Value, String> {
        expr::eval(text, &self.ctx())
    }

    /// An expression whose value must already be known (counts, sizes).
    fn eval_constant(&self, text: &str, what: &str) -> Result<i64, String> {
        let v = self.eval(text)?;
        if !v.constant {
            return Err(alloc::format!("{what} must only use numbers and symbols defined earlier"));
        }
        Ok(v.v)
    }

    fn define(&mut self, name: &str, value: i64) -> Result<(), String> {
        if operand::register(name).is_some() {
            return Err(alloc::format!("{name} is a register name"));
        }
        let full = full_name(&self.scope, name);
        if self.first_pass && self.symbols.contains_key(&full) {
            return Err(alloc::format!("{full} is defined more than once"));
        }
        self.symbols.insert(full, Sym { value, line: self.line });
        Ok(())
    }

    fn define_label(&mut self, name: &str) -> Result<(), String> {
        if !is_ident(name) {
            return Err(alloc::format!("invalid label name: {name}"));
        }
        if !name.starts_with('.') {
            self.scope = String::from(name);
        }
        self.define(name, self.here() as i64)
    }

    fn line(&mut self, raw: &str) -> Result<(), String> {
        let text = strip_comment(raw).trim();
        if text.is_empty() {
            return Ok(());
        }
        let mut rest = text;

        // `label:` at the start of the line.
        let ident_len = rest.find(|c: char| !expr::is_ident_char(c)).unwrap_or(rest.len());
        if ident_len > 0 && rest[ident_len..].starts_with(':') {
            self.define_label(&rest[..ident_len])?;
            rest = rest[ident_len + 1..].trim();
            if rest.is_empty() {
                return Ok(());
            }
        }

        let (first, after) = split_word(rest);
        let (second, after_second) = split_word(after);
        if second.eq_ignore_ascii_case("equ") {
            let value = self.eval(after_second)?;
            return self.define(first, value.v);
        }
        // `name db ...` without a colon.
        let data_word = find(&DATA, second).is_some()
            || find(&RESERVE, second).is_some()
            || second.eq_ignore_ascii_case("times");
        if data_word && is_ident(first) && operand::register(first).is_none() {
            self.define_label(first)?;
            rest = after;
        }
        self.statement(rest)
    }

    /// A directive or instruction, without a label.
    fn statement(&mut self, text: &str) -> Result<(), String> {
        let (word, mut args) = split_word(text);
        let lower = word.to_ascii_lowercase();

        if lower == "times" {
            let (count_text, body) = split_times(args)?;
            let count = self.eval_constant(count_text, "the times count")?;
            if !(0..=1 << 24).contains(&count) {
                return Err(String::from("times count out of range"));
            }
            for _ in 0..count {
                self.statement(body)?;
            }
            return Ok(());
        }
        if let Some(unit) = find(&DATA, &lower) {
            return self.data(unit, args);
        }
        if let Some(unit) = find(&RESERVE, &lower) {
            let count = self.eval_constant(args, "the reserve size")?;
            if !(0..=1 << 26).contains(&count) {
                return Err(String::from("reserve size out of range"));
            }
            self.out.resize(self.out.len() + count as usize * unit, 0);
            return Ok(());
        }
        if lower == "align" || lower == "alignb" {
            let n = self.eval_constant(args, "the alignment")?;
            if n <= 0 || n & (n - 1) != 0 {
                return Err(String::from("alignment must be a power of two"));
            }
            let fill = if lower == "align" { 0x90 } else { 0 };
            while self.here() % n as u64 != 0 {
                self.out.push(fill);
            }
            return Ok(());
        }
        if lower == "org" {
            let v = self.eval_constant(args, "org")?;
            if v as u64 != self.origin {
                return Err(alloc::format!("programs always load at {:#x}; remove the org line", self.origin));
            }
            return Ok(());
        }
        if IGNORED.contains(&lower.as_str()) || lower.starts_with("[bits") || lower.starts_with("[section") {
            return Ok(());
        }

        // Prefixes.
        let mut prefixes = Vec::new();
        let mut mnemonic = lower;
        loop {
            let prefix = match mnemonic.as_str() {
                "rep" | "repe" | "repz" => 0xF3,
                "repne" | "repnz" => 0xF2,
                "lock" => 0xF0,
                _ => break,
            };
            prefixes.push(prefix);
            let (next, rest) = split_word(args);
            if next.is_empty() {
                return Err(String::from("prefix without an instruction"));
            }
            mnemonic = next.to_ascii_lowercase();
            args = rest;
        }

        let ctx = self.ctx();
        let operands = operand::split_operands(args)
            .iter()
            .map(|o| operand::parse(o, &ctx))
            .collect::<Result<Vec<_>, _>>()?;
        let bytes = encode::encode(&mnemonic, &prefixes, &operands, self.here(), self.final_pass)?;
        self.out.extend_from_slice(&bytes);
        Ok(())
    }

    fn data(&mut self, unit: usize, args: &str) -> Result<(), String> {
        let items = operand::split_operands(args);
        if items.is_empty() {
            return Err(String::from("missing data"));
        }
        for item in items {
            let quote = item.chars().next().filter(|c| matches!(c, '\'' | '"' | '`'));
            if let (Some(q), true) = (quote, item.len() >= 2 && item.ends_with(quote.unwrap_or(' '))) {
                let body = &item[1..item.len() - 1];
                if !body.contains(q) {
                    let bytes = expr::unquote(body, q)?;
                    let padded = bytes.len().next_multiple_of(unit).max(unit);
                    self.out.extend_from_slice(&bytes);
                    self.out.resize(self.out.len() + padded - bytes.len(), 0);
                    continue;
                }
            }
            let v = self.eval(&item)?;
            if self.final_pass && unit < 8 {
                let bits = unit as u32 * 8;
                let min = -(1i64 << (bits - 1));
                let max = (1i64 << bits) - 1;
                if v.v < min || v.v > max {
                    return Err(alloc::format!("value {} does not fit in {bits} bits", v.v));
                }
            }
            self.out.extend_from_slice(&v.v.to_le_bytes()[..unit]);
        }
        Ok(())
    }
}

/// Splits `times <count> <statement>` at the first space after which a word made only of
/// letters begins (a directive or mnemonic), so counts like `510 - ($ - $$)` stay whole.
fn split_times(args: &str) -> Result<(&str, &str), String> {
    for (i, c) in args.char_indices() {
        if !c.is_whitespace() {
            continue;
        }
        let count = args[..i].trim();
        let body = args[i..].trim_start();
        let (word, _) = split_word(body);
        if !count.is_empty() && word.len() >= 2 && word.chars().all(|c| c.is_ascii_alphabetic()) {
            return Ok((count, body));
        }
    }
    Err(String::from("times needs a count and a statement"))
}

/// Assembles `source` for loading at `origin`.
pub fn assemble(source: &str, origin: u64) -> Result<Program, Error> {
    assemble_with_sizes(source, origin).map(|(p, _)| p)
}

/// Like `assemble`, also returning how many bytes each source line produced.
pub(crate) fn assemble_with_sizes(source: &str, origin: u64) -> Result<(Program, Vec<usize>), Error> {
    let mut symbols = BTreeMap::new();
    let mut sizes: Vec<usize> = Vec::new();
    let mut image = Vec::new();
    for pass_no in 0..3 {
        let mut pass = Pass {
            symbols: core::mem::take(&mut symbols),
            origin,
            final_pass: pass_no == 2,
            first_pass: pass_no == 0,
            out: Vec::new(),
            scope: String::new(),
            line: 0,
        };
        for (i, raw) in source.lines().enumerate() {
            pass.line = i + 1;
            let start = pass.out.len();
            pass.line(raw).map_err(|message| Error { line: i + 1, message })?;
            let size = pass.out.len() - start;
            if pass_no == 0 {
                sizes.push(size);
            } else if sizes[i] != size {
                return Err(Error { line: i + 1, message: String::from("internal error: size changed between passes") });
            }
        }
        symbols = pass.symbols;
        image = pass.out;
    }
    let entry = ["_start", "start"]
        .iter()
        .find_map(|name| symbols.get(*name))
        .map_or(0, |sym| (sym.value as u64).wrapping_sub(origin));
    if entry > image.len() as u64 {
        return Err(Error { line: 0, message: String::from("the entry point is outside the program") });
    }
    Ok((Program { image, entry: entry as u32, zero_fill: 0 }, sizes))
}

#[cfg(test)]
mod tests;
