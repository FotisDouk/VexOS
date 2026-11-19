Welcome!

This file lives on the OS disk. Files you create inside the OS are saved
on the disk and survive reboots and rebuilds (cargo xtask build --fresh
wipes them).

Type 'help' for the list of commands, 'help <command>' for details.

File types come from the extension:
  .txt   text                 .asm   assembly source (runnable)
  .vex   executable program   .v     script of shell commands
  .sys   kernel               .boot  bootloader
  .raw   raw data             other  treated as text

Try:
  ls /programs              see the example programs
  hello                     run /programs/hello.vex
  guess                     a small game written in assembly
  edit /programs/hello.asm  change it, then run it with: hello.asm
  asm /programs/hello.asm   rebuild hello.vex from the source
  colors                    run a script

You have full control: /system/kernel.sys is the kernel and
/system/boot.boot is the bootloader. Deleting them makes this disk
unbootable. peek, poke, in, out and sector give raw access to memory,
I/O ports and the disk.
