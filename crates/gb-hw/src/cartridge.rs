//! Cartridge parsing and memory bank controllers.

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MbcKind {
    None,
    Mbc1,
    Mbc2,
    Mbc3,
    Mbc5,
}

/// What the cartridge header says the console should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgbSupport {
    /// Monochrome only.
    None,
    /// Runs on both; colour enhancements available.
    Enhanced,
    /// Refuses to boot on a DMG.
    Required,
}

#[derive(Debug)]
pub struct CartridgeError(pub String);

impl std::fmt::Display for CartridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CartridgeError {}

/// Header facts a front-end may want to show or act on.
#[derive(Debug, Clone)]
pub struct CartridgeInfo {
    pub title: String,
    pub mbc: MbcKind,
    pub rom_banks: usize,
    pub ram_bytes: usize,
    pub battery: bool,
    pub rtc: bool,
    pub cgb: CgbSupport,
    pub header_checksum_ok: bool,
    pub global_checksum_ok: bool,
}

pub struct Cartridge {
    rom: Vec<u8>,
    ram: Vec<u8>,
    kind: MbcKind,
    battery: bool,
    rom_banks: usize,
    ram_banks: usize,

    ram_enabled: bool,
    /// Low bank register: 5 bits on MBC1/2, 7 on MBC3, 9 on MBC5.
    bank_lo: usize,
    /// Secondary register: RAM bank / upper ROM bits / RTC register select.
    bank_hi: usize,
    /// MBC1 only: 0 = simple ROM banking, 1 = RAM banking / upper-ROM remap.
    mode: u8,

    rtc: Option<Rtc>,
    /// Set whenever battery-backed RAM (or the RTC) changed since the last save.
    pub ram_dirty: bool,

    info: CartridgeInfo,
}

impl Cartridge {
    pub fn new(rom: Vec<u8>) -> Result<Self, CartridgeError> {
        if rom.len() < 0x150 {
            return Err(CartridgeError(format!(
                "ROM is {} bytes; a Game Boy header alone needs 336",
                rom.len()
            )));
        }

        let code = rom[0x147];
        let (kind, battery, rtc) = match code {
            0x00 => (MbcKind::None, false, false),
            0x08 => (MbcKind::None, false, false),
            0x09 => (MbcKind::None, true, false),
            0x01 | 0x02 => (MbcKind::Mbc1, false, false),
            0x03 => (MbcKind::Mbc1, true, false),
            0x05 => (MbcKind::Mbc2, false, false),
            0x06 => (MbcKind::Mbc2, true, false),
            0x0f | 0x10 => (MbcKind::Mbc3, true, true),
            0x11 | 0x12 => (MbcKind::Mbc3, false, false),
            0x13 => (MbcKind::Mbc3, true, false),
            0x19 | 0x1a | 0x1c | 0x1d => (MbcKind::Mbc5, false, false),
            0x1b | 0x1e => (MbcKind::Mbc5, true, false),
            other => {
                return Err(CartridgeError(format!(
                    "unsupported cartridge type 0x{other:02X} at header offset 0x147"
                )))
            }
        };

        // The header's ROM-size byte is advisory; trust the file, but sanity-check it.
        let rom_banks = rom.len().max(0x8000).div_ceil(0x4000);

        let ram_bytes = match kind {
            // MBC2 has 512 half-bytes on the controller itself and no size byte.
            MbcKind::Mbc2 => 512,
            _ => match rom[0x149] {
                0x00 => 0,
                0x01 => 2 * 1024,
                0x02 => 8 * 1024,
                0x03 => 32 * 1024,
                0x04 => 128 * 1024,
                0x05 => 64 * 1024,
                _ => 0,
            },
        };
        let ram_banks = (ram_bytes / 0x2000).max(1);

        let title = {
            let raw = &rom[0x134..0x144];
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[..end])
                .chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
                .trim()
                .to_string()
        };

        let cgb = match rom[0x143] {
            0x80 => CgbSupport::Enhanced,
            0xc0 => CgbSupport::Required,
            _ => CgbSupport::None,
        };

        let header_checksum_ok = {
            let sum = rom[0x134..0x14d]
                .iter()
                .fold(0u8, |acc, &b| acc.wrapping_sub(b).wrapping_sub(1));
            sum == rom[0x14d]
        };
        let global_checksum_ok = {
            let stored = u16::from_be_bytes([rom[0x14e], rom[0x14f]]);
            let sum = rom
                .iter()
                .enumerate()
                .filter(|&(i, _)| i != 0x14e && i != 0x14f)
                .fold(0u16, |acc, (_, &b)| acc.wrapping_add(b as u16));
            stored == sum
        };

        let info = CartridgeInfo {
            title: if title.is_empty() {
                "UNTITLED".to_string()
            } else {
                title
            },
            mbc: kind,
            rom_banks,
            ram_bytes,
            battery,
            rtc,
            cgb,
            header_checksum_ok,
            global_checksum_ok,
        };

        Ok(Cartridge {
            rom,
            ram: vec![0; ram_bytes],
            kind,
            battery,
            rom_banks,
            ram_banks,
            ram_enabled: false,
            bank_lo: 1,
            bank_hi: 0,
            mode: 0,
            rtc: rtc.then(Rtc::new),
            ram_dirty: false,
            info,
        })
    }

    pub fn info(&self) -> &CartridgeInfo {
        &self.info
    }

    pub fn cgb_support(&self) -> CgbSupport {
        self.info.cgb
    }

    pub fn has_battery(&self) -> bool {
        self.battery
    }

    /// Battery-backed contents, in the layout `load_save` expects.
    pub fn save_data(&self) -> Vec<u8> {
        let mut out = self.ram.clone();
        if let Some(rtc) = &self.rtc {
            out.extend_from_slice(&rtc.serialize());
        }
        out
    }

    pub fn load_save(&mut self, data: &[u8]) {
        let n = self.ram.len().min(data.len());
        self.ram[..n].copy_from_slice(&data[..n]);
        if let Some(rtc) = &mut self.rtc {
            if data.len() >= self.ram.len() + Rtc::SAVE_LEN {
                rtc.deserialize(&data[self.ram.len()..]);
            }
        }
        self.ram_dirty = false;
    }

    /// ROM bank mapped at 0x0000-0x3FFF (only MBC1 mode 1 moves it off zero).
    fn lower_bank(&self) -> usize {
        match self.kind {
            MbcKind::Mbc1 if self.mode == 1 => (self.bank_hi << 5) % self.rom_banks,
            _ => 0,
        }
    }

    /// ROM bank mapped at 0x4000-0x7FFF.
    fn upper_bank(&self) -> usize {
        let bank = match self.kind {
            MbcKind::None => 1,
            MbcKind::Mbc1 => {
                let lo = if self.bank_lo & 0x1f == 0 {
                    1
                } else {
                    self.bank_lo & 0x1f
                };
                (self.bank_hi << 5) | lo
            }
            MbcKind::Mbc2 => (self.bank_lo & 0x0f).max(1),
            MbcKind::Mbc3 => (self.bank_lo & 0x7f).max(1),
            MbcKind::Mbc5 => self.bank_lo & 0x1ff,
        };
        bank % self.rom_banks
    }

    /// Which ROM bank is currently visible at `addr`. Recompiled blocks are keyed
    /// on this, since the same address holds different code in different banks.
    pub fn bank_at(&self, addr: u16) -> u16 {
        (match addr {
            0x0000..=0x3fff => self.lower_bank(),
            _ => self.upper_bank(),
        }) as u16
    }

    pub fn read_rom(&self, addr: u16) -> u8 {
        let idx = match addr {
            0x0000..=0x3fff => self.lower_bank() * 0x4000 + addr as usize,
            _ => self.upper_bank() * 0x4000 + (addr as usize - 0x4000),
        };
        self.rom.get(idx).copied().unwrap_or(0xff)
    }

    pub fn write_rom(&mut self, addr: u16, value: u8) {
        match self.kind {
            MbcKind::None => {}
            MbcKind::Mbc1 => match addr {
                0x0000..=0x1fff => self.ram_enabled = value & 0x0f == 0x0a,
                0x2000..=0x3fff => self.bank_lo = (value & 0x1f) as usize,
                0x4000..=0x5fff => self.bank_hi = (value & 0x03) as usize,
                0x6000..=0x7fff => self.mode = value & 1,
                _ => {}
            },
            MbcKind::Mbc2 => {
                // Address bit 8 picks which register the write lands in.
                if addr < 0x4000 {
                    if addr & 0x0100 == 0 {
                        self.ram_enabled = value & 0x0f == 0x0a;
                    } else {
                        self.bank_lo = (value & 0x0f) as usize;
                    }
                }
            }
            MbcKind::Mbc3 => match addr {
                0x0000..=0x1fff => self.ram_enabled = value & 0x0f == 0x0a,
                0x2000..=0x3fff => self.bank_lo = (value & 0x7f) as usize,
                0x4000..=0x5fff => self.bank_hi = value as usize,
                0x6000..=0x7fff => {
                    if let Some(rtc) = &mut self.rtc {
                        rtc.write_latch(value);
                    }
                }
                _ => {}
            },
            MbcKind::Mbc5 => match addr {
                0x0000..=0x1fff => self.ram_enabled = value & 0x0f == 0x0a,
                0x2000..=0x2fff => self.bank_lo = (self.bank_lo & 0x100) | value as usize,
                0x3000..=0x3fff => {
                    self.bank_lo = (self.bank_lo & 0xff) | ((value as usize & 1) << 8)
                }
                0x4000..=0x5fff => self.bank_hi = (value & 0x0f) as usize,
                _ => {}
            },
        }
    }

    pub fn read_ram(&self, addr: u16) -> u8 {
        if !self.ram_enabled {
            return 0xff;
        }
        let off = addr as usize - 0xa000;

        if self.kind == MbcKind::Mbc2 {
            // 512 x 4 bits, echoed through the whole A000-BFFF window.
            return 0xf0 | (self.ram[off & 0x1ff] & 0x0f);
        }
        if let (MbcKind::Mbc3, Some(rtc)) = (self.kind, &self.rtc) {
            if (0x08..=0x0c).contains(&self.bank_hi) {
                return rtc.read(self.bank_hi as u8);
            }
        }
        if self.ram.is_empty() {
            return 0xff;
        }
        let bank = match self.kind {
            MbcKind::Mbc1 if self.mode == 1 => self.bank_hi % self.ram_banks,
            MbcKind::Mbc1 => 0,
            MbcKind::None => 0,
            _ => self.bank_hi % self.ram_banks,
        };
        self.ram[(bank * 0x2000 + off) % self.ram.len()]
    }

    pub fn write_ram(&mut self, addr: u16, value: u8) {
        if !self.ram_enabled {
            return;
        }
        let off = addr as usize - 0xa000;

        if self.kind == MbcKind::Mbc2 {
            self.ram[off & 0x1ff] = value & 0x0f;
            self.ram_dirty = true;
            return;
        }
        if self.kind == MbcKind::Mbc3 && (0x08..=0x0c).contains(&self.bank_hi) {
            if let Some(rtc) = &mut self.rtc {
                rtc.write(self.bank_hi as u8, value);
                self.ram_dirty = true;
            }
            return;
        }
        if self.ram.is_empty() {
            return;
        }
        let bank = match self.kind {
            MbcKind::Mbc1 if self.mode == 1 => self.bank_hi % self.ram_banks,
            MbcKind::Mbc1 => 0,
            MbcKind::None => 0,
            _ => self.bank_hi % self.ram_banks,
        };
        let len = self.ram.len();
        self.ram[(bank * 0x2000 + off) % len] = value;
        self.ram_dirty = true;
    }
}

/// MBC3's real-time clock. Time is derived from the host clock, so the counter
/// keeps advancing while the emulator is closed, exactly like the real cell.
struct Rtc {
    /// Wall-clock second the counter is measured from.
    base: u64,
    /// Latched register snapshot presented to the game.
    latched: [u8; 5],
    last_latch_write: u8,
    halted: bool,
    /// Offset applied when the game writes the counter registers directly.
    offset: i64,
    day_carry: bool,
    /// Seconds accumulated at the moment the clock was halted.
    halt_value: u64,
}

impl Rtc {
    const SAVE_LEN: usize = 8 + 5 + 2;

    fn new() -> Self {
        Rtc {
            base: now_secs(),
            latched: [0; 5],
            last_latch_write: 0xff,
            halted: false,
            offset: 0,
            day_carry: false,
            halt_value: 0,
        }
    }

    fn elapsed(&self) -> u64 {
        let raw = if self.halted {
            self.halt_value
        } else {
            now_secs().saturating_sub(self.base)
        };
        (raw as i64 + self.offset).max(0) as u64
    }

    fn registers(&self) -> [u8; 5] {
        let secs = self.elapsed();
        let days = secs / 86_400;
        [
            (secs % 60) as u8,
            (secs / 60 % 60) as u8,
            (secs / 3600 % 24) as u8,
            (days & 0xff) as u8,
            (((days >> 8) & 1) as u8)
                | ((self.halted as u8) << 6)
                | (((self.day_carry || days > 511) as u8) << 7),
        ]
    }

    fn write_latch(&mut self, value: u8) {
        if self.last_latch_write == 0x00 && value == 0x01 {
            self.latched = self.registers();
        }
        self.last_latch_write = value;
    }

    fn read(&self, reg: u8) -> u8 {
        self.latched[(reg - 0x08) as usize]
    }

    fn write(&mut self, reg: u8, value: u8) {
        let mut regs = self.registers();
        let idx = (reg - 0x08) as usize;
        regs[idx] = value;
        self.latched[idx] = value;

        if idx == 4 {
            let was_halted = self.halted;
            self.halted = value & 0x40 != 0;
            self.day_carry = value & 0x80 != 0;
            if self.halted && !was_halted {
                self.halt_value = now_secs().saturating_sub(self.base);
            } else if !self.halted && was_halted {
                self.base = now_secs().saturating_sub(self.halt_value);
            }
        }

        // Re-derive the offset so the requested time reads back verbatim.
        let target = regs[0] as u64
            + regs[1] as u64 * 60
            + regs[2] as u64 * 3600
            + (regs[3] as u64 | ((regs[4] as u64 & 1) << 8)) * 86_400;
        let raw = if self.halted {
            self.halt_value
        } else {
            now_secs().saturating_sub(self.base)
        };
        self.offset = target as i64 - raw as i64;
    }

    fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::SAVE_LEN);
        out.extend_from_slice(&(self.elapsed()).to_le_bytes());
        out.extend_from_slice(&self.latched);
        out.push(self.halted as u8);
        out.push(self.day_carry as u8);
        out
    }

    fn deserialize(&mut self, data: &[u8]) {
        if data.len() < Self::SAVE_LEN {
            return;
        }
        let elapsed = u64::from_le_bytes(data[0..8].try_into().unwrap());
        self.latched.copy_from_slice(&data[8..13]);
        self.halted = data[13] != 0;
        self.day_carry = data[14] != 0;
        self.base = now_secs().saturating_sub(elapsed);
        self.halt_value = elapsed;
        self.offset = 0;
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
