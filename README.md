# gbtoexe

Turns a Game Boy cartridge into a native Windows executable.

This is a **static recompiler**, not a packaged emulator. The cartridge's SM83
machine code is translated to x86-64 ahead of time and written into the output
`.exe`. When you run that file, the game's own instructions execute on your
processor directly — there is no fetch-decode-execute loop anywhere in the
output, and nothing in it interprets a Game Boy instruction.

```
$ gbtoexe game.gb
GAME TITLE  MBC3+battery  64 ROM banks, 32 KiB cartridge RAM
recompiled 2914 blocks to 749.5 KiB of x86-64 in 7.02ms
wrote game.exe (2.2 MiB)
```

The result needs nothing installed: no runtime, no DLLs, no emulator, no ROM
file beside it. Converting needs nothing either — `gbtoexe` contains its own
x86-64 assembler and never shells out to a compiler or a linker.

## What is and is not translated

The **CPU** is translated. The **hardware is not**, because it cannot be: the
picture processor, the sound channels, the timers and the cartridge mapper are
silicon, not code, so the output carries a small native model of them (about
2500 lines) that the translated code calls into for memory access. That is the
honest shape of any static recompiler — what changes is that the game's logic is
real machine code rather than something being interpreted.

## Building

The converter runs on Linux, macOS or Windows. Producing a Windows `.exe` needs
a cross-linker and the Rust target:

```sh
rustup target add x86_64-pc-windows-gnu
pacman -S mingw-w64-gcc          # Arch; see the Makefile for other systems
make dist
```

`dist/` then holds `gbtoexe` and the runtime stub it stamps games onto.

## Using it

```sh
gbtoexe game.gb                     # -> game.exe
gbtoexe game.gb -o ~/games/Game.exe --scale 5 --palette pocket
gbtoexe game.gbc --model cgb --volume 40
gbtoexe game.gb --verify            # convert, then run it briefly to check
```

`gbtoexe --help` lists everything. The options worth knowing:

| Option | What it does |
| --- | --- |
| `--model auto\|dmg\|cgb` | Which machine to be. Default follows the cartridge header. |
| `--palette <name>` | `grey`, `dmg`, `pocket`, `light`, or four `RRGGBB` values. Monochrome games only. |
| `--scale 1-8` | Starting window size. The window is resizable either way. |
| `--no-audio`, `--volume 0-100` | Sound. |
| `--aot fixed\|full\|off` | How much to translate before the game runs. See below. |
| `--verify [frames]` | Run the converted game for a moment and report what it did. |

### Playing

Arrow keys move. `Z`/`X` (or `A`/`S`) are B and A, `Enter` is Start, `Backspace`
or `Shift` is Select. `Tab` fast-forwards, `Esc` quits. Games with a battery
write a `.sav` file next to the executable.

## How it works

**Discovery.** Recursive descent from the cartridge's entry point, the eight
restart vectors and the five interrupt vectors, following every jump and call
whose destination is written into the instruction.

**Translation.** Each basic block becomes a run of x86-64. The Game Boy's
registers live in a state structure addressed through `rbp`; flags are computed
explicitly, since the two processors disagree about what a carry means. Where
control flow is known, blocks are linked with a direct `jmp`, so a chain of them
runs without ever returning to the runtime.

**What cannot be found ahead of time.** Computed jumps (`JP HL`, jump tables),
bank-switched code — the same address holds different code depending on what the
mapper has selected — and routines the game copies into RAM and runs from there,
which are not in the file at all. Those reach a dispatcher that consults an
inline cache built into the translated code itself; a hit jumps straight to the
right block with no round trip. A miss translates the block on the spot and
caches it. **Nothing is ever interpreted**, and self-modifying code is handled:
writes to a page that some block was built from throw that translation away.

**Timing.** Cycles the game owes the hardware accumulate in a counter and are
handed over at every point the hardware could notice — any memory access, and
the end of every block. Instruction timings match the hardware exactly, including
the extra cycle a taken branch costs, and there are tests that assert it.

### `--aot` and why `fixed` is the default

`fixed` translates the always-mapped bank, where every address means one thing.
`full` also tries each banked entry point against every bank it might belong to.

Measured on a 1 MB cartridge:

| Mode | Blocks | Output size | Speed |
| --- | --- | --- | --- |
| `fixed` | 2 914 | 2.2 MiB | 14.9× real time |
| `full` | 68 948 | 26.1 MiB | 10.1× real time |

Both produce byte-identical frames. `full` is bigger *and* slower, because most
of what it translates is cartridge data that merely decodes as plausible code,
and the bulk hurts locality. Since translating a block takes a few microseconds,
leaving the rest to first use costs nothing observable. `full` is kept for the
case where you want no first-touch translation at all.

## Testing

```sh
make test
```

56 tests. The ones that matter:

- **Differential testing** (`crates/gb-recomp/tests/differential.rs`) runs every
  non-control-flow opcode and all 256 `CB`-prefixed opcodes against a reference
  interpreter written independently from the documentation, from randomised
  starting states, comparing registers, flags and memory byte for byte.
- **Execution tests** (`crates/gb-recomp/tests/execution.rs`) cover control flow,
  cycle-exact timing, RAM-resident code and self-modifying code.
- **Discovery tests** cover the code/data heuristics, including that a
  speculative entry point landing in data is rejected.

The recompiler emits Windows-ABI code, which x86-64 Linux can also execute, so
the whole pipeline is testable natively; converted games have also been checked
under Wine, where they produce frames identical to a native run.

## Layout

| Crate | What it is |
| --- | --- |
| `gb-recomp` | The recompiler: decoder, discovery, x86-64 encoder, code cache, host loop. |
| `gb-hw` | The hardware: picture processor, sound, timers, joypad, cartridge mappers. No CPU. |
| `gb-runtime` | The stub games are stamped onto. Win32 window and sound, no dependencies. |
| `gb-payload` | The container format that staples a game onto the stub. |
| `gbtoexe` | The command-line converter. |

## Limits

- Output is x86-64 Windows. The recompiler is not portable to Arm as it stands —
  the backend emits x86-64 directly.
- No Super Game Boy borders, no link cable, no infrared, no boot-ROM animation
  (the machine starts in its post-boot state, so no boot ROM is needed or
  included).
- Mappers: none, MBC1, MBC2, MBC3 (with clock), MBC5. Not MBC6/7, MMM01, or the
  unlicensed mappers.
- **Bring your own cartridge.** This repository contains no ROMs and the tool
  downloads nothing.

## Licence

MIT or Apache-2.0, at your option.
