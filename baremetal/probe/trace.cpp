// baremetal/probe/trace.cpp
//
// Headless Einstein harness that boots a Newton 2.x ROM with function-entry
// tracing enabled. The JIT translator injects a log unit at every ROM address
// listed in the symbols file, producing a line per entry call in a format
// that matches the bare-metal hypervisor's `--features trace` output exactly,
// so the two traces can be diffed.

#include <atomic>
#include <cctype>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>

#include "Emulator/Log/TStdOutLog.h"
#include "Emulator/Network/TUsermodeNetwork.h"
#include "Emulator/ROM/TFlatROMImageWithREX.h"
#include "Emulator/Screen/TNullScreenManager.h"
#include "Emulator/Sound/TNullSoundManager.h"
#include "Emulator/TARMProcessor.h"
#include "Emulator/TEmulator.h"
#include "Emulator/TMemory.h"
#include "Emulator/TTracer.h"
#include "Emulator/JIT/Generic/TJITGeneric_Macros.h"
#include "Emulator/JIT/Generic/TJITGenericROMPatch.h"

// TNativePrimitives.cpp references gToolkit; provide the same null stub as
// probe.cpp since we link without the Toolkit layer.
class TToolkit;
TToolkit* gToolkit = nullptr;

// ns_trace + full_ns_trace mirror of the bare-metal hypervisor's
// features of the same name. Four ROM word edits at load time, plus
// one JIT injection that pokes `gInterpreter[+124] = 1` once
// gInterpreter has been allocated, so every NS-level DoSend / DoMessage
// / DoFastApply reaches Print. Mirrors:
//   - src/rom_patches.rs (TraceSetOptions / ConsumeFrame / PrintObject)
//   - src/heap_check.rs `force_interpreter_trace_on`
//
// Layout invariants (717006 ROM, NewtonOS 2.x):
//   0x0035e7d8: TraceSetOptions: `teq r0, #2` -> `teq r0, #0`. Real Refs
//               are never 0, so the function falls through to the
//               setup-with-defaults branch that opens the +104/+105/+108
//               /+112/+116 trace gates instead of the early "tracing off"
//               exit.
//   0x0035e7d4: TraceSetOptions: `mov r7, #0` -> `mov r7, #3`. The
//               first store to TInterpreter+0x7C selects the trace mode
//               family; #3 is the broader default the EQRef/symbol-match
//               branch later in the same function already writes for one
//               specific symbol, so #3 is downstream-safe.
//   0x000e6a1c: ConsumeFrame: `teq r0, #0` -> `teq r0, #FF`. Forces the
//               PrintObject call path even when the depth check would
//               otherwise short-circuit.
//   0x0033cb24: PrintObjectAux: `bl …` -> `mov r0, #8`. Caps print depth
//               so we don't recurse forever rendering frames.
struct RomEdit {
	KUInt32 addr;
	KUInt32 word;
	const char* what;
};
constexpr RomEdit kRomEdits[] = {
	{ 0x0035e7d8, 0xe3300000, "TraceSetOptions: teq r0, #0 (ns_trace)" },
	{ 0x0035e7d4, 0xe3a07003, "TraceSetOptions: mov r7, #3 (full_ns_trace)" },
	{ 0x000e6a1c, 0xe33000ff, "ConsumeFrame: teq r0, #0xFF (full_ns_trace)" },
	{ 0x0033cb24, 0xe3a00008, "PrintObjectAux: mov r0, #8 (full_ns_trace)" },
};
constexpr KUInt32 kGInterpreterPtrVA   = 0x0c105458; // *gInterpreter
constexpr KUInt32 kInterpreterTraceOff = 124;        // +0x7C field

// Injection: at InitScriptGlobals entry, poke gInterpreter[+124] = 1.
// InitScriptGlobals runs after the TInterpreter is allocated and well
// before bootinitnsglobals starts executing, so the trace flag is in
// place by the time the first DoSend / DoMessage fires.
T_ROM_INJECTION(0x001f1828, kROMPatchVoid, kROMPatchVoid, kROMPatchVoid,
				"ns_trace: poke gInterpreter[+124]=1 at InitScriptGlobals entry") {
	static bool sDone = false;
	if (sDone) return ioUnit;
	TMemory* mem = ioCPU->GetMemory();
	KUInt32 gInterpreter = 0;
	if (mem->Read(kGInterpreterPtrVA, gInterpreter)) {
		std::fprintf(stderr, "ns_trace: failed to read gInterpreter ptr at VA %#010x\n",
			kGInterpreterPtrVA);
		sDone = true;
		return ioUnit;
	}
	if (gInterpreter == 0) {
		std::fprintf(stderr, "ns_trace: gInterpreter still NIL at InitScriptGlobals entry\n");
		sDone = true;
		return ioUnit;
	}
	if (mem->Write(gInterpreter + kInterpreterTraceOff, 1)) {
		std::fprintf(stderr, "ns_trace: failed to write gInterpreter[+%u]=1\n",
			kInterpreterTraceOff);
	} else {
		std::fprintf(stderr, "ns_trace: gInterpreter[+%u]=1 (gInterpreter=%#010x)\n",
			kInterpreterTraceOff, gInterpreter);
	}
	sDone = true;
	return ioUnit;
}

// Capture PHammerOutTranslator output. Stock Print/Putc implementations
// hand bytes off to vfprintf/fputc against a FILE* the kernel never
// drains, so the trace events we just enabled would otherwise vanish.
// Replace each method body with native code that writes into a per-line
// buffer flushed on '\n'; output goes to stderr to keep it separate
// from the function-trace stream that NewtonTrace already writes to its
// `output.txt`.
static std::string& GetReplineBuffer()
{
	static std::string sLine;
	return sLine;
}
static void FlushReplineByte(KUInt32 ch)
{
	std::string& line = GetReplineBuffer();
	const char c = static_cast<char>(ch & 0xFF);
	if (c == '\r' || c == '\n') {
		// Newton uses \r as the line terminator; \n appears occasionally
		// (not visible in early-boot fmt strings). Treat both as flush.
		std::fprintf(stderr, "REP> %s\n", line.c_str());
		line.clear();
	} else if (c >= ' ' && c < 0x7f) {
		line.push_back(c);
	} else {
		// control byte; render as \xHH so we don't lose visibility
		char esc[5];
		std::snprintf(esc, sizeof(esc), "\\x%02x", c & 0xFF);
		line.append(esc);
	}
}

// PHammerOutTranslator::Print(this, fmt, ...) at 0x000e6a90.
// We don't try to render the format string against varargs - that would
// require reconstructing a va_list from r2/r3+stack and risks crashing
// on %s if any "ptr" arg is actually an integer. Instead we substitute
// %d/%x format conversions with the literal numeric value of the arg
// slot they would have consumed, leaving %s as "<%s>" so the line
// shape matches our hypervisor rep_print output.
static void DumpFormatString(TARMProcessor* ioCPU, KUInt32 fmtAddr)
{
	TMemory* mem = ioCPU->GetMemory();
	char fmt[1024];
	KUInt32 amount = sizeof(fmt);
	if (mem->FastReadString(fmtAddr, &amount, fmt)) return;
	KUInt32 argSlot = 0;
	auto nextArg = [&]() -> KUInt32 {
		KUInt32 v;
		switch (argSlot) {
			case 0: v = ioCPU->GetRegister(2); break;
			case 1: v = ioCPU->GetRegister(3); break;
			default: {
				KUInt32 sp = ioCPU->GetRegister(13);
				KUInt32 word = 0;
				if (mem->ReadAligned(sp + 4 * (argSlot - 2), word)) word = 0xDEADBEEF;
				v = word;
				break;
			}
		}
		argSlot++;
		return v;
	};
	for (const char* p = fmt; *p; ) {
		char c = *p++;
		if (c != '%') { FlushReplineByte((KUInt8) c); continue; }
		// parse flags
		bool leftJustify = false;
		while (*p && std::strchr("-+ #0", *p)) {
			if (*p == '-') leftJustify = true;
			p++;
		}
		// parse width (literal digits, or `*` -> consume int arg)
		int width = 0;
		if (*p == '*') { width = (int) nextArg(); p++; }
		else while (*p && std::isdigit((unsigned char) *p)) {
			width = width * 10 + (*p - '0');
			p++;
		}
		// parse precision (literal digits, or `*` -> consume int arg)
		if (*p == '.') {
			p++;
			if (*p == '*') { (void) nextArg(); p++; }
			else while (*p && std::isdigit((unsigned char) *p)) p++;
		}
		while (*p == 'l' || *p == 'L' || *p == 'h') p++;
		char spec = *p; if (spec) p++;
		char buf[64];
		auto emit = [&](const char* s) {
			int len = (int) std::strlen(s);
			if (!leftJustify) for (int i = len; i < width; ++i) FlushReplineByte(' ');
			for (const char* q = s; *q; ++q) FlushReplineByte((KUInt8) *q);
			if (leftJustify) for (int i = len; i < width; ++i) FlushReplineByte(' ');
		};
		switch (spec) {
			case 'd': case 'i':
				std::snprintf(buf, sizeof(buf), "%d", (int) nextArg()); emit(buf); break;
			case 'u':
				std::snprintf(buf, sizeof(buf), "%u", (unsigned) nextArg()); emit(buf); break;
			case 'x':
				std::snprintf(buf, sizeof(buf), "%x", (unsigned) nextArg()); emit(buf); break;
			case 'X':
				std::snprintf(buf, sizeof(buf), "%X", (unsigned) nextArg()); emit(buf); break;
			case 'p':
				std::snprintf(buf, sizeof(buf), "%#x", (unsigned) nextArg()); emit(buf); break;
			case 'c':
				FlushReplineByte(nextArg() & 0xFF); break;
			case 's': {
				KUInt32 sa = nextArg();
				char s[256];
				KUInt32 sn = sizeof(s);
				if (mem->FastReadString(sa, &sn, s)) {
					std::snprintf(buf, sizeof(buf), "<bad-str %#x>", sa);
					emit(buf);
				} else {
					emit(s);
				}
				break;
			}
			case '%':
				FlushReplineByte('%'); break;
			default:
				FlushReplineByte('%');
				if (spec) FlushReplineByte((KUInt8) spec);
				break;
		}
	}
}

T_ROM_INJECTION(0x000e6a90, kROMPatchVoid, kROMPatchVoid, kROMPatchVoid,
				"ns_trace: capture PHammerOutTranslator::Print")
{
	DumpFormatString(ioCPU, ioCPU->GetRegister(1));
	return ioUnit;
}

T_ROM_INJECTION(0x000e6ad0, kROMPatchVoid, kROMPatchVoid, kROMPatchVoid,
				"ns_trace: capture PHammerOutTranslator::Putc")
{
	FlushReplineByte(ioCPU->GetRegister(1));
	return ioUnit;
}

namespace {

constexpr KUInt32 kDefaultBootSeconds = 30;
constexpr KUInt32 kDefaultRAMSize = 4 * 1024 * 1024; // MP2x00 bare RAM, 4 MiB.

void usage(const char* argv0) {
	std::fprintf(stderr,
		"usage: %s <rom.bin> <rex.bin|-> <symbols.txt> <output.txt> [wall-seconds]\n"
		"  rom.bin        path to the Newton ROM dump\n"
		"  rex.bin        path to Einstein.rex, or - for the builtin\n"
		"  symbols.txt    classifier-style code-symbols.txt (0xADDR\\tNAME)\n"
		"  output.txt     file to write trace lines to\n"
		"  wall-seconds   host wall-clock seconds to run (default: %u)\n",
		argv0, kDefaultBootSeconds);
}

} // namespace

int main(int argc, char** argv) {
	if (argc < 5 || argc > 6) { usage(argv[0]); return 2; }

	const char* romPath = argv[1];
	const char* rexPath = (std::strcmp(argv[2], "-") != 0) ? argv[2] : nullptr;
	const char* symbolsPath = argv[3];
	const char* outputPath = argv[4];
	unsigned wallSeconds = (argc >= 6)
		? static_cast<unsigned>(std::strtoul(argv[5], nullptr, 0))
		: kDefaultBootSeconds;

	// Tracer must be enabled before TEmulator::Run so the first translation
	// of each page sees IsEnabled() == true and injects the trace units.
	TTracer::Enable(symbolsPath, outputPath);

	const char* flashPath = "/tmp/newton-trace.flash";
	(void) std::remove(flashPath);

	TStdOutLog log;
	TFlatROMImageWithREX rom(romPath, rexPath);
	if (rom.GetErrorCode() != TROMImage::kNoError) {
		std::fprintf(stderr, "trace: failed to load ROM (code %d)\n", rom.GetErrorCode());
		return 1;
	}
	std::fprintf(stderr, "trace: ROM id = %d, %u JIT patches registered\n",
		rom.GetROMId(), TJITGenericPatchManager::GetNumPatches());

	// Apply ns_trace + full_ns_trace ROM word edits in-place. CreateImage
	// already byte-swapped to host-LE and ran the JIT patch manager, so
	// these direct word writes land cleanly with no T_ROM_PATCH collision
	// at any of the four addresses.
	{
		KUInt32* romWords = reinterpret_cast<KUInt32*>(rom.GetPointer());
		for (const RomEdit& edit : kRomEdits) {
			const KUInt32 idx = edit.addr / 4;
			std::fprintf(stderr, "ns_trace: ROM[%#010x] %#010x -> %#010x  (%s)\n",
				edit.addr, romWords[idx], edit.word, edit.what);
			romWords[idx] = edit.word;
		}
	}

	TNullSoundManager sound(&log);
	TNullScreenManager screen(&log);
	TUsermodeNetwork net(&log);

	TEmulator emu(&log, &rom, flashPath, &sound, &screen, &net, kDefaultRAMSize);

	std::fprintf(stdout, "trace: booting ROM for %u seconds wall-clock...\n", wallSeconds);
	std::fflush(stdout);

	std::thread emuThread([&]() { emu.Run(); });

	auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(wallSeconds);
	while (std::chrono::steady_clock::now() < deadline) {
		std::this_thread::sleep_for(std::chrono::milliseconds(500));
	}

	emu.Stop();
	emuThread.join();
	TTracer::Flush();

	std::fprintf(stdout, "trace: done\n");
	std::fflush(stdout);

	// Skip destructors — same reason as probe.cpp: the interrupt-manager
	// and network threads need a clean shutdown path we haven't wired up.
	std::_Exit(0);
}
