//! The runtime stub.
//!
//! `gbtoexe` stamps a converted game onto the end of this executable: the ROM,
//! the display settings, and the native x86-64 the recompiler produced. On
//! startup the stub finds that payload, maps the translated code into executable
//! memory, and starts running it. Anything the converter could not resolve ahead
//! of time is translated here, on first use.
//!
//! Run without a payload it is an ordinary converter-less player: give it a ROM
//! path and it translates the whole thing at startup.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::time::Duration;
use std::time::Instant;

use gb_hw::{Config, Model};
use gb_payload::{Payload, ReadError, Settings};
use gb_recomp::exec::BlockKey;
use gb_recomp::machine::Emu;

#[cfg(windows)]
mod win32;

/// Seconds per LCD frame: 4194304 / 70224.
const FRAME_SECONDS: f64 = 1.0 / 59.727_5;

fn main() {
    if let Err(message) = run() {
        report_error(&message);
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn report_error(message: &str) {
    win32::report("Game Boy", message);
}

#[cfg(not(windows))]
fn report_error(message: &str) {
    eprintln!("error: {message}");
}

/// Where the game came from, and where its save file should live.
struct Loaded {
    payload: Payload,
    save_path: PathBuf,
}

fn load() -> Result<Loaded, String> {
    match gb_payload::extract_from_self() {
        Ok(payload) => {
            let dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| PathBuf::from("."));
            let save_path = dir.join(format!("{}.sav", payload.settings.save_name));
            Ok(Loaded { payload, save_path })
        }

        // No payload: this is a bare stub, so play a ROM given on the command line.
        Err(ReadError::NotPresent) => {
            let path = std::env::args().nth(1).ok_or_else(|| {
                "This program has no game in it.\n\n\
                 Convert a ROM with gbtoexe, or pass a ROM file as an argument."
                    .to_string()
            })?;
            let rom = std::fs::read(&path).map_err(|e| format!("could not read {path}: {e}"))?;
            let stem = Path::new(&path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "game".into());
            let settings = Settings {
                title: stem.clone(),
                save_name: stem,
                ..Settings::default()
            };
            let save_path = PathBuf::from(&path).with_extension("sav");
            Ok(Loaded {
                payload: Payload {
                    rom,
                    settings,
                    code: Vec::new(),
                    blocks: Vec::new(),
                },
                save_path,
            })
        }

        Err(other) => Err(other.to_string()),
    }
}

fn build(payload: &Payload) -> Result<Emu, String> {
    let settings = &payload.settings;
    let config = Config {
        model: match settings.model {
            gb_payload::Model::Auto => Model::Auto,
            gb_payload::Model::Dmg => Model::Dmg,
            gb_payload::Model::Cgb => Model::Cgb,
        },
        sample_rate: settings.sample_rate,
        dmg_palette: settings.palette,
    };

    let mut emu = Emu::new(payload.rom.clone(), &config).map_err(|e| e.to_string())?;

    // Reinstate the code the converter already produced. It holds no absolute
    // addresses, so it is valid at whatever address the arena landed on.
    if !payload.code.is_empty() {
        let map: Vec<(BlockKey, u32, u32)> = payload
            .blocks
            .iter()
            .map(|b| {
                (
                    BlockKey {
                        bank: b.bank,
                        addr: b.addr,
                    },
                    b.offset,
                    b.end as u32,
                )
            })
            .collect();
        emu.install_translated(&payload.code, &map);
    }
    Ok(emu)
}

fn run() -> Result<(), String> {
    let loaded = load()?;
    let mut emu = build(&loaded.payload)?;

    if emu.has_battery() {
        if let Ok(data) = std::fs::read(&loaded.save_path) {
            emu.load_save(&data);
        }
    }

    let title = if loaded.payload.settings.title.is_empty() {
        emu.info().title.clone()
    } else {
        loaded.payload.settings.title.clone()
    };

    // A frame count in the environment runs the game without a window and
    // reports what it did. It is how the converter checks its own output, and
    // it works the same on every platform.
    if let Some(frames) = std::env::var("GBTOEXE_FRAMES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        return self_test(&mut emu, &title, frames, &loaded.save_path);
    }

    play(
        &mut emu,
        &title,
        &loaded.payload.settings,
        &loaded.save_path,
    )
}

/// Run a fixed number of frames with no window, and say what happened.
fn self_test(emu: &mut Emu, title: &str, frames: usize, save_path: &Path) -> Result<(), String> {
    let started = Instant::now();
    let mut audio = Vec::new();
    let mut samples = 0usize;

    for _ in 0..frames {
        emu.run_frame();
        emu.drain_audio(&mut audio);
        samples += audio.len();
        audio.clear();
        if let Some(fault) = &emu.fault {
            return Err(fault.clone());
        }
    }
    let elapsed = started.elapsed();

    let frame = emu.framebuffer();
    let mut shades: Vec<u32> = frame.to_vec();
    shades.sort_unstable();
    shades.dedup();

    let mut report = String::new();
    report.push_str(&format!("{title}: ran {frames} frames in {elapsed:.2?}\n"));
    report.push_str(&format!(
        "  {:.1} frames/second ({:.1}x real time)\n",
        frames as f64 / elapsed.as_secs_f64(),
        frames as f64 / elapsed.as_secs_f64() * FRAME_SECONDS
    ));
    report.push_str(&format!(
        "  {} blocks translated, {} KiB of native code\n",
        emu.blocks_translated(),
        emu.code_bytes() / 1024
    ));
    report.push_str(&format!("  {samples} audio samples produced\n"));
    report.push_str(&format!("  {} distinct colours on screen\n", shades.len()));

    if let Ok(path) = std::env::var("GBTOEXE_SCREENSHOT") {
        write_ppm(&path, frame).map_err(|e| e.to_string())?;
        report.push_str(&format!("  wrote {path}\n"));
    }

    // A windowed build has no console to print to, so allow a file as well.
    print!("{report}");
    if let Ok(path) = std::env::var("GBTOEXE_REPORT") {
        let _ = std::fs::write(path, &report);
    }

    save_if_needed(emu, save_path);
    Ok(())
}

fn write_ppm(path: &str, frame: &[u32]) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = Vec::with_capacity(gb_hw::SCREEN_W * gb_hw::SCREEN_H * 3 + 32);
    write!(out, "P6\n{} {}\n255\n", gb_hw::SCREEN_W, gb_hw::SCREEN_H)?;
    for &pixel in frame {
        out.push((pixel >> 16) as u8);
        out.push((pixel >> 8) as u8);
        out.push(pixel as u8);
    }
    std::fs::write(path, out)
}

fn save_if_needed(emu: &mut Emu, path: &Path) {
    if !emu.has_battery() || !emu.save_dirty() {
        return;
    }
    if std::fs::write(path, emu.save_data()).is_ok() {
        emu.clear_save_dirty();
    }
}

#[cfg(windows)]
fn play(emu: &mut Emu, title: &str, settings: &Settings, save_path: &Path) -> Result<(), String> {
    let mut window =
        win32::Window::new(title, settings.scale.max(1) as u32).ok_or("could not open a window")?;
    let mut audio = if settings.audio {
        win32::Audio::new(settings.sample_rate)
    } else {
        None
    };
    let mut queue: Vec<f32> = Vec::new();
    let mut clock = win32::Clock::new();
    let mut last_save = Instant::now();
    let volume = settings.volume as f32 / 100.0;

    while window.pump() {
        let held = window.buttons();
        for (i, &down) in held.iter().enumerate() {
            emu.set_button(win32::button_for(i), down);
        }

        emu.run_frame();
        if let Some(fault) = &emu.fault {
            return Err(fault.clone());
        }

        window.present(emu.framebuffer(), settings.keep_aspect);

        if let Some(audio) = audio.as_mut() {
            emu.drain_audio(&mut queue);
            if volume != 1.0 {
                for sample in queue.iter_mut() {
                    *sample *= volume;
                }
            }
            audio.submit(&mut queue);
        } else {
            queue.clear();
            emu.drain_audio(&mut queue);
            queue.clear();
        }

        if window.turbo() {
            clock.resync();
        } else {
            clock.wait(FRAME_SECONDS);
        }

        if last_save.elapsed() > Duration::from_secs(3) {
            save_if_needed(emu, save_path);
            last_save = Instant::now();
        }
    }

    save_if_needed(emu, save_path);
    Ok(())
}

/// Off Windows there is no window to open, so a plain run is a self test.
#[cfg(not(windows))]
fn play(emu: &mut Emu, title: &str, _settings: &Settings, save_path: &Path) -> Result<(), String> {
    self_test(emu, title, 600, save_path)
}
