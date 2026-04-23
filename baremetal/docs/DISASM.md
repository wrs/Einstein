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

## Rebuild

After `symbols.txt` changes, ROM swap, or REx update:

```
bash baremetal/scripts/build-rom-disasm.sh
```

Takes ~30 s.
