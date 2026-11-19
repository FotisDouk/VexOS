use super::*;
use std::path::PathBuf;
use std::process::Command;

const ORIGIN: u64 = 0x400_0000;

fn asm(src: &str) -> Vec<u8> {
    match assemble(src, ORIGIN) {
        Ok(p) => p.image,
        Err(e) => panic!("{e}\nsource:\n{src}"),
    }
}

fn asm_err(src: &str) -> Error {
    match assemble(src, ORIGIN) {
        Ok(_) => panic!("expected an error for:\n{src}"),
        Err(e) => e,
    }
}

#[test]
fn simple_encodings() {
    assert_eq!(asm("mov rax, rbx"), [0x48, 0x89, 0xD8]);
    assert_eq!(asm("ret"), [0xC3]);
    assert_eq!(asm("mov eax, 1\nint 0x80"), [0xB8, 1, 0, 0, 0, 0xCD, 0x80]);
    assert_eq!(asm("push r12"), [0x41, 0x54]);
    assert_eq!(asm("mov rax, [r12]"), [0x49, 0x8B, 0x04, 0x24]);
}

#[test]
fn labels_and_jumps() {
    // Backward jump: E9 rel32 = -5.
    assert_eq!(asm("top: jmp top"), [0xE9, 0xFB, 0xFF, 0xFF, 0xFF]);
    // Forward jump over one nop.
    assert_eq!(asm("jmp near end\nnop\nend:"), [0xE9, 1, 0, 0, 0, 0x90]);
    // Local labels belong to the previous global label.
    let code = asm("a:\n.loop: jmp .loop\nb:\n.loop: jmp .loop");
    assert_eq!(code.len(), 10);
    assert_eq!(asm_err("x:\nx:").message, "x is defined more than once");
}

#[test]
fn entry_point() {
    let p = assemble("db 1, 2\n_start: ret", ORIGIN).unwrap();
    assert_eq!(p.entry, 2);
    let p = assemble("ret", ORIGIN).unwrap();
    assert_eq!(p.entry, 0);
}

#[test]
fn data_and_strings() {
    assert_eq!(asm("db 'ab', 0"), b"ab\0");
    assert_eq!(asm("db `a\\n`"), b"a\n");
    assert_eq!(asm("dw 'a'"), [b'a', 0]);
    assert_eq!(asm("msg db 'hi'\nlen equ $ - msg\ndb len"), [b'h', b'i', 2]);
    assert_eq!(asm("times 3 db 7"), [7, 7, 7]);
    assert_eq!(asm("db 1\ntimes 4 - ($ - $$) db 0"), [1, 0, 0, 0]);
    assert_eq!(asm("x equ 2 + 3 * 4\ndb x, (2 + 3) * 4, 1 << 3, -1"), [14, 20, 8, 0xFF]);
    assert_eq!(asm("db 'a;b' ; comment"), b"a;b");
}

#[test]
fn forward_equ_and_labels() {
    // A forward `equ` and a forward label used as immediates.
    let code = asm("mov eax, later\nmov ebx, count\ncount equ 3\nlater:");
    assert_eq!((code[0], code[5]), (0xB8, 0xBB));
    let later = ORIGIN as u32 + code.len() as u32;
    assert_eq!(code[1..5], later.to_le_bytes());
    assert_eq!(code[6..10], 3u32.to_le_bytes());
}

#[test]
fn errors_name_the_line() {
    let e = asm_err("nop\nmov rax\n");
    assert_eq!(e.line, 2);
    assert_eq!(asm_err("bogus rax").message, "unknown instruction: bogus");
    assert_eq!(asm_err("jmp nowhere").message, "undefined symbol: nowhere");
    assert_eq!(asm_err("mov [rax], 1").message, "operation size not specified (add byte/word/dword/qword)");
    assert_eq!(asm_err("mov ah, sil").message, "ah/bh/ch/dh cannot be used in an instruction that needs a REX prefix");
    assert!(asm_err("org 0").message.contains("always load at"));
    assert!(asm_err("db 300").message.contains("does not fit"));
    assert!(asm_err("jmp short far\ntimes 200 nop\nfar:").message.contains("short jump"));
}

#[test]
fn vex_round_trip() {
    let p = assemble("db 1\n_start: ret", ORIGIN).unwrap();
    let vex = to_vex(&p);
    let (header, image) = parse_vex(&vex).unwrap();
    assert_eq!(header, VexHeader { entry: 1, image_size: 2, zero_fill: 0 });
    assert_eq!(image, [1, 0xC3]);
    assert!(parse_vex(b"VEX0").is_err());
    let mut bad = vex.clone();
    bad.push(0);
    assert!(parse_vex(&bad).is_err());
}

fn find_nasm() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NASM") {
        return Some(PathBuf::from(p));
    }
    let exe = format!("nasm{}", std::env::consts::EXE_SUFFIX);
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).map(|d| d.join(&exe)).collect())
        .unwrap_or_default();
    candidates.push(PathBuf::from(r"C:\Program Files\NASM\nasm.exe"));
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join(r"bin\NASM\nasm.exe"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Assembles the corpus with NASM and with vasm and compares the bytes.
#[test]
fn matches_nasm() {
    let Some(nasm) = find_nasm() else {
        eprintln!("NASM not found; skipping the NASM comparison");
        return;
    };
    let source = include_str!("corpus.asm");
    let dir = std::env::temp_dir().join(format!("vasm-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src_path = dir.join("corpus.asm");
    let out_path = dir.join("corpus.bin");
    std::fs::write(&src_path, source).unwrap();
    let status = Command::new(nasm).args(["-f", "bin", "-o"]).arg(&out_path).arg(&src_path).status().unwrap();
    assert!(status.success(), "nasm failed on the corpus");
    let expected = std::fs::read(&out_path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    let (program, sizes) = assemble_with_sizes(source, ORIGIN).unwrap_or_else(|e| panic!("{e}"));
    let ours = program.image;
    let mut offset = 0;
    for (i, (line, size)) in source.lines().zip(sizes).enumerate() {
        let (a, b) = (ours.get(offset..offset + size), expected.get(offset..offset + size));
        assert!(
            a == b,
            "first difference at line {}: `{}`\n  vasm: {:02x?}\n  nasm: {:02x?}",
            i + 1,
            line.trim(),
            a.unwrap_or(&[]),
            b.unwrap_or(&[])
        );
        offset += size;
    }
    assert_eq!(ours.len(), expected.len(), "vasm and nasm produced different lengths");
}
