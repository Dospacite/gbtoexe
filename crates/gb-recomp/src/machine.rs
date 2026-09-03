//! The host side of a converted game: hardware, code cache, and the loop that
//! keeps handing control back to translated code.

use std::ffi::c_void;

use gb_hw::{Button, Cartridge, CartridgeError, Config, Mmu};

use crate::abi::*;
use crate::exec::{BlockInfo, BlockKey, CodeCache, DEFAULT_ARENA};
use crate::translate::{translate_block, Context};

/// Everything the helpers reach through `state.host`.
///
/// Kept in its own allocation so a helper can hold `&mut GbState` and
/// `&mut Host` at once without the two overlapping.
pub struct Host {
    pub mmu: Mmu,
    pub cache: CodeCache,
    pub jump_table: Vec<JumpEntry>,
    /// Pages outside ROM that some translated block was built from.
    code_pages: [bool; 256],
}

pub struct Emu {
    pub state: Box<GbState>,
    pub host: Box<Host>,
    trampoline: Trampoline,
    ctx: Context,
    pub cgb: bool,
    /// Set by a `STOP` that was not a speed switch.
    pub stopped: bool,
    /// Non-fatal diagnostics, e.g. an opcode real hardware locks up on.
    pub fault: Option<String>,
}

// ---- helpers called from translated code ---------------------------------

/// Hand the hardware every cycle the game has run since it was last told.
unsafe fn settle(state: &mut GbState, host: &mut Host, extra: u32) {
    let owed = std::mem::take(&mut state.pending) + extra;
    if owed != 0 {
        host.mmu.tick(owed);
    }
}

unsafe extern "win64" fn helper_read(state: *mut GbState, addr: u32) -> u8 {
    let s = &mut *state;
    let host = &mut *(s.host as *mut Host);
    settle(s, host, 4);
    host.mmu.read(addr as u16)
}

unsafe extern "win64" fn helper_write(state: *mut GbState, addr: u32, value: u32) {
    let s = &mut *state;
    let host = &mut *(s.host as *mut Host);
    settle(s, host, 4);

    let addr = addr as u16;
    host.mmu.write(addr, value as u8);

    if addr < 0x8000 {
        // A mapper write can change what every banked address means, so every
        // cached indirect-jump target has to be re-proved.
        s.bank_gen = s.bank_gen.wrapping_add(1);
    } else if addr >= 0xa000 {
        let page = (addr >> 8) as usize;
        if host.code_pages[page] {
            host.cache.invalidate_page(page as u8);
            host.code_pages[page] = false;
            s.bank_gen = s.bank_gen.wrapping_add(1);
        }
    }
}

unsafe extern "win64" fn helper_sync(state: *mut GbState) {
    let s = &mut *state;
    let host = &mut *(s.host as *mut Host);
    settle(s, host, 0);

    if s.ime_delay > 0 {
        s.ime_delay -= 1;
        if s.ime_delay == 0 {
            s.ime = 1;
        }
    }

    // Only leave for an interrupt that can actually be taken; a pending bit with
    // interrupts disabled is the normal state of affairs and must not cost a
    // trip back to the host on every block.
    let interrupt = s.ime != 0 && host.mmu.pending_interrupts() != 0;
    if interrupt || host.mmu.ppu.frame_ready {
        s.exit = EXIT_YIELD;
    }
}

unsafe extern "win64" fn helper_halt(state: *mut GbState) {
    let s = &mut *state;
    let host = &mut *(s.host as *mut Host);
    settle(s, host, 0);
    s.halted = 1;
    s.exit = EXIT_HALT;
}

unsafe extern "win64" fn helper_stop(state: *mut GbState) {
    let s = &mut *state;
    let host = &mut *(s.host as *mut Host);
    settle(s, host, 0);
    // On a Game Boy Color this is usually a speed switch, and execution resumes.
    s.exit = if host.mmu.stop() {
        EXIT_NONE
    } else {
        EXIT_STOP
    };
}

impl Emu {
    pub fn new(rom: Vec<u8>, config: &Config) -> Result<Emu, CartridgeError> {
        Self::with_arena(rom, config, DEFAULT_ARENA)
    }

    pub fn with_arena(
        rom: Vec<u8>,
        config: &Config,
        arena_bytes: usize,
    ) -> Result<Emu, CartridgeError> {
        let (mmu, cgb) = gb_hw::build(rom, config)?;
        let ctx = Context {
            lower_window_fixed: lower_window_is_fixed(&mmu.cart),
        };
        let cache = CodeCache::new(arena_bytes)
            .ok_or_else(|| CartridgeError("could not reserve memory for translated code".into()))?;
        let trampoline = cache.trampoline();

        let mut host = Box::new(Host {
            mmu,
            cache,
            jump_table: vec![JumpEntry::EMPTY; JUMP_CACHE_SLOTS],
            code_pages: [false; 256],
        });

        let mut state = Box::new(if cgb {
            GbState::new_cgb()
        } else {
            GbState::default()
        });
        state.read8 = Some(helper_read);
        state.write8 = Some(helper_write);
        state.sync = Some(helper_sync);
        state.halt = Some(helper_halt);
        state.stop = Some(helper_stop);
        state.jump_table = host.jump_table.as_mut_ptr();
        state.host = (&mut *host) as *mut Host as *mut c_void;

        Ok(Emu {
            state,
            host,
            trampoline,
            ctx,
            cgb,
            stopped: false,
            fault: None,
        })
    }

    pub fn context(&self) -> Context {
        self.ctx
    }

    /// Translate the block at `key` if it is not already in the cache.
    fn ensure_block(&mut self, key: BlockKey) -> BlockInfo {
        if let Some(info) = self.host.cache.lookup(key) {
            return info;
        }
        let Host {
            mmu,
            cache,
            code_pages,
            ..
        } = &mut *self.host;
        let block = translate_block(key.addr, self.ctx, &|a| mmu.peek(a));

        // Remember which pages this block was built from, so a write to any of
        // them can throw it away.
        if key.addr >= 0x8000 {
            let mut page = key.addr >> 8;
            let last = block.end.wrapping_sub(1) >> 8;
            loop {
                code_pages[page as usize & 0xff] = true;
                if page == last {
                    break;
                }
                page = page.wrapping_add(1);
            }
        }
        cache.install(key, block)
    }

    /// Native entry point for a Game Boy address, translating on the spot if
    /// this is the first time the game has gone there.
    fn entry_for(&mut self, pc: u16) -> *const u8 {
        let bank = self.host.mmu.bank_at(pc);
        let info = self.ensure_block(BlockKey { bank, addr: pc });
        let target = self.host.cache.host_ptr(info.offset);

        // Prime the inline cache so the next indirect jump here stays native.
        let slot = (pc as u32 & JUMP_CACHE_MASK) as usize;
        self.host.jump_table[slot] = JumpEntry {
            pc: pc as u32,
            gen: self.state.bank_gen,
            target: target as u64,
        };
        target
    }

    /// Push the program counter and vector to the highest-priority interrupt.
    fn service_interrupt(&mut self) {
        let pending = self.host.mmu.pending_interrupts();
        if pending == 0 {
            return;
        }
        self.state.halted = 0;
        self.stopped = false;
        if self.state.ime == 0 {
            return;
        }

        self.state.ime = 0;
        let bit = pending.trailing_zeros() as u8;
        self.host.mmu.ack_interrupt(bit);

        self.host.mmu.tick(8);
        let pc = self.state.pc;
        self.state.sp = self.state.sp.wrapping_sub(1);
        self.host.mmu.tick(4);
        self.host.mmu.write(self.state.sp, (pc >> 8) as u8);
        self.state.sp = self.state.sp.wrapping_sub(1);
        self.host.mmu.tick(4);
        self.host.mmu.write(self.state.sp, pc as u8);

        self.state.pc = 0x0040 + bit as u16 * 8;
        self.host.mmu.tick(4);
    }

    fn flush_pending(&mut self) {
        let owed = std::mem::take(&mut self.state.pending);
        if owed != 0 {
            self.host.mmu.tick(owed);
        }
    }

    /// Run until the LCD finishes the next frame.
    pub fn run_frame(&mut self) {
        self.host.mmu.ppu.frame_ready = false;
        // A game that switches the LCD off never reaches vblank, so cap the work.
        let budget = gb_hw::FRAME_CYCLES as u64 * 4;
        let start = self.host.mmu.total_cycles();

        while !self.host.mmu.ppu.frame_ready {
            if self.host.mmu.total_cycles() - start > budget {
                break;
            }
            self.step();
        }
    }

    /// Advance by one block, or by one idle cycle if the CPU is not running.
    pub fn step(&mut self) {
        self.flush_pending();
        self.service_interrupt();

        if self.state.halted != 0 || self.stopped {
            self.host.mmu.tick(4);
            return;
        }
        if self.fault.is_some() {
            self.host.mmu.tick(4);
            return;
        }

        let entry = self.entry_for(self.state.pc);
        self.state.exit = EXIT_NONE;
        unsafe { (self.trampoline)(&mut *self.state, entry) };

        match self.state.exit {
            EXIT_STOP => self.stopped = true,
            EXIT_ILLEGAL => {
                let opcode = self.host.mmu.peek(self.state.pc);
                self.fault = Some(format!(
                    "the game reached {:#06x}, where {opcode:#04x} is not an instruction \
                     the hardware implements",
                    self.state.pc
                ));
            }
            _ => {}
        }
        self.flush_pending();
    }

    // ---- front-end surface ----------------------------------------------

    pub fn set_button(&mut self, button: Button, pressed: bool) {
        self.host.mmu.joypad.set(button, pressed);
    }

    pub fn framebuffer(&self) -> &[u32] {
        &self.host.mmu.ppu.framebuffer
    }

    pub fn drain_audio(&mut self, out: &mut Vec<f32>) {
        self.host.mmu.apu.drain(out);
    }

    pub fn info(&self) -> &gb_hw::CartridgeInfo {
        self.host.mmu.cart.info()
    }

    pub fn has_battery(&self) -> bool {
        self.host.mmu.cart.has_battery()
    }

    pub fn save_dirty(&self) -> bool {
        self.host.mmu.cart.ram_dirty
    }

    pub fn clear_save_dirty(&mut self) {
        self.host.mmu.cart.ram_dirty = false;
    }

    pub fn save_data(&self) -> Vec<u8> {
        self.host.mmu.cart.save_data()
    }

    pub fn load_save(&mut self, data: &[u8]) {
        self.host.mmu.cart.load_save(data);
    }

    /// Translate a set of entry points before the game starts running, so the
    /// common path is already native the first time it is reached.
    pub fn pretranslate(&mut self, keys: &[BlockKey]) -> usize {
        let mut done = 0;
        for &key in keys {
            if self.host.cache.lookup(key).is_none() {
                self.ensure_block(key);
                done += 1;
            }
        }
        done
    }

    pub fn install_translated(&mut self, blob: &[u8], map: &[(BlockKey, u32, u32)]) {
        self.host.cache.import(blob, map);
    }

    pub fn blocks_translated(&self) -> usize {
        self.host.cache.block_count()
    }

    pub fn code_bytes(&self) -> usize {
        self.host.cache.used()
    }
}

/// Whether anything can remap 0x0000-0x3FFF. Only MBC1 can, and only on
/// cartridges big enough for the high bank bits to mean something.
pub fn lower_window_is_fixed(cart: &Cartridge) -> bool {
    let info = cart.info();
    !(info.mbc == gb_hw::MbcKind::Mbc1 && info.rom_banks > 32)
}
