//! Picture processing unit: scanline renderer with per-mode dot timing.

pub const SCREEN_W: usize = 160;
pub const SCREEN_H: usize = 144;

/// Dots per scanline, including blanking.
const DOTS_PER_LINE: u32 = 456;
const LINES_PER_FRAME: u8 = 154;

const MODE_HBLANK: u8 = 0;
const MODE_VBLANK: u8 = 1;
const MODE_OAM: u8 = 2;
const MODE_DRAW: u8 = 3;

pub const IRQ_VBLANK: u8 = 0x01;
pub const IRQ_STAT: u8 = 0x02;

/// The four shades a DMG screen can show, darkest last, as 0xRRGGBB.
pub type DmgPalette = [u32; 4];

pub const PALETTE_GREY: DmgPalette = [0xffffff, 0xaaaaaa, 0x555555, 0x000000];
pub const PALETTE_DMG: DmgPalette = [0x9bbc0f, 0x8bac0f, 0x306230, 0x0f380f];
pub const PALETTE_POCKET: DmgPalette = [0xc4cfa1, 0x8b956d, 0x4d533c, 0x1f1f1f];
pub const PALETTE_LIGHT: DmgPalette = [0x00b581, 0x009a71, 0x00694a, 0x004f3b];

pub struct Ppu {
    /// Two 8 KiB banks; only bank 0 exists on DMG.
    pub vram: Vec<u8>,
    vram_bank: usize,
    pub oam: [u8; 0xa0],

    lcdc: u8,
    stat: u8,
    scy: u8,
    scx: u8,
    ly: u8,
    lyc: u8,
    bgp: u8,
    obp0: u8,
    obp1: u8,
    wy: u8,
    wx: u8,

    // CGB palette RAM, 8 palettes x 4 colours x 2 bytes.
    bg_pal: [u8; 64],
    obj_pal: [u8; 64],
    bcps: u8,
    ocps: u8,
    /// FF6C bit 0: when set, fall back to DMG's X-coordinate sprite priority.
    opri: u8,

    mode: u8,
    dots: u32,
    /// Counts only the lines the window actually drew on, which is not LY.
    window_line: u8,
    window_active: bool,

    /// STAT interrupts fire on the rising edge of the OR of all enabled sources.
    stat_line: bool,

    cgb: bool,
    dmg_palette: DmgPalette,

    /// Per-pixel record of the background for sprite mixing:
    /// bits 0-1 colour index, bit 7 the CGB "BG over OBJ" attribute.
    bg_line: [u8; SCREEN_W],

    pub framebuffer: Vec<u32>,
    pub frame_ready: bool,
    /// Interrupt requests raised since the MMU last drained them.
    pub irq: u8,
}

impl Ppu {
    pub fn new(cgb: bool, dmg_palette: DmgPalette) -> Self {
        Ppu {
            vram: vec![0; 0x4000],
            vram_bank: 0,
            oam: [0; 0xa0],
            lcdc: 0x91,
            stat: 0x85,
            scy: 0,
            scx: 0,
            ly: 0,
            lyc: 0,
            bgp: 0xfc,
            obp0: 0xff,
            obp1: 0xff,
            wy: 0,
            wx: 0,
            bg_pal: [0xff; 64],
            obj_pal: [0xff; 64],
            bcps: 0,
            ocps: 0,
            opri: 0,
            mode: MODE_OAM,
            dots: 0,
            window_line: 0,
            window_active: false,
            stat_line: false,
            cgb,
            dmg_palette,
            bg_line: [0; SCREEN_W],
            framebuffer: vec![dmg_palette[0]; SCREEN_W * SCREEN_H],
            frame_ready: false,
            irq: 0,
        }
    }

    pub fn set_dmg_palette(&mut self, palette: DmgPalette) {
        self.dmg_palette = palette;
    }

    fn lcd_on(&self) -> bool {
        self.lcdc & 0x80 != 0
    }

    pub fn mode(&self) -> u8 {
        self.mode
    }

    pub fn ly(&self) -> u8 {
        self.ly
    }

    /// True during HBlank, which is when HDMA is allowed to move a block.
    pub fn in_hblank(&self) -> bool {
        self.lcd_on() && self.mode == MODE_HBLANK
    }

    pub fn step(&mut self, t: u32) {
        if !self.lcd_on() {
            return;
        }
        for _ in 0..t / 4 {
            self.tick4();
        }
    }

    fn tick4(&mut self) {
        self.dots += 4;

        match self.mode {
            MODE_OAM if self.dots >= 80 => {
                self.set_mode(MODE_DRAW);
            }
            MODE_DRAW if self.dots >= 80 + self.draw_length() => {
                self.render_scanline();
                self.set_mode(MODE_HBLANK);
            }
            MODE_HBLANK if self.dots >= DOTS_PER_LINE => {
                self.dots -= DOTS_PER_LINE;
                self.advance_line();
                if self.ly == SCREEN_H as u8 {
                    self.set_mode(MODE_VBLANK);
                    self.irq |= IRQ_VBLANK;
                    self.frame_ready = true;
                } else {
                    self.set_mode(MODE_OAM);
                }
            }
            MODE_VBLANK if self.dots >= DOTS_PER_LINE => {
                self.dots -= DOTS_PER_LINE;
                self.advance_line();
                if self.ly == 0 {
                    self.window_line = 0;
                    self.set_mode(MODE_OAM);
                }
            }
            _ => {}
        }
        self.update_stat_line();
    }

    /// Mode 3 stretches for fine scroll, an active window, and each sprite fetched.
    fn draw_length(&self) -> u32 {
        let mut len = 172 + (self.scx % 8) as u32;
        if self.lcdc & 0x20 != 0 && self.wy <= self.ly && self.wx < 167 {
            len += 6;
        }
        len += 6 * self.visible_sprites().len() as u32;
        len.min(289)
    }

    fn advance_line(&mut self) {
        self.ly = (self.ly + 1) % LINES_PER_FRAME;
        if self.ly == 0 {
            self.window_line = 0;
        }
    }

    fn set_mode(&mut self, mode: u8) {
        self.mode = mode;
        self.stat = (self.stat & 0xfc) | mode;
    }

    /// STAT's interrupt line is level-triggered internally; only its rise fires.
    fn update_stat_line(&mut self) {
        let coincidence = self.ly == self.lyc;
        if coincidence {
            self.stat |= 0x04;
        } else {
            self.stat &= !0x04;
        }

        let line = (self.stat & 0x40 != 0 && coincidence)
            || (self.stat & 0x20 != 0 && self.mode == MODE_OAM)
            || (self.stat & 0x10 != 0 && self.mode == MODE_VBLANK)
            || (self.stat & 0x08 != 0 && self.mode == MODE_HBLANK);

        if line && !self.stat_line {
            self.irq |= IRQ_STAT;
        }
        self.stat_line = line;
    }

    // ---- register access -------------------------------------------------

    pub fn read_reg(&self, addr: u16) -> u8 {
        match addr {
            0xff40 => self.lcdc,
            0xff41 => self.stat | 0x80,
            0xff42 => self.scy,
            0xff43 => self.scx,
            0xff44 => self.ly,
            0xff45 => self.lyc,
            0xff47 => self.bgp,
            0xff48 => self.obp0,
            0xff49 => self.obp1,
            0xff4a => self.wy,
            0xff4b => self.wx,
            0xff4f if self.cgb => 0xfe | self.vram_bank as u8,
            0xff68 if self.cgb => self.bcps | 0x40,
            0xff69 if self.cgb => self.bg_pal[(self.bcps & 0x3f) as usize],
            0xff6a if self.cgb => self.ocps | 0x40,
            0xff6b if self.cgb => self.obj_pal[(self.ocps & 0x3f) as usize],
            0xff6c if self.cgb => self.opri | 0xfe,
            _ => 0xff,
        }
    }

    pub fn write_reg(&mut self, addr: u16, value: u8) {
        match addr {
            0xff40 => {
                let was_on = self.lcd_on();
                self.lcdc = value;
                if was_on && !self.lcd_on() {
                    // Switching the LCD off parks it at the top of the frame.
                    self.ly = 0;
                    self.dots = 0;
                    self.window_line = 0;
                    self.set_mode(MODE_HBLANK);
                    self.stat_line = false;
                    let blank = self.shade(0);
                    self.framebuffer.fill(blank);
                    self.frame_ready = true;
                } else if !was_on && self.lcd_on() {
                    self.dots = 0;
                    self.ly = 0;
                    self.set_mode(MODE_OAM);
                }
            }
            0xff41 => self.stat = (value & 0x78) | (self.stat & 0x07),
            0xff42 => self.scy = value,
            0xff43 => self.scx = value,
            0xff44 => {}
            0xff45 => self.lyc = value,
            0xff47 => self.bgp = value,
            0xff48 => self.obp0 = value,
            0xff49 => self.obp1 = value,
            0xff4a => self.wy = value,
            0xff4b => self.wx = value,
            0xff4f if self.cgb => self.vram_bank = (value & 1) as usize,
            0xff68 if self.cgb => self.bcps = value & 0xbf,
            0xff69 if self.cgb => {
                self.bg_pal[(self.bcps & 0x3f) as usize] = value;
                if self.bcps & 0x80 != 0 {
                    self.bcps = (self.bcps & 0x80) | ((self.bcps + 1) & 0x3f);
                }
            }
            0xff6a if self.cgb => self.ocps = value & 0xbf,
            0xff6b if self.cgb => {
                self.obj_pal[(self.ocps & 0x3f) as usize] = value;
                if self.ocps & 0x80 != 0 {
                    self.ocps = (self.ocps & 0x80) | ((self.ocps + 1) & 0x3f);
                }
            }
            0xff6c if self.cgb => self.opri = value & 1,
            _ => {}
        }
    }

    pub fn read_vram(&self, addr: u16) -> u8 {
        self.vram[self.vram_bank * 0x2000 + (addr as usize - 0x8000)]
    }

    pub fn write_vram(&mut self, addr: u16, value: u8) {
        self.vram[self.vram_bank * 0x2000 + (addr as usize - 0x8000)] = value;
    }

    /// Bank-explicit access, used by HDMA which always targets the current bank.
    pub fn vram_byte(&self, bank: usize, offset: usize) -> u8 {
        self.vram[bank * 0x2000 + offset]
    }

    pub fn vram_bank(&self) -> usize {
        self.vram_bank
    }

    pub fn read_oam(&self, addr: u16) -> u8 {
        self.oam[addr as usize - 0xfe00]
    }

    pub fn write_oam(&mut self, addr: u16, value: u8) {
        self.oam[addr as usize - 0xfe00] = value;
    }

    // ---- rendering -------------------------------------------------------

    fn shade(&self, index: u8) -> u32 {
        self.dmg_palette[(index & 3) as usize]
    }

    /// Expand a 15-bit CGB colour, lifting it out of the washed-out linear mapping
    /// that a naive 5-to-8-bit shift produces on a modern backlit display.
    fn cgb_color(pal: &[u8; 64], palette: usize, index: usize) -> u32 {
        let off = palette * 8 + index * 2;
        let raw = u16::from_le_bytes([pal[off], pal[off + 1]]);
        let r = (raw & 0x1f) as u32;
        let g = ((raw >> 5) & 0x1f) as u32;
        let b = ((raw >> 10) & 0x1f) as u32;

        let cr = ((r * 26 + g * 4 + b * 2) / 32).min(31);
        let cg = ((g * 24 + b * 8) / 32).min(31);
        let cb = ((r * 6 + g * 4 + b * 22) / 32).min(31);
        ((cr * 255 / 31) << 16) | ((cg * 255 / 31) << 8) | (cb * 255 / 31)
    }

    /// OAM entries that intersect the current line, already in draw order
    /// (later entries paint first so earlier ones win).
    fn visible_sprites(&self) -> Vec<usize> {
        if self.lcdc & 0x02 == 0 {
            return Vec::new();
        }
        let height: i32 = if self.lcdc & 0x04 != 0 { 16 } else { 8 };
        let line = self.ly as i32;

        let mut found: Vec<usize> = Vec::with_capacity(10);
        for i in 0..40 {
            let y = self.oam[i * 4] as i32 - 16;
            if line >= y && line < y + height {
                found.push(i);
                if found.len() == 10 {
                    break;
                }
            }
        }

        // DMG resolves overlap by X first; CGB uses OAM order unless OPRI says otherwise.
        if !self.cgb || self.opri & 1 != 0 {
            found.sort_by_key(|&i| (self.oam[i * 4 + 1], i));
        }
        found.reverse();
        found
    }

    fn render_scanline(&mut self) {
        let line = self.ly as usize;
        if line >= SCREEN_H {
            return;
        }
        self.render_background(line);
        self.render_sprites(line);
    }

    fn render_background(&mut self, line: usize) {
        let row = &mut self.framebuffer[line * SCREEN_W..(line + 1) * SCREEN_W];

        // On DMG, LCDC bit 0 blanks background and window entirely.
        // On CGB the same bit only drops their priority over sprites.
        if !self.cgb && self.lcdc & 0x01 == 0 {
            row.fill(self.dmg_palette[0]);
            self.bg_line = [0; SCREEN_W];
            return;
        }

        let window_enabled = self.lcdc & 0x20 != 0 && self.wy as usize <= line && self.wx <= 166;
        let mut window_used = false;

        for (x, pixel) in row.iter_mut().enumerate() {
            let in_window = window_enabled && x + 7 >= self.wx as usize;

            let (map_base, tx, ty, fine_x, fine_y) = if in_window {
                window_used = true;
                let wx = x + 7 - self.wx as usize;
                let wy = self.window_line as usize;
                let base = if self.lcdc & 0x40 != 0 {
                    0x1c00
                } else {
                    0x1800
                };
                (base, wx / 8, wy / 8, wx % 8, wy % 8)
            } else {
                let bx = (x + self.scx as usize) & 0xff;
                let by = (line + self.scy as usize) & 0xff;
                let base = if self.lcdc & 0x08 != 0 {
                    0x1c00
                } else {
                    0x1800
                };
                (base, bx / 8, by / 8, bx % 8, by % 8)
            };

            let map_index = map_base + ty * 32 + tx;
            let tile = self.vram[map_index];
            let attr = if self.cgb {
                self.vram[0x2000 + map_index]
            } else {
                0
            };

            let bank = ((attr >> 3) & 1) as usize;
            let palette = (attr & 0x07) as usize;
            let flip_x = attr & 0x20 != 0;
            let flip_y = attr & 0x40 != 0;

            let tile_addr = if self.lcdc & 0x10 != 0 {
                tile as usize * 16
            } else {
                (0x1000 + (tile as i8 as i32) * 16) as usize
            };

            let py = if flip_y { 7 - fine_y } else { fine_y };
            let px = if flip_x { 7 - fine_x } else { fine_x };

            let base = bank * 0x2000 + tile_addr + py * 2;
            let lo = self.vram[base];
            let hi = self.vram[base + 1];
            let bit = 7 - px;
            let color = ((hi >> bit) & 1) << 1 | ((lo >> bit) & 1);

            self.bg_line[x] = color | (attr & 0x80);

            *pixel = if self.cgb {
                Self::cgb_color(&self.bg_pal, palette, color as usize)
            } else {
                self.dmg_palette[((self.bgp >> (color * 2)) & 3) as usize]
            };
        }

        // The window's own line counter only moves on lines where it drew.
        if window_used {
            self.window_line = self.window_line.wrapping_add(1);
            self.window_active = true;
        }
    }

    fn render_sprites(&mut self, line: usize) {
        let sprites = self.visible_sprites();
        if sprites.is_empty() {
            return;
        }
        let height: i32 = if self.lcdc & 0x04 != 0 { 16 } else { 8 };
        // CGB only: LCDC bit 0 clear lets every sprite through regardless of priority.
        let bg_master_priority = !self.cgb || self.lcdc & 0x01 != 0;

        for &i in &sprites {
            let y = self.oam[i * 4] as i32 - 16;
            let x = self.oam[i * 4 + 1] as i32 - 8;
            let tile = self.oam[i * 4 + 2];
            let attr = self.oam[i * 4 + 3];

            let flip_x = attr & 0x20 != 0;
            let flip_y = attr & 0x40 != 0;
            let behind_bg = attr & 0x80 != 0;

            let mut row_in_sprite = line as i32 - y;
            if flip_y {
                row_in_sprite = height - 1 - row_in_sprite;
            }

            // 8x16 sprites ignore the low bit of the tile number.
            let tile_index = if height == 16 { tile & 0xfe } else { tile } as usize;
            let bank = if self.cgb {
                ((attr >> 3) & 1) as usize
            } else {
                0
            };
            let base = bank * 0x2000 + tile_index * 16 + row_in_sprite as usize * 2;
            let lo = self.vram[base];
            let hi = self.vram[base + 1];

            for px in 0..8i32 {
                let screen_x = x + px;
                if screen_x < 0 || screen_x >= SCREEN_W as i32 {
                    continue;
                }
                let sx = screen_x as usize;
                let bit = if flip_x { px } else { 7 - px };
                let color = ((hi >> bit) & 1) << 1 | ((lo >> bit) & 1);
                if color == 0 {
                    continue; // colour 0 is transparent for sprites
                }

                let bg_color = self.bg_line[sx] & 3;
                let bg_has_priority =
                    bg_master_priority && (behind_bg || self.bg_line[sx] & 0x80 != 0);
                if bg_has_priority && bg_color != 0 {
                    continue;
                }

                self.framebuffer[line * SCREEN_W + sx] = if self.cgb {
                    Self::cgb_color(&self.obj_pal, (attr & 0x07) as usize, color as usize)
                } else {
                    let pal = if attr & 0x10 != 0 {
                        self.obp1
                    } else {
                        self.obp0
                    };
                    self.dmg_palette[((pal >> (color * 2)) & 3) as usize]
                };
            }
        }
    }
}
