// baremetal/probe/trace.cpp
//
// Headless Einstein harness that boots a Newton 2.x ROM with function-entry
// tracing enabled. The JIT translator injects a log unit at every ROM address
// listed in the symbols file, producing a line per entry call in a format
// that matches the bare-metal hypervisor's `--features trace` output exactly,
// so the two traces can be diffed.

#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
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

// TNativePrimitives.cpp references gToolkit; provide the same null stub as
// probe.cpp since we link without the Toolkit layer.
class TToolkit;
TToolkit* gToolkit = nullptr;

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
