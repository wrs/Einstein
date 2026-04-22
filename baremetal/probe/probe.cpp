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
#include <errno.h>
#include <map>
#include <mutex>
#include <set>
#include <sys/stat.h>
#include <sys/types.h>
#include <thread>
#include <tuple>
#include <vector>

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

// Data aborts. Two views:
// - `dabort_by_pc`: counts per faulting PC (so a spin-in-one-place shows as
//   one big count instead of flooding the chronological list).
// - `dabort_first`: the first `kDabortCap` aborts in order, with full
//   context. This is the boot-time diagnostic — if we're trying to find
//   where Einstein diverges from another emulator, the first handful of
//   aborts are what matter.
struct DabortKey {
	uint32_t pc;
	uint32_t far;
	uint32_t fsr;
	uint32_t mode;
	bool operator<(const DabortKey& o) const {
		return std::tie(pc, far, fsr, mode) < std::tie(o.pc, o.far, o.fsr, o.mode);
	}
};
std::map<DabortKey, uint64_t> dabort_by_key;
struct DabortEvent {
	uint64_t seq;
	uint32_t pc;
	uint32_t far;
	uint32_t fsr;
	uint32_t mode;
};
constexpr size_t kDabortCap = 64;
std::vector<DabortEvent> dabort_first;
uint64_t dabort_total { 0 };

// Prefetch aborts (instruction fetch faults): same shape, fewer fields.
struct PabortKey {
	uint32_t pc;
	uint32_t ifsr;
	uint32_t mode;
	bool operator<(const PabortKey& o) const {
		return std::tie(pc, ifsr, mode) < std::tie(o.pc, o.ifsr, o.mode);
	}
};
std::map<PabortKey, uint64_t> pabort_by_key;
struct PabortEvent {
	uint64_t seq;
	uint32_t pc;
	uint32_t ifsr;
	uint32_t mode;
};
std::vector<PabortEvent> pabort_first;
uint64_t pabort_total { 0 };

// Endianness-patch classifier bitmap. One bit per 32-bit word in guest ROM
// space (0..0x01000000). Index = addr / 4; LSB-first within each byte.
// 16 MiB / 4 bytes / 8 bits = 524 288 bytes. Bit set ≡ the JIT actually
// executed an endianness-sensitive subword access instruction at this PC.
constexpr size_t kClassifyWordCount = (16u * 1024u * 1024u) / 4u; // 4 Mi words
constexpr size_t kClassifyBitmapBytes = kClassifyWordCount / 8u;  // 524 288
std::vector<uint8_t> ba_site_bitmap(kClassifyBitmapBytes, 0);
uint64_t ba_site_records { 0 };
// Per-kind tallies for the byte/halfword/swpb breakdown in summary.
uint64_t ba_site_by_kind[4] { 0, 0, 0, 0 };

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

extern "C" void probe_record_data_abort(uint32_t pc, uint32_t far,
	uint32_t fsr, uint32_t mode) {
	std::lock_guard<std::mutex> lock(probe_state::mu);
	probe_state::dabort_by_key[probe_state::DabortKey{pc, far, fsr, mode}]++;
	if (probe_state::dabort_first.size() < probe_state::kDabortCap) {
		probe_state::dabort_first.push_back(
			{probe_state::dabort_total, pc, far, fsr, mode});
	}
	probe_state::dabort_total++;
}

extern "C" void probe_record_prefetch_abort(uint32_t pc, uint32_t ifsr,
	uint32_t mode) {
	std::lock_guard<std::mutex> lock(probe_state::mu);
	probe_state::pabort_by_key[probe_state::PabortKey{pc, ifsr, mode}]++;
	if (probe_state::pabort_first.size() < probe_state::kDabortCap) {
		probe_state::pabort_first.push_back(
			{probe_state::pabort_total, pc, ifsr, mode});
	}
	probe_state::pabort_total++;
}

extern "C" void probe_record_ba_site(uint32_t pc, uint32_t kind) {
	if (pc >= 16u * 1024u * 1024u || (pc & 3u) != 0) return;
	const uint32_t word_idx = pc >> 2;
	std::lock_guard<std::mutex> lock(probe_state::mu);
	probe_state::ba_site_bitmap[word_idx >> 3] |= uint8_t(1u << (word_idx & 7u));
	probe_state::ba_site_records++;
	if (kind < 4) probe_state::ba_site_by_kind[kind]++;
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

	std::fprintf(f, "\n=====> Data aborts: %llu total, %zu unique (pc,far,fsr,mode) tuples\n",
		static_cast<unsigned long long>(probe_state::dabort_total),
		probe_state::dabort_by_key.size());
	std::fprintf(f, "first %zu aborts in order (seq : pc far fsr mode):\n",
		probe_state::dabort_first.size());
	for (const auto& e : probe_state::dabort_first) {
		std::fprintf(f, "  #%-5llu  PC=0x%08X  FAR=0x%08X  FSR=0x%08X  mode=%s\n",
			static_cast<unsigned long long>(e.seq),
			e.pc, e.far, e.fsr, mode_name(e.mode));
	}
	std::fprintf(f, "aggregated by tuple (count : pc far fsr mode):\n");
	for (const auto& [k, n] : probe_state::dabort_by_key) {
		std::fprintf(f, "  %8llu  PC=0x%08X  FAR=0x%08X  FSR=0x%08X  mode=%s\n",
			static_cast<unsigned long long>(n),
			k.pc, k.far, k.fsr, mode_name(k.mode));
	}

	std::fprintf(f, "\n=====> Prefetch aborts: %llu total, %zu unique (pc,ifsr,mode) tuples\n",
		static_cast<unsigned long long>(probe_state::pabort_total),
		probe_state::pabort_by_key.size());
	std::fprintf(f, "first %zu prefetch aborts in order:\n",
		probe_state::pabort_first.size());
	for (const auto& e : probe_state::pabort_first) {
		std::fprintf(f, "  #%-5llu  PC=0x%08X  IFSR=0x%08X  mode=%s\n",
			static_cast<unsigned long long>(e.seq),
			e.pc, e.ifsr, mode_name(e.mode));
	}
	std::fprintf(f, "aggregated by tuple:\n");
	for (const auto& [k, n] : probe_state::pabort_by_key) {
		std::fprintf(f, "  %8llu  PC=0x%08X  IFSR=0x%08X  mode=%s\n",
			static_cast<unsigned long long>(n),
			k.pc, k.ifsr, mode_name(k.mode));
	}

	std::fprintf(f, "<===== End of instrumentation summary\n");
}

// ==========================================================================
//  Classifier bitmap dump
// ==========================================================================

// FNV-1a-32. Streamed via a state parameter so we can hash ROM+REX in sequence.
uint32_t fnv1a_32(const void* data, size_t len, uint32_t state = 0x811C9DC5u) {
	const uint8_t* p = static_cast<const uint8_t*>(data);
	for (size_t i = 0; i < len; ++i) {
		state ^= p[i];
		state *= 0x01000193u;
	}
	return state;
}

bool read_file_bytes(const char* path, std::vector<uint8_t>& out) {
	std::FILE* f = std::fopen(path, "rb");
	if (!f) return false;
	std::fseek(f, 0, SEEK_END);
	long sz = std::ftell(f);
	std::fseek(f, 0, SEEK_SET);
	if (sz < 0) { std::fclose(f); return false; }
	out.resize(static_cast<size_t>(sz));
	size_t got = std::fread(out.data(), 1, out.size(), f);
	std::fclose(f);
	return got == out.size();
}

// mkdir -p for a single path. Returns true on success (created or existed).
bool mkdir_p(const char* path) {
	std::string buf(path);
	for (size_t i = 1; i < buf.size(); ++i) {
		if (buf[i] == '/') {
			buf[i] = 0;
			if (::mkdir(buf.c_str(), 0755) != 0 && errno != EEXIST) return false;
			buf[i] = '/';
		}
	}
	if (::mkdir(buf.c_str(), 0755) != 0 && errno != EEXIST) return false;
	return true;
}

uint64_t popcount_bytes(const std::vector<uint8_t>& b) {
	uint64_t n = 0;
	for (uint8_t x : b) n += __builtin_popcount(x);
	return n;
}

bool write_file(const char* path, const void* data, size_t len) {
	std::FILE* f = std::fopen(path, "wb");
	if (!f) return false;
	size_t got = std::fwrite(data, 1, len, f);
	std::fclose(f);
	return got == len;
}

// Dump the classifier bitmaps to baremetal/classify/<hash>/. Returns 0 on
// success, nonzero on failure (caller logs but continues shutdown).
int dump_classifier_bitmaps(const char* romPath, const char* rexPath) {
	std::vector<uint8_t> romBytes;
	if (!read_file_bytes(romPath, romBytes)) {
		std::fprintf(stderr, "classify: failed to read %s for hashing\n", romPath);
		return 1;
	}
	uint32_t hash = fnv1a_32(romBytes.data(), romBytes.size());
	if (rexPath) {
		std::vector<uint8_t> rexBytes;
		if (!read_file_bytes(rexPath, rexBytes)) {
			std::fprintf(stderr, "classify: failed to read %s for hashing\n", rexPath);
			return 1;
		}
		hash = fnv1a_32(rexBytes.data(), rexBytes.size(), hash);
	}

	char dir[256];
	std::snprintf(dir, sizeof(dir), "baremetal/classify/%08x", hash);
	if (!mkdir_p(dir)) {
		std::fprintf(stderr, "classify: mkdir_p(%s) failed: %s\n", dir, std::strerror(errno));
		return 1;
	}

	std::lock_guard<std::mutex> lock(probe_state::mu);
	uint64_t ba_bits = popcount_bytes(probe_state::ba_site_bitmap);

	char p1[320];
	std::snprintf(p1, sizeof(p1), "%s/byte-access.bitmap", dir);
	if (!write_file(p1, probe_state::ba_site_bitmap.data(), probe_state::ba_site_bitmap.size())) {
		std::fprintf(stderr, "classify: write %s failed\n", p1);
		return 1;
	}

	std::fprintf(stdout,
		"\n=====> Endianness-patch classifier bitmap written to %s/\n"
		"  rom+rex fnv1a32 = 0x%08x%s\n"
		"  byte-access.bitmap popcount=%llu  executions=%llu\n"
		"    (byte=%llu  halfword/signed/dword=%llu  swpb=%llu)\n",
		dir, hash, rexPath ? "" : " (rom only; no rex on cmdline)",
		static_cast<unsigned long long>(ba_bits),
		static_cast<unsigned long long>(probe_state::ba_site_records),
		static_cast<unsigned long long>(probe_state::ba_site_by_kind[0]),
		static_cast<unsigned long long>(probe_state::ba_site_by_kind[1]),
		static_cast<unsigned long long>(probe_state::ba_site_by_kind[3]));
	return 0;
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

	// PHASE B diagnostic: dump gGlobalsThatLiveAcrossReboot around +0x20
	// on Einstein post-boot. On baremetal this word reads 0x6db60000 at
	// RExScanner entry (poison in high halfword) which sends the scanner
	// to base 0xB1FC4C instead of 0x71FC4C. If Einstein has 0 there, we
	// know something in Einstein prevents the poison from landing.
	std::fprintf(stdout, "\n=====> gGlobalsThatLiveAcrossReboot (Einstein post-boot)\n");
	for (int off = 0x00; off <= 0x30; off += 4) {
		Boolean fault = false;
		KUInt32 v = mem->ReadP(0x0400d1c4u + off, fault);
		std::fprintf(stdout, "  PA 0x%08X (+%#04x) = 0x%08X%s\n",
			0x0400d1c4u + off, off, (unsigned) v, fault ? " (FAULT)" : "");
	}
	std::fprintf(stdout, "  PA 0x%08X (+0x2e8 REx[0]) = 0x%08X\n",
		0x0400d1c4u + 0x2e8, (unsigned) mem->ReadP(0x0400d1c4u + 0x2e8, *(Boolean*)alloca(sizeof(Boolean))));
	std::fprintf(stdout, "  PA 0x%08X (+0x2ec REx[1]) = 0x%08X\n",
		0x0400d1c4u + 0x2ec, (unsigned) mem->ReadP(0x0400d1c4u + 0x2ec, *(Boolean*)alloca(sizeof(Boolean))));

	mmu->FDump(stdout);
	dump_instrumentation(stdout);
	dump_classifier_bitmaps(romPath, rexPath);
	std::fflush(stdout);

	// Skip destructors: the interrupt-manager thread + network thread need
	// careful teardown that TEmulator's dtor doesn't always complete on the
	// probe's fast-exit path. We've captured everything we need to stdout;
	// dropping the kernel-owned resources is fine.
	std::_Exit(0);
}
