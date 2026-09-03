//! `gbtoexe` — turn a Game Boy cartridge into a native Windows executable.
//!
//! The cartridge's SM83 code is statically recompiled to x86-64 and stamped onto
//! a runtime stub together with the ROM. The result runs the game's own code on
//! the host processor; nothing in the output interprets Game Boy instructions.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use gb_hw::{Cartridge, CgbSupport, Config};
use gb_payload::{BlockEntry, Settings};
use gb_recomp::discover::{discover_with, peek};
use gb_recomp::exec::CodeCache;
use gb_recomp::machine::lower_window_is_fixed;
use gb_recomp::translate::{translate_block, Context};

const USAGE: &str = "\
gbtoexe — convert a Game Boy ROM into a native executable

USAGE:
    gbtoexe <rom> [-o <output>] [options]

OUTPUT:
    -o, --output <path>     Where to write the executable
                            (default: the ROM's name with a .exe extension)
        --stub <path>       Runtime stub to stamp onto
                            (default: found next to gbtoexe)

MACHINE:
        --model <m>         auto, dmg or cgb (default: auto, from the header)
        --palette <p>       grey, dmg, pocket, light, or four RRGGBB values
                            separated by commas (monochrome games only)

WINDOW:
        --title <text>      Window title (default: the cartridge title)
        --scale <n>         Initial window size, 1-8 (default: 4)
        --stretch           Fill the window instead of keeping the 10:9 shape

SOUND:
        --no-audio          Convert without sound
        --volume <0-100>    Output level (default: 70)
        --sample-rate <hz>  Audio sample rate (default: 48000)

TRANSLATION:
        --aot <mode>        How much to translate before the game runs:
                              fixed  the always-mapped bank (default)
                              full   also guess at every switchable bank
                              off    nothing; translate entirely on demand
                            Whatever is left out is translated the first time the
                            game reaches it, which costs a few microseconds.
        --budget <n>        Cap on blocks translated ahead of time
                            (default: 200000)
        --no-aot            Same as --aot off
        --verify [frames]   Run the converted game briefly to check it starts
    -v, --verbose           Report what discovery and translation found
    -h, --help              Show this message
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("gbtoexe: {message}");
            ExitCode::FAILURE
        }
    }
}

/// How much of the cartridge to translate before it runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Aot {
    /// Nothing; everything is translated the first time it is reached.
    Off,
    /// Only the bank that is always mapped, where addresses are unambiguous.
    Fixed,
    /// Also try each banked entry point against every bank it might belong to.
    Full,
}

struct Options {
    rom_path: PathBuf,
    output: Option<PathBuf>,
    stub: Option<PathBuf>,
    settings: Settings,
    budget: usize,
    aot: Aot,
    verify: Option<usize>,
    verbose: bool,
}

fn parse_palette(spec: &str) -> Result<[u32; 4], String> {
    match spec {
        "grey" | "gray" => Ok(gb_hw::PALETTE_GREY),
        "dmg" | "green" => Ok(gb_hw::PALETTE_DMG),
        "pocket" => Ok(gb_hw::PALETTE_POCKET),
        "light" => Ok(gb_hw::PALETTE_LIGHT),
        custom => {
            let parts: Vec<&str> = custom.split(',').collect();
            if parts.len() != 4 {
                return Err(format!(
                    "--palette wants a name or four RRGGBB values, got {custom:?}"
                ));
            }
            let mut shades = [0u32; 4];
            for (slot, part) in shades.iter_mut().zip(parts) {
                *slot = u32::from_str_radix(part.trim().trim_start_matches('#'), 16)
                    .map_err(|_| format!("{part:?} is not a six-digit hex colour"))?;
            }
            Ok(shades)
        }
    }
}

fn parse_args() -> Result<Option<Options>, String> {
    let mut args = std::env::args().skip(1).peekable();
    let mut rom_path: Option<PathBuf> = None;
    let mut options = Options {
        rom_path: PathBuf::new(),
        output: None,
        stub: None,
        settings: Settings::default(),
        budget: 200_000,
        aot: Aot::Fixed,
        verify: None,
        verbose: false,
    };

    while let Some(arg) = args.next() {
        let mut value = |name: &str| -> Result<String, String> {
            args.next().ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-o" | "--output" => options.output = Some(PathBuf::from(value("--output")?)),
            "--stub" => options.stub = Some(PathBuf::from(value("--stub")?)),
            "--title" => options.settings.title = value("--title")?,
            "--save-name" => options.settings.save_name = value("--save-name")?,
            "--stretch" => options.settings.keep_aspect = false,
            "--no-audio" => options.settings.audio = false,
            "--no-aot" => options.aot = Aot::Off,
            "--aot" => {
                options.aot = match value("--aot")?.as_str() {
                    "off" | "none" => Aot::Off,
                    "fixed" => Aot::Fixed,
                    "full" | "all" => Aot::Full,
                    other => {
                        return Err(format!(
                            "unknown --aot mode {other:?}; try fixed, full or off"
                        ))
                    }
                }
            }
            "-v" | "--verbose" => options.verbose = true,
            "--model" => {
                options.settings.model = match value("--model")?.as_str() {
                    "auto" => gb_payload::Model::Auto,
                    "dmg" => gb_payload::Model::Dmg,
                    "cgb" | "color" => gb_payload::Model::Cgb,
                    other => return Err(format!("unknown model {other:?}; try auto, dmg or cgb")),
                }
            }
            "--palette" => options.settings.palette = parse_palette(&value("--palette")?)?,
            "--scale" => {
                let raw = value("--scale")?;
                let scale: u8 = raw.parse().map_err(|_| format!("bad --scale {raw:?}"))?;
                options.settings.scale = scale.clamp(1, 8);
            }
            "--volume" => {
                let raw = value("--volume")?;
                let volume: u8 = raw.parse().map_err(|_| format!("bad --volume {raw:?}"))?;
                options.settings.volume = volume.min(100);
            }
            "--sample-rate" => {
                let raw = value("--sample-rate")?;
                options.settings.sample_rate = raw
                    .parse()
                    .map_err(|_| format!("bad --sample-rate {raw:?}"))?;
            }
            "--budget" => {
                let raw = value("--budget")?;
                options.budget = raw.parse().map_err(|_| format!("bad --budget {raw:?}"))?;
            }
            "--verify" => {
                // The frame count is optional, so only take it if it is a number.
                let frames = match args.peek().and_then(|v| v.parse::<usize>().ok()) {
                    Some(n) => {
                        args.next();
                        n
                    }
                    None => 120,
                };
                options.verify = Some(frames);
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option {other:?}; try --help"))
            }
            path => {
                if rom_path.is_some() {
                    return Err("more than one ROM given".into());
                }
                rom_path = Some(PathBuf::from(path));
            }
        }
    }

    options.rom_path = rom_path.ok_or("no ROM given; try --help")?;
    Ok(Some(options))
}

/// Look for the runtime stub in the places it is normally installed, then in
/// the build tree so the tool works from a checkout.
fn find_stub(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return if path.is_file() {
            Ok(path.to_path_buf())
        } else {
            Err(format!("no stub at {}", path.display()))
        };
    }
    if let Ok(from_env) = std::env::var("GBTOEXE_STUB") {
        let path = PathBuf::from(from_env);
        if path.is_file() {
            return Ok(path);
        }
    }

    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("stubs/gb-runtime.exe"));
            candidates.push(dir.join("gb-runtime.exe"));
            // Running out of target/debug or target/release during development.
            candidates.push(dir.join("../x86_64-pc-windows-gnu/release/gb-runtime.exe"));
            candidates.push(dir.join("../../x86_64-pc-windows-gnu/release/gb-runtime.exe"));
            candidates.push(dir.join("../x86_64-pc-windows-gnu/debug/gb-runtime.exe"));
        }
    }
    for path in candidates {
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("could not find the runtime stub.\n       \
         Build it with: cargo build --release --target x86_64-pc-windows-gnu -p gb-runtime\n       \
         or point at one with --stub"
        .into())
}

fn run() -> Result<(), String> {
    let Some(mut options) = parse_args()? else {
        print!("{USAGE}");
        return Ok(());
    };

    let rom = std::fs::read(&options.rom_path)
        .map_err(|e| format!("could not read {}: {e}", options.rom_path.display()))?;

    // Parsing the header first gives a clear error before any work is done.
    let cart = Cartridge::new(rom.clone()).map_err(|e| e.to_string())?;
    let info = cart.info().clone();

    let stem = options
        .rom_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "game".into());
    if options.settings.title == Settings::default().title {
        options.settings.title = if info.title.is_empty() {
            stem.clone()
        } else {
            info.title.clone()
        };
    }
    if options.settings.save_name == Settings::default().save_name {
        options.settings.save_name = stem.clone();
    }

    let output = options
        .output
        .clone()
        .unwrap_or_else(|| options.rom_path.with_extension("exe"));
    let stub_path = find_stub(options.stub.as_deref())?;
    let stub = std::fs::read(&stub_path)
        .map_err(|e| format!("could not read {}: {e}", stub_path.display()))?;
    if gb_payload::extract(&stub).is_ok() {
        return Err(format!(
            "{} already has a game stamped into it; use a fresh stub",
            stub_path.display()
        ));
    }

    println!(
        "{}  {}  {} ROM banks, {} KiB cartridge RAM{}",
        info.title,
        describe_mapper(&info),
        info.rom_banks,
        info.ram_bytes / 1024,
        match info.cgb {
            CgbSupport::None => "",
            CgbSupport::Enhanced => ", colour enhanced",
            CgbSupport::Required => ", colour required",
        }
    );
    if !info.header_checksum_ok {
        eprintln!("gbtoexe: warning: the header checksum is wrong; this may not be a ROM");
    }

    // ---- recompile ----
    let started = Instant::now();
    let (code, blocks) = if options.aot == Aot::Off {
        (Vec::new(), Vec::new())
    } else {
        let per_bank = if options.aot == Aot::Full { 4096 } else { 0 };
        translate_ahead_of_time(&rom, &cart, options.budget, per_bank, options.verbose)?
    };
    let elapsed = started.elapsed();

    if options.aot == Aot::Off {
        println!("no ahead-of-time translation; the game translates itself as it runs");
    } else {
        println!(
            "recompiled {} blocks to {:.1} KiB of x86-64 in {:.2?}",
            blocks.len(),
            code.len() as f64 / 1024.0,
            elapsed
        );
    }

    // ---- stamp ----
    let payload = gb_payload::build(&rom, &options.settings, &code, &blocks);
    let mut image = stub;
    image.extend_from_slice(&payload);
    std::fs::write(&output, &image)
        .map_err(|e| format!("could not write {}: {e}", output.display()))?;
    make_executable(&output);

    println!(
        "wrote {} ({:.1} MiB)",
        output.display(),
        image.len() as f64 / (1024.0 * 1024.0)
    );

    if let Some(frames) = options.verify {
        verify(&rom, &options.settings, frames)?;
    }
    Ok(())
}

fn describe_mapper(info: &gb_hw::CartridgeInfo) -> String {
    let name = match info.mbc {
        gb_hw::MbcKind::None => "no mapper",
        gb_hw::MbcKind::Mbc1 => "MBC1",
        gb_hw::MbcKind::Mbc2 => "MBC2",
        gb_hw::MbcKind::Mbc3 => "MBC3",
        gb_hw::MbcKind::Mbc5 => "MBC5",
    };
    let mut parts = vec![name.to_string()];
    if info.battery {
        parts.push("battery".into());
    }
    if info.rtc {
        parts.push("clock".into());
    }
    parts.join("+")
}

/// Discover and translate everything reachable without running the game.
fn translate_ahead_of_time(
    rom: &[u8],
    cart: &Cartridge,
    budget: usize,
    per_bank: usize,
    verbose: bool,
) -> Result<(Vec<u8>, Vec<BlockEntry>), String> {
    let report = discover_with(rom, budget, per_bank);
    if verbose {
        println!(
            "discovery: {} blocks, {} shared entry points into banked code{}",
            report.blocks.len(),
            report.shared_entries,
            if report.truncated {
                " (stopped at the budget)"
            } else {
                ""
            }
        );
    }
    if report.truncated {
        eprintln!(
            "gbtoexe: warning: discovery hit the --budget of {budget} blocks; \
             the rest will be translated as the game runs"
        );
    }

    let ctx = Context {
        lower_window_fixed: lower_window_is_fixed(cart),
    };
    let mut cache = CodeCache::new(gb_recomp::exec::DEFAULT_ARENA)
        .ok_or("could not reserve memory for translated code")?;

    for key in &report.blocks {
        let bank = key.bank;
        let block = translate_block(key.addr, ctx, &|addr| peek(rom, bank, addr));
        cache.install(*key, block);
    }

    let (code, map) = cache.export();
    let blocks = map
        .into_iter()
        .map(|(key, offset, end)| BlockEntry {
            bank: key.bank,
            addr: key.addr,
            offset,
            end: end as u16,
        })
        .collect();
    Ok((code, blocks))
}

/// Run the converted game on this machine for a moment to check it starts.
fn verify(rom: &[u8], settings: &Settings, frames: usize) -> Result<(), String> {
    let config = Config {
        model: match settings.model {
            gb_payload::Model::Auto => gb_hw::Model::Auto,
            gb_payload::Model::Dmg => gb_hw::Model::Dmg,
            gb_payload::Model::Cgb => gb_hw::Model::Cgb,
        },
        sample_rate: settings.sample_rate,
        dmg_palette: settings.palette,
    };
    let mut emu = gb_recomp::machine::Emu::new(rom.to_vec(), &config).map_err(|e| e.to_string())?;

    let started = Instant::now();
    for _ in 0..frames {
        emu.run_frame();
        if let Some(fault) = &emu.fault {
            return Err(format!("the game faulted during verification: {fault}"));
        }
    }
    let elapsed = started.elapsed();

    // A frame that never changes any pixel usually means nothing is running.
    let frame = emu.framebuffer();
    let distinct = {
        let mut seen: Vec<u32> = frame.to_vec();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    };

    println!(
        "verified: {frames} frames in {elapsed:.2?} ({:.0}x real time), \
         {} blocks, {} distinct colours on screen",
        frames as f64 / elapsed.as_secs_f64() / 59.7275,
        emu.blocks_translated(),
        distinct
    );
    if distinct < 2 {
        eprintln!("gbtoexe: warning: the screen is a single flat colour after {frames} frames");
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        let mut perms = metadata.permissions();
        perms.set_mode(perms.mode() | 0o111);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}
