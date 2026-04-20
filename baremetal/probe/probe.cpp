// baremetal/probe/probe.cpp
//
// Headless Einstein harness that boots a Newton 2.x ROM far enough for the
// guest to set up its MMU, then walks the translation tables and prints a
// typed descriptor map. Answers HIGHLEVEL.md open question §16.2 (does 2.x
// use fine tables / tiny pages that would block native A53 stage-1 walks?).
//
// Compile by adding an add_executable(NewtonProbe ...) target that reuses
// Einstein's ${common_sources}; see baremetal/probe/CMakeLists.txt.

#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <map>
#include <mutex>
#include <set>
#include <thread>
#include <tuple>

#include "Emulator/TEmulator.h"
#include "Emulator/TMMU.h"
#include "Emulator/TMemory.h"
#include "Emulator/TARMProcessor.h"
#include "Emulator/JIT/TJIT.h"
#include "Emulator/Log/TStdOutLog.h"
#include "Emulator/Network/TNetworkManager.h"
#include "Emulator/Network/TUsermodeNetwork.h"
#include "Emulator/ROM/TFlatROMImageWithREX.h"
#include "Emulator/Screen/TNullScreenManager.h"
#include "Emulator/Sound/TNullSoundManager.h"
#include "baremetal/probe/probe_sink.h"

// ==========================================================================
//  Instrumentation sink
// ==========================================================================

namespace probe_state {

std::mutex mu;

// CP15 accesses keyed by (opc1, CRn, CRm, opc2, dir). Value tracks count and
// the first PC that issued the tuple, for traceability.
struct Cp15Key {
	uint32_t opc1, crn, crm, opc2, dir;
	bool operator<(const Cp15Key& o) const {
		return std::tie(opc1, crn, crm, opc2, dir)
			< std::tie(o.opc1, o.crn, o.crm, o.opc2, o.dir);
	}
};
struct Cp15Val {
	uint64_t count { 0 };
	uint32_t first_pc { 0 };
	uint32_t last_value { 0 };
};
std::map<Cp15Key, Cp15Val> cp15;

// SWP instruction counters and the set of unique call sites.
uint64_t swp_word_count { 0 };
uint64_t swp_byte_count { 0 };
std::set<uint32_t> swp_pcs;

// Mode transitions: (old_mode, new_mode) -> count, first_pc.
struct ModeKey {
	uint32_t old_mode, new_mode;
	bool operator<(const ModeKey& o) const {
		return std::tie(old_mode, new_mode) < std::tie(o.old_mode, o.new_mode);
	}
};
struct ModeVal {
	uint64_t count { 0 };
	uint32_t first_pc { 0 };
};
std::map<ModeKey, ModeVal> mode_transitions;

// Cumulative instruction time spent in each mode, approximated by counting
// mode transitions weighted by guest instruction counter. The emulator doesn't
// expose a precise per-mode cycle counter here; this is the PC count at each
// transition, so mode_stays[m] is "entries into mode m".
std::map<uint32_t, uint64_t> mode_entries;

} // namespace probe_state

extern "C" void probe_record_cp15(uint32_t pc, uint32_t cpopc, uint32_t crn,
	uint32_t crm, uint32_t cp, uint32_t dir, uint32_t value) {
	std::lock_guard<std::mutex> lock(probe_state::mu);
	probe_state::Cp15Key k { cpopc, crn, crm, cp, dir };
	auto& v = probe_state::cp15[k];
	if (v.count == 0) v.first_pc = pc;
	v.count++;
	v.last_value = value;
}

extern "C" void probe_record_swp(uint32_t pc, uint32_t is_byte) {
	std::lock_guard<std::mutex> lock(probe_state::mu);
	if (is_byte) probe_state::swp_byte_count++;
	else probe_state::swp_word_count++;
	probe_state::swp_pcs.insert(pc);
}

extern "C" void probe_record_mode(uint32_t pc, uint32_t old_mode, uint32_t new_mode) {
	std::lock_guard<std::mutex> lock(probe_state::mu);
	probe_state::ModeKey k { old_mode, new_mode };
	auto& v = probe_state::mode_transitions[k];
	if (v.count == 0) v.first_pc = pc;
	v.count++;
	probe_state::mode_entries[new_mode]++;
}

extern "C" void probe_record_rom_write(uint32_t, uint32_t, uint32_t) {
	// Not wired to any emitter yet; keeping the symbol for the header.
}

namespace {

const char* mode_name(uint32_t m) {
	switch (m) {
		case 0x10: return "USR";
		case 0x11: return "FIQ";
		case 0x12: return "IRQ";
		case 0x13: return "SVC";
		case 0x17: return "ABT";
		case 0x1B: return "UND";
		case 0x1F: return "SYS";
		default:   return "???";
	}
}

void dump_instrumentation(FILE* f) {
	std::lock_guard<std::mutex> lock(probe_state::mu);

	std::fprintf(f, "\n=====> CP15 register transfers: %zu unique tuples\n",
		probe_state::cp15.size());
	std::fprintf(f, "%-4s %-4s %-4s %-4s %-4s  %12s  %-10s  %-10s\n",
		"dir", "op1", "CRn", "CRm", "op2", "count", "first_pc", "last_val");
	for (const auto& [k, v] : probe_state::cp15) {
		std::fprintf(f, "%-4s %4u %4u %4u %4u  %12llu  0x%08X  0x%08X\n",
			k.dir ? "MRC" : "MCR",
			k.opc1, k.crn, k.crm, k.opc2,
			static_cast<unsigned long long>(v.count),
			v.first_pc, v.last_value);
	}

	std::fprintf(f, "\n=====> SWP instructions: %llu word, %llu byte, %zu unique sites\n",
		static_cast<unsigned long long>(probe_state::swp_word_count),
		static_cast<unsigned long long>(probe_state::swp_byte_count),
		probe_state::swp_pcs.size());
	std::fprintf(f, "unique PCs:");
	int printed = 0;
	for (auto pc : probe_state::swp_pcs) {
		if (printed++ % 6 == 0) std::fprintf(f, "\n  ");
		std::fprintf(f, " 0x%08X", pc);
	}
	std::fprintf(f, "\n");

	std::fprintf(f, "\n=====> ARM mode transitions: %zu unique edges\n",
		probe_state::mode_transitions.size());
	std::fprintf(f, "%-6s %-6s  %12s  %-10s\n", "from", "to", "count", "first_pc");
	for (const auto& [k, v] : probe_state::mode_transitions) {
		std::fprintf(f, "%-6s %-6s  %12llu  0x%08X\n",
			mode_name(k.old_mode), mode_name(k.new_mode),
			static_cast<unsigned long long>(v.count), v.first_pc);
	}

	std::fprintf(f, "\n=====> Entries into each mode\n");
	for (const auto& [m, n] : probe_state::mode_entries) {
		std::fprintf(f, "  %s: %llu\n",
			mode_name(m), static_cast<unsigned long long>(n));
	}
	std::fprintf(f, "<===== End of instrumentation summary\n");
}

} // namespace

// The probe intentionally links without the Toolkit/app layer. Provide the
// minimum stubs those layers would otherwise supply so the emulator core
// links cleanly. TNativePrimitives.cpp guards `gToolkit` with `if (gToolkit)`
// so a null pointer is sufficient; PrintStd is never reached through that
// guard but is declared below in case a future path exercises it.
class TToolkit;
TToolkit* gToolkit = nullptr;

namespace {

constexpr KUInt32 kDefaultBootSeconds = 30;
constexpr KUInt32 kDefaultRAMSize = 4 * 1024 * 1024; // MP2x00 bare RAM, 4 MiB.

void usage(const char* argv0) {
	std::fprintf(stderr,
		"usage: %s <rom.bin> [rex.bin|-] [wall-seconds]\n"
		"  rom.bin        path to the Newton ROM dump (8 MiB, big-endian as captured)\n"
		"  rex.bin        path to Einstein.rex, or - for the builtin (default: -)\n"
		"  wall-seconds   host wall-clock seconds to let the ROM run (default: %u)\n",
		argv0, kDefaultBootSeconds);
}

}

int main(int argc, char** argv) {
	if (argc < 2 || argc > 4) { usage(argv[0]); return 2; }

	const char* romPath = argv[1];
	const char* rexPath = (argc >= 3 && std::strcmp(argv[2], "-") != 0) ? argv[2] : nullptr;
	unsigned wallSeconds = (argc >= 4) ? static_cast<unsigned>(std::strtoul(argv[3], nullptr, 0)) : kDefaultBootSeconds;

	// We intentionally allocate the flash file under /tmp so repeated runs
	// don't accumulate state anywhere persistent; the probe is a read-only
	// experiment on ROM behavior.
	const char* flashPath = "/tmp/newton-probe.flash";
	(void) std::remove(flashPath);

	TStdOutLog log;
	TFlatROMImageWithREX rom(romPath, rexPath);
	if (rom.GetErrorCode() != TROMImage::kNoError) {
		std::fprintf(stderr, "probe: failed to load ROM (code %d)\n", rom.GetErrorCode());
		return 1;
	}

	TNullSoundManager sound(&log);
	TNullScreenManager screen(&log);
	// TNetworkManager is abstract; usermode NAT is the simplest concrete
	// backing in the tree and the probe never actually exercises the network.
	TUsermodeNetwork net(&log);

	TEmulator emu(&log, &rom, flashPath, &sound, &screen, &net, kDefaultRAMSize);

	// Run in a background thread and stop after wallSeconds of wall-clock
	// time. TEmulator::Run uses the full JIT and drives interrupts, which
	// is orders of magnitude faster than JIT::Step one-at-a-time.
	std::fprintf(stdout, "probe: booting ROM for %u seconds wall-clock...\n", wallSeconds);
	std::fflush(stdout);

	std::thread emuThread([&]() { emu.Run(); });

	// Simple wall-clock fence: let the ROM cook for the full duration so
	// any late mapping passes (task creation, app launch, NewtonScript heap
	// growth) get a chance to install descriptors. We dump the final table
	// state regardless of how far boot has progressed.
	bool mmuReported = false;
	auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(wallSeconds);
	while (std::chrono::steady_clock::now() < deadline) {
		std::this_thread::sleep_for(std::chrono::milliseconds(500));
		if (!mmuReported && emu.GetMemory()->IsMMUEnabled()) {
			std::fprintf(stdout, "probe: MMU came up at PC=0x%08X\n",
				static_cast<unsigned>(emu.GetProcessor()->GetRegister(TARMProcessor::kR15)));
			std::fflush(stdout);
			mmuReported = true;
		}
	}

	emu.Stop();
	emuThread.join();

	auto* mem = emu.GetMemory();
	auto* mmu = mem->GetMMU();

	std::fprintf(stdout, "\nprobe: PC=0x%08X  MMU enabled=%d  TTB=0x%08X  DACR=0x%08X\n",
		static_cast<unsigned>(emu.GetProcessor()->GetRegister(TARMProcessor::kR15)),
		static_cast<int>(mem->IsMMUEnabled()),
		static_cast<unsigned>(mem->GetTranslationTableBase()),
		static_cast<unsigned>(mem->GetDomainAccessControl()));

	mmu->FDump(stdout);
	dump_instrumentation(stdout);
	std::fflush(stdout);

	// Skip destructors: the interrupt-manager thread + network thread need
	// careful teardown that TEmulator's dtor doesn't always complete on the
	// probe's fast-exit path. We've captured everything we need to stdout;
	// dropping the kernel-owned resources is fine.
	std::_Exit(0);
}
