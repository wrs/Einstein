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

// Heap allocator call log. We watch BL targets equal to NewPtr/NewHandle/
// NewBlock/NewIndirectBlock/operator-new entries and stream each call's
// args + LR to stdout. Sequence number lets us correlate the order with
// the baremetal hypervisor's identical probe.
namespace probe_state {
std::atomic<uint64_t> alloc_seq { 0 };
}
extern "C" TMemory* g_probe_mem = nullptr;

// Walk a chain of unconditional `b <imm24>` instructions starting at `pc`.
// Returns the final landing PC, or `pc` if the first instruction isn't a
// branch. Capped at 8 hops to avoid loops. Used by probe_record_call to
// see through the Newton ROM's BL → REx-JT → main-ROM-JT → fn chain.
static uint32_t walk_branch_chain(uint32_t pc) {
	if (!g_probe_mem) return pc;
	for (int i = 0; i < 8; ++i) {
		KUInt32 insn = 0;
		if (g_probe_mem->ReadAligned(pc & ~3u, insn)) break;
		// Unconditional B: cond=0xE, opcode 1010, L=0.
		// Format: 1110 1010 imm24
		if ((insn & 0xFF000000u) != 0xEA000000u) break;
		int32_t off = (int32_t) ((insn & 0x00FFFFFFu) << 8) >> 6; // sign-extend, ×4
		pc = (pc & ~3u) + 8u + (uint32_t) off;
	}
	return pc;
}

extern "C" void probe_record_call(uint32_t target_pc, uint32_t lr,
	uint32_t r0, uint32_t r1, uint32_t r2, uint32_t r3) {
	// Strip the +4 PC-prefetch offset, then walk B trampolines to the
	// real function entry.
	target_pc = walk_branch_chain(target_pc - 4);
	const char* name = nullptr;
	switch (target_pc) {
		case 0x00141538: name = "NewHandle";        break;
		case 0x00142b28: name = "NewPtr";           break;
		case 0x00311db8: name = "NewBlock";         break;
		case 0x003120bc: name = "NewIndirectBlock"; break;
		case 0x00318ee8: name = "__nw__FUi";        break;
		default: return;
	}
	uint64_t seq = probe_state::alloc_seq.fetch_add(1, std::memory_order_relaxed);
	std::lock_guard<std::mutex> lock(probe_state::mu);
	std::fprintf(stdout,
		"alloc %5llu %s pc=0x%08x r0=0x%08x r1=0x%08x r2=0x%08x r3=0x%08x lr=0x%08x\n",
		(unsigned long long) seq, name, target_pc, r0, r1, r2, r3, lr);
	std::fflush(stdout);
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

// ==========================================================================
//  task_dump — mirror of baremetal/src/task_dump.rs
//
// Walks gScheduler's per-priority run queues and gObjectTable's task
// entries, printing the same one-line-per-task format. Lets us diff
// the Phase B wedge state against Einstein at matching boot points.
// See baremetal/docs/STRUCTURES.md for the kernel struct layouts.
// ==========================================================================

constexpr KUInt32 kGScheduler    = 0x0c100fd0;
constexpr KUInt32 kGCurrentTask  = 0x0c101000;
constexpr KUInt32 kGWantSchedule = 0x0c100fd4;
constexpr KUInt32 kGHoldSchedule = 0x0c100fd8;
constexpr KUInt32 kGCurrGlobals  = 0x0c10105c;
constexpr KUInt32 kGObjectTable  = 0x0c10fc34;

constexpr KUInt32 kTSHighestPri  = 0x14;
constexpr KUInt32 kTSPriBitmap   = 0x18;
constexpr KUInt32 kTSQueuesBase  = 0x1c;
constexpr KUInt32 kTSLastRemoved = 0x11c;

constexpr KUInt32 kTTPriority    = 0x80;
constexpr KUInt32 kTTQItem       = 0x94;
constexpr KUInt32 kTTGlobals     = 0xa0;

constexpr KUInt32 kOTBucketsBase = 0x10;
constexpr KUInt32 kOTNumBuckets  = 128;
constexpr KUInt32 kObjTypeTask   = 3; // 717006-empirical (DDK enum is wrong)

bool td_read(TMemory* mem, KUInt32 va, KUInt32& out) {
	// ReadAligned returns true on fault, false on success.
	return !mem->ReadAligned(va, out);
}

bool td_find_task_name(TMemory* mem, KUInt32 globals_va, KUInt32& out_name) {
	if (globals_va == 0 || globals_va == ~0u) return false;
	for (int off = 4; off <= 128; off += 4) {
		KUInt32 v;
		if (!td_read(mem, globals_va - off, v)) continue;
		KUInt8 b0 = (v >> 24) & 0xff, b1 = (v >> 16) & 0xff;
		KUInt8 b2 = (v >> 8) & 0xff,  b3 = v & 0xff;
		auto printable = [](KUInt8 b) { return b >= 0x20 && b <= 0x7e; };
		if (!printable(b0) || !printable(b1) || !printable(b2) || !printable(b3)) continue;
		auto alpha = [](KUInt8 b) { return (b>='a'&&b<='z')||(b>='A'&&b<='Z'); };
		int alpha_n = alpha(b0)+alpha(b1)+alpha(b2)+alpha(b3);
		if (alpha_n >= 2) { out_name = v; return true; }
	}
	return false;
}

void td_dump_task_line(TMemory* mem, KUInt32 task_va, const char* state_label) {
	KUInt32 prio=0, globals=0, qnext=0, qprev=0;
	KUInt32 wq1n=0, wq1p=0, wq2n=0, wq2p=0;
	td_read(mem, task_va + kTTPriority, prio);
	td_read(mem, task_va + kTTGlobals,  globals);
	td_read(mem, task_va + kTTQItem,    qnext);
	td_read(mem, task_va + kTTQItem+4,  qprev);
	td_read(mem, task_va + 0xbc, wq1n);
	td_read(mem, task_va + 0xc0, wq1p);
	td_read(mem, task_va + 0xc8, wq2n);
	td_read(mem, task_va + 0xcc, wq2p);
	KUInt32 idword=0; td_read(mem, task_va, idword);
	KUInt32 nm=0;
	bool has_name = td_find_task_name(mem, globals, nm);
	if (has_name) {
		std::fprintf(stdout,
			"  [%s] task 0x%08x id=0x%x prio=%u name='%c%c%c%c' globals=0x%08x q=0x%08x/0x%08x wq1=0x%08x/0x%08x wq2=0x%08x/0x%08x\n",
			state_label, task_va, idword, prio,
			(int)((nm>>24)&0xff), (int)((nm>>16)&0xff),
			(int)((nm>>8)&0xff),  (int)(nm&0xff),
			globals, qnext, qprev, wq1n, wq1p, wq2n, wq2p);
	} else {
		std::fprintf(stdout,
			"  [%s] task 0x%08x id=0x%x prio=%u name=? globals=0x%08x q=0x%08x/0x%08x wq1=0x%08x/0x%08x wq2=0x%08x/0x%08x\n",
			state_label, task_va, idword, prio,
			globals, qnext, qprev, wq1n, wq1p, wq2n, wq2p);
	}
}

const char* td_classify(TMemory* mem, KUInt32 task_va, KUInt32 current) {
	if (task_va == current) return "RUN";
	KUInt32 qn=0, qp=0, w1=0, w2=0;
	td_read(mem, task_va + kTTQItem,   qn);
	td_read(mem, task_va + kTTQItem+4, qp);
	td_read(mem, task_va + 0xbc, w1);
	td_read(mem, task_va + 0xc8, w2);
	if (w1 != 0 || w2 != 0) return "WAIT";
	if (qn != 0 || qp != 0) return "RDY";
	return "BLK";
}

void task_dump(TMemory* mem, const char* tag) {
	KUInt32 sched=0, curr=0, want=0, hold=0, glob=0;
	td_read(mem, kGScheduler,    sched);
	td_read(mem, kGCurrentTask,  curr);
	td_read(mem, kGWantSchedule, want);
	td_read(mem, kGHoldSchedule, hold);
	td_read(mem, kGCurrGlobals,  glob);
	if (sched == 0) {
		std::fprintf(stdout, "task_dump[%s]: gScheduler unset\n", tag);
		return;
	}
	KUInt32 highest=0, bitmap=0, lastrem=0;
	td_read(mem, sched + kTSHighestPri,  highest);
	td_read(mem, sched + kTSPriBitmap,   bitmap);
	td_read(mem, sched + kTSLastRemoved, lastrem);
	std::fprintf(stdout,
		"task_dump[%s]: gSched=0x%x curr=0x%x highest_pri=%u bitmap=0x%x last_rem=0x%x want=%u hold=%u curr_glob=0x%x\n",
		tag, sched, curr, highest, bitmap, lastrem, want, hold, glob);
	if (curr) {
		std::fprintf(stdout, "  current:\n");
		td_dump_task_line(mem, curr, "RUN");
	}
	for (int p = 0; p < 32; ++p) {
		if (((bitmap >> p) & 1) == 0) continue;
		KUInt32 qva = sched + kTSQueuesBase + p * 8;
		std::fprintf(stdout, "  prio %d queue@0x%x:\n", p, qva);
		KUInt32 head=0; td_read(mem, qva, head);
		KUInt32 cur = head;
		int steps = 0;
		while (cur != 0 && steps < 32) {
			td_dump_task_line(mem, cur, td_classify(mem, cur, curr));
			KUInt32 next=0;
			td_read(mem, cur + kTTQItem, next);
			if (next == cur) break;
			cur = next;
			++steps;
		}
	}
	std::fprintf(stdout, "  all tasks (object table walk):\n");
	int total = 0, tasks = 0;
	int by_type[16] = {0};
	for (KUInt32 b = 0; b < kOTNumBuckets; ++b) {
		KUInt32 head_va = kGObjectTable + kOTBucketsBase + b * 4;
		KUInt32 node=0; td_read(mem, head_va, node);
		int steps = 0;
		while (node != 0 && steps < 128) {
			++total;
			KUInt32 id=0; td_read(mem, node, id);
			by_type[id & 0xf]++;
			if ((id & 0xf) == kObjTypeTask) {
				++tasks;
				td_dump_task_line(mem, node, td_classify(mem, node, curr));
			}
			KUInt32 nxt=0; td_read(mem, node + 4, nxt);
			node = nxt;
			++steps;
		}
	}
	std::fprintf(stdout,
		"  object table: %d tasks (of %d kernel objects); types[0..15]=", tasks, total);
	for (int i = 0; i < 16; ++i)
		std::fprintf(stdout, "%d ", by_type[i]);
	std::fprintf(stdout, " (3=Task, 8=Mon, 9=Phys are confirmed)\n");
	std::fflush(stdout);
}

// Dump the SWIBoot context-save area (task+0x10..+0x54) of `task_va`,
// plus 16 words of the user stack at saved sp_usr. Used to cross-check
// our baremetal save area against Einstein's at the same task slot.
void task_dump_save_area(TMemory* mem, KUInt32 task_va) {
	const char* names[17] = {
		"r0 ","r1 ","r2 ","r3 ",
		"r4 ","r5 ","r6 ","r7 ",
		"r8 ","r9 ","sl ","fp ",
		"ip ",
		"sp_usr","lr_usr","PC ","SPSR",
	};
	KUInt32 idword=0; td_read(mem, task_va, idword);
	KUInt32 globals=0; td_read(mem, task_va + kTTGlobals, globals);
	KUInt32 nm=0; td_find_task_name(mem, globals, nm);
	std::fprintf(stdout, "  save-area task=0x%08x id=0x%x name='%c%c%c%c':\n",
		task_va, idword,
		(int)((nm>>24)&0xff), (int)((nm>>16)&0xff),
		(int)((nm>>8)&0xff),  (int)(nm&0xff));
	for (int i = 0; i < 17; ++i) {
		KUInt32 off = 0x10 + i*4;
		KUInt32 v=0; td_read(mem, task_va + off, v);
		std::fprintf(stdout, "    +0x%02x %-6s = 0x%08x\n", off, names[i], v);
	}
	KUInt32 sp_usr=0; td_read(mem, task_va + 0x44, sp_usr);
	if (sp_usr != 0 && sp_usr != ~0u) {
		std::fprintf(stdout, "    user stack window @ sp_usr=0x%08x (+/-0x80):\n", sp_usr);
		for (int i = 0; i < 32; ++i) {
			int off = (i - 8) * 4;
			KUInt32 va = sp_usr + (KUInt32)off;
			KUInt32 v=0; td_read(mem, va, v);
			const char* mark = (off == 0) ? " <- sp" : "";
			std::fprintf(stdout, "      [%+4d] 0x%08x = 0x%08x%s\n", off, va, v, mark);
		}
		// Stage-1 walk: print PA backing sp_usr so we can compare across
		// implementations. Einstein has TranslateR(va, pa).
		KUInt32 pa = 0;
		bool fault = mem->TranslateR(sp_usr, pa);
		std::fprintf(stdout, "    sp_usr stage-1 walk: VA 0x%08x -> PA 0x%08x  (fault=%d)\n",
			sp_usr, pa, fault ? 1 : 0);
	}
	std::fflush(stdout);
}

// Walk Einstein's stage-1 page tables (rooted at TTBR0) and report
// every PA that's mapped by 2+ distinct VAs. Cross-check for our
// hypervisor's heap-aliasing bug — see baremetal/INVESTIGATION.md
// "alias-onset detector" finding. If Einstein shows 0 duplicates
// at the equivalent boot offset, the divergence is on our side.
//
// Walks all 4096 L1 entries; for each coarse, walks 256 L2 entries.
// Builds a PA -> [VAs] map and prints any PA with multiple mappings.
void duplicate_pa_scan(TMemory* mem) {
	KUInt32 ttbr = mem->GetTranslationTableBase() & 0xFFFF'C000u;
	std::map<KUInt32, std::vector<KUInt32>> pa_to_vas;
	for (KUInt32 l1_idx = 0; l1_idx < 4096; ++l1_idx) {
		Boolean fault = false;
		KUInt32 l1 = mem->ReadP(ttbr + l1_idx * 4, fault);
		if (fault) continue;
		KUInt32 typ = l1 & 3;
		if (typ == 2) {
			// Section: 1 MiB at l1[31:20].
			KUInt32 pa = l1 & 0xFFF00000u;
			KUInt32 va = l1_idx << 20;
			pa_to_vas[pa].push_back(va);
		} else if (typ == 1) {
			// Coarse: walk 256 L2 entries.
			KUInt32 l2_base = l1 & 0xFFFFFC00u;
			for (KUInt32 l2_idx = 0; l2_idx < 256; ++l2_idx) {
				Boolean f2 = false;
				KUInt32 l2 = mem->ReadP(l2_base + l2_idx * 4, f2);
				if (f2) continue;
				KUInt32 t2 = l2 & 3;
				KUInt32 pa = 0;
				if (t2 == 1) {
					// Large page (64 KiB).
					pa = l2 & 0xFFFF0000u;
				} else if (t2 == 2 || t2 == 3) {
					// Small page (4 KiB).
					pa = l2 & 0xFFFFF000u;
				} else {
					continue;
				}
				KUInt32 va = (l1_idx << 20) | (l2_idx << 12);
				pa_to_vas[pa].push_back(va);
			}
		}
	}
	// Counts split between ROM (PA < 0x01000000), RAM
	// (0x04000000 <= PA < 0x04400000), and "other" (the rest).
	// The kernel's post-ship patch table at VA 0x01a00000..0x01c20000
	// aliases ROM PAs ~33 times by design (see docs/NEWTON_INTERNALS.md);
	// those aren't bugs. Our heap-aliasing wedge is in RAM.
	int total_pas = 0;
	int rom_dup_pas = 0, rom_dup_vas = 0;
	int ram_dup_pas = 0, ram_dup_vas = 0;
	for (const auto& [pa, vas] : pa_to_vas) {
		++total_pas;
		if (vas.size() <= 1) continue;
		if (pa < 0x01000000u) {
			++rom_dup_pas;
			rom_dup_vas += vas.size();
		} else if (pa >= 0x04000000u && pa < 0x04400000u) {
			++ram_dup_pas;
			ram_dup_vas += vas.size();
		}
	}
	std::fprintf(stdout,
		"dup-pa-scan: TTBR=0x%08x  total_unique_PAs=%d  ROM_dup_PAs=%d (alias VAs=%d)  RAM_dup_PAs=%d (alias VAs=%d)\n",
		ttbr, total_pas, rom_dup_pas, rom_dup_vas, ram_dup_pas, ram_dup_vas);
	if (ram_dup_pas == 0) {
		std::fprintf(stdout, "  (no RAM duplicate PA mappings — only ROM jump-table aliases)\n");
		std::fflush(stdout);
		return;
	}
	std::fprintf(stdout, "  RAM duplicates (L2 entry value + ARMv4 subpage AP[3:0]):\n");
	int rows_printed = 0;
	for (const auto& [pa, vas] : pa_to_vas) {
		if (vas.size() <= 1) continue;
		if (pa < 0x04000000u || pa >= 0x04400000u) continue; // RAM only
		std::fprintf(stdout, "    PA=0x%08x mapped by %zu VAs:\n", pa, vas.size());
		for (KUInt32 va : vas) {
			// Re-read the L2 entry for this VA so we can dump its raw
			// bits and decode the per-subpage AP fields.
			KUInt32 l1_idx = va >> 20;
			Boolean f1 = false;
			KUInt32 l1 = mem->ReadP(ttbr + l1_idx * 4, f1);
			KUInt32 l2_val = 0;
			KUInt32 l2_addr = 0;
			if (!f1 && (l1 & 3) == 1) {
				KUInt32 l2_base = l1 & 0xFFFFFC00u;
				KUInt32 l2_idx = (va >> 12) & 0xFF;
				l2_addr = l2_base + l2_idx * 4;
				Boolean f2 = false;
				l2_val = mem->ReadP(l2_addr, f2);
				if (f2) l2_val = 0xDEADBEEF;
			}
			// ARMv4 small-page L2: bits[31:12]=PA, bits[11:10]=AP3,
			//   bits[9:8]=AP2, bits[7:6]=AP1, bits[5:4]=AP0,
			//   bits[3:2]=CB, bits[1:0]=10.
			// AP value: 00 = no access, 01 = privileged R/W (user
			// FAULT on access), 10 = priv R/W + user RO, 11 = full R/W.
			KUInt32 ap0 = (l2_val >> 4) & 3;
			KUInt32 ap1 = (l2_val >> 6) & 3;
			KUInt32 ap2 = (l2_val >> 8) & 3;
			KUInt32 ap3 = (l2_val >> 10) & 3;
			auto ap_label = [](KUInt32 ap) {
				switch (ap) {
					case 0: return "00=NA";
					case 1: return "01=PR/W";
					case 2: return "10=PR/W+UR";
					case 3: return "11=R/W";
					default: return "??";
				}
			};
			std::fprintf(stdout,
				"      VA=0x%08x L2@0x%08x=0x%08x  AP[3..0]=[%s,%s,%s,%s]\n",
				va, l2_addr, l2_val,
				ap_label(ap3), ap_label(ap2), ap_label(ap1), ap_label(ap0));
		}
		++rows_printed;
		if (rows_printed >= 16) {
			std::fprintf(stdout, "    ...(stopped after 16 RAM entries)\n");
			break;
		}
	}
	std::fflush(stdout);
}

// Dump 128 bytes of a heap header at `heap_va` (which equals base+16
// for a Newton heap; the caller passes 0x0ca6b010 for the legitimate
// RelocHeap created by NewHeap call #3, base=0x0ca6b000). Cross-check
// for baremetal/INVESTIGATION.md's "current stop": our hypervisor sees
// heap[+0]=0x002dd804 + further header corruption; this lets us see
// what Einstein has at the same VA at the equivalent boot offset.
//
// Reads via ReadAligned (i.e. through the kernel's stage-1 view —
// same VA the kernel itself would dereference). On a Newton heap the
// invariants are heap[+0]=heap-16, heap[+8]=0x736b6961 ('skia' magic,
// little-endian).
void heap_header_dump(TMemory* mem, KUInt32 heap_va) {
	KUInt32 magic_at_8 = 0;
	td_read(mem, heap_va + 8, magic_at_8);
	std::fprintf(stdout,
		"heap-dump @ VA=0x%08x  magic[+8]=0x%08x  %s\n",
		heap_va, magic_at_8,
		magic_at_8 == 0x736b6961u ? "(skia magic OK)" :
		magic_at_8 == 0u          ? "(zero - heap not yet created)" :
		                            "(*** unexpected magic ***)");
	for (int off = 0x00; off < 0x80; off += 16) {
		KUInt32 w[4] = {0,0,0,0};
		td_read(mem, heap_va + off + 0,  w[0]);
		td_read(mem, heap_va + off + 4,  w[1]);
		td_read(mem, heap_va + off + 8,  w[2]);
		td_read(mem, heap_va + off + 12, w[3]);
		std::fprintf(stdout,
			"  heap[+0x%02x]  0x%08x 0x%08x 0x%08x 0x%08x\n",
			off, w[0], w[1], w[2], w[3]);
	}
	// Also resolve VA -> PA so we can see whether Einstein's heap
	// hops backing pages the way ours does (PA 0x0401f000 ->
	// 0x04032000 across boot).
	KUInt32 pa = 0;
	bool fault = mem->TranslateR(heap_va, pa);
	std::fprintf(stdout,
		"  heap-dump VA->PA: VA 0x%08x -> PA 0x%08x  (fault=%d)\n",
		heap_va, pa, fault ? 1 : 0);
	std::fflush(stdout);
}

// Heap allocation enumerator. NewHeap initialises every Newton heap with
// the magic bytes "aiks" (ASCII, little-endian as 0x736b6961) at heap+24
// (ROM 0x00310e80: `str r1, [r7, #8]` where r1 = "aiks" and r7 = base+16).
// Scan all of guest RAM (VA 0x0c000000..0x0c800000) for that magic, then
// dump each detected heap's header so we can compare which NewHeap calls
// Einstein has made by tag tag T against the baremetal trace.
constexpr KUInt32 kAiksMagic = 0x736b6961u;
void heap_alloc_enum(TMemory* mem, const char* tag) {
	std::fprintf(stdout, "heap-alloc-enum[%s]: starting scan\n", tag);
	std::fflush(stdout);
	// Scan word-aligned VAs in 0x0c000000..0x0c800000 (8 MiB max, the Newton
	// kernel + apps RAM space). Skip ATTR/IO/MEM PCMCIA windows (they're
	// outside this range), and skip ROM (low addresses already filtered).
	int found = 0;
	KUInt32 va = 0x0c000000u;
	uint64_t iter = 0;
	const uint64_t kMaxIter = 5'000'000ULL; // bound runtime
	while (va < 0x0d000000u && iter < kMaxIter) {
		++iter;
		KUInt32 word = 0;
		if (!td_read(mem, va, word)) {
			// Unmapped — skip whole 4 KiB page.
			va = (va + 0x1000u) & ~0xFFFu;
			continue;
		}
		if (word != kAiksMagic) { va += 4; continue; }
		// Candidate hit. Read the rest of the header to validate: heap[+0]
		// (= +0 from base, which is va-24) holds heap_base in NewHeap (a
		// self-pointer, but with the +16 adjustment, NewHeap stores r5
		// (which is base) at base+16 and r7+0 = base+16 = self+0).
		// Easier validation: the +0xcc heap byte at NewHeap 0x00310e70
		// (`mov r0, #204`) is written to heap[+4], so check heap[+4]=204.
		KUInt32 base16_field = 0;
		if (!td_read(mem, va + (-16 - 12) + 4, base16_field)) continue;
		// Actually NewHeap layout post-init:
		//   base[+4]    = 0xcc (204)            (str r0, [r7, #-12] @ 0x310e70)
		//   base[+16]   = base                  (str r5, [r7, #0])
		//   base[+24]   = 'aiks'                (str r1, [r7, #8])
		//   base[+40]   = chunk_size            (str r4, [r7, #28])  ← actually arg1
		//   base[+72]   = chunk_size            (str r6, [r7, #56])
		// The magic is at base+24, so base = va-24.
		KUInt32 base = va - 24;
		KUInt32 b4 = 0;   td_read(mem, base + 4,  b4);
		KUInt32 b16 = 0;  td_read(mem, base + 16, b16);
		KUInt32 b56 = 0;  td_read(mem, base + 56, b56);
		KUInt32 b72 = 0;  td_read(mem, base + 72, b72);
		if (b16 != base) { va += 4; continue; } // Self-pointer check.
		++found;
		std::fprintf(stdout,
			"  heap[%2d] base=0x%08x  byte+4=0x%08x  arg1@+56=0x%08x  chunk_size@+72=0x%08x\n",
			found - 1, base, b4, b56, b72);
		// Walk the block list within this heap. Header/state per block:
		//   allocated: block[+0]=slack-byte<<8 (low byte 0); block[+4]=size
		//   free:      block[+0]=size; block[+4]=next-free-pointer-ish
		// Walk from arena+204 (the first-block VA per NewHeap layout)
		// advancing by `size` chosen as block[+4] if nonzero else block[+0].
		// Cap at 256 blocks per heap to bound the dump.
		KUInt32 walk = base + 204;
		KUInt32 heap_end = base + b56;     // arena+arg1 = end-of-arena
		int n = 0;
		while (walk < heap_end && n < 256) {
			KUInt32 b0 = 0, b4w = 0, b8 = 0, b12 = 0;
			td_read(mem, walk + 0,  b0);
			td_read(mem, walk + 4,  b4w);
			td_read(mem, walk + 8,  b8);
			td_read(mem, walk + 12, b12);
			KUInt32 sz = (b4w != 0) ? b4w : b0;
			const char* state = ((b0 & 0xff) == 0) ? "ALLOC" : "FREE ";
			if (sz < 16 || sz > (heap_end - walk)) {
				std::fprintf(stdout,
					"    block[%3d] @0x%08x SIZE-OOR sz=0x%08x b0=0x%08x b4=0x%08x  (stop walk)\n",
					n, walk, sz, b0, b4w);
				break;
			}
			std::fprintf(stdout,
				"    block[%3d] @0x%08x %s sz=0x%08x b0=0x%08x b8=0x%08x task=0x%08x\n",
				n, walk, state, sz, b0, b8, b12);
			walk += sz;
			++n;
		}
		// Don't re-scan inside this heap.
		va = base + b56;
	}
	std::fprintf(stdout, "heap-alloc-enum[%s]: found %d heaps with 'aiks' magic\n",
		tag, found);
	std::fflush(stdout);
}

// Locale-init cross-check vs baremetal hypervisor. The baremetal boot
// wedges in ROMCacheLocaleAttributes at PC=0xeccac because *gLocaleCache
// (VA 0x0c106198) is still 0 — InitInternationalUtils hasn't run when
// bootinitnsglobals NS code calls FSetLocale. Dump the same memory on
// Einstein to see (a) whether *gLocaleCache becomes non-zero before
// FSetLocale would run, and (b) what the surrounding state looks like.
void locale_dump(TMemory* mem, const char* tag) {
	KUInt32 glc = 0;
	td_read(mem, 0x0c106198u, glc);
	std::fprintf(stdout, "locale-dump[%s]: *gLocaleCache(0x0c106198)=0x%08x\n",
		tag, glc);
	// Dump 11 cache slots (gLocaleCache spans 0x0c106198..0x0c1061c4
	// = 11 × 4 bytes) plus 2 derefs for slot 0/1 to compare against
	// the baremetal probe data:
	for (int i = 0; i < 11; ++i) {
		KUInt32 va = 0x0c106198u + i * 4;
		KUInt32 v = 0;
		td_read(mem, va, v);
		KUInt32 deref = 0;
		if (v != 0) td_read(mem, v, deref);
		std::fprintf(stdout,
			"  gLocaleCache[%2d] @0x%08x = 0x%08x  *deref = 0x%08x\n",
			i, va, v, deref);
	}
	// gSpaceStr / dictionary globals.
	for (int i = 0; i < 6; ++i) {
		KUInt32 va = 0x0c100f84u + i * 4;
		KUInt32 v = 0;
		td_read(mem, va, v);
		const char* name = "";
		switch (va) {
			case 0x0c100f84: name = " gSpaceStr"; break;
			case 0x0c100f88: name = " gZeroStr"; break;
			case 0x0c100f8c: name = " gTimeLexDictionary"; break;
			case 0x0c100f90: name = " gDateLexDictionary"; break;
			case 0x0c100f94: name = " gPhoneLexDictionary"; break;
			case 0x0c100f98: name = " gNumberLexDictionary"; break;
		}
		std::fprintf(stdout, "  @0x%08x = 0x%08x%s\n", va, v, name);
	}
	std::fflush(stdout);
}

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
	// Make memory accessible to probe_record_call's branch-chain walker.
	extern TMemory* g_probe_mem;
	g_probe_mem = emu.GetMemory();

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
	auto next_dump = std::chrono::steady_clock::now() + std::chrono::seconds(2);
	int dump_n = 0;
	while (std::chrono::steady_clock::now() < deadline) {
		std::this_thread::sleep_for(std::chrono::milliseconds(500));
		auto now = std::chrono::steady_clock::now();
		if (!mmuReported && emu.GetMemory()->IsMMUEnabled()) {
			std::fprintf(stdout, "probe: MMU came up at PC=0x%08X\n",
				static_cast<unsigned>(emu.GetProcessor()->GetRegister(TARMProcessor::kR15)));
			std::fflush(stdout);
			mmuReported = true;
		}
		if (mmuReported && now >= next_dump) {
			char tag[32];
			std::snprintf(tag, sizeof(tag), "t=%ds", dump_n * 2 + 2);
			// Locale-cache check FIRST so it survives any later crash.
			locale_dump(emu.GetMemory(), tag);
			// Heap-allocation enumeration — what NewHeap calls have run.
			heap_alloc_enum(emu.GetMemory(), tag);
			task_dump(emu.GetMemory(), tag);
			// Cross-check the cdsv-vs-pckm slot: in our hypervisor this
			// task struct currently faults with FAR=0x6e657774 ("newt")
			// on resume because user RAM at sp_usr+8 holds the literal
			// ASCII fourcc. Dump Einstein's view of the same struct so
			// we can see (a) whether the saved sp_usr matches and (b)
			// what's in the user stack at that VA.
			task_dump_save_area(emu.GetMemory(), 0x0c118dd8u);
			// Cross-check the RelocHeap header at the legitimate
			// VA=0x0ca6b010 (NewHeap #3, base=0x0ca6b000, size=2 MiB).
			// On baremetal this header gets corrupted partway through
			// boot — see baremetal/INVESTIGATION.md current stop.
			heap_header_dump(emu.GetMemory(), 0x0ca6b010u);
			// Duplicate-PA scan: enumerate every PA mapped by 2+ VAs.
			// On baremetal we see PA 0x0401f000 shared between heap #1,
			// heap #3, and newt's stack region. If Einstein shows 0
			// duplicates, divergence is on our side.
			duplicate_pa_scan(emu.GetMemory());
			next_dump += std::chrono::seconds(2);
			++dump_n;
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

	std::fprintf(stdout, "\n=====> task_dump (final state)\n");
	task_dump(mem, "final");

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
