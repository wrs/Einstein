# Test packages

Minimal NewtonScript-only packages (no native code) for exercising the
install path — the store/pager/RegisterNewPackage half via
`scripts/pkg-repl-install.py`, or the full Dock path via UnixNPI (README
"External serial port"). Each `.ns` file is a NEWT/0 `MakePkg()`
description in the shape of `newt64/defs/pkg.ns`; `./build.sh` writes
the `.pkg` files next to the sources (NEWT/0 stamps a build date into
the header, so the binaries are not checked in).

| package | one 'form part containing | what it probed |
|---|---|---|
| `minimal` | app symbol, text, a protoFloatNGo form | smallest thing that installs (844 B, one 1 KiB store chunk) |
| `padplain` | + 1200 bytes of string padding | multi-chunk storage, LZ-compressed |
| `padnocomp` | same, header flag `kDirNoCompressionFlag` | multi-chunk storage, uncompressed |
| `ticon` | + an Extras icon (bits/mask bitmaps) | binary objects in the part |
| `tfunc` | + a `helper` bytecode function slot | function objects in the part |
| `tfuncnc` | `tfunc` with `kDirNoCompressionFlag` | same, uncompressed |
| `tinst` | + an `installScript` | install-time script execution |
| `tdock` | `ticon` under another name | Dock/UnixNPI install of a not-yet-installed package |

Findings from these (PLAN item 1): none of the content variants
mattered; every package after the first on a store failed until the
pager was made 4 KiB-granular (`docs/STRUCTURES.md`
"TROMDomainManager1K").
