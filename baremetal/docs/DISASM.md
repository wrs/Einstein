# Symbol-annotated ROM disassembly

`baremetal/scripts/disasm-out/rom.dis` (gitignored) is the full
717006 ROM + Einstein.rex disassembly, with every branch target
labelled and every function headed by its symbol name.

Built by `baremetal/scripts/build-rom-disasm.sh`:

- Byteswaps main ROM to LE (matches what the guest CPU sees).
- Overlays `Einstein.rex` at offset 0x00800000, same byteswap.
- Runs `arm-none-eabi-objdump -D -b binary -m arm -EL` over the
  combined 16 MiB image.
- Post-processes with `_Data_/symbols.txt` (~52 000 entries) to:
  - Suffix every `b` / `bl` / `bCC` / `blx` target with `<symbol>`.
  - Prepend a blank line + `ADDR <symbol>:` header before each
    function's first instruction — greppable.

## Use this file; don't manually decode hex

For any ROM code inspection, grep `rom.dis`:

- `grep -A20 '<FunctionName>:'` — function body.
- `grep 'bl 0x<addr>'` — find callers of an address.
- `grep -n '^\s*<hex>:'` — jump to a specific PC.

Do NOT:

- Byte-swap ROM bytes by hand in Python.
- Guess at opcode encodings from raw hex.
- Assume instruction semantics without looking up the decoded mnemonic.

The disasm already has every instruction decoded and every
call/branch target labelled. Hand-decoding is both slower and
error-prone.

## Jump-table aliasing — DON'T mistake the thunk for the body

The post-ship REx jump table (`0x01A00000..0x01C20000`) duplicates
every patchable function symbol at TWO addresses in
`_Data_/demangled_symbols.txt`:

- The **base-ROM body**, e.g. `0x00258EC0 TUDomainManager::Get`.
- The **REx jump-table thunk**, e.g. `0x01BD2974 TUDomainManager::Get`.

Callers `bl` the thunk; the thunk redirects (or is overwritten by
REx to redirect) to the body.

When you see a `bl 0x01Bxxxxx <Func>` and want to read Func, **do
NOT chase 0x01Bxxxxx in `rom.dis`** — that address is past the
disassembled range and you'll come up empty. Instead:

```
grep -i '<funcname>' _Data_/demangled_symbols.txt
```

You will see two hits. The smaller one (≤ 0x00800000) is the
real body — grep that in `rom.dis`. The larger one is the
thunk; ignore it.

Same pattern for: `Get__15TUDomainManagerFRUli` (body 0x258EC0,
thunk 0x1BD2974), `PageMonProc__15TUDomainManagerFlPv` (body
0x25925C, thunk 0x1BD7BE4), and ~hundreds of other kernel
functions. If a function only appears at 0x01Bxxxxx with no base-
ROM twin, its body is REx-resident — see "REx-resident bodies"
in `NEWTON_INTERNALS.md`.

## Rebuild

After `symbols.txt` changes, ROM swap, or REx update:

```
bash baremetal/scripts/build-rom-disasm.sh
```

Takes ~30 s.
