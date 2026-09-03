//! The Game Boy's peripherals, modelled in software.
//!
//! There is deliberately no CPU here. `gbtoexe` translates the cartridge's SM83
//! code into native x86-64 ahead of time; this crate is what that translated code
//! calls into whenever it touches something the host cannot do directly — video,
//! sound, timers, the cartridge mapper.

pub mod apu;
pub mod cartridge;
pub mod joypad;
pub mod mmu;
pub mod ppu;
pub mod timer;

pub use cartridge::{Cartridge, CartridgeError, CartridgeInfo, CgbSupport, MbcKind};
pub use joypad::Button;
pub use mmu::Mmu;
pub use ppu::{
    DmgPalette, PALETTE_DMG, PALETTE_GREY, PALETTE_LIGHT, PALETTE_POCKET, SCREEN_H, SCREEN_W,
};

/// Clocks per second of the base (non-double-speed) CPU.
pub const CPU_HZ: u32 = 4_194_304;
/// Clocks in one LCD frame; the screen refreshes at 59.7275 Hz.
pub const FRAME_CYCLES: u32 = 70_224;

/// Which machine to model. `Auto` follows the cartridge header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    Auto,
    Dmg,
    Cgb,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub model: Model,
    pub sample_rate: u32,
    /// Shades used for monochrome games; ignored in CGB mode.
    pub dmg_palette: DmgPalette,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            model: Model::Auto,
            sample_rate: 48_000,
            dmg_palette: ppu::PALETTE_GREY,
        }
    }
}

/// Builds the peripheral set for a cartridge, resolving `Model::Auto`.
pub fn build(rom: Vec<u8>, config: &Config) -> Result<(Mmu, bool), CartridgeError> {
    let cart = Cartridge::new(rom)?;

    let cgb = match config.model {
        Model::Dmg => false,
        Model::Cgb => true,
        Model::Auto => !matches!(cart.cgb_support(), CgbSupport::None),
    };
    if !cgb && cart.cgb_support() == CgbSupport::Required {
        return Err(CartridgeError(
            "this cartridge requires a Game Boy Color; convert it with --model cgb".into(),
        ));
    }

    let dmg_compat = cgb && cart.cgb_support() == CgbSupport::None;
    let mut mmu = Mmu::new(cart, cgb, config.sample_rate, config.dmg_palette);
    if dmg_compat {
        seed_compat_palettes(&mut mmu, config.dmg_palette);
    }
    Ok((mmu, cgb))
}

/// A CGB running a monochrome cartridge normally inherits its palettes from the
/// boot ROM. There is no boot ROM here, so seed them from the chosen shades;
/// otherwise such a game renders black on black.
fn seed_compat_palettes(mmu: &mut Mmu, palette: DmgPalette) {
    for (i, &rgb) in palette.iter().enumerate() {
        let r = ((rgb >> 16) & 0xff) as u16 >> 3;
        let g = ((rgb >> 8) & 0xff) as u16 >> 3;
        let b = (rgb & 0xff) as u16 >> 3;
        let packed = r | (g << 5) | (b << 10);
        for pal in 0..8u8 {
            let index = pal * 8 + i as u8 * 2;
            for (select, data) in [(0xff68u16, 0xff69u16), (0xff6a, 0xff6b)] {
                mmu.ppu.write_reg(select, 0x80 | index);
                mmu.ppu.write_reg(data, packed as u8);
                mmu.ppu.write_reg(data, (packed >> 8) as u8);
            }
        }
    }
    mmu.ppu.write_reg(0xff68, 0);
    mmu.ppu.write_reg(0xff6a, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x100..0x104].copy_from_slice(&[0x00, 0xc3, 0x50, 0x01]);
        rom[0x147] = 0x00;
        let sum = rom[0x134..0x14d]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_sub(b).wrapping_sub(1));
        rom[0x14d] = sum;
        rom
    }

    #[test]
    fn header_is_parsed() {
        let (mmu, cgb) = build(test_rom(), &Config::default()).unwrap();
        assert!(mmu.cart.info().header_checksum_ok);
        assert_eq!(mmu.cart.info().mbc, MbcKind::None);
        assert!(!cgb);
    }

    #[test]
    fn rejects_a_file_too_short_to_be_a_rom() {
        assert!(build(vec![0; 16], &Config::default()).is_err());
    }

    #[test]
    fn ram_writes_bump_the_code_version() {
        let (mut mmu, _) = build(test_rom(), &Config::default()).unwrap();
        let before = mmu.code_version[0xc1];
        mmu.write(0xc100, 0x42);
        assert_eq!(mmu.code_version[0xc1], before + 1);
        assert_eq!(mmu.read(0xc100), 0x42);
    }

    #[test]
    fn the_ppu_reaches_vblank() {
        let (mut mmu, _) = build(test_rom(), &Config::default()).unwrap();
        for _ in 0..FRAME_CYCLES / 4 {
            mmu.tick(4);
        }
        assert!(mmu.ppu.frame_ready);
    }

    #[test]
    fn the_apu_produces_samples() {
        let (mut mmu, _) = build(test_rom(), &Config::default()).unwrap();
        for _ in 0..FRAME_CYCLES / 4 {
            mmu.tick(4);
        }
        let mut audio = Vec::new();
        mmu.apu.drain(&mut audio);
        assert!(audio.len() > 1200, "got {} samples", audio.len());
    }

    #[test]
    fn battery_ram_survives_a_save_and_reload() {
        // A cartridge type with battery-backed RAM: MBC1 + RAM + battery.
        let mut rom = test_rom();
        rom[0x147] = 0x03;
        rom[0x149] = 0x02; // 8 KiB of cartridge RAM
        let sum = rom[0x134..0x14d]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_sub(b).wrapping_sub(1));
        rom[0x14d] = sum;

        let (mut mmu, _) = build(rom.clone(), &Config::default()).unwrap();
        assert!(mmu.cart.has_battery());
        assert!(!mmu.cart.ram_dirty);

        mmu.write(0x0000, 0x0a); // unlock cartridge RAM
        mmu.write(0xa000, 0x5a);
        mmu.write(0xa001, 0xa5);
        assert!(
            mmu.cart.ram_dirty,
            "writing battery RAM should mark it dirty"
        );
        let saved = mmu.cart.save_data();

        let (mut restored, _) = build(rom, &Config::default()).unwrap();
        restored.cart.load_save(&saved);
        restored.write(0x0000, 0x0a);
        assert_eq!(restored.read(0xa000), 0x5a);
        assert_eq!(restored.read(0xa001), 0xa5);
        assert!(!restored.cart.ram_dirty, "a fresh load is not unsaved work");
    }

    #[test]
    fn a_cartridge_without_a_battery_reports_no_save() {
        let (mmu, _) = build(test_rom(), &Config::default()).unwrap();
        assert!(!mmu.cart.has_battery());
    }

    #[test]
    fn joypad_reads_back_inverted() {
        let (mut mmu, _) = build(test_rom(), &Config::default()).unwrap();
        mmu.joypad.write(0x10);
        mmu.joypad.set(Button::A, true);
        assert_eq!(mmu.joypad.read() & 0x01, 0);
        mmu.joypad.set(Button::A, false);
        assert_eq!(mmu.joypad.read() & 0x01, 1);
    }
}
