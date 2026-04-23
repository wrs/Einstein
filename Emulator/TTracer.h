// ==============================
// File:            TTracer.h
// Project:         Einstein
//
// Function-entry tracer. When enabled, logs a line every time the emulated
// CPU executes the first instruction of a known Newton function (as listed
// in a classifier-style 0xADDR\tNAME symbol file). Output format matches the
// bare-metal hypervisor's tracer exactly so the two logs can be diffed:
//
//   trace <seq:5> <pc:#010x> <name> (<mode>) r0=<..> r1=<..> r2=<..> r3=<..>
//
// Integration: the JIT translator calls TTracer::Lookup(inVAddr) at page
// translation time and, on a hit, injects a traceFunctionEntry JIT unit that
// calls TTracer::LogEntry at dispatch time. Disabled by default — zero cost
// unless Enable() was called before the first page is translated.
// ==============================

#ifndef _TTRACER_H
#define _TTRACER_H

#include <K/Defines/KDefinitions.h>

class TARMProcessor;

class TTracer
{
public:
	// Load symbols from `symbolsPath` (lines of "0xADDR\tNAME") and open
	// `outputPath` for writing. On failure logs to stderr and leaves the
	// tracer disabled. Call before the first JIT translation runs.
	static void Enable(const char* symbolsPath, const char* outputPath);

	// Fast hot-path predicate for the JIT translator. Safe to call from any
	// thread; becomes true only between Enable() and program exit.
	static Boolean IsEnabled() { return sEnabled; }

	// Returns the symbol name for `pc` (ROM virtual address of the first
	// instruction of a function), or nullptr if `pc` is not a known entry.
	// The returned pointer is stable for the lifetime of the process — the
	// caller stores it in the JIT unit stream.
	static const char* Lookup(KUInt32 pc);

	// Called by the injected JIT unit on function entry. Emits one trace
	// line to the output file.
	static void LogEntry(TARMProcessor* ioCPU, KUInt32 pc, const char* name);

	// Flush output. Not required for correctness but useful before abnormal
	// termination (segfault in the emulator thread, etc.).
	static void Flush();

private:
	static volatile Boolean sEnabled;
};

#endif
// _TTRACER_H
