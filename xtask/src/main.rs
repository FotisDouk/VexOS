use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

type Result<T = ()> = std::result::Result<T, String>;

const KERNEL_LOAD: u64 = 0x100000;
const SECTOR: usize = 512;
/// The whole disk image, which is also the size of the file system.
const IMAGE_SECTORS: usize = 64 * 1024 * 1024 / SECTOR;
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

// The kernel's `qemu-test` feature reports through QEMU's isa-debug-exit device; QEMU then exits
// with status (code << 1) | 1. Keep in sync with kernel/src/qemu.rs.
const DEBUG_EXIT_DEVICE: &str = "isa-debug-exit,iobase=0xf4,iosize=0x04";
const EXIT_SUCCESS: i32 = (0x10 << 1) | 1;
const EXIT_FAILED: i32 = (0x11 << 1) | 1;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// `build/os.img`: the normal image, for QEMU and real hardware.
    Normal,
    /// `build/os-test.img`: built with the kernel's `qemu-test` feature, exits QEMU when done.
    Test,
}

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let fresh = args.iter().any(|a| a == "--fresh");
    args.retain(|a| a != "--fresh");
    let (cmd, rest) = match args.split_first() {
        Some((cmd, rest)) => (cmd.as_str(), rest),
        None => ("help", &[][..]),
    };
    let result = match cmd {
        "build" => build(Kind::Normal, fresh).map(|_| ()),
        "run" => build(Kind::Normal, fresh).and_then(|image| run(&image, rest)),
        "test" => host_tests().and_then(|_| build(Kind::Test, true)).and_then(|image| test(&image, rest)),
        _ => {
            eprintln!("usage: cargo xtask <build | run [qemu args] | test [qemu args]> [--fresh]");
            eprintln!("  --fresh: reformat build/os.img instead of keeping the files saved on it");
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

/// A cargo command with the variables of the outer `cargo xtask` invocation removed, so the
/// inner build uses the target directory's own toolchain and config.
fn cargo(dir: &Path) -> Command {
    let mut cargo = Command::new("cargo");
    cargo.current_dir(dir);
    for var in ["RUSTUP_TOOLCHAIN", "CARGO", "RUSTC", "RUSTDOC", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_TARGET_DIR"] {
        cargo.env_remove(var);
    }
    cargo
}

/// Unit tests of the crates that also build for the host.
fn host_tests() -> Result {
    for krate in HOST_TESTED {
        let mut cmd = cargo(&root().join(krate));
        cmd.args(["test", "--quiet"]);
        run_checked(&mut cmd, &format!("cargo test ({krate})"))?;
    }
    Ok(())
}

const HOST_TESTED: &[&str] = &["kfs", "vasm"];

fn build(kind: Kind, fresh: bool) -> Result<PathBuf> {
    let root = root();
    let out = build_dir();
    fs::create_dir_all(&out).map_err(|e| format!("creating {}: {e}", out.display()))?;

    // The kernel embeds the BIOS call trampoline, so assemble it first.
    let nasm = find_tool("NASM", "nasm", &nasm_fallbacks())?;
    let defines = [format!("-DIMAGE_SECTORS={IMAGE_SECTORS}")];
    assemble(&nasm, "bioscall", &defines)?;

    let kernel_dir = root.join("kernel");
    let mut cmd = cargo(&kernel_dir);
    cmd.args(["build", "--release"]);
    if kind == Kind::Test {
        cmd.args(["--features", "qemu-test"]);
    }
    run_checked(&mut cmd, "cargo build (kernel)")?;

    let elf_path = kernel_dir.join("target/x86_64-unknown-none/release/kernel");
    let elf = fs::read(&elf_path).map_err(|e| format!("reading {}: {e}", elf_path.display()))?;
    let kernel = elf_to_flat(&elf)?;

    let stage1 = assemble(&nasm, "stage1", &defines)?;
    let stage2 = assemble(&nasm, "stage2", &defines)?;
    if stage1.len() != SECTOR {
        return Err(format!("stage1 is {} bytes, expected exactly {SECTOR}", stage1.len()));
    }
    let boot = [stage1, stage2].concat();
    if boot.len() != kfs::BOOT_SECTORS as usize * SECTOR {
        return Err(format!(
            "stage1 + stage2 is {} bytes, but kfs reserves {} sectors for the boot area",
            boot.len(),
            kfs::BOOT_SECTORS
        ));
    }

    let image_path = out.join(match kind {
        Kind::Normal => "os.img",
        Kind::Test => "os-test.img",
    });
    let (mut disk, kept) = open_or_format(&image_path, fresh)?;
    install(&mut disk, &boot, &kernel)?;
    let image = disk.into_device().data;
    fs::write(&image_path, &image).map_err(|e| format!("writing {}: {e}", image_path.display()))?;
    println!(
        "Built {} ({} KiB kernel, {} MiB disk, {})",
        image_path.display(),
        kernel.len() / 1024,
        image.len() / (1024 * 1024),
        if kept { "files on it kept" } else { "freshly formatted" }
    );
    Ok(image_path)
}

type Disk = kfs::Fs<kfs::MemDisk>;

/// Opens an existing image so the files saved on it survive the rebuild, or formats a new one.
fn open_or_format(path: &Path, fresh: bool) -> Result<(Disk, bool)> {
    if !fresh {
        if let Ok(data) = fs::read(path) {
            if data.len() == IMAGE_SECTORS * SECTOR {
                if let Ok(disk) = kfs::Fs::mount(kfs::MemDisk { data }) {
                    return Ok((disk, true));
                }
            }
            println!("{} has no usable file system; formatting it", path.display());
        }
    }
    let disk = kfs::Fs::format(kfs::MemDisk::new(IMAGE_SECTORS)).map_err(fs_err("formatting"))?;
    Ok((disk, false))
}

fn fs_err(what: &str) -> impl Fn(kfs::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// Writes the bootloader and kernel, and copies files from `disk/` that are not on the image yet.
fn install(disk: &mut Disk, boot: &[u8], kernel: &[u8]) -> Result {
    for dir in ["/system", "/programs"] {
        match disk.mkdir(dir) {
            Ok(()) | Err(kfs::Error::AlreadyExists) => {}
            Err(e) => return Err(format!("creating {dir}: {e}")),
        }
    }
    disk.write(kfs::BOOT_PATH, boot).map_err(fs_err("writing the bootloader"))?;
    disk.write(kfs::KERNEL_PATH, kernel).map_err(fs_err("writing the kernel"))?;
    copy_tree(disk, &root().join("disk"), "/")
}

fn copy_tree(disk: &mut Disk, dir: &Path, dest: &str) -> Result {
    let Ok(entries) = fs::read_dir(dir) else { return Ok(()) };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = kfs::resolve(dest, &name);
        if entry.path().is_dir() {
            match disk.mkdir(&path) {
                Ok(()) | Err(kfs::Error::AlreadyExists) => {}
                Err(e) => return Err(format!("creating {path}: {e}")),
            }
            copy_tree(disk, &entry.path(), &path)?;
        } else if disk.stat(&path).is_err() {
            let data = fs::read(entry.path()).map_err(|e| format!("reading {}: {e}", entry.path().display()))?;
            disk.write(&path, &data).map_err(fs_err(&path))?;
            // Example programs also get a ready-made executable next to the source.
            if let Some(stem) = path.strip_suffix(".asm") {
                let vex_path = format!("{stem}.vex");
                let source = String::from_utf8_lossy(&data);
                let program = vasm::assemble(&source, vasm::PROGRAM_BASE)
                    .map_err(|e| format!("{}: {e}", entry.path().display()))?;
                disk.write(&vex_path, &vasm::to_vex(&program)).map_err(fs_err(&vex_path))?;
            }
        }
    }
    Ok(())
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
    cmd.args(["-device", DEBUG_EXIT_DEVICE, "-display", "none", "-serial", "file:serial.log"])
        .stdout(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("starting qemu: {e}"))?;

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(e) => break Err(format!("waiting for qemu: {e}")),
        }
        if start.elapsed() > TEST_TIMEOUT {
            break Err(format!("the kernel did not finish within {}s", TEST_TIMEOUT.as_secs()));
        }
        thread::sleep(Duration::from_millis(50));
    };
    stop(&mut child);

    println!("----- serial output -----");
    print!("{}", fs::read_to_string(&log).unwrap_or_default());
    println!("-------------------------");
    let status = status?;
    match status.code() {
        Some(EXIT_SUCCESS) => {
            println!("Boot test passed in {:.1}s", start.elapsed().as_secs_f32());
            Ok(())
        }
        Some(EXIT_FAILED) => Err("the kernel reported a failure (see the serial output above)".into()),
        _ => Err(format!(
            "QEMU exited unexpectedly ({status}); with -no-reboot this usually means a triple fault"
        )),
    }
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
