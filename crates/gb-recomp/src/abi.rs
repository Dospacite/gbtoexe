//! The contract between translated machine code and the host runtime.
//!
//! Translated code keeps the Game Boy's register file in memory at `[rbp + n]`
//! rather than pinning it to host registers. Every field below therefore has a
//! fixed offset that the encoder bakes into instructions, and the layout is
//! checked against the real struct at compile time.

use std::ffi::c_void;

/// Register pairs are stored low byte first so a 16-bit load reads `BC`, `DE`
/// or `HL` directly with no shifting.
pub const OFF_C: i32 = 0x00;
pub const OFF_B: i32 = 0x01;
pub const OFF_BC: i32 = 0x00;
pub const OFF_E: i32 = 0x02;
pub const OFF_D: i32 = 0x03;
pub const OFF_DE: i32 = 0x02;
pub const OFF_L: i32 = 0x04;
pub const OFF_H: i32 = 0x05;
pub const OFF_HL: i32 = 0x04;
pub const OFF_SP: i32 = 0x06;
pub const OFF_PC: i32 = 0x08;
pub const OFF_A: i32 = 0x0a;
pub const OFF_F: i32 = 0x0b;
pub const OFF_IME: i32 = 0x0c;
pub const OFF_IME_DELAY: i32 = 0x0d;
pub const OFF_HALTED: i32 = 0x0e;
/// Non-zero tells the block to stop and hand control back to the host loop.
pub const OFF_EXIT: i32 = 0x0f;
/// Cycles the game has run that the hardware has not been told about yet.
pub const OFF_PENDING: i32 = 0x10;
/// Bumped on every mapper write; invalidates inline jump-cache entries.
pub const OFF_BANK_GEN: i32 = 0x14;

pub const OFF_READ8: i32 = 0x18;
pub const OFF_WRITE8: i32 = 0x20;
pub const OFF_SYNC: i32 = 0x28;
pub const OFF_HALT: i32 = 0x30;
pub const OFF_STOP: i32 = 0x38;
pub const OFF_JUMP_TABLE: i32 = 0x40;
pub const OFF_HOST: i32 = 0x48;
/// A word of workspace for translated code, used where a value has to survive
/// a helper call (the low byte of a return address, for instance).
pub const OFF_SCRATCH: i32 = 0x50;

/// Every offset the encoder uses must fit in a signed 8-bit displacement,
/// which keeps generated instructions one byte shorter.
const _: () = assert!(OFF_SCRATCH < 128);

/// Reason a block returned to the host.
pub const EXIT_NONE: u8 = 0;
/// Needs the block at `pc`; either not translated yet or the inline cache missed.
pub const EXIT_DISPATCH: u8 = 1;
pub const EXIT_HALT: u8 = 2;
pub const EXIT_STOP: u8 = 3;
/// The hardware wants attention: an interrupt is pending, or the frame ended.
pub const EXIT_YIELD: u8 = 4;
/// The game reached an opcode the hardware has no wiring for, and would wedge.
pub const EXIT_ILLEGAL: u8 = 5;

pub type Read8 = unsafe extern "win64" fn(*mut GbState, u32) -> u8;
pub type Write8 = unsafe extern "win64" fn(*mut GbState, u32, u32);
pub type Sync = unsafe extern "win64" fn(*mut GbState);
/// Entry point into translated code: `(state, first block)`.
pub type Trampoline = unsafe extern "win64" fn(*mut GbState, *const u8);

/// One slot of the inline jump cache that translated code consults before
/// falling back to the host for an indirect branch.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JumpEntry {
    pub pc: u32,
    pub gen: u32,
    pub target: u64,
}

impl JumpEntry {
    pub const EMPTY: JumpEntry = JumpEntry {
        // No real program counter is this large, so the slot can never match.
        pc: u32::MAX,
        gen: 0,
        target: 0,
    };
}

/// Number of slots in the inline jump cache. Must be a power of two.
pub const JUMP_CACHE_SLOTS: usize = 4096;
pub const JUMP_CACHE_MASK: u32 = JUMP_CACHE_SLOTS as u32 - 1;

/// The machine state translated code operates on.
///
/// `rbp` points here for the entire time translated code is running.
#[repr(C)]
pub struct GbState {
    pub c: u8,
    pub b: u8,
    pub e: u8,
    pub d: u8,
    pub l: u8,
    pub h: u8,
    pub sp: u16,
    pub pc: u16,
    pub a: u8,
    pub f: u8,
    pub ime: u8,
    pub ime_delay: u8,
    pub halted: u8,
    pub exit: u8,
    pub pending: u32,
    pub bank_gen: u32,

    pub read8: Option<Read8>,
    pub write8: Option<Write8>,
    pub sync: Option<Sync>,
    pub halt: Option<Sync>,
    pub stop: Option<Sync>,
    pub jump_table: *mut JumpEntry,
    /// Opaque pointer back to the Rust side that owns the hardware.
    pub host: *mut c_void,
    pub scratch: u32,
    _pad: u32,
}

impl Default for GbState {
    fn default() -> Self {
        GbState {
            c: 0x13,
            b: 0x00,
            e: 0xd8,
            d: 0x00,
            l: 0x4d,
            h: 0x01,
            sp: 0xfffe,
            pc: 0x0100,
            a: 0x01,
            f: 0xb0,
            ime: 0,
            ime_delay: 0,
            halted: 0,
            exit: EXIT_NONE,
            pending: 0,
            bank_gen: 0,
            read8: None,
            write8: None,
            sync: None,
            halt: None,
            stop: None,
            jump_table: std::ptr::null_mut(),
            host: std::ptr::null_mut(),
            scratch: 0,
            _pad: 0,
        }
    }
}

impl GbState {
    /// Post-boot register state for a Game Boy Color. `A = 0x11` is how games
    /// tell the two machines apart.
    pub fn new_cgb() -> Self {
        GbState {
            c: 0x00,
            b: 0x00,
            e: 0x56,
            d: 0xff,
            l: 0x0d,
            h: 0x00,
            a: 0x11,
            f: 0x80,
            ..GbState::default()
        }
    }
}

/// Compile-time proof that the constants above describe the real struct.
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(GbState, c) == OFF_C as usize);
    assert!(offset_of!(GbState, b) == OFF_B as usize);
    assert!(offset_of!(GbState, e) == OFF_E as usize);
    assert!(offset_of!(GbState, d) == OFF_D as usize);
    assert!(offset_of!(GbState, l) == OFF_L as usize);
    assert!(offset_of!(GbState, h) == OFF_H as usize);
    assert!(offset_of!(GbState, sp) == OFF_SP as usize);
    assert!(offset_of!(GbState, pc) == OFF_PC as usize);
    assert!(offset_of!(GbState, a) == OFF_A as usize);
    assert!(offset_of!(GbState, f) == OFF_F as usize);
    assert!(offset_of!(GbState, ime) == OFF_IME as usize);
    assert!(offset_of!(GbState, ime_delay) == OFF_IME_DELAY as usize);
    assert!(offset_of!(GbState, halted) == OFF_HALTED as usize);
    assert!(offset_of!(GbState, exit) == OFF_EXIT as usize);
    assert!(offset_of!(GbState, pending) == OFF_PENDING as usize);
    assert!(offset_of!(GbState, bank_gen) == OFF_BANK_GEN as usize);
    assert!(offset_of!(GbState, read8) == OFF_READ8 as usize);
    assert!(offset_of!(GbState, write8) == OFF_WRITE8 as usize);
    assert!(offset_of!(GbState, sync) == OFF_SYNC as usize);
    assert!(offset_of!(GbState, halt) == OFF_HALT as usize);
    assert!(offset_of!(GbState, stop) == OFF_STOP as usize);
    assert!(offset_of!(GbState, jump_table) == OFF_JUMP_TABLE as usize);
    assert!(offset_of!(GbState, host) == OFF_HOST as usize);
    assert!(offset_of!(GbState, scratch) == OFF_SCRATCH as usize);
};

/// GB flag register bits.
pub const FLAG_Z: u8 = 0x80;
pub const FLAG_N: u8 = 0x40;
pub const FLAG_H: u8 = 0x20;
pub const FLAG_C: u8 = 0x10;
