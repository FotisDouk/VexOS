use super::*;

const DISK_SECTORS: usize = 8192; // 4 MiB

fn fresh() -> Fs<MemDisk> {
    Fs::format(MemDisk::new(DISK_SECTORS)).unwrap()
}

fn names(fs: &mut Fs<MemDisk>, path: &str) -> Vec<String> {
    let mut n: Vec<String> = fs.read_dir(path).unwrap().into_iter().map(|i| i.name).collect();
    n.sort();
    n
}

#[test]
fn resolve_paths() {
    assert_eq!(resolve("/", "a/b"), "/a/b");
    assert_eq!(resolve("/a/b", ".."), "/a");
    assert_eq!(resolve("/a/b", "../../.."), "/");
    assert_eq!(resolve("/a", "./c//d/"), "/a/c/d");
    assert_eq!(resolve("/a", "/x/./y"), "/x/y");
    assert_eq!(resolve("/", ""), "/");
}

#[test]
fn file_types() {
    assert_eq!(FileType::from_name("a.txt"), FileType::Text);
    assert_eq!(FileType::from_name("HELLO.ASM"), FileType::Asm);
    assert_eq!(FileType::from_name("x.vex"), FileType::Exe);
    assert_eq!(FileType::from_name("run.v"), FileType::Script);
    assert_eq!(FileType::from_name("kernel.sys"), FileType::Kernel);
    assert_eq!(FileType::from_name("boot.boot"), FileType::Boot);
    assert_eq!(FileType::from_name("d.raw"), FileType::Raw);
    assert_eq!(FileType::from_name("notes"), FileType::Text);
    assert_eq!(FileType::from_name(".vex"), FileType::Text);
    assert_eq!(FileType::from_name("x.foo"), FileType::Text);
}

#[test]
fn names_are_validated() {
    assert!(valid_name("hello world.txt"));
    assert!(!valid_name(""));
    assert!(!valid_name(".."));
    assert!(!valid_name("a/b"));
    assert!(!valid_name("a\"b"));
    assert!(!valid_name(&"x".repeat(48)));
    let mut fs = fresh();
    assert_eq!(fs.write("/bad\"name", b"x"), Err(Error::InvalidName));
}

#[test]
fn write_read_overwrite() {
    let mut fs = fresh();
    fs.write("/a.txt", b"hello").unwrap();
    assert_eq!(fs.read("/a.txt").unwrap(), b"hello");
    let (_, used_before) = fs.usage();
    let big = vec![7u8; 5000];
    fs.write("/a.txt", &big).unwrap();
    assert_eq!(fs.read("/a.txt").unwrap(), big);
    fs.write("/a.txt", b"").unwrap();
    assert_eq!(fs.read("/a.txt").unwrap(), b"");
    let (_, used_after) = fs.usage();
    assert_eq!(used_after, used_before - 1, "old extents were not freed");
}

#[test]
fn directories() {
    let mut fs = fresh();
    fs.mkdir("/docs").unwrap();
    fs.mkdir("/docs/deep").unwrap();
    fs.write("/docs/deep/x.txt", b"x").unwrap();
    assert_eq!(fs.mkdir("/docs"), Err(Error::AlreadyExists));
    assert_eq!(fs.mkdir("/nope/sub"), Err(Error::NotFound));
    assert_eq!(fs.write("/docs/deep/x.txt/y", b""), Err(Error::NotADirectory));
    assert_eq!(fs.read("/docs"), Err(Error::IsADirectory));
    assert_eq!(fs.write("/docs", b"x"), Err(Error::IsADirectory));
    assert_eq!(names(&mut fs, "/"), ["docs"]);
    assert!(fs.stat("/docs/deep").unwrap().is_dir);
    assert!(fs.stat("/").unwrap().is_dir);
}

#[test]
fn directories_grow() {
    let mut fs = fresh();
    fs.mkdir("/many").unwrap();
    for i in 0..100 {
        fs.write(&format!("/many/f{i}.txt"), format!("{i}").as_bytes()).unwrap();
    }
    // Also grow the root directory, which lives in the superblock.
    for i in 0..20 {
        fs.mkdir(&format!("/d{i}")).unwrap();
    }
    assert_eq!(fs.read_dir("/many").unwrap().len(), 100);
    assert_eq!(fs.read_dir("/").unwrap().len(), 21);
    for i in 0..100 {
        assert_eq!(fs.read(&format!("/many/f{i}.txt")).unwrap(), format!("{i}").as_bytes());
    }
}

#[test]
fn remove() {
    let mut fs = fresh();
    let (_, used_empty) = fs.usage();
    fs.mkdir("/d").unwrap();
    fs.write("/d/a.txt", &[1; 2000]).unwrap();
    fs.mkdir("/d/e").unwrap();
    assert_eq!(fs.remove("/d", false), Err(Error::DirectoryNotEmpty));
    assert_eq!(fs.remove("/", true), Err(Error::Root));
    fs.remove("/d", true).unwrap();
    assert_eq!(fs.stat("/d").unwrap_err(), Error::NotFound);
    assert_eq!(fs.usage().1, used_empty, "space was not freed");
    assert_eq!(fs.remove("/d", false), Err(Error::NotFound));
}

#[test]
fn rename_and_move() {
    let mut fs = fresh();
    fs.mkdir("/a").unwrap();
    fs.mkdir("/b").unwrap();
    fs.write("/a/f.txt", b"data").unwrap();
    fs.rename("/a/f.txt", "/a/g.txt").unwrap();
    assert_eq!(names(&mut fs, "/a"), ["g.txt"]);
    fs.rename("/a/g.txt", "/b").unwrap();
    assert_eq!(fs.read("/b/g.txt").unwrap(), b"data");
    assert!(fs.read_dir("/a").unwrap().is_empty());
    // Replace an existing file.
    fs.write("/b/h.txt", b"old").unwrap();
    fs.rename("/b/g.txt", "/b/h.txt").unwrap();
    assert_eq!(fs.read("/b/h.txt").unwrap(), b"data");
    assert_eq!(names(&mut fs, "/b"), ["h.txt"]);
    // Directories move with their contents, but not into themselves.
    fs.rename("/b", "/a").unwrap();
    assert_eq!(fs.read("/a/b/h.txt").unwrap(), b"data");
    assert_eq!(fs.rename("/a", "/a/b"), Err(Error::MoveIntoSelf));
    assert_eq!(fs.rename("/", "/x"), Err(Error::Root));
}

#[test]
fn copy() {
    let mut fs = fresh();
    fs.mkdir("/d").unwrap();
    fs.write("/x.txt", b"abc").unwrap();
    fs.copy("/x.txt", "/y.txt").unwrap();
    fs.copy("/x.txt", "/d").unwrap();
    assert_eq!(fs.read("/y.txt").unwrap(), b"abc");
    assert_eq!(fs.read("/d/x.txt").unwrap(), b"abc");
}

#[test]
fn boot_area_and_kernel_pointer() {
    let mut fs = fresh();
    fs.mkdir("/system").unwrap();
    let boot = vec![0xB0u8; 17 * 512];
    fs.write(BOOT_PATH, &boot).unwrap();
    assert_eq!(fs.device().data[..17 * 512], boot[..]);
    assert_eq!(fs.stat(BOOT_PATH).unwrap().extent.start, 0);
    assert_eq!(fs.write(BOOT_PATH, &vec![0; 17 * 512 + 1]), Err(Error::BootTooLarge));

    let kernel = vec![0x4Bu8; 3000];
    fs.write(KERNEL_PATH, &kernel).unwrap();
    let k = fs.kernel_extent();
    assert_eq!((k.sectors, k.size), (6, 3000));
    let start = k.start as usize * 512;
    assert_eq!(fs.device().data[start..start + 3000], kernel[..]);

    // Renaming the boot file keeps it in the boot area.
    fs.rename(BOOT_PATH, "/system/old.boot").unwrap();
    fs.write("/system/old.boot", b"new boot").unwrap();
    assert_eq!(&fs.device().data[..8], b"new boot");

    // Deleting destroys: the boot area is zeroed and the kernel pointer cleared.
    fs.remove("/system/old.boot", false).unwrap();
    assert!(fs.device().data[..17 * 512].iter().all(|&b| b == 0));
    fs.remove(KERNEL_PATH, false).unwrap();
    assert_eq!(fs.kernel_extent().sectors, 0);
    fs.write(KERNEL_PATH, b"k").unwrap();
    assert_eq!(fs.kernel_extent().size, 1);
    fs.rename(KERNEL_PATH, "/kernel.sys").unwrap();
    assert_eq!(fs.kernel_extent().sectors, 0, "moving the kernel away must clear the pointer");
}

#[test]
fn persists_across_mount() {
    let mut fs = fresh();
    fs.mkdir("/d").unwrap();
    fs.write("/d/f.txt", b"persist").unwrap();
    let used = fs.usage().1;
    let disk = fs.into_device();
    let mut fs = Fs::mount(disk).unwrap();
    assert_eq!(fs.read("/d/f.txt").unwrap(), b"persist");
    assert_eq!(fs.usage().1, used);
    assert!(matches!(Fs::mount(MemDisk::new(DISK_SECTORS)), Err(Error::NotFormatted)));
}

#[test]
fn out_of_space() {
    let mut fs = Fs::format(MemDisk::new(256)).unwrap();
    assert_eq!(fs.write("/big.raw", &vec![0; 300 * 512]), Err(Error::NoSpace));
    let used = fs.usage().1;
    fs.write("/fits.raw", &vec![1; 100 * 512]).unwrap();
    assert_eq!(fs.usage().1, used + 100);
}

#[test]
fn format_keeps_boot_area() {
    let mut disk = MemDisk::new(DISK_SECTORS);
    disk.data[..4].copy_from_slice(b"BOOT");
    let fs = Fs::format(disk).unwrap();
    assert_eq!(&fs.into_device().data[..4], b"BOOT");
}
