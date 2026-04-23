// ==============================
// File:            TTracer.cpp
// Project:         Einstein
// ==============================

#include "Emulator/TTracer.h"

#include <atomic>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <errno.h>
#include <memory>
#include <mutex>
#include <string>
#include <unordered_map>
#include <vector>

#include "Emulator/TARMProcessor.h"

// Storage owned by this translation unit. The name pool keeps std::string
// instances alive so the const char* returned from Lookup() stays valid.
namespace {

std::unordered_map<KUInt32, const char*> sSymbols;
// Vector of unique_ptr<string> — pointers stable across reallocation.
std::vector<std::unique_ptr<std::string>> sNamePool;

std::FILE* sOutFile = nullptr;
std::mutex sOutMu;
std::atomic<uint64_t> sSeq { 0 };

const char* ModeLabel(KUInt32 inMode)
{
	switch (inMode & 0x1F) {
		case 0x10: return "usr";
		case 0x11: return "fiq";
		case 0x12: return "irq";
		case 0x13: return "svc";
		case 0x17: return "abt";
		case 0x1B: return "und";
		case 0x1F: return "sys";
		default:   return "???";
	}
}

// Parse one "0xADDR\tNAME" line, tolerating trailing whitespace. Returns
// true on success and fills *outAddr / *outName (pointer into `line`).
bool ParseLine(char* line, KUInt32* outAddr, char** outName)
{
	// Strip trailing \r\n.
	size_t n = std::strlen(line);
	while (n > 0 && (line[n - 1] == '\n' || line[n - 1] == '\r' || line[n - 1] == ' ' || line[n - 1] == '\t')) {
		line[--n] = 0;
	}
	if (n == 0 || line[0] == '#') return false;

	// Address.
	char* end = nullptr;
	unsigned long addr = std::strtoul(line, &end, 0);
	if (end == line) return false;
	if (*end != '\t' && *end != ' ') return false;
	while (*end == '\t' || *end == ' ') end++;
	if (*end == 0) return false;

	*outAddr = static_cast<KUInt32>(addr);
	*outName = end;
	return true;
}

} // namespace

volatile Boolean TTracer::sEnabled = false;

void
TTracer::Enable(const char* symbolsPath, const char* outputPath)
{
	std::FILE* f = std::fopen(symbolsPath, "r");
	if (!f) {
		std::fprintf(stderr, "TTracer: can't open symbols file %s: %s\n",
			symbolsPath, std::strerror(errno));
		return;
	}
	char buf[512];
	size_t count = 0;
	while (std::fgets(buf, sizeof(buf), f)) {
		KUInt32 addr = 0;
		char* name = nullptr;
		if (!ParseLine(buf, &addr, &name)) continue;
		auto owned = std::make_unique<std::string>(name);
		const char* cstr = owned->c_str();
		sNamePool.push_back(std::move(owned));
		sSymbols[addr] = cstr;
		count++;
	}
	std::fclose(f);

	sOutFile = std::fopen(outputPath, "w");
	if (!sOutFile) {
		std::fprintf(stderr, "TTracer: can't open output file %s: %s\n",
			outputPath, std::strerror(errno));
		sSymbols.clear();
		sNamePool.clear();
		return;
	}
	// Line-buffered so partial output survives an abrupt crash in the
	// emulator thread.
	std::setvbuf(sOutFile, nullptr, _IOLBF, 0);

	std::fprintf(stderr, "TTracer: loaded %zu symbols from %s, logging to %s\n",
		count, symbolsPath, outputPath);
	sEnabled = true;
}

const char*
TTracer::Lookup(KUInt32 pc)
{
	auto it = sSymbols.find(pc);
	if (it == sSymbols.end()) return nullptr;
	return it->second;
}

void
TTracer::LogEntry(TARMProcessor* ioCPU, KUInt32 pc, const char* name)
{
	if (!sOutFile) return;
	const KUInt64 seq = sSeq.fetch_add(1, std::memory_order_relaxed) + 1;
	const KUInt32 mode = static_cast<KUInt32>(ioCPU->GetMode());
	const KUInt32 r0 = ioCPU->GetRegister(0);
	const KUInt32 r1 = ioCPU->GetRegister(1);
	const KUInt32 r2 = ioCPU->GetRegister(2);
	const KUInt32 r3 = ioCPU->GetRegister(3);

	std::lock_guard<std::mutex> lock(sOutMu);
	// %#08x / %#010x in C omits the "0x" prefix when the value is zero, so
	// hard-code the prefix to byte-match Rust's {:#010x} format.
	std::fprintf(sOutFile,
		"trace %5llu 0x%08x %s (%s) r0=0x%08x r1=0x%08x r2=0x%08x r3=0x%08x\n",
		static_cast<unsigned long long>(seq),
		static_cast<unsigned>(pc),
		name,
		ModeLabel(mode),
		static_cast<unsigned>(r0),
		static_cast<unsigned>(r1),
		static_cast<unsigned>(r2),
		static_cast<unsigned>(r3));
}

void
TTracer::Flush()
{
	if (sOutFile) {
		std::lock_guard<std::mutex> lock(sOutMu);
		std::fflush(sOutFile);
	}
}
