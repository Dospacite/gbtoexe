//! Address decoding, DMA engines, and the CGB-only speed switch.

use crate::apu::Apu;
use crate::cartridge::Cartridge;
use crate::joypad::Joypad;
use crate::ppu::{DmgPalette, Ppu};
use crate::timer::Timer;

/// Bit 3 of IF: serial transfer complete.
const IRQ_SERIAL: u8 = 0x08;

#[derive(Clone, Copy, PartialEq, Eq)]
enum HdmaMode {
    Off,
    /// Copies 0x10 bytes at each HBlank until the length runs out.
    HBlank,
}

pub struct Mmu {
    pub cart: Cartridge,
    pub ppu: Ppu,
    pub apu: Apu,
    pub timer: Timer,
    pub joypad: Joypad,

    /// 8 banks of 4 KiB; DMG only ever uses the first two.
    wram: Vec<u8>,
    wram_bank: usize,
    hram: [u8; 0x7f],

    ie: u8,
    iflag: u8,

    cgb: bool,
    double_speed: bool,
    speed_switch_armed: bool,

    // OAM DMA, which runs alongside the CPU rather than instantly.
    dma_source: u8,
    dma_active: bool,
    dma_index: usize,
    dma_timer: u32,

    // CGB VRAM DMA.
    hdma_src: u16,
    hdma_dst: u16,
    hdma_len: u8,
    hdma_mode: HdmaMode,

    // Serial port. Nothing is connected, so transfers complete into 0xFF.
    sb: u8,
    sc: u8,
    serial_timer: i32,

    /// Cycles consumed since the last `take_cycles`, for frame pacing.
    elapsed: u64,
    /// Monotonic cycle count since power-on.
    clock: u64,

    /// Bumped whenever a 256-byte page outside ROM is written. Blocks recompiled
    /// out of RAM record the version they were built from and are thrown away
    /// when it moves, which is what makes self-modifying code safe.
    pub code_version: [u32; 256],
}

impl Mmu {
    pub fn new(cart: Cartridge, cgb: bool, sample_rate: u32, palette: DmgPalette) -> Self {
        Mmu {
            cart,
            ppu: Ppu::new(cgb, palette),
            apu: Apu::new(sample_rate),
            timer: Timer::new(),
            joypad: Joypad::new(),
            wram: vec![0; 0x8000],
            wram_bank: 1,
            hram: [0; 0x7f],
            ie: 0,
            iflag: 0xe1,
            cgb,
            double_speed: false,
            speed_switch_armed: false,
            dma_source: 0,
            dma_active: false,
            dma_index: 0,
            dma_timer: 0,
            hdma_src: 0,
            hdma_dst: 0,
            hdma_len: 0xff,
            hdma_mode: HdmaMode::Off,
            sb: 0,
            sc: 0,
            serial_timer: 0,
            elapsed: 0,
            clock: 0,
            code_version: [0; 256],
        }
    }

    pub fn is_double_speed(&self) -> bool {
        self.double_speed
    }

    /// Total cycles the hardware has been advanced by, for pacing.
    pub fn total_cycles(&self) -> u64 {
        self.clock
    }

    pub fn take_cycles(&mut self) -> u64 {
        std::mem::take(&mut self.elapsed)
    }

    fn step_dma(&mut self, t: u32) {
        if !self.dma_active {
            return;
        }
        self.dma_timer += t;
        // One byte every 4 cycles, 160 bytes total.
        while self.dma_active && self.dma_timer >= 4 {
            self.dma_timer -= 4;
            let src = ((self.dma_source as u16) << 8) + self.dma_index as u16;
            let value = self.read_dma_source(src);
            self.ppu.oam[self.dma_index] = value;
            self.dma_index += 1;
            if self.dma_index == 0xa0 {
                self.dma_active = false;
            }
        }
    }

    /// DMA reads bypass the OAM/VRAM access rules the CPU is subject to.
    fn read_dma_source(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7fff => self.cart.read_rom(addr),
            0x8000..=0x9fff => self.ppu.read_vram(addr),
            0xa000..=0xbfff => self.cart.read_ram(addr),
            0xc000..=0xcfff => self.wram[(addr - 0xc000) as usize],
            0xd000..=0xdfff => self.wram[self.wram_bank * 0x1000 + (addr - 0xd000) as usize],
            _ => 0xff,
        }
    }

    fn step_serial(&mut self, t: u32) {
        if self.sc & 0x80 == 0 || self.sc & 0x01 == 0 {
            return; // idle, or waiting on an external clock that will never come
        }
        self.serial_timer -= t as i32;
        if self.serial_timer <= 0 {
            self.sb = 0xff;
            self.sc &= !0x80;
            self.iflag |= IRQ_SERIAL;
        }
    }

    fn hdma_copy_block(&mut self) {
        for i in 0..0x10u16 {
            let src = self.hdma_src.wrapping_add(i);
            let value = self.read_dma_source(src);
            let dst = 0x8000 | ((self.hdma_dst.wrapping_add(i)) & 0x1fff);
            self.ppu.write_vram(dst, value);
        }
        self.hdma_src = self.hdma_src.wrapping_add(0x10);
        self.hdma_dst = self.hdma_dst.wrapping_add(0x10);
        if self.hdma_len == 0 {
            self.hdma_len = 0xff;
            self.hdma_mode = HdmaMode::Off;
        } else {
            self.hdma_len -= 1;
        }
    }

    /// HBlank DMA moves one block per HBlank; call once per PPU mode change.
    fn step_hdma(&mut self, was_hblank: bool) {
        if self.hdma_mode == HdmaMode::HBlank && !was_hblank && self.ppu.in_hblank() {
            self.hdma_copy_block();
        }
    }

    fn read_io(&mut self, addr: u16) -> u8 {
        match addr {
            0xff00 => self.joypad.read(),
            0xff01 => self.sb,
            0xff02 => self.sc | if self.cgb { 0x7c } else { 0x7e },
            0xff04..=0xff07 => self.timer.read(addr),
            0xff0f => self.iflag | 0xe0,
            0xff10..=0xff3f => self.apu.read(addr),
            0xff40..=0xff4b => self.ppu.read_reg(addr),
            0xff4d if self.cgb => {
                0x7e | ((self.double_speed as u8) << 7) | self.speed_switch_armed as u8
            }
            0xff4f | 0xff68..=0xff6c => self.ppu.read_reg(addr),
            0xff51 if self.cgb => (self.hdma_src >> 8) as u8,
            0xff52 if self.cgb => self.hdma_src as u8,
            0xff53 if self.cgb => (self.hdma_dst >> 8) as u8,
            0xff54 if self.cgb => self.hdma_dst as u8,
            0xff55 if self.cgb => {
                self.hdma_len
                    | if self.hdma_mode == HdmaMode::Off {
                        0x80
                    } else {
                        0
                    }
            }
            0xff70 if self.cgb => 0xf8 | self.wram_bank as u8,
            _ => 0xff,
        }
    }

    fn write_io(&mut self, addr: u16, value: u8) {
        match addr {
            0xff00 => self.joypad.write(value),
            0xff01 => self.sb = value,
            0xff02 => {
                self.sc = value & 0x83;
                if self.sc & 0x81 == 0x81 {
                    // 8 bits at 8192 Hz, or eight times that on CGB's fast clock.
                    self.serial_timer = if self.cgb && value & 0x02 != 0 {
                        512
                    } else {
                        4096
                    };
                }
            }
            0xff04..=0xff07 => self.timer.write(addr, value),
            0xff0f => self.iflag = value & 0x1f,
            0xff10..=0xff3f => self.apu.write(addr, value),
            0xff46 => {
                self.dma_source = value;
                self.dma_active = true;
                self.dma_index = 0;
                self.dma_timer = 0;
            }
            0xff40..=0xff4b => self.ppu.write_reg(addr, value),
            0xff4d if self.cgb => self.speed_switch_armed = value & 1 != 0,
            0xff4f | 0xff68..=0xff6c => self.ppu.write_reg(addr, value),
            0xff51 if self.cgb => self.hdma_src = (self.hdma_src & 0xff) | ((value as u16) << 8),
            0xff52 if self.cgb => self.hdma_src = (self.hdma_src & 0xff00) | (value as u16 & 0xf0),
            0xff53 if self.cgb => {
                self.hdma_dst = (self.hdma_dst & 0xff) | ((value as u16 & 0x1f) << 8)
            }
            0xff54 if self.cgb => self.hdma_dst = (self.hdma_dst & 0xff00) | (value as u16 & 0xf0),
            0xff55 if self.cgb => {
                if self.hdma_mode == HdmaMode::HBlank && value & 0x80 == 0 {
                    self.hdma_mode = HdmaMode::Off; // writing 0 to bit 7 cancels
                    self.hdma_len |= 0x80;
                    return;
                }
                self.hdma_len = value & 0x7f;
                if value & 0x80 != 0 {
                    self.hdma_mode = HdmaMode::HBlank;
                    if self.ppu.in_hblank() {
                        self.hdma_copy_block();
                    }
                } else {
                    // General-purpose DMA stalls the CPU until it finishes.
                    let blocks = self.hdma_len as u32 + 1;
                    self.hdma_mode = HdmaMode::Off;
                    for _ in 0..blocks {
                        self.hdma_copy_block();
                    }
                    self.hdma_len = 0xff;
                    let stall = blocks * 32 * if self.double_speed { 2 } else { 1 };
                    self.tick(stall);
                }
            }
            0xff70 if self.cgb => self.wram_bank = (value as usize & 7).max(1),
            _ => {}
        }
    }
}

/// The bus the recompiled code calls into for every memory access.
impl Mmu {
    pub fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7fff => self.cart.read_rom(addr),
            0x8000..=0x9fff => self.ppu.read_vram(addr),
            0xa000..=0xbfff => self.cart.read_ram(addr),
            0xc000..=0xcfff => self.wram[(addr - 0xc000) as usize],
            0xd000..=0xdfff => self.wram[self.wram_bank * 0x1000 + (addr - 0xd000) as usize],
            0xe000..=0xefff => self.wram[(addr - 0xe000) as usize],
            0xf000..=0xfdff => self.wram[self.wram_bank * 0x1000 + (addr - 0xf000) as usize],
            0xfe00..=0xfe9f => {
                if self.dma_active {
                    0xff
                } else {
                    self.ppu.read_oam(addr)
                }
            }
            0xfea0..=0xfeff => 0x00,
            0xff00..=0xff7f => self.read_io(addr),
            0xff80..=0xfffe => self.hram[(addr - 0xff80) as usize],
            0xffff => self.ie,
        }
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000..=0x7fff => self.cart.write_rom(addr, value),
            0x8000..=0x9fff => self.ppu.write_vram(addr, value),
            0xa000..=0xbfff => {
                self.cart.write_ram(addr, value);
                self.code_version[(addr >> 8) as usize] += 1;
            }
            0xc000..=0xcfff => {
                self.wram[(addr - 0xc000) as usize] = value;
                self.code_version[(addr >> 8) as usize] += 1;
            }
            0xd000..=0xdfff => {
                self.wram[self.wram_bank * 0x1000 + (addr - 0xd000) as usize] = value;
                self.code_version[(addr >> 8) as usize] += 1;
            }
            0xe000..=0xefff => {
                self.wram[(addr - 0xe000) as usize] = value;
                self.code_version[(addr >> 8) as usize] += 1;
            }
            0xf000..=0xfdff => {
                self.wram[self.wram_bank * 0x1000 + (addr - 0xf000) as usize] = value;
                self.code_version[(addr >> 8) as usize] += 1;
            }
            0xfe00..=0xfe9f => {
                if !self.dma_active {
                    self.ppu.write_oam(addr, value);
                }
            }
            0xfea0..=0xfeff => {}
            0xff00..=0xff7f => self.write_io(addr, value),
            0xff80..=0xfffe => {
                self.hram[(addr - 0xff80) as usize] = value;
                self.code_version[(addr >> 8) as usize] += 1;
            }
            0xffff => self.ie = value & 0x1f,
        }
    }

    pub fn tick(&mut self, t: u32) {
        self.elapsed += t as u64;
        self.clock += t as u64;

        // The timer counts CPU clocks, so it speeds up with the CPU.
        self.timer.step(t);
        self.step_dma(t);
        self.step_serial(t);

        // Video and audio are on a fixed clock regardless of CPU speed.
        let video_t = if self.double_speed { t / 2 } else { t };
        let was_hblank = self.ppu.in_hblank();
        self.ppu.step(video_t);
        self.step_hdma(was_hblank);
        self.apu.step(video_t);

        self.iflag |= self.ppu.irq | self.timer.irq | self.joypad.irq;
        self.ppu.irq = 0;
        self.timer.irq = 0;
        self.joypad.irq = 0;
    }

    pub fn pending_interrupts(&self) -> u8 {
        self.ie & self.iflag & 0x1f
    }

    pub fn ack_interrupt(&mut self, bit: u8) {
        self.iflag &= !(1 << bit);
    }

    /// Read without advancing anything. The recompiler uses this to fetch the
    /// instruction stream it is translating, which must not disturb the machine.
    pub fn peek(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7fff => self.cart.read_rom(addr),
            0xa000..=0xbfff => self.cart.read_ram(addr),
            0xc000..=0xcfff => self.wram[(addr - 0xc000) as usize],
            0xd000..=0xdfff => self.wram[self.wram_bank * 0x1000 + (addr - 0xd000) as usize],
            0xe000..=0xefff => self.wram[(addr - 0xe000) as usize],
            0xf000..=0xfdff => self.wram[self.wram_bank * 0x1000 + (addr - 0xf000) as usize],
            0xff80..=0xfffe => self.hram[(addr - 0xff80) as usize],
            // Code never runs from video memory or the register file.
            _ => 0xff,
        }
    }

    /// Bank currently visible at `addr`, used as part of a block's cache key.
    pub fn bank_at(&self, addr: u16) -> u16 {
        match addr {
            0x0000..=0x7fff => self.cart.bank_at(addr),
            0xd000..=0xdfff | 0xf000..=0xfdff => 0x100 | self.wram_bank as u16,
            _ => 0,
        }
    }

    pub fn stop(&mut self) -> bool {
        if self.cgb && self.speed_switch_armed {
            self.double_speed = !self.double_speed;
            self.speed_switch_armed = false;
            self.timer.write(0xff04, 0);
            return true;
        }
        false
    }
}
