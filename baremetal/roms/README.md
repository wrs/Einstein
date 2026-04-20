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
