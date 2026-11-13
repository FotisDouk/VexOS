use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

type Result<T = ()> = std::result::Result<T, String>;

const KERNEL_LOAD: u64 = 0x100000;
const SECTOR: usize = 512;
const IMAGE_ALIGN: usize = 1024 * 1024;
const BOOT_OK_MARKER: &str = "Boot complete";
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((cmd, rest)) => (cmd.as_str(), rest),
        None => ("help", &[][..]),
    };
    let result = match cmd {
        "build" => build().map(|_| ()),
        "run" => build().and_then(|image| run(&image, rest)),
        "test" => build().and_then(|image| test(&image, rest)),
        _ => {
            eprintln!("usage: cargo xtask <build | run [qemu args] | test [qemu args]>");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn build_dir() -> PathBuf {
    root().join("build")
}

fn build() -> Result<PathBuf> {
    let root = root();
    let out = build_dir();
    fs::create_dir_all(&out).map_err(|e| format!("creating {}: {e}", out.display()))?;

    let kernel_dir = root.join("kernel");
    let mut cargo = Command::new("cargo");
    cargo.args(["build", "--release"]).current_dir(&kernel_dir);
    for var in ["RUSTUP_TOOLCHAIN", "CARGO", "RUSTC", "RUSTDOC", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_TARGET_DIR"] {
        cargo.env_remove(var);
    }
    run_checked(&mut cargo, "cargo build (kernel)")?;

    let elf_path = kernel_dir.join("target/x86_64-unknown-none/release/kernel");
    let elf = fs::read(&elf_path).map_err(|e| format!("reading {}: {e}", elf_path.display()))?;
    let mut kernel = elf_to_flat(&elf)?;
    kernel.resize(kernel.len().next_multiple_of(SECTOR), 0);
    let kernel_sectors = kernel.len() / SECTOR;

    let nasm = find_tool("NASM", "nasm", &nasm_fallbacks())?;
    let defines = |image_sectors: usize| {
        vec![format!("-DKERNEL_SECTORS={kernel_sectors}"), format!("-DIMAGE_SECTORS={image_sectors}")]
    };

    let stage2 = assemble(&nasm, "stage2", &defines(0))?;
    if stage2.len() % SECTOR != 0 {
        return Err(format!("stage2 is {} bytes, not a whole number of sectors", stage2.len()));
    }

    let image_len = (SECTOR + stage2.len() + kernel.len()).next_multiple_of(IMAGE_ALIGN);
    let stage1 = assemble(&nasm, "stage1", &defines(image_len / SECTOR))?;
    if stage1.len() != SECTOR {
        return Err(format!("stage1 is {} bytes, expected exactly {SECTOR}", stage1.len()));
    }

    let mut image = Vec::with_capacity(image_len);
    image.extend_from_slice(&stage1);
    image.extend_from_slice(&stage2);
    image.extend_from_slice(&kernel);
    image.resize(image_len, 0);

    let image_path = out.join("os.img");
    fs::write(&image_path, &image).map_err(|e| format!("writing {}: {e}", image_path.display()))?;
    println!(
        "Built {} ({} KiB kernel, {} MiB image)",
        image_path.display(),
        kernel.len() / 1024,
        image_len / IMAGE_ALIGN
    );
    Ok(image_path)
}

fn assemble(nasm: &Path, name: &str, defines: &[String]) -> Result<Vec<u8>> {
    let src_dir = root().join("Bootloader");
    let output = build_dir().join(format!("{name}.bin"));
    let mut cmd = Command::new(nasm);
    cmd.args(["-f", "bin"])
        .arg(format!("-I{}/", src_dir.display()))
        .args(defines)
        .arg(src_dir.join(format!("{name}.asm")))
        .arg("-o")
        .arg(&output);
    run_checked(&mut cmd, &format!("nasm ({name})"))?;
    fs::read(&output).map_err(|e| format!("reading {}: {e}", output.display()))
}

fn elf_to_flat(elf: &[u8]) -> Result<Vec<u8>> {
    let u16_at = |off: usize| elf.get(off..off + 2).map(|b| u16::from_le_bytes(b.try_into().unwrap()));
    let u32_at = |off: usize| elf.get(off..off + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let u64_at = |off: usize| elf.get(off..off + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
    let bad = || "kernel is not a valid x86_64 ELF file".to_string();

    if elf.get(..4) != Some(b"\x7FELF") || elf.get(4) != Some(&2) || elf.get(5) != Some(&1) {
        return Err(bad());
    }
    if u16_at(18) != Some(0x3E) {
        return Err(bad());
    }
    if u16_at(16) != Some(2) {
        return Err("kernel must be a static, non-PIE executable (ET_EXEC); check kernel/.cargo/config.toml".into());
    }
    let entry = u64_at(24).ok_or_else(bad)?;
    if entry != KERNEL_LOAD {
        return Err(format!("kernel entry is {entry:#x}, expected {KERNEL_LOAD:#x}; check kernel/linker.ld"));
    }

    let phoff = u64_at(32).ok_or_else(bad)? as usize;
    let phentsize = u16_at(54).ok_or_else(bad)? as usize;
    let phnum = u16_at(56).ok_or_else(bad)? as usize;

    let mut segments = Vec::new();
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if u32_at(ph).ok_or_else(bad)? != 1 {
            continue;
        }
        let offset = u64_at(ph + 8).ok_or_else(bad)? as usize;
        let paddr = u64_at(ph + 24).ok_or_else(bad)?;
        let filesz = u64_at(ph + 32).ok_or_else(bad)? as usize;
        if filesz > 0 {
            segments.push((paddr, offset, filesz));
        }
    }

    let base = segments.iter().map(|s| s.0).min().ok_or("kernel has no loadable segments")?;
    if base != KERNEL_LOAD {
        return Err(format!("kernel is linked at {base:#x}, expected {KERNEL_LOAD:#x}"));
    }
    let end = segments.iter().map(|&(paddr, _, size)| paddr + size as u64).max().unwrap();

    let mut flat = vec![0u8; (end - base) as usize];
    for (paddr, offset, size) in segments {
        let src = elf.get(offset..offset + size).ok_or_else(bad)?;
        let dst = (paddr - base) as usize;
        flat[dst..dst + size].copy_from_slice(src);
    }
    Ok(flat)
}

fn qemu_command(image: &Path, extra: &[String]) -> Result<Command> {
    let qemu = find_tool("QEMU", "qemu-system-x86_64", &qemu_fallbacks())?;
    let mut cmd = Command::new(qemu);
    cmd.current_dir(build_dir())
        .arg("-drive")
        .arg(format!("format=raw,file={}", image.file_name().unwrap().to_string_lossy()))
        .args(["-m", "512M", "-no-reboot"])
        .args(extra);
    Ok(cmd)
}

fn run(image: &Path, extra: &[String]) -> Result {
    let mut cmd = qemu_command(image, extra)?;
    cmd.args(["-serial", "stdio"]);
    run_checked(&mut cmd, "qemu")
}

fn test(image: &Path, extra: &[String]) -> Result {
    let log = build_dir().join("serial.log");
    let _ = fs::remove_file(&log);

    let mut cmd = qemu_command(image, extra)?;
    cmd.args(["-display", "none", "-serial", "file:serial.log"]).stdout(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("starting qemu: {e}"))?;

    let start = Instant::now();
    let outcome = loop {
        let output = fs::read_to_string(&log).unwrap_or_default();
        if output.contains(BOOT_OK_MARKER) {
            break Ok(());
        }
        if output.contains("KERNEL PANIC") {
            break Err("kernel panicked".to_string());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                break Err(format!("QEMU exited early ({status}); with -no-reboot this usually means a triple fault"))
            }
            Ok(None) => {}
            Err(e) => break Err(format!("waiting for qemu: {e}")),
        }
        if start.elapsed() > TEST_TIMEOUT {
            break Err(format!("kernel did not report \"{BOOT_OK_MARKER}\" within {}s", TEST_TIMEOUT.as_secs()));
        }
        thread::sleep(Duration::from_millis(200));
    };
    stop(&mut child);

    println!("----- serial output -----");
    print!("{}", fs::read_to_string(&log).unwrap_or_default());
    println!("-------------------------");
    outcome?;
    println!("Boot test passed in {:.1}s", start.elapsed().as_secs_f32());
    Ok(())
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn run_checked(cmd: &mut Command, what: &str) -> Result {
    let status = cmd.status().map_err(|e| format!("failed to start {what}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed ({status})"))
    }
}

fn find_tool(env_var: &str, name: &str, fallbacks: &[PathBuf]) -> Result<PathBuf> {
    if let Some(path) = env::var_os(env_var) {
        return Ok(PathBuf::from(path));
    }
    let exe = format!("{name}{}", env::consts::EXE_SUFFIX);
    let on_path = env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).map(|dir| dir.join(&exe)).collect::<Vec<_>>())
        .unwrap_or_default();
    on_path
        .into_iter()
        .chain(fallbacks.iter().cloned())
        .find(|p| p.is_file())
        .ok_or_else(|| format!("could not find {name}; install it, add it to PATH, or set {env_var}=<path>"))
}

fn nasm_fallbacks() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from(r"C:\Program Files\NASM\nasm.exe")];
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        paths.push(Path::new(&local).join(r"bin\NASM\nasm.exe"));
    }
    paths
}

fn qemu_fallbacks() -> Vec<PathBuf> {
    vec![PathBuf::from(r"C:\Program Files\qemu\qemu-system-x86_64.exe")]
}
