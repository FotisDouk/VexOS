//! x86-64 instruction encoding.
//!
//! Every choice between encodings of different lengths depends only on register operands and
//! on `Value::constant` (never on a value that might change between passes), so pass 1 and
//! pass 2 always agree on instruction sizes.

use crate::expr::Value;
use crate::operand::{Distance, Mem, Operand, Parsed, Reg, RegKind};
use alloc::string::String;
use alloc::vec::Vec;

type R<T> = Result<T, String>;

fn err<T>(msg: &str) -> R<T> {
    Err(String::from(msg))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Disp {
    None,
    D8(i8),
    D32(i32),
    /// RIP-relative to an absolute target address.
    Rip(i64),
}

struct Inst {
    prefixes: Vec<u8>,
    p66: bool,
    w: bool,
    r: bool,
    x: bool,
    b: bool,
    force_rex: bool,
    no_rex: bool,
    opcode: Vec<u8>,
    modrm: Option<u8>,
    sib: Option<u8>,
    disp: Disp,
    imm: Vec<u8>,
    /// Relative branch: (target, width of the displacement in bytes), placed after the opcode.
    rel: Option<(i64, u8)>,
}

impl Inst {
    fn new(opcode: &[u8]) -> Inst {
        Inst {
            prefixes: Vec::new(),
            p66: false,
            w: false,
            r: false,
            x: false,
            b: false,
            force_rex: false,
            no_rex: false,
            opcode: opcode.to_vec(),
            modrm: None,
            sib: None,
            disp: Disp::None,
            imm: Vec::new(),
            rel: None,
        }
    }

    /// Sets the operand size: 2 adds 0x66, 8 sets REX.W.
    fn size(&mut self, size: u8) {
        self.p66 = size == 2;
        self.w = size == 8;
    }

    fn note_reg(&mut self, reg: Reg) {
        match reg.kind {
            RegKind::High8 => self.no_rex = true,
            RegKind::Rex8 => self.force_rex = true,
            _ => {}
        }
    }

    /// ModRM with a register in the r/m field.
    fn rm_reg(&mut self, reg_field: u8, rm: Reg) {
        self.note_reg(rm);
        self.r |= reg_field >= 8;
        self.b |= rm.num >= 8;
        self.modrm = Some(0xC0 | (reg_field & 7) << 3 | (rm.num & 7));
    }

    /// The `reg` field holds a register operand.
    fn reg_operand(&mut self, reg: Reg) -> u8 {
        self.note_reg(reg);
        reg.num
    }

    /// ModRM (+ SIB + displacement) for a memory operand.
    fn rm_mem(&mut self, reg_field: u8, mem: &Mem) -> R<()> {
        self.r |= reg_field >= 8;
        let reg = (reg_field & 7) << 3;
        let disp = mem.disp;
        match (mem.base, mem.index) {
            (None, None) => {
                self.modrm = Some(reg | 0b101);
                self.disp = Disp::Rip(disp.v);
            }
            (base, index) => {
                let short = disp.constant && i8::try_from(disp.v).is_ok();
                let disp32 = || i32::try_from(disp.v).map_err(|_| String::from("address offset does not fit in 32 bits"));
                let (mode, d) = match base {
                    None => (0b00, Disp::D32(if disp.constant { disp32()? } else { disp.v as i32 })),
                    Some(b) if disp.constant && disp.v == 0 && b.num & 7 != 5 => (0b00, Disp::None),
                    Some(_) if short => (0b01, Disp::D8(disp.v as i8)),
                    Some(_) => (0b10, Disp::D32(if disp.constant { disp32()? } else { disp.v as i32 })),
                };
                self.disp = d;
                let needs_sib = index.is_some() || base.is_none() || base.is_some_and(|b| b.num & 7 == 4);
                if needs_sib {
                    let (scale_bits, index_num) = match index {
                        Some((r, s)) => (s.trailing_zeros() as u8, r.num),
                        None => (0, 4), // no index
                    };
                    let base_num = base.map_or(5, |b| b.num);
                    self.x |= index_num >= 8 && index.is_some();
                    self.b |= base.is_some_and(|b| b.num >= 8);
                    self.modrm = Some(mode << 6 | reg | 0b100);
                    self.sib = Some(scale_bits << 6 | (index_num & 7) << 3 | (base_num & 7));
                } else {
                    let b = base.unwrap();
                    self.b |= b.num >= 8;
                    self.modrm = Some(mode << 6 | reg | (b.num & 7));
                }
            }
        }
        Ok(())
    }

    fn rm(&mut self, reg_field: u8, op: &Operand) -> R<()> {
        match op {
            Operand::Reg(r) => {
                self.rm_reg(reg_field, *r);
                Ok(())
            }
            Operand::Mem(m) => self.rm_mem(reg_field, m),
            Operand::Imm(_) => err("expected a register or memory operand"),
        }
    }

    fn finish(self, addr: u64, check: bool) -> R<Vec<u8>> {
        let rex = 0x40 | (self.w as u8) << 3 | (self.r as u8) << 2 | (self.x as u8) << 1 | self.b as u8;
        let need_rex = rex != 0x40 || self.force_rex;
        if need_rex && self.no_rex {
            return err("ah/bh/ch/dh cannot be used in an instruction that needs a REX prefix");
        }
        let mut out = self.prefixes.clone();
        if self.p66 {
            out.push(0x66);
        }
        if need_rex {
            out.push(rex);
        }
        out.extend_from_slice(&self.opcode);
        if let Some((_, width)) = self.rel {
            out.extend(core::iter::repeat_n(0, width as usize));
        }
        if let Some(m) = self.modrm {
            out.push(m);
        }
        if let Some(s) = self.sib {
            out.push(s);
        }
        let disp_at = out.len();
        match self.disp {
            Disp::None => {}
            Disp::D8(d) => out.push(d as u8),
            Disp::D32(d) => out.extend_from_slice(&d.to_le_bytes()),
            Disp::Rip(_) => out.extend_from_slice(&[0; 4]),
        }
        out.extend_from_slice(&self.imm);
        let end = addr as i64 + out.len() as i64;
        if let Disp::Rip(target) = self.disp {
            let rel = target.wrapping_sub(end);
            if check && i32::try_from(rel).is_err() {
                return err("address is too far for a RIP-relative operand; load it into a register first");
            }
            out[disp_at..disp_at + 4].copy_from_slice(&(rel as i32).to_le_bytes());
        }
        if let Some((target, width)) = self.rel {
            let at = self.prefixes.len() + self.p66 as usize + need_rex as usize + self.opcode.len();
            let rel = target.wrapping_sub(end);
            if width == 1 {
                if check && i8::try_from(rel).is_err() {
                    return err("jump target out of range for a short jump");
                }
                out[at] = rel as u8;
            } else {
                if check && i32::try_from(rel).is_err() {
                    return err("jump target out of range");
                }
                out[at..at + 4].copy_from_slice(&(rel as i32).to_le_bytes());
            }
        }
        Ok(out)
    }
}

fn imm_bytes(v: Value, size: u8, check: bool, signed_only: bool) -> R<Vec<u8>> {
    let x = v.v;
    let ok = match size {
        1 => (-128..=255).contains(&x),
        2 => (-32768..=65535).contains(&x),
        4 if signed_only => i32::try_from(x).is_ok(),
        4 => (i32::MIN as i64..=u32::MAX as i64).contains(&x),
        _ => true,
    };
    if check && !ok {
        return Err(alloc::format!("value {x} does not fit in {} bit(s)", size as u32 * 8));
    }
    Ok(x.to_le_bytes()[..size as usize].to_vec())
}

fn fits_i8(v: Value) -> bool {
    v.constant && i8::try_from(v.v).is_ok()
}

/// Size of an instruction's operation, from its register operands or explicit memory size.
fn op_size(ops: &[&Parsed]) -> R<u8> {
    let mut size = None;
    for p in ops {
        let s = match p.op {
            Operand::Reg(r) => Some(r.size),
            Operand::Mem(m) => m.size,
            Operand::Imm(_) => p.size_hint,
        };
        if let Some(s) = s {
            if size.is_some_and(|prev| prev != s) {
                return err("operand sizes do not match");
            }
            size = Some(s);
        }
    }
    size.ok_or_else(|| String::from("operation size not specified (add byte/word/dword/qword)"))
}

const CONDITIONS: [(&str, u8); 30] = [
    ("o", 0), ("no", 1), ("b", 2), ("c", 2), ("nae", 2), ("ae", 3), ("nb", 3), ("nc", 3), ("e", 4), ("z", 4),
    ("ne", 5), ("nz", 5), ("be", 6), ("na", 6), ("a", 7), ("nbe", 7), ("s", 8), ("ns", 9), ("p", 10), ("pe", 10),
    ("np", 11), ("po", 11), ("l", 12), ("nge", 12), ("ge", 13), ("nl", 13), ("le", 14), ("ng", 14), ("g", 15),
    ("nle", 15),
];

fn condition(suffix: &str) -> Option<u8> {
    CONDITIONS.iter().find(|(name, _)| *name == suffix).map(|&(_, cc)| cc)
}

const ALU: [&str; 8] = ["add", "or", "adc", "sbb", "and", "sub", "xor", "cmp"];
const SHIFTS: [(&str, u8); 8] =
    [("rol", 0), ("ror", 1), ("rcl", 2), ("rcr", 3), ("shl", 4), ("sal", 4), ("shr", 5), ("sar", 7)];
const UNARY: [(&str, u8); 6] = [("not", 2), ("neg", 3), ("mul", 4), ("div", 6), ("idiv", 7), ("imul", 5)];

const NO_OPERANDS: &[(&str, &[u8])] = &[
    ("nop", &[0x90]), ("hlt", &[0xF4]), ("cli", &[0xFA]), ("sti", &[0xFB]), ("cld", &[0xFC]), ("std", &[0xFD]),
    ("clc", &[0xF8]), ("stc", &[0xF9]), ("cmc", &[0xF5]), ("ret", &[0xC3]), ("leave", &[0xC9]),
    ("int3", &[0xCC]), ("iretq", &[0x48, 0xCF]), ("cbw", &[0x66, 0x98]), ("cwde", &[0x98]),
    ("cdqe", &[0x48, 0x98]), ("cwd", &[0x66, 0x99]), ("cdq", &[0x99]), ("cqo", &[0x48, 0x99]),
    ("pushf", &[0x9C]), ("pushfq", &[0x9C]), ("popf", &[0x9D]), ("popfq", &[0x9D]), ("cpuid", &[0x0F, 0xA2]),
    ("rdtsc", &[0x0F, 0x31]), ("rdmsr", &[0x0F, 0x32]), ("wrmsr", &[0x0F, 0x30]), ("ud2", &[0x0F, 0x0B]),
    ("pause", &[0xF3, 0x90]), ("wbinvd", &[0x0F, 0x09]),
    ("movsb", &[0xA4]), ("movsw", &[0x66, 0xA5]), ("movsd", &[0xA5]), ("movsq", &[0x48, 0xA5]),
    ("stosb", &[0xAA]), ("stosw", &[0x66, 0xAB]), ("stosd", &[0xAB]), ("stosq", &[0x48, 0xAB]),
    ("lodsb", &[0xAC]), ("lodsw", &[0x66, 0xAD]), ("lodsd", &[0xAD]), ("lodsq", &[0x48, 0xAD]),
    ("scasb", &[0xAE]), ("scasw", &[0x66, 0xAF]), ("scasd", &[0xAF]), ("scasq", &[0x48, 0xAF]),
    ("cmpsb", &[0xA6]), ("cmpsw", &[0x66, 0xA7]), ("cmpsd", &[0xA7]), ("cmpsq", &[0x48, 0xA7]),
];

pub fn encode(mnemonic: &str, prefixes: &[u8], ops: &[Parsed], addr: u64, check: bool) -> R<Vec<u8>> {
    let m = mnemonic.to_ascii_lowercase();
    let mut inst = build(&m, ops, check)?;
    let mut all = prefixes.to_vec();
    all.extend_from_slice(&inst.prefixes);
    inst.prefixes = all;
    inst.finish(addr, check)
}

fn expect_count(ops: &[Parsed], n: usize, m: &str) -> R<()> {
    if ops.len() == n {
        Ok(())
    } else {
        Err(alloc::format!("{m} takes {n} operand(s)"))
    }
}

fn build(m: &str, ops: &[Parsed], check: bool) -> R<Inst> {
    if let Some((_, bytes)) = NO_OPERANDS.iter().find(|(name, _)| *name == m) {
        if !(ops.is_empty() || (m == "ret" && ops.len() == 1)) {
            return Err(alloc::format!("{m} takes no operands"));
        }
        if m == "ret" && ops.len() == 1 {
            let Operand::Imm(v) = ops[0].op else { return err("ret takes an immediate") };
            let mut i = Inst::new(&[0xC2]);
            i.imm = imm_bytes(v, 2, check, false)?;
            return Ok(i);
        }
        return Ok(Inst::new(bytes));
    }
    if let Some(n) = ALU.iter().position(|&a| a == m) {
        expect_count(ops, 2, m)?;
        return alu(n as u8, &ops[0], &ops[1], check);
    }
    if let Some(&(_, n)) = SHIFTS.iter().find(|(name, _)| *name == m) {
        expect_count(ops, 2, m)?;
        return shift(n, &ops[0], &ops[1], check);
    }
    if m == "imul" && ops.len() > 1 {
        return imul(ops, check);
    }
    if let Some(&(_, n)) = UNARY.iter().find(|(name, _)| *name == m) {
        expect_count(ops, 1, m)?;
        let size = op_size(&[&ops[0]])?;
        let mut i = Inst::new(&[if size == 1 { 0xF6 } else { 0xF7 }]);
        i.size(size);
        i.rm(n, &ops[0].op)?;
        return Ok(i);
    }
    if m == "inc" || m == "dec" {
        expect_count(ops, 1, m)?;
        let size = op_size(&[&ops[0]])?;
        let mut i = Inst::new(&[if size == 1 { 0xFE } else { 0xFF }]);
        i.size(size);
        i.rm((m == "dec") as u8, &ops[0].op)?;
        return Ok(i);
    }
    if let Some(cc) = m.strip_prefix('j').and_then(condition) {
        expect_count(ops, 1, m)?;
        let Operand::Imm(target) = ops[0].op else { return err("conditional jumps need a label") };
        return Ok(if ops[0].distance == Distance::Short {
            let mut i = Inst::new(&[0x70 + cc]);
            i.rel = Some((target.v, 1));
            i
        } else {
            let mut i = Inst::new(&[0x0F, 0x80 + cc]);
            i.rel = Some((target.v, 4));
            i
        });
    }
    if let Some(cc) = m.strip_prefix("set").and_then(condition) {
        expect_count(ops, 1, m)?;
        if op_size(&[&ops[0]]).unwrap_or(1) != 1 {
            return err("set<cc> writes a byte");
        }
        let mut i = Inst::new(&[0x0F, 0x90 + cc]);
        i.rm(0, &ops[0].op)?;
        return Ok(i);
    }
    if let Some(cc) = m.strip_prefix("cmov").and_then(condition) {
        expect_count(ops, 2, m)?;
        let Operand::Reg(dst) = ops[0].op else { return err("cmov needs a register destination") };
        if dst.size == 1 {
            return err("cmov does not work on bytes");
        }
        let mut i = Inst::new(&[0x0F, 0x40 + cc]);
        i.size(op_size(&[&ops[0], &ops[1]])?);
        let reg = i.reg_operand(dst);
        i.rm(reg, &ops[1].op)?;
        return Ok(i);
    }
    match m {
        "mov" => {
            expect_count(ops, 2, m)?;
            mov(&ops[0], &ops[1], check)
        }
        "test" => {
            expect_count(ops, 2, m)?;
            test(&ops[0], &ops[1], check)
        }
        "xchg" => {
            expect_count(ops, 2, m)?;
            xchg(&ops[0], &ops[1])
        }
        "lea" => {
            expect_count(ops, 2, m)?;
            let (Operand::Reg(dst), Operand::Mem(mem)) = (ops[0].op, ops[1].op) else {
                return err("lea needs a register and a memory operand");
            };
            if dst.size == 1 {
                return err("lea cannot target a byte register");
            }
            let mut i = Inst::new(&[0x8D]);
            i.size(dst.size);
            let reg = i.reg_operand(dst);
            i.rm_mem(reg, &mem)?;
            Ok(i)
        }
        "movzx" | "movsx" => {
            expect_count(ops, 2, m)?;
            let Operand::Reg(dst) = ops[0].op else { return err("movzx/movsx need a register destination") };
            let src_size = match ops[1].op {
                Operand::Reg(r) => r.size,
                Operand::Mem(mm) => mm.size.ok_or("source size not specified (byte or word)")?,
                Operand::Imm(_) => return err("movzx/movsx need a register or memory source"),
            };
            if src_size > 2 || dst.size <= src_size {
                return err("movzx/movsx extend a byte or word into a larger register");
            }
            let base = if m == "movzx" { 0xB6 } else { 0xBE };
            let mut i = Inst::new(&[0x0F, base + (src_size == 2) as u8]);
            i.size(dst.size);
            let reg = i.reg_operand(dst);
            i.rm(reg, &ops[1].op)?;
            Ok(i)
        }
        "movsxd" => {
            expect_count(ops, 2, m)?;
            let Operand::Reg(dst) = ops[0].op else { return err("movsxd needs a register destination") };
            let mut i = Inst::new(&[0x63]);
            i.size(8);
            if dst.size != 8 {
                return err("movsxd needs a 64-bit destination");
            }
            let reg = i.reg_operand(dst);
            i.rm(reg, &ops[1].op)?;
            Ok(i)
        }
        "push" | "pop" => {
            expect_count(ops, 1, m)?;
            push_pop(m == "push", &ops[0], check)
        }
        "jmp" | "call" => {
            expect_count(ops, 1, m)?;
            match ops[0].op {
                Operand::Imm(target) => {
                    let short = m == "jmp" && ops[0].distance == Distance::Short;
                    let mut i = Inst::new(&[if short { 0xEB } else if m == "jmp" { 0xE9 } else { 0xE8 }]);
                    i.rel = Some((target.v, if short { 1 } else { 4 }));
                    Ok(i)
                }
                op => {
                    if let Operand::Reg(r) = op {
                        if r.size != 8 {
                            return err("indirect jumps and calls need a 64-bit register");
                        }
                    }
                    let mut i = Inst::new(&[0xFF]);
                    i.rm(if m == "jmp" { 4 } else { 2 }, &op)?;
                    Ok(i)
                }
            }
        }
        "loop" | "loope" | "loopz" | "loopne" | "loopnz" | "jrcxz" => {
            expect_count(ops, 1, m)?;
            let Operand::Imm(target) = ops[0].op else { return err("needs a label") };
            let opcode = match m {
                "loop" => 0xE2,
                "loope" | "loopz" => 0xE1,
                "loopne" | "loopnz" => 0xE0,
                _ => 0xE3,
            };
            let mut i = Inst::new(&[opcode]);
            i.rel = Some((target.v, 1));
            Ok(i)
        }
        "int" => {
            expect_count(ops, 1, m)?;
            let Operand::Imm(v) = ops[0].op else { return err("int needs a number") };
            if v.v == 3 && v.constant {
                return Ok(Inst::new(&[0xCC]));
            }
            let mut i = Inst::new(&[0xCD]);
            i.imm = imm_bytes(v, 1, check, false)?;
            Ok(i)
        }
        "in" | "out" => {
            expect_count(ops, 2, m)?;
            let (acc, port) = if m == "in" { (&ops[0], &ops[1]) } else { (&ops[1], &ops[0]) };
            let Operand::Reg(a) = acc.op else { return err("in/out transfer through al, ax or eax") };
            if a.num != 0 || a.kind != RegKind::General || a.size == 8 {
                return err("in/out transfer through al, ax or eax");
            }
            let wide = (a.size != 1) as u8;
            let out = (m == "out") as u8 * 2;
            let mut i = match port.op {
                Operand::Reg(r) if r.num == 2 && r.size == 2 => Inst::new(&[0xEC + out + wide]),
                Operand::Imm(v) => {
                    let mut i = Inst::new(&[0xE4 + out + wide]);
                    i.imm = imm_bytes(v, 1, check, false)?;
                    i
                }
                _ => return err("the port must be dx or a number 0-255"),
            };
            i.p66 = a.size == 2;
            Ok(i)
        }
        "lgdt" | "lidt" | "sgdt" | "sidt" | "invlpg" => {
            expect_count(ops, 1, m)?;
            let Operand::Mem(mem) = ops[0].op else { return err("needs a memory operand") };
            let n = match m {
                "sgdt" => 0,
                "sidt" => 1,
                "lgdt" => 2,
                "lidt" => 3,
                _ => 7,
            };
            let mut i = Inst::new(&[0x0F, 0x01]);
            i.rm_mem(n, &mem)?;
            Ok(i)
        }
        _ => Err(alloc::format!("unknown instruction: {m}")),
    }
}

fn alu(n: u8, dst: &Parsed, src: &Parsed, check: bool) -> R<Inst> {
    match (dst.op, src.op) {
        (d, Operand::Imm(v)) => {
            let size = op_size(&[dst, src])?;
            let acc = matches!(d, Operand::Reg(r) if r.num == 0 && r.kind == RegKind::General);
            let mut i;
            if size != 1 && fits_i8(v) {
                // Sign-extended 8-bit immediate.
                i = Inst::new(&[0x83]);
                i.rm(n, &d)?;
                i.imm = imm_bytes(v, 1, check, false)?;
            } else if acc {
                // Short form for al / ax / eax / rax.
                i = Inst::new(&[0x04 + 8 * n + (size != 1) as u8]);
                i.imm = imm_bytes(v, size.min(4), check, size == 8)?;
            } else {
                i = Inst::new(&[if size == 1 { 0x80 } else { 0x81 }]);
                i.rm(n, &d)?;
                i.imm = imm_bytes(v, size.min(4), check, size == 8)?;
            }
            i.size(size);
            Ok(i)
        }
        (Operand::Mem(_), Operand::Mem(_)) => err("two memory operands"),
        (d, Operand::Reg(s)) => {
            let size = op_size(&[dst, src])?;
            let mut i = Inst::new(&[8 * n + (size != 1) as u8]);
            i.size(size);
            let reg = i.reg_operand(s);
            i.rm(reg, &d)?;
            Ok(i)
        }
        (Operand::Reg(d), s @ Operand::Mem(_)) => {
            let size = op_size(&[dst, src])?;
            let mut i = Inst::new(&[8 * n + 2 + (size != 1) as u8]);
            i.size(size);
            let reg = i.reg_operand(d);
            i.rm(reg, &s)?;
            Ok(i)
        }
        _ => err("invalid operands"),
    }
}

fn test(a: &Parsed, b: &Parsed, check: bool) -> R<Inst> {
    let (rm, other) = match (a.op, b.op) {
        (Operand::Reg(_), Operand::Mem(_)) => (b, a),
        _ => (a, b),
    };
    match other.op {
        Operand::Imm(v) => {
            let size = op_size(&[rm])?;
            let acc = matches!(rm.op, Operand::Reg(r) if r.num == 0 && r.kind == RegKind::General);
            let mut i = match (acc, size) {
                (true, 1) => Inst::new(&[0xA8]),
                (true, _) => Inst::new(&[0xA9]),
                (false, 1) => Inst::new(&[0xF6]),
                (false, _) => Inst::new(&[0xF7]),
            };
            i.size(size);
            if !acc {
                i.rm(0, &rm.op)?;
            }
            i.imm = imm_bytes(v, size.min(4), check, size == 8)?;
            Ok(i)
        }
        Operand::Reg(r) => {
            let size = op_size(&[rm, other])?;
            let mut i = Inst::new(&[0x84 + (size != 1) as u8]);
            i.size(size);
            let reg = i.reg_operand(r);
            i.rm(reg, &rm.op)?;
            Ok(i)
        }
        Operand::Mem(_) => err("two memory operands"),
    }
}

fn xchg(a: &Parsed, b: &Parsed) -> R<Inst> {
    let size = op_size(&[a, b])?;
    if let (Operand::Reg(x), Operand::Reg(y)) = (a.op, b.op) {
        let acc = |r: Reg| r.num == 0 && r.kind == RegKind::General && r.size != 1;
        if acc(x) || acc(y) {
            let other = if acc(x) { y } else { x };
            let mut i = Inst::new(&[0x90 + (other.num & 7)]);
            i.size(size);
            i.b = other.num >= 8;
            return Ok(i);
        }
    }
    let (rm, reg) = match (a.op, b.op) {
        (Operand::Reg(r), _) => (b.op, r),
        (_, Operand::Reg(r)) => (a.op, r),
        _ => return err("xchg needs a register"),
    };
    let mut i = Inst::new(&[0x86 + (size != 1) as u8]);
    i.size(size);
    let field = i.reg_operand(reg);
    i.rm(field, &rm)?;
    Ok(i)
}

fn mov(dst: &Parsed, src: &Parsed, check: bool) -> R<Inst> {
    match (dst.op, src.op) {
        (Operand::Reg(cr), Operand::Reg(r)) if cr.kind == RegKind::Control => {
            control(0x22, cr, r)
        }
        (Operand::Reg(r), Operand::Reg(cr)) if cr.kind == RegKind::Control => {
            control(0x20, cr, r)
        }
        (Operand::Reg(d), Operand::Imm(v)) => {
            let mut i;
            match d.size {
                8 if v.constant && (0..=u32::MAX as i64).contains(&v.v) => {
                    i = Inst::new(&[0xB8 + (d.num & 7)]);
                    i.imm = imm_bytes(v, 4, check, false)?;
                }
                8 if !v.constant || i32::try_from(v.v).is_ok() => {
                    i = Inst::new(&[0xC7]);
                    i.size(8);
                    i.rm_reg(0, d);
                    i.imm = imm_bytes(v, 4, check, true)?;
                    return Ok(i);
                }
                8 => {
                    i = Inst::new(&[0xB8 + (d.num & 7)]);
                    i.size(8);
                    i.imm = v.v.to_le_bytes().to_vec();
                }
                1 => {
                    i = Inst::new(&[0xB0 + (d.num & 7)]);
                    i.note_reg(d);
                    i.imm = imm_bytes(v, 1, check, false)?;
                }
                size => {
                    i = Inst::new(&[0xB8 + (d.num & 7)]);
                    i.size(size);
                    i.imm = imm_bytes(v, size, check, false)?;
                }
            }
            i.b = d.num >= 8;
            Ok(i)
        }
        (Operand::Mem(mem), Operand::Imm(v)) => {
            let size = op_size(&[dst, src])?;
            let mut i = Inst::new(&[if size == 1 { 0xC6 } else { 0xC7 }]);
            i.size(size);
            i.rm_mem(0, &mem)?;
            i.imm = imm_bytes(v, size.min(4), check, size == 8)?;
            Ok(i)
        }
        (d, Operand::Reg(s)) => {
            let size = op_size(&[dst, src])?;
            let mut i = Inst::new(&[0x88 + (size != 1) as u8]);
            i.size(size);
            let reg = i.reg_operand(s);
            i.rm(reg, &d)?;
            Ok(i)
        }
        (Operand::Reg(d), s @ Operand::Mem(_)) => {
            let size = op_size(&[dst, src])?;
            let mut i = Inst::new(&[0x8A + (size != 1) as u8]);
            i.size(size);
            let reg = i.reg_operand(d);
            i.rm(reg, &s)?;
            Ok(i)
        }
        _ => err("invalid operands for mov"),
    }
}

fn control(opcode: u8, cr: Reg, r: Reg) -> R<Inst> {
    if r.size != 8 || r.kind != RegKind::General {
        return err("control registers move to and from 64-bit registers");
    }
    let mut i = Inst::new(&[0x0F, opcode]);
    i.rm_reg(cr.num, r);
    Ok(i)
}

fn push_pop(push: bool, op: &Parsed, check: bool) -> R<Inst> {
    match op.op {
        Operand::Reg(r) => {
            if r.size != 8 || r.kind != RegKind::General {
                return err("push/pop work on 64-bit registers");
            }
            let mut i = Inst::new(&[if push { 0x50 } else { 0x58 } + (r.num & 7)]);
            i.b = r.num >= 8;
            Ok(i)
        }
        Operand::Imm(v) if push => {
            let mut i;
            if fits_i8(v) {
                i = Inst::new(&[0x6A]);
                i.imm = imm_bytes(v, 1, check, false)?;
            } else {
                i = Inst::new(&[0x68]);
                i.imm = imm_bytes(v, 4, check, true)?;
            }
            Ok(i)
        }
        Operand::Mem(mem) => {
            if mem.size.is_some_and(|s| s != 8) {
                return err("push/pop memory operands are qwords");
            }
            let mut i = Inst::new(&[if push { 0xFF } else { 0x8F }]);
            i.rm_mem(if push { 6 } else { 0 }, &mem)?;
            Ok(i)
        }
        Operand::Imm(_) => err("cannot pop into a number"),
    }
}

fn shift(n: u8, dst: &Parsed, count: &Parsed, check: bool) -> R<Inst> {
    let size = op_size(&[dst])?;
    let wide = (size != 1) as u8;
    let mut i = match count.op {
        Operand::Imm(v) if v.constant && v.v == 1 => Inst::new(&[0xD0 + wide]),
        Operand::Imm(v) => {
            let mut i = Inst::new(&[0xC0 + wide]);
            i.imm = imm_bytes(v, 1, check, false)?;
            i
        }
        Operand::Reg(r) if r.num == 1 && r.size == 1 && r.kind == RegKind::General => Inst::new(&[0xD2 + wide]),
        _ => return err("shift count must be a number or cl"),
    };
    i.size(size);
    i.rm(n, &dst.op)?;
    Ok(i)
}

fn imul(ops: &[Parsed], check: bool) -> R<Inst> {
    let Operand::Reg(dst) = ops[0].op else { return err("imul needs a register destination") };
    if dst.size == 1 {
        return err("imul with two or three operands does not work on bytes");
    }
    let (src, imm) = match ops {
        [_, s] => match s.op {
            Operand::Imm(v) => (Operand::Reg(dst), Some(v)),
            other => (other, None),
        },
        [_, s, t] => match t.op {
            Operand::Imm(v) => (s.op, Some(v)),
            _ => return err("the third imul operand must be a number"),
        },
        _ => return err("imul takes 1 to 3 operands"),
    };
    let mut i = match imm {
        None => Inst::new(&[0x0F, 0xAF]),
        Some(v) if fits_i8(v) => {
            let mut i = Inst::new(&[0x6B]);
            i.imm = imm_bytes(v, 1, check, false)?;
            i
        }
        Some(v) => {
            let mut i = Inst::new(&[0x69]);
            i.imm = imm_bytes(v, dst.size.min(4), check, dst.size == 8)?;
            i
        }
    };
    i.size(dst.size);
    let reg = i.reg_operand(dst);
    i.rm(reg, &src)?;
    Ok(i)
}
