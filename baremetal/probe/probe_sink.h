// baremetal/probe/probe_sink.h
//
// C ABI for the NewtonProbe instrumentation. Empty when NEWTON_PROBE_INSTRUMENT
// is not defined, so regular Einstein builds see no footprint at all.
//
// The header is included from TARMProcessor.cpp and from the SWP JIT template.
// All counters are collected in probe.cpp; the sink functions below are the
// boundary.

#ifndef NEWTON_PROBE_SINK_H
#define NEWTON_PROBE_SINK_H

#if defined(NEWTON_PROBE_INSTRUMENT)

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/// CP15 register transfer. `dir`: 0 = MCR (write to CP), 1 = MRC (read from CP).
void probe_record_cp15(uint32_t pc, uint32_t cpopc, uint32_t crn, uint32_t crm,
	uint32_t cp, uint32_t dir, uint32_t value);

/// SWP/SWPB executed. `is_byte`: true for SWPB, false for SWP.
void probe_record_swp(uint32_t pc, uint32_t is_byte);

/// ARM mode transition from `old_mode` to `new_mode`. Both encodings are the
/// raw CPSR M[4:0] values (kUserMode=0x10, kSupervisorMode=0x13, ...).
void probe_record_mode(uint32_t pc, uint32_t old_mode, uint32_t new_mode);

/// A guest store that hits a virtual address below the ROM end (`< 0x01000000`).
/// Useful to detect self-modifying ROM or any writes that land on the ROM
/// image itself after the MMU has remapped ROM windows into RAM.
void probe_record_rom_write(uint32_t pc, uint32_t vaddr, uint32_t value);

#ifdef __cplusplus
} // extern "C"
#endif

#else // !NEWTON_PROBE_INSTRUMENT

// Stubs so callers can unconditionally invoke without ifdefs at the call site.
#define probe_record_cp15(pc, cpopc, crn, crm, cp, dir, value) ((void) 0)
#define probe_record_swp(pc, is_byte) ((void) 0)
#define probe_record_mode(pc, old_mode, new_mode) ((void) 0)
#define probe_record_rom_write(pc, vaddr, value) ((void) 0)

#endif

#endif // NEWTON_PROBE_SINK_H
