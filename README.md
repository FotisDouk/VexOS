# kernel-test

A hobby x86_64 operating system: a hand-written BIOS bootloader (NASM) and a Rust kernel.

## Requirements

- [Rust](https://rustup.rs). The right nightly and target are installed automatically from `rust-toolchain.toml`.
- [NASM](https://www.nasm.us)
- [QEMU](https://www.qemu.org) (`qemu-system-x86_64`)

## Build and run

```
cargo xtask run      # build build/os.img and boot it in QEMU
cargo xtask test     # headless boot test
cargo xtask build    # only build the image
```

On Windows, `build.bat` does the same (`build.bat`, `build.bat test`, ...).

## Running on a real PC

The image boots through **legacy BIOS**. On a modern PC, enable *CSM* / *Legacy boot* in the firmware settings and turn off Secure Boot. Machines that only support UEFI (many made since about 2020) cannot boot it yet.

1. Write `build/os.img` to a USB stick. **This erases the stick.**
   - Windows: [Rufus](https://rufus.ie). Select the image and choose **DD image** mode when asked.
   - Linux: `sudo dd if=build/os.img of=/dev/sdX bs=1M conv=fsync` (check `/dev/sdX` carefully).
2. Boot from the stick using the firmware's boot menu (often F12, F11, F8 or Esc).

## Running in other virtual machines

Convert the raw image with `qemu-img` (included with QEMU), then attach it as the VM's hard disk. Use BIOS firmware, not EFI.

| VM | Convert | VM settings |
|----|---------|-------------|
| VirtualBox | `qemu-img convert -f raw -O vdi build/os.img os.vdi` | Type "Other/Unknown (64-bit)", EFI off |
| VMware | `qemu-img convert -f raw -O vmdk build/os.img os.vmdk` | Firmware: BIOS |
| Hyper-V | `qemu-img convert -f raw -O vpc -o subformat=fixed build/os.img os.vhd` | Generation 1 VM, disk on IDE |

## Credit
Claude Code has assisted in the development of the OS

## License

Non-Commercial Share-Alike Software License v1.1. See [LICENSE](LICENSE).
