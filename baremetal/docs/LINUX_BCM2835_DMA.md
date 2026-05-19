# Linux BCM2835 DMA driver — survey for hypervisor comparison

Survey captured 2026-05-20 from `raspberrypi/linux@rpi-6.6.y`. Source:
`drivers/dma/bcm2835-dma.c` (Florian Meier 2013, ported from BCM2708 /
OMAP DMA engines). The driver covers three SoC variants behind one
binding: classic 32-bit `brcm,bcm2835-dma` (Pi Zero 2 W / BCM2837),
`brcm,bcm2711-dma` (Pi 4) with 40-bit channels, and
`brcm,bcm2712-dma` (Pi 5). Pi Zero 2 W exclusively uses the classic
32-bit path.

## 1. Control-block layout

```c
struct bcm2835_dma_cb {
    uint32_t info;     // +0x00  TI (transfer information)
    uint32_t src;      // +0x04  SOURCE_AD
    uint32_t dst;      // +0x08  DEST_AD
    uint32_t length;   // +0x0C  TXFR_LEN
    uint32_t stride;   // +0x10  STRIDE  (2D mode)
    uint32_t next;     // +0x14  NEXTCONBK
    uint32_t pad[2];   // +0x18  pads to 32 bytes
};
```

Six functional words plus two pads — exactly the BCM2835 datasheet
§4.2.1.1 layout. The 32-byte total is significant: the DMA engine
requires CB descriptors to be 256-bit aligned. The driver enforces this
via `dma_pool_create(dev, sizeof(struct bcm2835_dma_cb), 32, 0)` — 3rd
arg is the alignment.

## 2. Channel ownership and DT mask

The classic Pi DTS reserves channels via the bitmask property:

```
dma: dma-controller@7e007000 {
    compatible = "brcm,bcm2835-dma";
    reg = <0x7e007000 0xf00>;
    interrupts = <1 16>, <1 17>, ... <1 28>;
    interrupt-names = "dma0", "dma1", ... "dma14", "dma-shared-all";
    #dma-cells = <1>;
    brcm,dma-channel-mask = <0x7f35>;
};
```

`0x7f35` = `0b0111_1111_0011_0101` — bits 0, 2, 4, 5, 8-14 enabled;
channels 1, 3, 6, 7, 15 are NOT in the mask. Channel 0 (BULK) is
reserved for the legacy API via `BCM2835_DMA_BULK_MASK = BIT(0)`.

In `bcm2835_dma_probe()` the mask is read by
`of_property_read_u32(... "brcm,dma-channel-mask", &chans_available)`.
The probe loops `for (i = chan_start; i < chan_end; i++)` and
`if (!(chans_available & (1 << i))) { irq[i] = -1; continue; }` to skip
masked-out channels.

`bcm2835_dma_chan_init(od, chan_id, irq, irq_flags)` then allocates the
`struct bcm2835_chan` (devm_kzalloc), wires
`c->chan_base = BCM2835_DMA_CHANIO(d->base, chan_id)` where
`BCM2835_DMA_CHAN(n) = n * 0x100`, stores `c->ch = chan_id`, sets
`c->is_lite_channel = true` if
`readl(c->chan_base + BCM2835_DMA_DEBUG) & BCM2835_DMA_DEBUG_LITE`.
No CS write happens here — channel reset is deferred to first
`terminate` or to the first `start_desc`.

## 3. Channel init / reset

There is no separate `bcm2835_dma_reset_channel`. Reset is folded into
the abort sequence at the bottom of `bcm2835_dma_abort()` (classic
path):

```c
if (!readl(chan_base + BCM2835_DMA_ADDR))   // CONBLK_AD == 0 -> already idle
    return;
writel(0, chan_base + BCM2835_DMA_NEXTCB);  // break the chain
writel(readl(...CS) | BCM2835_DMA_ABORT | BCM2835_DMA_ACTIVE, ...CS);
while ((readl(...CS) & BCM2835_DMA_ABORT) && --timeout) cpu_relax();
writel(readl(...CS) & ~BCM2835_DMA_ACTIVE, ...CS);
if (!timeout && !(readl(...TI) & (S_DREQ | D_DREQ)))
    dev_err(... "failed to complete pause...");
writel(BCM2835_DMA_RESET, chan_base + BCM2835_DMA_CS);  // BIT(31)
```

The order: (a) zero NEXTCONBK to prevent the engine from picking up a
fresh CB while we're tearing down, (b) set both `ABORT | ACTIVE`
simultaneously, (c) poll ABORT to clear with a 100-loop budget, (d)
clear ACTIVE, (e) finally pulse RESET. The DREQ-pending check skips the
"failed to pause" error if the channel is genuinely waiting on a
peripheral DREQ.

`bcm2835_dma_start_desc()` also writes
`writel(BIT(31), c->chan_base + BCM2835_DMA_CS)` immediately before
loading CONBLK_AD and going active — i.e., every new descriptor
implicitly resets the channel.

## 4. IRQ wiring

The DT lists one IRQ per channel (`<1 16>` through `<1 28>`, where
`<1 X>` is GPU IRQ X). For BCM2837 these come through the legacy ARM
GIC interrupt controller as GPU IRQs 16-28. Channel N maps to GPU IRQ
(16 + N) for channels 0-11; channels 12-14 share GPU IRQ 27 (so the DT
lists `<1 27>` three times); IRQ 28 is "dma-shared-all" for channel 15.

The probe uses `platform_get_irq_byname(pdev, "dmaN")` first, falling
back to indexed `platform_get_irq` for legacy DT. Where multiple
channels resolve to the same IRQ, the probe sets `IRQF_SHARED` on each,
and the handler checks
`if (c->irq_flags & IRQF_SHARED) { flags = readl(CS); if (!(flags & BCM2835_DMA_INT)) return IRQ_NONE; }`
before claiming. There is a global `BCM2835_DMA_INT_STATUS = 0xfe0`
register listed in the defines but the driver does NOT use it on the
hot path — it goes straight to the per-channel `CS.INT` bit.

## 5. Cyclic transfer setup

`bcm2835_dma_prep_dma_cyclic(chan, buf_addr, buf_len, period_len,
direction, flags)`:

```c
u32 info = WAIT_RESP(c->dreq) | WIDE_SOURCE(c->dreq) | WIDE_DEST(c->dreq)
         | BURST_LENGTH(c->dreq);
u32 extra = 0;
if (flags & DMA_PREP_INTERRUPT)  extra |= BCM2835_DMA_INT_EN;
else                              period_len = buf_len;
if (c->dreq != 0)                 info |= BCM2835_DMA_PER_MAP(c->dreq);

if (direction == DMA_DEV_TO_MEM) {
    src = phys_to_dma(dev, c->cfg.src_addr); dst = buf_addr;
    info |= BCM2835_DMA_S_DREQ | BCM2835_DMA_D_INC;
} else {  // DMA_MEM_TO_DEV — HDMI audio TX path
    dst = phys_to_dma(dev, c->cfg.dst_addr); src = buf_addr;
    info |= BCM2835_DMA_D_DREQ | BCM2835_DMA_S_INC;
}

frames = DIV_ROUND_UP(buf_len, period_len)
       * bcm2835_dma_frames_for_length(period_len, max_len);

d = bcm2835_dma_create_cb_chain(c, direction, /*cyclic=*/true,
                                info, extra, frames, src, dst,
                                buf_len, period_len, GFP_NOWAIT);

// close the loop:
d->cb_list[d->frames - 1].cb->next = d->cb_list[0].paddr;
```

So:

- `frames` = (buf_len / period_len) × ceil(period_len / max_frame_length).
  For HDMI audio with `period_len <= MAX_DMA_LEN = 1 GiB` (or
  `MAX_LITE_DMA_LEN = 64K-4` on lite channels),
  `frames == buf_len / period_len` — one CB per ALSA period.
- The "extra" bits (which is `INT_EN` when DMA_PREP_INTERRUPT is set)
  are applied by `create_cb_set_length()` **only at the boundary where
  total_len wraps** (i.e., at the last CB of each period). So INT_EN
  fires once per period, not once per CB. For multi-CB periods (lite
  channels with big periods) only the period-closing CB carries INT_EN.
- The final CB's `next` is rewritten to `cb_list[0].paddr` — the ring
  closes.

`bcm2835_dma_create_cb_chain` does the heavy lifting: `dma_pool_alloc`
per frame, fill `info/src/dst`, link
`cb_list[frame-1].cb->next = cb_entry->paddr`, advance src/dst by
length depending on the S_INC/D_INC flags.

## 6. TI bits for peripheral-paced MEM_TO_DEV

For an HDMI-audio-style sink with `maxburst = 2`:

```c
#define BCM2835_DMA_INT_EN        BIT(0)
#define BCM2835_DMA_WAIT_RESP     BIT(3)
#define BCM2835_DMA_D_INC         BIT(4)
#define BCM2835_DMA_D_WIDTH       BIT(5)
#define BCM2835_DMA_D_DREQ        BIT(6)
#define BCM2835_DMA_S_INC         BIT(8)
#define BCM2835_DMA_S_WIDTH       BIT(9)
#define BCM2835_DMA_S_DREQ        BIT(10)
#define BCM2835_DMA_BURST_LENGTH(x) (((x) & 15) << 12)
#define BCM2835_DMA_PER_MAP(x)    ((x & 31) << 16)
#define BCM2835_DMA_WAIT(x)       ((x & 31) << 21)
```

The helpers:

```c
#define WAIT_RESP(x)    ((x & BCM2835_DMA_NO_WAIT_RESP) ? 0 : BCM2835_DMA_WAIT_RESP)
#define WIDE_SOURCE(x)  ((x & BCM2835_DMA_WIDE_SOURCE) ? BCM2835_DMA_S_WIDTH : 0)
#define WIDE_DEST(x)    ((x & BCM2835_DMA_WIDE_DEST)   ? BCM2835_DMA_D_WIDTH : 0)
#define BURST_LENGTH(x) ((x & BCM2835_DMA_BURST) ? BCM2835_DMA_BURST_LENGTH(3) : 0)
```

Notable: `BURST_LENGTH` is binary — either off, or **3** (which encodes
a 3-beat burst, i.e. 4 words because the field is "extra beats beyond
the first"). The driver does NOT take a runtime maxburst value; the
consumer encodes "I want bursts" via a flag bit in the upper part of
the dreq word passed through the of_dma cell.

A typical TX TI ends up as:
`PER_MAP(dreq) | WAIT_RESP | D_DREQ | S_INC | BURST_LENGTH(3) | (INT_EN on period-boundary CB)`

`D_WIDTH` / `S_WIDTH` (128-bit-wide accesses) are off unless the
consumer set the WIDE_SOURCE/WIDE_DEST cookie bit — irrelevant on
classic channels driving 32-bit MMIO FIFOs. `WAIT(N)` (the
inter-transfer delay field) is never set by this driver.

## 7. Bus-address translation

The driver delegates entirely to
`phys_to_dma(chan->device->dev, c->cfg.src_addr)` and
`phys_to_dma(dev, c->cfg.dst_addr)`. There is no hand-coded
`| 0xC000_0000` (RAM → bus alias) nor `| 0x7E00_0000` (peripheral bus
alias) in the file. The translation comes from the SoC's `dma-ranges`
DT property which `dma-direct` consumes. On classic BCM2837 the
`soc { dma-ranges = <0xC0000000 0x00000000 0x3F000000>; }` style entry
produces the `0xC000_0000` ORing implicitly. Buffers from `dma_map_*`
and `dma_pool_alloc` already return bus addresses; the driver writes
them straight to CB fields.

The only explicit address mangling in the file is
`to_40bit_cbaddr(addr) = addr >> 5` (for BCM2711's 40-bit channels
which take a shifted CB pointer), which **is not used on Pi Zero 2 W**
— the classic path stores raw `dma_addr_t` in `control_block->next`
and the same in `CONBLK_AD`.

## 8. Arming the channel

```c
static void bcm2835_dma_start_desc(struct bcm2835_chan *c) {
    struct virt_dma_desc *vd = vchan_next_desc(&c->vc);
    ...
    c->desc = d = to_bcm2835_dma_desc(&vd->tx);

    // classic path:
    writel(BIT(31), c->chan_base + BCM2835_DMA_CS);          // RESET
    writel(d->cb_list[0].paddr, c->chan_base + BCM2835_DMA_ADDR);  // CONBLK_AD
    writel(BCM2835_DMA_ACTIVE | BCM2835_DMA_CS_FLAGS(c->dreq),
           c->chan_base + BCM2835_DMA_CS);
}
```

Three writes, in order: RESET → CONBLK_AD → ACTIVE+flags. The flags
come from `BCM2835_DMA_CS_FLAGS(x)` which masks the
priority/wait-for-writes/disable-debug subset out of the dreq cookie:

```c
#define BCM2835_DMA_CS_FLAGS(x) (x & (BCM2835_DMA_PRIORITY(15) |
                                      BCM2835_DMA_PANIC_PRIORITY(15) |
                                      BCM2835_DMA_WAIT_FOR_WRITES |
                                      BCM2835_DMA_DIS_DEBUG))
```

So the consumer can stuff PRIORITY/PANIC_PRIORITY/WAIT_FOR_WRITES/
DIS_DEBUG into the upper bits of its DT dma-cell and they end up in CS.
CONBLK_AD is set to the *first* CB's physical address — the engine
fetches the CB, loads info/src/dst/length/next into the live registers,
and runs.

## 9. IRQ handler

```c
static irqreturn_t bcm2835_dma_callback(int irq, void *data) {
    if (c->irq_flags & IRQF_SHARED) {
        flags = readl(c->chan_base + BCM2835_DMA_CS);
        if (!(flags & BCM2835_DMA_INT)) return IRQ_NONE;
    }
    spin_lock_irqsave(&c->vc.lock, flags);

    // ACK the INT and keep the channel running:
    writel(BCM2835_DMA_INT | BCM2835_DMA_ACTIVE | BCM2835_DMA_CS_FLAGS(c->dreq),
           c->chan_base + BCM2835_DMA_CS);

    d = c->desc;
    if (d) {
        if (d->cyclic)            vchan_cyclic_callback(&d->vd);
        else if (!readl(c->chan_base + BCM2835_DMA_ADDR)) {
            vchan_cookie_complete(&c->desc->vd);
            bcm2835_dma_start_desc(c);
        }
    }
    spin_unlock_irqrestore(&c->vc.lock, flags);
    return IRQ_HANDLED;
}
```

ACK semantics: write-1-to-clear on `CS.INT`. Critically, the driver
re-asserts `ACTIVE | CS_FLAGS` in the *same write* — the comment
explains that this keeps the channel armed for cyclic and is harmless
if the descriptor has already finished. For cyclic,
`vchan_cyclic_callback` runs the client-supplied callback (e.g.,
ALSA's pcm_period_elapsed) without dequeueing the descriptor. For
non-cyclic, it checks `BCM2835_DMA_ADDR == 0` (the engine zeroes
CONBLK_AD on completion) and only then dequeues + starts the next desc.

## 10. Cache management

CBs live in a per-channel
`dma_pool_create(dev_name, dev, sizeof(cb), 32, 0)` allocated lazily in
`bcm2835_dma_alloc_chan_resources`. dma_pool gives coherent memory by
default on platforms where coherent allocators are required — no
explicit `dma_sync_*` calls in the driver for CB writes.

Data buffers are NOT allocated by this driver — consumers (ALSA, MMC,
SPI) supply them via `dma_map_single`/`dma_alloc_coherent` and pass
`buf_addr` into `prep_dma_cyclic`. For HDMI audio the kernel uses
`snd_dma_alloc_pages(SNDRV_DMA_TYPE_DEV)` which is coherent on this
platform, so no explicit sync is needed in the hot path.

There is one peculiarity: an `od->zero_page` is mapped at probe time
(`dma_map_page_attrs(ZERO_PAGE(0), ..., DMA_TO_DEVICE,
DMA_ATTR_SKIP_CPU_SYNC)`), and `prep_dma_cyclic` will set
`BCM2835_DMA_S_IGNORE` when the source buffer equals zero_page — for
clients that want "DMA out N zero bytes" without burning RAM bandwidth.

## 11. Stop / terminate

```c
static int bcm2835_dma_terminate_all(struct dma_chan *chan) {
    spin_lock_irqsave(&c->vc.lock, flags);
    if (c->desc) {
        vchan_terminate_vdesc(&c->desc->vd);  // mark for free
        c->desc = NULL;
        bcm2835_dma_abort(c);                  // kick the HW
    }
    vchan_get_all_descriptors(&c->vc, &head);  // drain pending queue
    spin_unlock_irqrestore(&c->vc.lock, flags);
    vchan_dma_desc_free_list(&c->vc, &head);   // free outside lock
    return 0;
}
```

Order: vchan bookkeeping first (sets vd->terminated, prevents re-issue),
null the current desc pointer, then call `bcm2835_dma_abort` (the full
sequence from §3 — break NEXTCONBK, ABORT|ACTIVE, poll, clear ACTIVE,
pulse RESET). Then drain and free outside the spinlock. The
`device_synchronize` callback just calls `vchan_synchronize(&c->vc)`
which waits for any in-flight callback to finish.

---

## Direct comparison points for the hypervisor

- **CB alignment:** must be 32-byte aligned. Linux enforces via
  `dma_pool` alignment arg.
- **Cyclic ring closure:** Linux closes the ring by patching
  `cb_list[N-1].next = cb_list[0].paddr` AFTER building the chain.
  INT_EN goes on period-boundary CBs only, not every CB.
- **Arm sequence:** RESET (`writel BIT(31) to CS`) → CONBLK_AD →
  ACTIVE+flags. Three writes.
- **IRQ ACK:** single write `INT | ACTIVE | CS_FLAGS` — clears INT and
  re-asserts ACTIVE in one go.
- **Abort:** NEXTCONBK=0 → `CS |= ABORT|ACTIVE` → poll ABORT clear
  (≤100 iters) → clear ACTIVE → `CS = RESET`. Tolerate timeout if DREQ
  is the reason the channel is starved.
- **TI for MEM_TO_DEV HDMI-audio TX:**
  `WAIT_RESP | D_DREQ | S_INC | BURST_LENGTH(3) | PER_MAP(dreq)`, plus
  `INT_EN` only on period-boundary CBs.
- **Address translation:** classic BCM2837 — no `>> 5`, no manual
  `| 0xC000_0000` in driver code (DT `dma-ranges` handles it via
  phys_to_dma). The hypervisor needs to translate guest IPA → bus
  address explicitly since there's no DT layer.
- **Reserved channels:** mask `0x7f35` — channels 1, 3, 6, 7, 15 are
  off-limits to Linux on classic Pi DTs. Channel 0 is BULK.
- **Per-channel IRQ:** GPU IRQ `(16 + N)` for N in 0..11; 12-14 share
  IRQ 27; channel 15 has its own IRQ 28.

Key paths in `drivers/dma/bcm2835-dma.c` (rpi-6.6.y):

- `struct bcm2835_dma_cb` — top-of-file region.
- `bcm2835_dma_abort`, `bcm2835_dma_start_desc`,
  `bcm2835_dma_callback` — mid-file.
- `bcm2835_dma_prep_dma_cyclic`, `bcm2835_dma_create_cb_chain`,
  `bcm2835_dma_create_cb_set_length` — main work functions.
- `bcm2835_dma_alloc_chan_resources` — dma_pool_create site.
- `bcm2835_dma_probe`, `bcm2835_dma_chan_init`,
  `bcm2835_dma_xlate` — bring-up.
- `bcm2835_dma_terminate_all`, `bcm2835_dma_synchronize` — teardown.
- DT: `arch/arm/boot/dts/broadcom/bcm2835-common.dtsi` for the `dma:`
  node and channel mask `0x7f35`.
