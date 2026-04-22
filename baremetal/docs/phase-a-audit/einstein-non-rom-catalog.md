# Einstein non-ROM-execution catalog

Output of the "what does Einstein do other than literally run
ROM instructions?" Explore subagent, 2026-04-21. Cited back into
`Emulator/` with file:line.

---

## 1. NATIVE PRIMITIVES DISPATCH (Coprocessor #10 Traps)

Einstein treats writes to coprocessor #10 as trap instructions.
The dispatch is in **TNativePrimitives::ExecuteNative()**
(`/Users/walter/Projects/newton/Einstein/Emulator/TNativePrimitives.cpp:177`).

**Instruction Format:** Bits 15-8 select the handler class, bits
7-0 select the operation within that class.

| Opcode (bits 15:8) | Handler | File | Line | Operations |
|---|---|---|---|---|
| 0x000000 | Flash Driver | TNativePrimitives.cpp | 194 | Identify, Cleanup, Init, Write, Erase, Query (opcodes 0x01-0x0C) |
| 0x000001 | Platform Driver | TNativePrimitives.cpp | 198 | PowerOn/Off, Events, Gestalts (opcodes 0x01-0x1E+) |
| 0x000002 | Sound Driver | TNativePrimitives.cpp | 202 | Output scheduling, volume, DMA setup (opcodes 0x01-0x0B) |
| 0x000003 | Battery Driver | TNativePrimitives.cpp | 206 | Status, voltage, charge level (opcodes 0x01-0x08+) |
| 0x000004 | Screen Driver | TNativePrimitives.cpp | 210 | Updates, orientation, backlight (opcodes 0x01-0x0A+) |
| 0x000005 | Tablet Driver | TNativePrimitives.cpp | 214 | Pen events, calibration (opcodes 0x01-0x0E) |
| 0x000006 | Serial Driver | TNativePrimitives.cpp | 218 | UART config, DMA setup (opcodes 0x01-0x05) |
| 0x000007 | In-Translator (UTF8) | TNativePrimitives.cpp | 222 | Text encoding conversion (opcodes 0x01-0x06) |
| 0x000008 | Out-Translator (UTF8) | TNativePrimitives.cpp | 226 | Text encoding conversion (opcodes 0x01-0x06) |
| 0x000009 | Host Calls (FFI) | TNativePrimitives.cpp | 230 | C library interop (opcodes 0x01-0x7F) |
| 0x00000A | Network Manager | TNativePrimitives.cpp | 234 | Ethernet/WiFi init, packet I/O (opcodes 0x01-0x0A+) |
| 0x00000B | iOS Integration (macOS only) | TNativePrimitives.cpp | 239 | Native bridge calls |
| 0x00000C | Printer Driver | TNativePrimitives.cpp | 244 | Print job setup (opcodes 0x01+) |

**High-bit patch dispatch:** If bit 31 is set, the lower 31 bits
index a virtualized call (TVirtualizedCalls). See
**TVirtualizedCallsPatches.h**:
- k__rt_sdiv (signed division)
- k__rt_udiv (unsigned division)
- kmemmove (optimized block copy)
- ksymcmp__FPcT1 (symbol comparison)

---

## 2. MEMORY MAP & MMIO RANGES

Defined in **TMemoryConsts.h** (lines 43-159). Handled via
**TMemory::Read/Write** (`Emulator/TMemory.cpp`).

| Address Range | Purpose | Handler in TMemory.cpp | Read | Write |
|---|---|---|---|---|
| 0x00000000 - 0x00800000 | Low ROM (0-8 MB) | Direct RAM | Yes | No |
| 0x00800000 - 0x01000000 | High ROM/REX (8-16 MB) | Direct RAM | Yes | No |
| 0x02000000 - 0x02400000 | Flash Bank 1 (Internal Storage) | TFlash | Yes | Via native primitives |
| 0x04000000 - 0x04000000+RAMSize | RAM | Direct RAM | Yes | Yes |
| 0x0F000008 | Platform Version (R) | Line 947 | Special register | - |
| 0x0F001000 | Memory Access Speed (R/W) | Line 1147 | Special register | Special register |
| 0x0F001800 | RAM Size (0x0F001C00 variant) | Line 868 | Returns 0xXYXY00XY pattern | - |
| 0x0F001C00 | RAM Size 2 (R) | Line 874 | Returns 0 | - |
| 0x0F080000 - 0x0F08FC00 | DMA Channel 1 Registers (R/W) | Line 877-880 | TDMAManager::ReadChannel1Register() | TDMAManager::WriteChannel1Register() |
| 0x0F08FC00 | DMA Assignment Register | Line 889-891 | TDMAManager::ReadChannelAssignmentRegister() | TDMAManager::WriteChannelAssignmentRegister() |
| 0x0F090000 - 0x0F098000 | DMA Channel 2 Registers (R/W) | Line 883-886 | TDMAManager::ReadChannel2Register() | TDMAManager::WriteChannel2Register() |
| 0x0F098000 | DMA Enable/Status Register | Line 892-894 | TDMAManager::ReadStatusRegister() | TDMAManager::WriteEnableRegister() |
| 0x0F098400 | DMA Disable Register (W) | - | - | TDMAManager::WriteDisableRegister() |
| 0x0F098800 | DMA Word Status Register (R) | Line 895-897 | TDMAManager::ReadWordStatusRegister() | - |
| 0x0F110000 | External Interrupt Mask (R/W) | TInterruptManager | Yes | Yes |
| 0x0F110400 | High Speed Clock (R, = 0x90) | Line 898-900 | Returns 0x90 | - |
| 0x0F181000 | Calendar Register / RTC Seconds | Line 901-903 | TInterruptManager::GetTimer() | TInterruptManager (write) |
| 0x0F181400 | Alarm Register (R/W) | Line 904-906 | TInterruptManager | TInterruptManager |
| 0x0F181800 | Ticks Register (3.6864 MHz clock) | Line 907-909 | TInterruptManager::GetTimer() | TInterruptManager |
| 0x0F182000 | Match Register 0 (FIQ Timer) | TInterruptManager | Yes | Yes |
| 0x0F182400 | Match Register 1 (IRQ Timer) | TInterruptManager | Yes | Yes |
| 0x0F182800 | Match Register 2 (Timer) | TInterruptManager | Yes | Yes |
| 0x0F182C00 | Match Register 3 (Scheduler) | TInterruptManager | Yes | Yes |
| 0x0F183000 | Interrupt Present Register (R) | Line 910-912 | TInterruptManager::GetIntStatus() | - |
| 0x0F183400 | Interrupt Control Register (R/W) | Line 913-915 | TInterruptManager | TInterruptManager (EnterFIQAtomic sets 0x0C400000) |
| 0x0F183800 | Interrupt Clear Register (W) | TInterruptManager | - | TInterruptManager::ClearInterrupt() |
| 0x0F183C00 | FIQ Mask Register (R/W) | Line 916-918 | TInterruptManager | TInterruptManager |
| 0x0F184000 | Int Enable/Disable Reg 1 (R/W) | Line 919-921 | TInterruptManager | TInterruptManager |
| 0x0F184400 | Int Enable/Disable Reg 2 (R/W) | Line 922-924 | TInterruptManager | TInterruptManager |
| 0x0F184800 | Int Enable/Disable Reg 3 (R/W) | Line 925-927 | TInterruptManager | TInterruptManager |
| 0x0F18C000 | GPIO Raised Register (R) | Line 928-930 | Returns GPIO state | - |
| 0x0F18C400 | GPIO Enable Register (R/W) | Line 931-933 | Returns GPIO state | TMemory::WriteGPIOEnable() |
| 0x0F18D400 | GPIO PCMCIA Card Detect (R) | Line 934-946 | Returns card present status | - |
| 0x0F1C0000 - 0x0F200000 | Serial Ports (Voyager) | TSerialPortDriver | Yes | Yes |
| 0x0F1C0000 | External Serial Port | TSerialPortDriver | Yes | Yes |
| 0x0F1D0000 | Infrared Serial Port | TSerialPortDriver | Yes | Yes |
| 0x0F1E0000 | Built-in Serial Port (Tablet) | TSerialPortDriver | Yes | Yes |
| 0x0F1F0000 | Modem Serial Port | TSerialPortDriver | Yes | Yes |
| 0x0F240000 | External Data Abort Register 1 (R) | Line 975-977 | Special register | - |
| 0x0F240800 | External Data Abort Register 3 (W) | Line 978-980 | - | Special register |
| 0x0F241000 | Bank Control Register (R/W) | Line 981-983 | Special register | Special register |
| 0x10000000 - 0x10400000 | Flash Bank 2 | TFlash | Yes | Via native primitives |
| 0x30000000 - 0x70000000 | PCMCIA Sockets 0-3 | TPCMCIAController | Yes | Yes |

---

## 3. INTERRUPT CONTROLLER & SOURCES

Defined in **TInterruptManager.h** (lines 63-88), implemented in
**TInterruptManager.cpp**.

**Interrupt Masks (used in VIC/enable registers):**

| Mask | Source | Priority | Handler |
|---|---|---|---|
| 0x00000004 | RTC Alarm | IRQ | TInterruptManager |
| 0x00000008 | Timer 0 (FIQ Timer) | FIQ | TInterruptManager |
| 0x00000010 | Timer 1 (IRQ Timer) | IRQ | TInterruptManager |
| 0x00000020 | Timer 2 (Timer) | IRQ | TInterruptManager |
| 0x00000040 | Timer 3 (Scheduler) | IRQ | TInterruptManager |
| 0x00000080 | DMA Channel 0 (Serial 0 RX) | IRQ | TDMAManager |
| 0x00000100 | DMA Channel 1 (Serial 0 TX) | IRQ | TDMAManager |
| 0x00000200 | DMA Channel 2 (IR RX/TX) | IRQ | TDMAManager |
| 0x00000400 | DMA Channel 3 (Audio TX) | IRQ | TSoundManager |
| 0x00000800 | DMA Channel 4 (Audio RX) | IRQ | TSoundManager |
| 0x00001000 | DMA Channel 5 (Tablet) | IRQ | TScreenManager |
| 0x00002000 | DMA Channel 6 (Serial 3 RX) | IRQ | TDMAManager |
| 0x00004000 | DMA Channel 7 (Serial 3 TX) | IRQ | TDMAManager |
| 0x00008000 | Keynes (BIO Interface) | FIQ | TPlatformManager |
| 0x00010000 | PCMCIA Socket 0 (GPIO) | IRQ | TPCMCIAController |
| 0x01000000 | GPIO (GPIO0-31) | IRQ | TMemory::HandleGPIOInterrupt() |
| 0x02000000 | PCMCIA Socket 1 (GPIO) | IRQ | TPCMCIAController |
| 0x08000000 | Platform Events (Power, Dock) | IRQ | TPlatformManager |
| 0x10000000 | Tablet | IRQ | TScreenManager |

**Power-off mask:** 0x0C400000 (Reset + FIQ enabled, IRQ enabled)

---

## 4. ROM PATCHING & REX INJECTION

Einstein applies ROM patches at startup. Two mechanisms:

**A. JIT Generic Patch System** (`Emulator/JIT/Generic/TJITGenericROMPatch.cpp`)

Patches are applied in `TJITGenericPatchManager::DoPatchROM()` during
ROM init. Each patch replaces a word at a ROM address with either:
- A new instruction (`TJITGenericPatch`, line 211)
- An SWI native call: `0xEF800000 | (patch_index & 0x3FFFFF)` (`TJITGenericPatchNativeCall`, line 272)
- An SWI injection: `0xEFC00000 | (patch_index & 0x3FFFFF)` (`TJITGenericPatchNativeInjection`, line 351)

**Patches applied:**

| Address (MP2100US) | Purpose | File:Line |
|---|---|---|
| 0x001412f8 | Avoid screen calibration | TJITGenericROMPatch.cpp:45 |
| 0x000db0d8 | BeaconDetect stub (1/2) | TJITGenericROMPatch.cpp:52 |
| 0x000db0dc | BeaconDetect stub (2/2) | TJITGenericROMPatch.cpp:54 |
| 0x000013f4 | Disable debugging statistics (gDebugger) | TJITGenericROMPatch.cpp:60 |
| 0x000013fc | Enable stdin/stdout (gNewtConfig) | TJITGenericROMPatch.cpp:66 |
| 0x0038CE6C | DebugStr logging | TJITGenericROMPatch.cpp:76 (T_ROM_PATCH) |
| 0x0038CE70 | Debugger breakpoint | TJITGenericROMPatch.cpp:96 (T_ROM_PATCH) |
| 0x00255578 | RealClockSeconds (host time injection) | TJITGenericROMPatch.cpp:110 (T_ROM_PATCH) |
| 0x00089B80 | FTimeInSeconds (2010 problem fix) | TJITGenericROMPatch.cpp:150 (T_ROM_PATCH) |
| 0x0008A8A8 | FDateFromSeconds (2010 problem fix) | TJITGenericROMPatch.cpp:160 (T_ROM_PATCH) |
| 0x420750 | Time base constant 1/4 (2008-01-01) | TJITGenericROMPatch.cpp:170 |
| 0x420798 | Time base constant 2/4 (2008-01-01) | TJITGenericROMPatch.cpp:172 |
| 0x4dca14 | Time base constant 3/4 (2008-01-01) | TJITGenericROMPatch.cpp:174 |
| 0x30F088 | Time base constant 4/4 (2008-01-01) | TJITGenericROMPatch.cpp:176 |
| 0x0008A20C | Ignore setting time (mov pc, lr) | TJITGenericROMPatch.cpp:178 |

**Patch framework constants** (`TJITGenericROMPatch.h:97-103`):
- `kPatchMask = 0xFF800000`: Detects SWI with P bit (bit 23) set
- `kNativeMask = 0xFFC00000`: Detects native call vs. injection (bit 22)
- `kSWIIndexMask = 0x003FFFFF`: Extracts patch index

**B. REX Files (T_ROM_*)**

Defined in `TAIFROMImageWithREXes.cpp`. Overlaid in high ROM:
- Newton REX0 file: Optional platform-specific extensions
- Einstein REX: Custom emulator support

Loaded and patched in `TROMImage::CreateImage()` (`ROM/TROMImage.cpp`).

---

## 5. COPROCESSOR HANDLING (CP14, CP15)

Implemented in **TARMProcessor::SystemCoprocRegisterTransfer()**
(`/Users/walter/Projects/newton/Einstein/Emulator/TARMProcessor.cpp:67`).

**CP15 (System Control) Operations:**

| Operation | CRn | CRm | CP | Opcode | Purpose | Returns/Sets |
|---|---|---|---|---|---|---|
| Read Main ID | 0 | 0 | 0 | MRC | CPU identification | 0x4401A100 (Intel ARMv4) or 0x41047102 (DEC) |
| Read Control | 1 | 0 | 0 | MRC | MMU/cache control | Returns current CTRL value |
| Write Control | 1 | 0 | 0 | MCR | Enable/disable MMU, caches | Sets mMMU state, privilege |
| Read TTB | 2 | 0 | 0 | MRC | Translation Table Base (MMU) | Returns MMU page table pointer |
| Write TTB | 2 | 0 | 0 | MCR | Set translation table | Updates TMMU |
| Read Domain | 3 | 0 | 0 | MRC | Domain access control | Returns current domain bits |
| Write Domain | 3 | 0 | 0 | MCR | Set domain permissions | Updates TMMU::mCurrentAPMode |
| Write FSR Clear | 5 | 0 | 0 | MCR | Clear fault status | Clears mFaultStatus |
| Read FAR | 6 | 0 | 0 | MRC | Fault Address Register | Returns mFaultAddress |
| Write FAR | 6 | 0 | 0 | MCR | Set fault address | Not typically written by OS |
| Cache Invalidate All | 7 | 0 | 0 | MCR | Flush all caches | No-op in emulator |
| TLB Invalidate All | 8 | 0 | 0 | MCR | Flush TLB | Clears TMMU translation cache |

**CP14 (Debug/Breakpoint):**

No explicit implementation; reads return 0, writes are ignored.

---

## 6. INITIAL CPU STATE

Set in **TARMProcessor::Reset()**
(`/Users/walter/Projects/newton/Einstein/Emulator/TARMProcessor.cpp:382`):

| Register | Value | Notes |
|---|---|---|
| R0-R12 | 0 | User registers |
| R13 (SP, banked) | 0x00000000 | Supervisor stack pointer |
| R14 (LR, banked) | 0x00000000 | Supervisor link register |
| R15 (PC) | 0x00000004 | Points to reset vector (ROM address 0x4, with prefetch factor of 4) |
| CPSR | kSupervisorMode (0x13) \| I-bit \| F-bit | 0x000000D3 (Supervisor, IRQ/FIQ disabled, Thumb disabled) |
| Mode | kSupervisorMode | Entry in Supervisor mode |
| MMU | Disabled initially | Enabled by ROM during boot |
| Privilege | true | Supervisor privilege |

**Exception Vector Addresses** (set by `DoUndefinedInstruction`,
`DoIRQInterrupt`, etc.):
- Reset: 0x00000000
- Undefined Instr: 0x00000004 (PC set to 0x00000008)
- SVC: 0x00000008
- Prefetch Abort: 0x0000000C
- Data Abort: 0x00000010
- IRQ: 0x00000018
- FIQ: 0x0000001C

---

## 7. TIMERS & RTC

Implemented in **TInterruptManager**
(`/Users/walter/Projects/newton/Einstein/Emulator/TInterruptManager.cpp`).

**Timer registers (all at 0.27 microsecond granularity / 3.6864 MHz):**

| Address | Name | Purpose | Fired by |
|---|---|---|---|
| 0x0F181800 | Ticks | Current system time (cycles since boot) | Incremented every instruction in emulation |
| 0x0F181000 | Calendar | Seconds since 1904-01-01 00:00:00 (Newton epoch) | Read from host clock, patched at 0x00255578 |
| 0x0F182000 | Match Reg 0 | FIQ Timer threshold | Compares to Ticks; triggers FIQ at match |
| 0x0F182400 | Match Reg 1 | IRQ Timer threshold | Compares to Ticks; triggers IRQ at match |
| 0x0F182800 | Match Reg 2 | General Timer threshold | Compares to Ticks; triggers IRQ at match |
| 0x0F182C00 | Match Reg 3 | Scheduler Timer threshold | Compares to Ticks; triggers IRQ at match |
| 0x0F181400 | Alarm Reg | Alarm time (seconds) | Compared to Calendar; triggers RTC alarm (mask 0x00000004) |

**Time base patches (2010 Y2K fix):**
- NewtonOS uses 1993-01-01 as epoch in user-facing APIs
- Patches shift times to 2008-01-01 (safeIntervalDeltaSeconds = 473299200)
- Will need updating in 2026 (per comments in `TJITGenericROMPatch.cpp:131-140`)

---

## 8. DMA CHANNELS

Implemented in **TDMAManager**
(`/Users/walter/Projects/newton/Einstein/Emulator/TDMAManager.cpp`).

**8 channels, assigned as:**

| Channel | Typical Use | Address Range | Registers |
|---|---|---|---|
| 0 | Serial Port 0 RX | 0x0F080000 (Bank 1) | Control, Source, Dest, Count |
| 1 | Serial Port 0 TX | 0x0F080000 (Bank 1) | Control, Source, Dest, Count |
| 2 | IR RX/TX | 0x0F080000 (Bank 1) | Control, Source, Dest, Count |
| 3 | Audio Transmit (Sound Out) | 0x0F080000 (Bank 1) | Control, Source, Dest, Count |
| 4 | Audio Receive (Sound In) | 0x0F080000 (Bank 1) | Control, Source, Dest, Count |
| 5 | Tablet Digitizer | 0x0F080000 (Bank 1) | Control, Source, Dest, Count |
| 6 | Serial Port 3 RX | 0x0F090000 (Bank 2) | Control, Source, Dest, Count |
| 7 | Serial Port 3 TX | 0x0F090000 (Bank 2) | Control, Source, Dest, Count |

**Control Registers:**

| Address | Name | Semantics |
|---|---|---|
| 0x0F08FC00 | Channel Assignment | Maps logical channels to physical |
| 0x0F098000 | Enable/Status | R: pending transfers; W: enable bits |
| 0x0F098400 | Disable | W: abort bits |
| 0x0F098800 | Word Status | R: bits set if words in channel word register |

**DMA Implementation:** TDMAManager emulates DMA register I/O but
does NOT perform actual transfers. Instead:
- ROM writes source/dest addresses
- ROM sets enable bits
- Emulator posts DMA complete interrupt immediately (no actual block copy)
- Works for simple packet transfers; insufficient for real streaming DMA

---

## 9. SERIAL PORTS

Implemented in **Serial/** subdirectory; base interface in
**TSerialPortDriver.h**.

**4 serial ports (Voyager-style UART):**

| Port | Base Address | Range | Purpose | Manager |
|---|---|---|---|---|
| 0 (extr) | 0x0F1C0000 | 0x0F1C0000 - 0x0F1D0000 | External serial (Host debug) | TSerialPortDriver |
| 1 (infr) | 0x0F1D0000 | 0x0F1D0000 - 0x0F1E0000 | Infrared | TSerialPortDriver |
| 2 (tblt) | 0x0F1E0000 | 0x0F1E0000 - 0x0F1F0000 | Built-in (Tablet) | TSerialPortDriver |
| 3 (mdem) | 0x0F1F0000 | 0x0F1F0000 - 0x0F200000 | Modem | TSerialPortDriver |

**Register layout (per UART; offsets from base):**
- +0x00: Data register (R/W)
- +0x04: Status register (R)
- +0x08: Control register (W)
- +0x0C: Baud rate register (W)

**Byte delivery:**
- Reads from port address trigger IRQ via DMA channel
- Writes to port address queue bytes
- TSerialHostPort classes bridge to host stdin/pipes/network

---

## 10. SCREEN & TABLET

Implemented in **TScreenManager**
(`/Users/walter/Projects/newton/Einstein/Emulator/Screen/TScreenManager.cpp`).

**Screen resolution:** 320x480 portrait (configurable via
TScreenManager constructor)

**Framebuffer setup:**
- Newton uses 4 bits per pixel (16 greys) by default (defined in TScreenManager.h:100-107)
- Framebuffer location: Typically in RAM, accessed via native primitive at 0x0F000000 range
- Orientation: 4 states (portrait, right, bottom, left; rotated = bottom/top + landscape bit)

**Tablet:**
- Pressure-sensitive stylus emulation
- Sample rate: Configurable (default 0x0000B400 = 46080 Hz approx., TScreenManager.h:109)
- States: PenIsUp (0), PenIsDown (1), TabletIsBypassed (8), TabletIsOff (9)
- Calibration: TNativePrimitives stores 5x KUInt32 calibration struct (mTabletCalibration, TNativePrimitives.h:291)
- Input path: Host app calls TScreenManager::PenDown(x, y, pressure, time) → triggers Tablet interrupt (mask 0x10000000)

**Screen native ops** (TNativePrimitives::ExecuteScreenDriverNative, TScreenManager.cpp:1564):
- Contrast, backlight, orientation control
- Framebuffer update commands
- DMA descriptor for screen refresh

---

## 11. SOUND

Implemented in **TSoundManager**
(`/Users/walter/Projects/newton/Einstein/Emulator/Sound/TSoundManager.h`).

**Audio I/O:**
- Output: 8-bit or 16-bit PCM samples (configurable)
- Input: Microphone data (stub - no input implemented)
- Sample rate: Configurable via native primitive
- DMA channels: 3 (output), 4 (input)
- Volume: 0x80000000 (silent) to 0x00000000 (max), normalized in range [0.0, 1.0]

**Sound native ops** (TNativePrimitives::ExecuteSoundDriverNative, TNativePrimitives.cpp:1060):
- ScheduleOutputBuffer (buffer address, size) → marks buffer for playback
- StartOutput / StopOutput → controls DMA
- OutputIsRunning → poll status
- OutputVolume → gets/sets volume

**Platform implementations:**
- Host/TSoundManager.cpp: Abstract base
- TSoundManagerNull: No-op (disables audio)
- TSoundManagerSDL: SDL audio output
- Android: Native audio via NDK

---

## 12. PCMCIA / FLASH CARDS

Implemented in **PCMCIA/** (`/Users/walter/Projects/newton/Einstein/Emulator/PCMCIA/`).

**2 PCMCIA sockets (expandable to 4):**

| Socket | Base | End | Interrupt Mask | Card Types |
|---|---|---|---|---|
| 0 | 0x30000000 | 0x40000000 | 0x00010000 (GPIO) | ATA, Ethernet (NE2000), RAM Linear |
| 1 | 0x40000000 | 0x50000000 | 0x02000000 (GPIO) | ATA, Ethernet (NE2000), RAM Linear |
| 2 | 0x50000000 | 0x60000000 | - | (not implemented) |
| 3 | 0x60000000 | 0x70000000 | - | (not implemented) |

**Card types:**

| Class | File | Features |
|---|---|---|
| TATACard | TATACard.cpp | IDE disk emulation (reads/writes backed by host file) |
| TNE2000Card | TNE2000Card.cpp | Ethernet NIC emulation (Novell NE2000 compatible) |
| TLinearCard | TLinearCard.cpp | Linear flash (memory-like card) |

**Controller** (TPCMCIAController.cpp):
- Attribute memory (card ID, config)
- Common memory (data access)
- Power control (Vcc, Vpp) via native primitive 0x0A (TPlatformManager::GetPCMCIAPowerSpec)
- Insertion/removal signaling via GPIO interrupt

---

## 13. FLASH STORAGE (Internal)

Backed by host file (`/Users/walter/Projects/newton/Einstein/Emulator/TFlash.cpp`).

**Flash banks:**

| Bank | Address | End | Size | Purpose |
|---|---|---|---|---|
| 1 | 0x02000000 | 0x02400000 | 4 MB | Internal store (user data) |
| 2 | 0x10000000 | 0x10400000 | 4 MB | More internal store |

**Flash native ops** (TNativePrimitives::ExecuteFlashDriverNative, TNativePrimitives.cpp:263):
- Identify (0x01): Returns flash chip ID and geometry
- Write (0x08): WriteToFlash16Bits() / WriteToFlash32Bits() depending on platform
- Erase (0x09): EraseFlash() with block size detection
- IsEraseComplete (0x0B): Immediate (no async erase)

**Platform detection hacks** (TNativePrimitives.cpp:378-386):
Detects 16/32-bit flash access via virtual table address:
- 0x0001E3D4 (MP2100US 32-bit), 0x0001E3E0 (MP2100D 32-bit)
- 0x0001E3BC (MP2100US 16-bit), 0x0001E168 (EM300 16-bit)

---

## 14. PLATFORM MANAGER & EVENTS

Implemented in **Platform/TPlatformManager**
(`/Users/walter/Projects/newton/Einstein/Emulator/Platform/TPlatformManager.cpp`).

**Power management:**

| Event | Trigger | Handler | Effects |
|---|---|---|---|
| PowerOn | Native 0x0F | TPlatformManager::PowerOn() | Enables interrupts, resumes execution |
| PowerOff | Native 0x0E | TPlatformManager::PowerOff() | Masks interrupts, pauses execution (unless mQuit) |
| PowerOnSubsystem | Native 0x0A with subsystem ID | Subsystem-specific | E.g., 0x1D = Flash power via mMemory->PowerOnFlash() |
| PowerOffSubsystem | Native 0x0B | Subsystem-specific | E.g., 0x1D = Flash power via mMemory->PowerOffFlash() |

**Event queue:**
- Locked/unlocked via native primitives 0x18/0x19
- Enqueued by Host app via SendAEvent()
- Delivered to ROM via GetNextEvent (native 0x15) with interrupt pending

**Gestalt / Platform ID:**

| Selector | Info | Handler |
|---|---|---|
| Platform Version | 0x00010002 (UP2) | Native 0x17 → mMemory->Write(..., kUP2Version) |
| Einstein Emulator | 0x03000002 | Gestalt call; returns struct with version |

**User info:**
- Name, company, owner, device type, serial number
- Retrieved via GetUserInfo (native 0x1B)
- Defined in Host/UserInfoDefinitions.h

---

## 15. HOST CALLS / NATIVE C INTEROP (FFI)

Implemented in **NativeCalls/TNativeCalls**
(`/Users/walter/Projects/newton/Einstein/Emulator/NativeCalls/TNativeCalls.cpp`).

**Mechanism:**
1. ROM NewtonScript layer marshals C function calls into FFI structures
2. Newton SOUPS module calls into native primitive 0x09 (ExecuteHostCallNative)
3. Einstein dispatches to TNativeCalls::{OpenLib, PrepareFFIStructure, CallFunction, SetArgValue_*}

**FFI structure:**
- Symbol name (max 256 chars)
- Return type (void, uint8-64, float, double, pointer, string, binary)
- Arg types (same enum as return)
- Arg values (in memory)

**Platform support:**
- Only on non-macOS, non-Android, 32-bit systems
- Uses libffi to wrap host C libraries
- TNativeCalls::mNativeCalls instantiated in TNativePrimitives constructor (TNativePrimitives.cpp:117)

---

## 16. VIRTUALIZED CALLS (ROM Code Injection)

Implemented in **NativeCalls/TVirtualizedCalls**
(`/Users/walter/Projects/newton/Einstein/Emulator/NativeCalls/TVirtualizedCalls.cpp`).

**Patches for runtime support:**

| Index (k_*) | Function | Purpose | File:Line |
|---|---|---|---|
| k__rt_sdiv | __rt_sdiv | Signed integer division (R0 = R1 / R0, remainder in R1) | TVirtualizedCalls.cpp:64 |
| k__rt_udiv | __rt_udiv | Unsigned integer division | TVirtualizedCalls.cpp:78 |
| kmemmove | memmove | Optimized block copy (handles overlapping regions) | TVirtualizedCalls.cpp:149 |
| ksymcmp__FPcT1 | symcmp__FPcT1 | Symbol comparison for NewtonScript | TVirtualizedCalls.cpp (not shown in excerpt) |

**Invocation:**
When instruction has bit 31 set (0x80000000), lower 31 bits are the
patch index passed to TVirtualizedCalls::Execute() (TNativePrimitives.cpp:186).

---

## 17. PRINTER SUPPORT

Implemented in **Printer/TPrinterManager**
(`/Users/walter/Projects/newton/Einstein/Emulator/Printer/`).

**Printer ops** (TNativePrimitives::ExecutePrinterDriverNative, TNativePrimitives.cpp:3237):
- SetupPrintJob (native 0x01): Allocates job ID
- SendPageData (native 0x02): Buffers page content
- ClosePrintJob (native 0x03): Finalizes job
- GetPrinterStatus (native 0x04): Returns printer ready state
- CancelPrintJob (native 0x05): Aborts job

**Platform implementations:**
- Host/TPrinterManager.cpp: Abstract interface
- Subclasses: Print to file, network printer, host print queue

---

## 18. NETWORK / ETHERNET

Implemented in **Network/**
(`/Users/walter/Projects/newton/Einstein/Emulator/Network/`).

**Network ops** (TNativePrimitives::ExecuteNetworkManagerNative, TNativePrimitives.cpp:2889):
- GetMACAddress (native 0x01): Returns emulated MAC
- OpenDriver (native 0x02): Initializes network hardware
- CloseDriver (native 0x03): Shuts down
- SendPacket (native 0x04): Transmits Ethernet frame
- ReceivePacket (native 0x05): Retrieves buffered packet
- SetPromiscuousMode (native 0x06): Raw packet capture

**Drivers:**
- TUsermodeNetwork: User-mode TCP/IP stack (no root/TAP required)
- TTapNetwork: TAP device (real Ethernet, requires root)

**PCMCIA NE2000 card:**
- NE2000-compatible Ethernet controller
- Mapped to socket 0 or 1; emulates physical NIC
- Packet I/O via DMA channels (not implemented; packets dropped)

---

## 19. HARDWARE DETECTION & STUBS

Einstein emulates absence of features via:

| Feature | Stub | Returns |
|---|---|---|
| External modem | No modem driver init | Error codes |
| IR port | Buffered but non-functional | Success, no data |
| Keyboard | Virtual keyboard via host UI | Depends on host app |
| Docking | GPIO interrupt on dock switch | None (no docking station hardware) |
| Expansion cards | PCMCIA socket empty | No card present (GPIO) |
| Backup battery | Always full | Via Battery status native primitive |
| AC adapter | Always plugged in | Via Battery status native primitive |

---

## 20. INSTRUCTION-LEVEL TRAPS

Einstein does NOT trap all instructions; only these special forms
trigger native handling:

**1. Coprocessor writes (CP10):**
- MCR to CP10 → TNativePrimitives::ExecuteNative()
- Format: `E xE y0000 CRn Rd CP Opc CRm`

**2. Undefined instruction (UND):**
- 0xE7xxxxxx with D bit set (for breakpoints)
- Format: Triggers TARMProcessor::DoUndefinedInstruction()
- Special UND opcodes in ROM:
  - SystemBootUND (0x0F000000) → TEmulator::SystemBootUND()
  - DebuggerUND (0x0F000001) → TEmulator::DebuggerUND()
  - TapFileCntlUND (0x0F000002) → TEmulator::TapFileCntlUND()

**3. Software interrupt (SWI):**
- 0xEFxxxxxx → Processor::DoSWI()
- Lower 24 bits: selector for ROM-level system calls (not Einstein natives)
- Patched SWI (bits 23:22 set) → rerouted to JIT patches

**4. Breakpoint (BKPT):**
- Format: 0xE120xxxx (rare; used for logging)
- Triggers TEmulator::Breakpoint(ID)

---

## 21. INITIAL REGISTER STATE AT BOOT

When TEmulator::Run() starts, the ARM processor is in state set by
TARMProcessor::Reset():

- **PC (R15):** 0x00000004 (reset vector + prefetch)
- **SP (R13 supervisor):** 0x00000000
- **CPSR:** 0x000000D3 (Supervisor mode, IRQs/FIQs disabled)
- **Other registers:** 0x00000000

The ROM's first instruction at 0x00000000 is a branch to the actual
entry point (typically 0x00000010 or 0x00001000 depending on ROM).

---

## 22. CRITICAL DIFFERENCES FROM BARE-METAL (FOR HYPERVISOR COMPARISON)

Einstein does things a bare-metal hypervisor might NOT:

1. **Patches ROM at load time** → Hypervisor must either:
   - Apply same patches, or
   - Intercept the target addresses via MMIO, or
   - Accept divergent ROM behavior

2. **Intercepts coprocessor writes (CP10)** → Hypervisor must trap
   ALL CP10 operations (MCR/MRC)

3. **Injects timer interrupts on schedule** → Hypervisor must emulate
   TInterruptManager's timer logic exactly

4. **Virtualizes DMA** → Hypervisor must handle DMA channel register
   writes and simulate completion

5. **Stubs many drivers (battery, modem, etc.)** → Hypervisor must
   return the same stub values or ROM will hang/fail

6. **Manages MMU page table setup** → Hypervisor must handle CP15
   domain/TTB writes identically

7. **Synchronizes platform events** → Hypervisor must deliver platform
   interrupts at the same logical times

8. **Flash storage backed by host file** → Hypervisor must implement
   persistent flash storage the same way
