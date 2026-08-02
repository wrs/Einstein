# Newton ROM dumps

Drop your own Newton 2.x ROM image in this directory as `newton.rom`. It is
`.gitignore`d and will never be committed.

## What goes here

- `newton.rom` — 8 MiB raw ROM dump from a MessagePad 2000/2100 or eMate
  (Newton OS 2.x). See Einstein's
  [Dumping The ROM](https://github.com/pguyot/Einstein/wiki/Dumping-The-Rom)
  wiki page for instructions if you own hardware and haven't dumped one yet.
- (Optional, future) `flash.img` — 8 MiB internal-store image used to persist
  guest state across reboots. Created on first boot when it doesn't exist.

## Per-version input directories

Each `rom-<ver>` cargo feature resolves its build inputs through
`resolve_rom_version()` in `build.rs`. For the default `rom-717006`
the historical locations keep working (`roms/newton.rom`,
`../_Data_/Einstein.rex`, `../_Data_/symbols.txt` — the latter two are
shared with the Einstein C++ project and must not move); a
`roms/717006/` directory overrides them when present. Any other
version (`rom-710031`, …) reads exclusively from
`roms/<ver>/{newton.rom, Einstein.rex, symbols.txt, code-symbols.txt}`.
When the ROM image is absent, the build stages a zero-length
placeholder (so `cargo check` works without the image) and the loader
halts loudly at boot.

## Why it isn't shipped

Newton OS ROMs are copyrighted Apple material. We can't redistribute them.
Einstein's convention — and ours — is that every developer provides their own
dump from hardware they own.

## Expected layout (2.x)

Per `../../Emulator/TMemoryConsts.h`:

- `0x000000 – 0x7FFFFF` — Low ROM (8 MiB)
- `0x800000 – 0xFFFFFF` — High ROM (second 8 MiB — on most 2.x ROMs this is
  where the REx lives; exact layout depends on the dump tool)

The hypervisor will sanity-check the image size and print the first few
bytes so mismatches surface immediately.
