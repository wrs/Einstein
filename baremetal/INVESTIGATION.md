# Phase B boot-stall investigation

Live notes. Update as we learn more; remove old updates as we move on to
new stalls.

## Currently at — alignment fault on garbage r5 at PC=0x13c814 (FVP, 2026-04-24)

The inline-stub mechanism is fully working: 27,799 ROM sites
patched (26,614 inline + 1,185 UDF). Boot advances past install,
past the original 0x4ed50 cross-page-DABT stall, past the
unrelated UND stall, into deep guest code at PC=0x13c814 where
a `str r2, [r5]` fires with `r5 = 0x000001d3` — looks like the
guest is using an uninitialised pointer (0x1D3 is also the SVC-mode
CPSR value; possibly a swapped MSR vs STR somewhere in the kernel,
or our stub is corrupting r5 along some path we haven't analysed).
