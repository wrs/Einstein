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
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <thread>

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
	std::fflush(stdout);

	// Skip destructors: the interrupt-manager thread + network thread need
	// careful teardown that TEmulator's dtor doesn't always complete on the
	// probe's fast-exit path. We've captured everything we need to stdout;
	// dropping the kernel-owned resources is fine.
	std::_Exit(0);
}
