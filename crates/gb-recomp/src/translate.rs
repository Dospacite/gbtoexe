//! SM83 basic block in, x86-64 machine code out.
//!
//! One Game Boy instruction becomes a short run of host instructions operating
//! on the state at `[rbp + n]`. Nothing is decoded at run time: control flow that
//! is known statically becomes a direct `jmp`, and control flow that is not goes
//! through an inline cache before it will consider asking the host for help.
//!
//! Timing is preserved exactly. Cycles the game owes the hardware accumulate in
//! `state.pending` and are handed over at every point the hardware could be
//! observed — any memory access, and the end of every block.

use crate::abi::*;
use crate::decode::{decode, Flow, Insn};
use crate::x64::*;

/// Bytes of stack a helper call needs: 32 for the Windows shadow space, plus 8
/// to bring RSP back to a 16-byte boundary at the call.
const CALL_FRAME: u32 = 40;

/// A jump out of a block whose destination block may not exist yet.
#[derive(Debug, Clone, Copy)]
pub struct Link {
    /// Offset of the rel32 field inside this block's code.
    pub site: usize,
    /// Game Boy address the jump goes to.
    pub target: u16,
}

pub struct Block {
    pub start: u16,
    /// One past the last byte of the last instruction translated.
    pub end: u16,
    pub code: Vec<u8>,
    pub links: Vec<Link>,
    pub instructions: usize,
}

/// What the translator is allowed to assume about the address space while it
/// works, which decides whether a jump can be linked directly.
#[derive(Debug, Clone, Copy)]
pub struct Context {
    /// True when nothing can remap 0x0000-0x3FFF, so its addresses are stable.
    pub lower_window_fixed: bool,
}

impl Default for Context {
    fn default() -> Self {
        Context {
            lower_window_fixed: true,
        }
    }
}

fn region(addr: u16) -> u8 {
    match addr {
        0x0000..=0x3fff => 0,
        0x4000..=0x7fff => 1,
        _ => 2,
    }
}

struct Emitter {
    asm: Asm,
    ctx: Context,
    links: Vec<Link>,
    /// Cycles run since the hardware was last told, not yet written to state.
    owed: u32,
    /// Sites that jump to this block's shared `ret`.
    exits: Vec<Patch>,
    block_start: u16,
}

/// Where an operand slot lives. Index 6 is `(HL)`.
fn reg_offset(idx: u8) -> i32 {
    match idx {
        0 => OFF_B,
        1 => OFF_C,
        2 => OFF_D,
        3 => OFF_E,
        4 => OFF_H,
        5 => OFF_L,
        _ => OFF_A,
    }
}

/// The flag bit a condition code tests, and whether it must be set.
fn condition(idx: u8) -> (u32, bool) {
    match idx {
        0 => (FLAG_Z as u32, false),
        1 => (FLAG_Z as u32, true),
        2 => (FLAG_C as u32, false),
        _ => (FLAG_C as u32, true),
    }
}

impl Emitter {
    fn new(ctx: Context, block_start: u16) -> Self {
        Emitter {
            asm: Asm::new(),
            ctx,
            links: Vec::new(),
            owed: 0,
            exits: Vec::new(),
            block_start,
        }
    }

    // ---- cycle accounting ------------------------------------------------

    fn charge(&mut self, cycles: u32) {
        self.owed += cycles;
    }

    /// Write accumulated cycles into the state so the hardware can see them.
    fn settle(&mut self) {
        if self.owed != 0 {
            self.asm.alu_imm(Alu::Add, State(OFF_PENDING), self.owed, 4);
            self.owed = 0;
        }
    }

    // ---- helper calls ----------------------------------------------------

    fn call_helper(&mut self, slot: i32) {
        self.settle();
        self.asm.mov_reg(RCX, RBP, 8);
        self.asm.alu_imm(Alu::Sub, Rm::R(RSP), CALL_FRAME, 8);
        self.asm.call_rm(State(slot));
        self.asm.alu_imm(Alu::Add, Rm::R(RSP), CALL_FRAME, 8);
    }

    /// Read from the address in EDX; the byte comes back in AL.
    fn read_mem(&mut self) {
        self.call_helper(OFF_READ8);
    }

    /// Write the byte in R8D to the address in EDX.
    fn write_mem(&mut self) {
        self.call_helper(OFF_WRITE8);
    }

    fn load_addr_imm(&mut self, addr: u16) {
        self.asm.mov_imm32(RDX, addr as u32);
    }

    fn load_addr_hl(&mut self) {
        self.asm.movzx(RDX, State(OFF_HL), 2);
    }

    // ---- operand access --------------------------------------------------

    /// Put operand `idx` in AL.
    fn load_operand(&mut self, idx: u8) {
        if idx == 6 {
            self.load_addr_hl();
            self.read_mem();
        } else {
            self.asm.mov_load(RAX, State(reg_offset(idx)), 1);
        }
    }

    /// Store AL into operand `idx`.
    fn store_operand(&mut self, idx: u8) {
        if idx == 6 {
            self.asm.movzx(R8, Rm::R(RAX), 1);
            self.load_addr_hl();
            self.write_mem();
        } else {
            self.asm.mov_store(State(reg_offset(idx)), RAX, 1);
        }
    }

    // ---- flag construction -----------------------------------------------

    /// Set F from parts: Z from AL being zero, plus whatever is already in ECX.
    /// `n` is the constant N flag for this operation.
    fn flags_from_al(&mut self, extra_in_ecx: bool, n: bool) {
        self.asm.test(Rm::R(RAX), RAX, 1);
        self.asm.setcc(Cc::E, Rm::R(R9));
        self.asm.movzx(R9, Rm::R(R9), 1);
        self.asm.shift_imm(Shift::Shl, Rm::R(R9), 7, 4);
        if !extra_in_ecx {
            self.asm.alu_reg(Alu::Xor, RCX, RCX, 4);
        }
        self.asm.alu_reg(Alu::Or, RCX, R9, 4);
        if n {
            self.asm.alu_imm(Alu::Or, Rm::R(RCX), FLAG_N as u32, 4);
        }
        self.asm.mov_store(State(OFF_F), RCX, 1);
    }

    /// The 8-bit add/subtract family.
    ///
    /// Half-carry and carry both fall out of one identity: the sum with all
    /// carries suppressed is `a ^ b ^ carry_in`, so XOR-ing that against the real
    /// result leaves a 1 exactly where a carry crossed a bit boundary.
    fn alu_add_sub(&mut self, subtract: bool, with_carry: bool, store: bool) {
        // AL holds the operand on entry; A and the carry come from state.
        self.asm.movzx(RCX, Rm::R(RAX), 1); // ecx = operand
        self.asm.movzx(RAX, State(OFF_A), 1); // eax = a
        self.asm.mov_reg(RDX, RAX, 4);
        self.asm.alu_reg(Alu::Xor, RDX, RCX, 4); // edx = a ^ b

        if with_carry {
            self.asm.movzx(R10, State(OFF_F), 1);
            self.asm.shift_imm(Shift::Shr, Rm::R(R10), 4, 4);
            self.asm.alu_imm(Alu::And, Rm::R(R10), 1, 4);
            self.asm.alu_reg(Alu::Xor, RDX, R10, 4); // fold carry-in into the guess
        }

        let op = if subtract { Alu::Sub } else { Alu::Add };
        self.asm.alu_reg(op, RAX, RCX, 4);
        if with_carry {
            self.asm.alu_reg(op, RAX, R10, 4);
        }
        self.asm.alu_reg(Alu::Xor, RDX, RAX, 4); // edx: bit 4 = H, bit 8 = C
        self.asm.alu_imm(Alu::And, Rm::R(RAX), 0xff, 4);

        if store {
            self.asm.mov_store(State(OFF_A), RAX, 1);
        }

        // ecx = H | C
        self.asm.mov_reg(RCX, RDX, 4);
        self.asm.alu_imm(Alu::And, Rm::R(RCX), 0x10, 4);
        self.asm.shift_imm(Shift::Shl, Rm::R(RCX), 1, 4); // 0x10 -> FLAG_H
        self.asm.shift_imm(Shift::Shr, Rm::R(RDX), 8, 4);
        self.asm.alu_imm(Alu::And, Rm::R(RDX), 1, 4);
        self.asm.shift_imm(Shift::Shl, Rm::R(RDX), 4, 4); // -> FLAG_C
        self.asm.alu_reg(Alu::Or, RCX, RDX, 4);

        self.flags_from_al(true, subtract);
    }

    /// AND / OR / XOR, whose flags are fixed apart from Z.
    fn alu_logic(&mut self, op: Alu, half_carry: bool) {
        self.asm.alu_store(op, State(OFF_A), RAX, 1);
        self.asm.mov_load(RAX, State(OFF_A), 1);
        if half_carry {
            self.asm.mov_imm32(RCX, FLAG_H as u32);
        } else {
            self.asm.alu_reg(Alu::Xor, RCX, RCX, 4);
        }
        self.flags_from_al(true, false);
    }

    fn inc_dec_8(&mut self, idx: u8, decrement: bool) {
        self.load_operand(idx);
        // Half-carry is decided by the low nibble before the change.
        self.asm.mov_reg(RCX, RAX, 1);
        self.asm.alu_imm(Alu::And, Rm::R(RCX), 0x0f, 1);
        self.asm
            .alu_imm(Alu::Cmp, Rm::R(RCX), if decrement { 0x00 } else { 0x0f }, 1);
        self.asm.setcc(Cc::E, Rm::R(R9));

        if decrement {
            self.asm.dec(Rm::R(RAX), 1);
        } else {
            self.asm.inc(Rm::R(RAX), 1);
        }
        // INC/DEC leave the carry flag alone.
        self.asm.movzx(RCX, State(OFF_F), 1);
        self.asm.alu_imm(Alu::And, Rm::R(RCX), FLAG_C as u32, 4);
        self.asm.movzx(R9, Rm::R(R9), 1);
        self.asm.shift_imm(Shift::Shl, Rm::R(R9), 5, 4);
        self.asm.alu_reg(Alu::Or, RCX, R9, 4);

        // Storing to (HL) calls out to the host, so build F while AL is still live.
        self.asm.alu_imm(Alu::And, Rm::R(RAX), 0xff, 4);
        self.asm.test(Rm::R(RAX), RAX, 1);
        self.asm.setcc(Cc::E, Rm::R(R9));
        self.asm.movzx(R9, Rm::R(R9), 1);
        self.asm.shift_imm(Shift::Shl, Rm::R(R9), 7, 4);
        self.asm.alu_reg(Alu::Or, RCX, R9, 4);
        if decrement {
            self.asm.alu_imm(Alu::Or, Rm::R(RCX), FLAG_N as u32, 4);
        }
        self.asm.mov_store(State(OFF_F), RCX, 1);

        self.store_operand(idx);
    }

    fn add_hl(&mut self, src_offset: i32) {
        self.asm.movzx(RAX, State(OFF_HL), 2);
        self.asm.movzx(RCX, State(src_offset), 2);
        self.asm.mov_reg(RDX, RAX, 4);
        self.asm.alu_reg(Alu::Xor, RDX, RCX, 4);
        self.asm.alu_reg(Alu::Add, RAX, RCX, 4);
        self.asm.alu_reg(Alu::Xor, RDX, RAX, 4); // bit 12 = H, bit 16 = C
        self.asm.mov_store(State(OFF_HL), RAX, 2);

        self.asm.movzx(RCX, State(OFF_F), 1);
        self.asm.alu_imm(Alu::And, Rm::R(RCX), FLAG_Z as u32, 4); // Z survives
        self.asm.mov_reg(R9, RDX, 4);
        self.asm.alu_imm(Alu::And, Rm::R(R9), 0x1000, 4);
        self.asm.shift_imm(Shift::Shr, Rm::R(R9), 7, 4); // -> FLAG_H
        self.asm.alu_reg(Alu::Or, RCX, R9, 4);
        self.asm.shift_imm(Shift::Shr, Rm::R(RDX), 16, 4);
        self.asm.alu_imm(Alu::And, Rm::R(RDX), 1, 4);
        self.asm.shift_imm(Shift::Shl, Rm::R(RDX), 4, 4); // -> FLAG_C
        self.asm.alu_reg(Alu::Or, RCX, RDX, 4);
        self.asm.mov_store(State(OFF_F), RCX, 1);
    }

    /// `ADD SP,e8` and `LD HL,SP+e8`: the flags come from the low byte only.
    fn sp_plus_offset(&mut self, offset: i8, dest: i32) {
        self.asm.movzx(RAX, State(OFF_SP), 2);
        self.asm.mov_imm32(RCX, offset as u8 as u32);
        self.asm.mov_reg(RDX, RAX, 4);
        self.asm.alu_imm(Alu::And, Rm::R(RDX), 0xff, 4);
        self.asm.mov_reg(R9, RDX, 4);
        self.asm.alu_reg(Alu::Xor, R9, RCX, 4);
        self.asm.alu_reg(Alu::Add, RDX, RCX, 4);
        self.asm.alu_reg(Alu::Xor, R9, RDX, 4); // bit 4 = H, bit 8 = C

        self.asm.mov_reg(RCX, R9, 4);
        self.asm.alu_imm(Alu::And, Rm::R(RCX), 0x10, 4);
        self.asm.shift_imm(Shift::Shl, Rm::R(RCX), 1, 4);
        self.asm.shift_imm(Shift::Shr, Rm::R(R9), 8, 4);
        self.asm.alu_imm(Alu::And, Rm::R(R9), 1, 4);
        self.asm.shift_imm(Shift::Shl, Rm::R(R9), 4, 4);
        self.asm.alu_reg(Alu::Or, RCX, R9, 4);
        self.asm.mov_store(State(OFF_F), RCX, 1); // Z and N are always clear

        self.asm.mov_imm32(RCX, offset as i32 as u32);
        self.asm.alu_reg(Alu::Add, RAX, RCX, 4);
        self.asm.mov_store(State(dest), RAX, 2);
    }

    fn push16(&mut self, high: i32, low: i32) {
        self.asm.dec(State(OFF_SP), 2);
        self.asm.movzx(R8, State(high), 1);
        self.asm.movzx(RDX, State(OFF_SP), 2);
        self.write_mem();
        self.asm.dec(State(OFF_SP), 2);
        self.asm.movzx(R8, State(low), 1);
        self.asm.movzx(RDX, State(OFF_SP), 2);
        self.write_mem();
    }

    fn push_imm16(&mut self, value: u16) {
        self.asm.dec(State(OFF_SP), 2);
        self.asm.mov_imm32(R8, (value >> 8) as u32);
        self.asm.movzx(RDX, State(OFF_SP), 2);
        self.write_mem();
        self.asm.dec(State(OFF_SP), 2);
        self.asm.mov_imm32(R8, (value & 0xff) as u32);
        self.asm.movzx(RDX, State(OFF_SP), 2);
        self.write_mem();
    }

    fn pop16(&mut self, high: i32, low: i32) {
        self.asm.movzx(RDX, State(OFF_SP), 2);
        self.read_mem();
        self.asm.mov_store(State(low), RAX, 1);
        self.asm.inc(State(OFF_SP), 2);
        self.asm.movzx(RDX, State(OFF_SP), 2);
        self.read_mem();
        self.asm.mov_store(State(high), RAX, 1);
        self.asm.inc(State(OFF_SP), 2);
    }

    /// Pop a return address off the Game Boy stack, leaving it in EAX.
    fn pop_pc(&mut self) {
        self.asm.movzx(RDX, State(OFF_SP), 2);
        self.read_mem();
        self.asm.movzx(RAX, Rm::R(RAX), 1);
        self.asm.mov_store(State(OFF_SCRATCH), RAX, 4);
        self.asm.inc(State(OFF_SP), 2);
        self.asm.movzx(RDX, State(OFF_SP), 2);
        self.read_mem();
        self.asm.inc(State(OFF_SP), 2);
        self.asm.movzx(RAX, Rm::R(RAX), 1);
        self.asm.shift_imm(Shift::Shl, Rm::R(RAX), 8, 4);
        self.asm.alu_load(Alu::Or, RAX, State(OFF_SCRATCH), 4);
    }

    // ---- rotates and shifts ----------------------------------------------

    /// Rotate or shift the byte in AL. Leaves the outgoing carry in ECX,
    /// already positioned as `FLAG_C`.
    fn rotate(&mut self, kind: u8) {
        // Carry-in, needed by RL and RR.
        if kind == 2 || kind == 3 {
            self.asm.movzx(R10, State(OFF_F), 1);
            self.asm.shift_imm(Shift::Shr, Rm::R(R10), 4, 4);
            self.asm.alu_imm(Alu::And, Rm::R(R10), 1, 4);
        }

        // Carry-out comes from whichever end is about to fall off.
        self.asm.movzx(RCX, Rm::R(RAX), 1);
        match kind {
            0 | 2 | 4 => {
                self.asm.shift_imm(Shift::Shr, Rm::R(RCX), 7, 4);
            }
            6 => {
                self.asm.alu_reg(Alu::Xor, RCX, RCX, 4); // SWAP clears the carry
            }
            _ => {
                self.asm.alu_imm(Alu::And, Rm::R(RCX), 1, 4);
            }
        }
        if kind != 6 {
            self.asm.alu_imm(Alu::And, Rm::R(RCX), 1, 4);
            self.asm.shift_imm(Shift::Shl, Rm::R(RCX), 4, 4);
        }

        match kind {
            0 => self.asm.shift_imm(Shift::Rol, Rm::R(RAX), 1, 1),
            1 => self.asm.shift_imm(Shift::Ror, Rm::R(RAX), 1, 1),
            2 => {
                self.asm.shift_imm(Shift::Shl, Rm::R(RAX), 1, 1);
                self.asm.alu_reg(Alu::Or, RAX, R10, 1);
            }
            3 => {
                self.asm.shift_imm(Shift::Shr, Rm::R(RAX), 1, 1);
                self.asm.shift_imm(Shift::Shl, Rm::R(R10), 7, 4);
                self.asm.alu_reg(Alu::Or, RAX, R10, 1);
            }
            4 => self.asm.shift_imm(Shift::Shl, Rm::R(RAX), 1, 1),
            5 => self.asm.shift_imm(Shift::Sar, Rm::R(RAX), 1, 1),
            6 => self.asm.shift_imm(Shift::Rol, Rm::R(RAX), 4, 1),
            _ => self.asm.shift_imm(Shift::Shr, Rm::R(RAX), 1, 1),
        }
        self.asm.movzx(RAX, Rm::R(RAX), 1);
    }

    /// Decimal adjust. Rare enough that branches cost nothing worth saving.
    fn daa(&mut self) {
        self.asm.movzx(RAX, State(OFF_A), 1);
        self.asm.movzx(RCX, State(OFF_F), 1);
        self.asm.alu_reg(Alu::Xor, R8, R8, 4); // adjustment
        self.asm.alu_reg(Alu::Xor, R9, R9, 4); // outgoing carry

        // A carry already set means the high digit needs correcting.
        self.asm.test_imm(Rm::R(RCX), FLAG_C as u32, 1);
        let skip_c = self.asm.jcc_placeholder(Cc::E);
        self.asm.alu_imm(Alu::Or, Rm::R(R8), 0x60, 4);
        self.asm.mov_imm32(R9, 1);
        self.asm.bind(skip_c);

        self.asm.test_imm(Rm::R(RCX), FLAG_H as u32, 1);
        let skip_h = self.asm.jcc_placeholder(Cc::E);
        self.asm.alu_imm(Alu::Or, Rm::R(R8), 0x06, 4);
        self.asm.bind(skip_h);

        // After a subtraction the digits are already valid; only undo the borrow.
        self.asm.test_imm(Rm::R(RCX), FLAG_N as u32, 1);
        let subtract = self.asm.jcc_placeholder(Cc::Ne);

        self.asm.mov_reg(RDX, RAX, 4);
        self.asm.alu_imm(Alu::And, Rm::R(RDX), 0x0f, 4);
        self.asm.alu_imm(Alu::Cmp, Rm::R(RDX), 9, 4);
        let no_low = self.asm.jcc_placeholder(Cc::Be);
        self.asm.alu_imm(Alu::Or, Rm::R(R8), 0x06, 4);
        self.asm.bind(no_low);

        self.asm.alu_imm(Alu::Cmp, Rm::R(RAX), 0x99, 4);
        let no_high = self.asm.jcc_placeholder(Cc::Be);
        self.asm.alu_imm(Alu::Or, Rm::R(R8), 0x60, 4);
        self.asm.mov_imm32(R9, 1);
        self.asm.bind(no_high);

        self.asm.alu_reg(Alu::Add, RAX, R8, 4);
        let done = self.asm.jmp_placeholder();
        self.asm.bind(subtract);
        self.asm.alu_reg(Alu::Sub, RAX, R8, 4);
        self.asm.bind(done);

        self.asm.alu_imm(Alu::And, Rm::R(RAX), 0xff, 4);
        self.asm.mov_store(State(OFF_A), RAX, 1);

        self.asm.alu_imm(Alu::And, Rm::R(RCX), FLAG_N as u32, 4); // N survives, H clears
        self.asm.shift_imm(Shift::Shl, Rm::R(R9), 4, 4);
        self.asm.alu_reg(Alu::Or, RCX, R9, 4);
        self.asm.test(Rm::R(RAX), RAX, 1);
        self.asm.setcc(Cc::E, Rm::R(R9));
        self.asm.movzx(R9, Rm::R(R9), 1);
        self.asm.shift_imm(Shift::Shl, Rm::R(R9), 7, 4);
        self.asm.alu_reg(Alu::Or, RCX, R9, 4);
        self.asm.mov_store(State(OFF_F), RCX, 1);
    }

    // ---- block exits -----------------------------------------------------

    /// Can a jump from this block to `target` be a plain `jmp`, or does the
    /// mapping at that address depend on state we cannot see from here?
    fn linkable(&self, target: u16) -> bool {
        match (region(self.block_start), region(target)) {
            // Anything in RAM can be rewritten under us.
            (2, _) | (_, 2) => false,
            // The fixed window is only really fixed if nothing can remap it.
            (0, 0) | (1, 0) => self.ctx.lower_window_fixed,
            // Staying inside the switchable window keeps the same bank.
            (1, 1) => true,
            // Entering the switchable window from the fixed one: bank unknown.
            (0, 1) => false,
            _ => false,
        }
    }

    /// Consult the inline cache for the address in the state's PC, and jump
    /// straight there on a hit. A miss hands the address to the host.
    fn inline_dispatch(&mut self) {
        self.asm.movzx(RAX, State(OFF_PC), 2);
        self.asm.mov_reg(RCX, RAX, 4);
        self.asm.alu_imm(Alu::And, Rm::R(RCX), JUMP_CACHE_MASK, 4);
        self.asm.shift_imm(Shift::Shl, Rm::R(RCX), 4, 4); // entries are 16 bytes
        self.asm.alu_load(Alu::Add, RCX, State(OFF_JUMP_TABLE), 8);

        self.asm
            .alu_store(Alu::Cmp, Rm::Mem { base: RCX, disp: 0 }, RAX, 4);
        let miss_pc = self.asm.jcc_placeholder(Cc::Ne);
        self.asm.mov_load(RDX, State(OFF_BANK_GEN), 4);
        self.asm
            .alu_store(Alu::Cmp, Rm::Mem { base: RCX, disp: 4 }, RDX, 4);
        let miss_gen = self.asm.jcc_placeholder(Cc::Ne);
        self.asm.jmp_rm(Rm::Mem { base: RCX, disp: 8 });

        self.asm.bind(miss_pc);
        self.asm.bind(miss_gen);
        self.asm
            .mov_rm_imm(State(OFF_EXIT), EXIT_DISPATCH as u32, 1);
        self.asm.ret();
    }

    /// Hand the hardware its cycles and leave if it wants control back.
    fn sync_and_check(&mut self) {
        self.call_helper(OFF_SYNC);
        self.asm.alu_imm(Alu::Cmp, State(OFF_EXIT), 0, 1);
        let patch = self.asm.jcc_placeholder(Cc::Ne);
        self.exits.push(patch);
    }

    /// Leave for a statically known address.
    fn end_direct(&mut self, target: u16) {
        self.asm.mov_rm_imm(State(OFF_PC), target as u32, 2);
        self.sync_and_check();

        if self.linkable(target) {
            // Until the linker fills this in it falls straight through to the
            // dispatch path below, so an unlinked block is still correct.
            let patch = self.asm.jmp_placeholder();
            let site = Asm::patch_site(&patch);
            self.asm.bind(patch);
            self.links.push(Link { site, target });
        }
        self.inline_dispatch();
    }

    /// Leave for the address in EAX.
    fn end_indirect(&mut self) {
        self.asm.mov_store(State(OFF_PC), RAX, 2);
        self.sync_and_check();
        self.inline_dispatch();
    }

    /// Emit the shared `ret` every early exit jumps to.
    fn finish(&mut self) {
        let exits = std::mem::take(&mut self.exits);
        for patch in exits {
            self.asm.bind(patch);
        }
        self.asm.ret();
    }

    // ---- instruction translation -----------------------------------------

    /// Emit one non-control-flow instruction. Control flow is handled by the
    /// block loop, which needs to decide about linking.
    fn instruction(&mut self, insn: Insn) {
        let op = insn.opcode;
        let next = insn.addr.wrapping_add(insn.len as u16);
        // Fetching the opcode and its immediates costs four cycles a byte.
        self.charge(4 * insn.len as u32);

        match op {
            0x00 => {}

            // 16-bit immediate loads
            0x01 => self.asm.mov_rm_imm(State(OFF_BC), insn.imm16 as u32, 2),
            0x11 => self.asm.mov_rm_imm(State(OFF_DE), insn.imm16 as u32, 2),
            0x21 => self.asm.mov_rm_imm(State(OFF_HL), insn.imm16 as u32, 2),
            0x31 => self.asm.mov_rm_imm(State(OFF_SP), insn.imm16 as u32, 2),

            // A through a pointer register
            0x02 | 0x12 | 0x22 | 0x32 => {
                self.asm.movzx(R8, State(OFF_A), 1);
                let pair = match op {
                    0x02 => OFF_BC,
                    0x12 => OFF_DE,
                    _ => OFF_HL,
                };
                self.asm.movzx(RDX, State(pair), 2);
                self.write_mem();
                if op == 0x22 {
                    self.asm.inc(State(OFF_HL), 2);
                } else if op == 0x32 {
                    self.asm.dec(State(OFF_HL), 2);
                }
            }
            0x0a | 0x1a | 0x2a | 0x3a => {
                let pair = match op {
                    0x0a => OFF_BC,
                    0x1a => OFF_DE,
                    _ => OFF_HL,
                };
                self.asm.movzx(RDX, State(pair), 2);
                self.read_mem();
                self.asm.mov_store(State(OFF_A), RAX, 1);
                if op == 0x2a {
                    self.asm.inc(State(OFF_HL), 2);
                } else if op == 0x3a {
                    self.asm.dec(State(OFF_HL), 2);
                }
            }

            // 16-bit increment and decrement touch no flags
            0x03 | 0x13 | 0x23 | 0x33 | 0x0b | 0x1b | 0x2b | 0x3b => {
                let pair = match op >> 4 {
                    0 => OFF_BC,
                    1 => OFF_DE,
                    2 => OFF_HL,
                    _ => OFF_SP,
                };
                if op & 0x0f == 0x03 {
                    self.asm.inc(State(pair), 2);
                } else {
                    self.asm.dec(State(pair), 2);
                }
                self.charge(4);
            }

            0x09 => {
                self.add_hl(OFF_BC);
                self.charge(4);
            }
            0x19 => {
                self.add_hl(OFF_DE);
                self.charge(4);
            }
            0x29 => {
                self.add_hl(OFF_HL);
                self.charge(4);
            }
            0x39 => {
                self.add_hl(OFF_SP);
                self.charge(4);
            }

            0x04 | 0x0c | 0x14 | 0x1c | 0x24 | 0x2c | 0x34 | 0x3c => self.inc_dec_8(op >> 3, false),
            0x05 | 0x0d | 0x15 | 0x1d | 0x25 | 0x2d | 0x35 | 0x3d => self.inc_dec_8(op >> 3, true),

            0x06 | 0x0e | 0x16 | 0x1e | 0x26 | 0x2e | 0x36 | 0x3e => {
                self.asm.mov_imm32(RAX, insn.imm8 as u32);
                self.store_operand(op >> 3);
            }

            // Accumulator rotates clear Z, unlike their CB-prefixed twins.
            0x07 | 0x0f | 0x17 | 0x1f => {
                self.asm.movzx(RAX, State(OFF_A), 1);
                self.rotate((op >> 3) & 3);
                self.asm.mov_store(State(OFF_A), RAX, 1);
                self.asm.mov_store(State(OFF_F), RCX, 1);
            }

            0x27 => self.daa(),
            0x2f => {
                self.asm.not(State(OFF_A), 1);
                self.asm
                    .alu_imm(Alu::Or, State(OFF_F), (FLAG_N | FLAG_H) as u32, 1);
            }
            0x37 => {
                self.asm.alu_imm(Alu::And, State(OFF_F), FLAG_Z as u32, 1);
                self.asm.alu_imm(Alu::Or, State(OFF_F), FLAG_C as u32, 1);
            }
            0x3f => {
                self.asm.movzx(RCX, State(OFF_F), 1);
                self.asm.mov_reg(RAX, RCX, 4);
                self.asm.alu_imm(Alu::And, Rm::R(RAX), FLAG_Z as u32, 4);
                self.asm.alu_imm(Alu::And, Rm::R(RCX), FLAG_C as u32, 4);
                self.asm.alu_imm(Alu::Xor, Rm::R(RCX), FLAG_C as u32, 4);
                self.asm.alu_reg(Alu::Or, RCX, RAX, 4);
                self.asm.mov_store(State(OFF_F), RCX, 1);
            }

            0x08 => {
                self.asm.movzx(R8, State(OFF_SP), 1);
                self.load_addr_imm(insn.imm16);
                self.write_mem();
                self.asm.movzx(R8, State(OFF_SP + 1), 1);
                self.load_addr_imm(insn.imm16.wrapping_add(1));
                self.write_mem();
            }

            // LD r,r' — 0x76 is HALT and never reaches here
            0x40..=0x7f => {
                let src = op & 7;
                let dst = (op >> 3) & 7;
                self.load_operand(src);
                self.store_operand(dst);
            }

            0x80..=0xbf => {
                self.load_operand(op & 7);
                self.alu_op((op >> 3) & 7);
            }
            0xc6 | 0xce | 0xd6 | 0xde | 0xe6 | 0xee | 0xf6 | 0xfe => {
                self.asm.mov_imm32(RAX, insn.imm8 as u32);
                self.alu_op((op >> 3) & 7);
            }

            0xc1 => self.pop16(OFF_B, OFF_C),
            0xd1 => self.pop16(OFF_D, OFF_E),
            0xe1 => self.pop16(OFF_H, OFF_L),
            0xf1 => {
                self.pop16(OFF_A, OFF_F);
                // The low nibble of F does not exist on real hardware.
                self.asm.alu_imm(Alu::And, State(OFF_F), 0xf0, 1);
            }
            0xc5 => {
                self.push16(OFF_B, OFF_C);
                self.charge(4);
            }
            0xd5 => {
                self.push16(OFF_D, OFF_E);
                self.charge(4);
            }
            0xe5 => {
                self.push16(OFF_H, OFF_L);
                self.charge(4);
            }
            0xf5 => {
                self.push16(OFF_A, OFF_F);
                self.charge(4);
            }

            // High page I/O
            0xe0 => {
                self.asm.movzx(R8, State(OFF_A), 1);
                self.load_addr_imm(0xff00 | insn.imm8 as u16);
                self.write_mem();
            }
            0xf0 => {
                self.load_addr_imm(0xff00 | insn.imm8 as u16);
                self.read_mem();
                self.asm.mov_store(State(OFF_A), RAX, 1);
            }
            0xe2 => {
                self.asm.movzx(R8, State(OFF_A), 1);
                self.asm.movzx(RDX, State(OFF_C), 1);
                self.asm.alu_imm(Alu::Or, Rm::R(RDX), 0xff00, 4);
                self.write_mem();
            }
            0xf2 => {
                self.asm.movzx(RDX, State(OFF_C), 1);
                self.asm.alu_imm(Alu::Or, Rm::R(RDX), 0xff00, 4);
                self.read_mem();
                self.asm.mov_store(State(OFF_A), RAX, 1);
            }
            0xea => {
                self.asm.movzx(R8, State(OFF_A), 1);
                self.load_addr_imm(insn.imm16);
                self.write_mem();
            }
            0xfa => {
                self.load_addr_imm(insn.imm16);
                self.read_mem();
                self.asm.mov_store(State(OFF_A), RAX, 1);
            }

            0xe8 => {
                self.sp_plus_offset(insn.imm8 as i8, OFF_SP);
                self.charge(8);
            }
            0xf8 => {
                self.sp_plus_offset(insn.imm8 as i8, OFF_HL);
                self.charge(4);
            }
            0xf9 => {
                self.asm.movzx(RAX, State(OFF_HL), 2);
                self.asm.mov_store(State(OFF_SP), RAX, 2);
                self.charge(4);
            }

            0xf3 => {
                self.asm.mov_rm_imm(State(OFF_IME), 0, 1);
                self.asm.mov_rm_imm(State(OFF_IME_DELAY), 0, 1);
            }
            // Interrupts come back on after the following instruction, which the
            // host applies at the next block boundary.
            0xfb => self.asm.mov_rm_imm(State(OFF_IME_DELAY), 1, 1),

            0xcb => self.cb_instruction(insn.cb),

            _ => {
                // Control flow and illegal opcodes are handled by the caller.
                debug_assert!(insn.flow != Flow::Normal, "unhandled opcode {op:#04x}");
            }
        }
        let _ = next;
    }

    fn alu_op(&mut self, kind: u8) {
        match kind {
            0 => self.alu_add_sub(false, false, true),
            1 => self.alu_add_sub(false, true, true),
            2 => self.alu_add_sub(true, false, true),
            3 => self.alu_add_sub(true, true, true),
            4 => self.alu_logic(Alu::And, true),
            5 => self.alu_logic(Alu::Xor, false),
            6 => self.alu_logic(Alu::Or, false),
            // CP is SUB that throws the result away.
            _ => self.alu_add_sub(true, false, false),
        }
    }

    fn cb_instruction(&mut self, cb: u8) {
        let idx = cb & 7;
        let bit = (cb >> 3) & 7;

        match cb >> 6 {
            0 => {
                self.load_operand(idx);
                self.rotate(bit);
                // Build F before a store to (HL) sends us through the host.
                self.flags_from_al(true, false);
                self.store_operand(idx);
            }
            1 => {
                self.load_operand(idx);
                self.asm.test_imm(Rm::R(RAX), 1 << bit, 1);
                self.asm.setcc(Cc::E, Rm::R(R9));
                self.asm.movzx(RCX, State(OFF_F), 1);
                self.asm.alu_imm(Alu::And, Rm::R(RCX), FLAG_C as u32, 4);
                self.asm.alu_imm(Alu::Or, Rm::R(RCX), FLAG_H as u32, 4);
                self.asm.movzx(R9, Rm::R(R9), 1);
                self.asm.shift_imm(Shift::Shl, Rm::R(R9), 7, 4);
                self.asm.alu_reg(Alu::Or, RCX, R9, 4);
                self.asm.mov_store(State(OFF_F), RCX, 1);
            }
            2 => {
                self.load_operand(idx);
                self.asm
                    .alu_imm(Alu::And, Rm::R(RAX), !(1u32 << bit) & 0xff, 1);
                self.store_operand(idx);
            }
            _ => {
                self.load_operand(idx);
                self.asm.alu_imm(Alu::Or, Rm::R(RAX), 1 << bit, 1);
                self.store_operand(idx);
            }
        }
    }
}

/// Translate one basic block starting at `start`.
///
/// A block runs until the first instruction that can move the program counter
/// somewhere other than straight ahead, and that instruction is included.
pub fn translate_block<F: Fn(u16) -> u8>(start: u16, ctx: Context, fetch: &F) -> Block {
    let mut em = Emitter::new(ctx, start);
    let mut pc = start;
    let mut count = 0usize;

    // A block has to end somewhere even if the code runs off into unmapped
    // space; the cap keeps a pathological stretch of 0x00 from filling the arena.
    const MAX_INSTRUCTIONS: usize = 2048;

    loop {
        let insn = decode(pc, fetch);
        count += 1;
        pc = pc.wrapping_add(insn.len as u16);
        let next = pc;

        match insn.flow {
            Flow::Normal => {
                em.instruction(insn);
                // A block never spans two mapping regions: what follows 0x3FFF
                // depends on the mapper, and is not this block's business.
                if count >= MAX_INSTRUCTIONS || region(next) != region(start) {
                    em.end_direct(next);
                    break;
                }
                continue;
            }

            Flow::Jump {
                target,
                conditional,
            } => {
                em.charge(4 * insn.len as u32);
                if conditional {
                    em.settle();
                    let (mask, want_set) = condition((insn.opcode >> 3) & 3);
                    em.asm.test_imm(State(OFF_F), mask, 1);
                    let taken = em
                        .asm
                        .jcc_placeholder(if want_set { Cc::Ne } else { Cc::E });
                    em.end_direct(next);
                    em.asm.bind(taken);
                    em.charge(4);
                    em.end_direct(target);
                } else {
                    em.charge(4);
                    em.end_direct(target);
                }
            }

            Flow::Call {
                target,
                conditional,
            } => {
                em.charge(4 * insn.len as u32);
                if conditional {
                    em.settle();
                    let (mask, want_set) = condition((insn.opcode >> 3) & 3);
                    em.asm.test_imm(State(OFF_F), mask, 1);
                    let taken = em
                        .asm
                        .jcc_placeholder(if want_set { Cc::Ne } else { Cc::E });
                    em.end_direct(next);
                    em.asm.bind(taken);
                }
                em.charge(4);
                em.push_imm16(next);
                em.end_direct(target);
            }

            Flow::Rst { target } => {
                em.charge(4 * insn.len as u32 + 4);
                em.push_imm16(next);
                em.end_direct(target);
            }

            Flow::Ret {
                conditional,
                enable_interrupts,
            } => {
                em.charge(4 * insn.len as u32 + 4);
                if conditional {
                    em.settle();
                    let (mask, want_set) = condition((insn.opcode >> 3) & 3);
                    em.asm.test_imm(State(OFF_F), mask, 1);
                    let taken = em
                        .asm
                        .jcc_placeholder(if want_set { Cc::Ne } else { Cc::E });
                    em.end_direct(next);
                    em.asm.bind(taken);
                    em.charge(4);
                }
                em.pop_pc();
                if enable_interrupts {
                    em.asm.mov_rm_imm(State(OFF_IME), 1, 1);
                    em.asm.mov_rm_imm(State(OFF_IME_DELAY), 0, 1);
                }
                em.end_indirect();
            }

            Flow::JumpIndirect => {
                em.charge(4);
                em.asm.movzx(RAX, State(OFF_HL), 2);
                em.end_indirect();
            }

            Flow::Halt => {
                em.charge(4);
                em.asm.mov_rm_imm(State(OFF_PC), next as u32, 2);
                em.call_helper(OFF_HALT);
                em.asm.ret();
            }

            Flow::Stop => {
                // Two bytes: the opcode swallows the one after it.
                em.charge(8);
                em.asm.mov_rm_imm(State(OFF_PC), next as u32, 2);
                em.call_helper(OFF_STOP);
                // A CGB speed switch resumes; a real stop does not.
                em.asm.alu_imm(Alu::Cmp, State(OFF_EXIT), 0, 1);
                let leave = em.asm.jcc_placeholder(Cc::Ne);
                em.exits.push(leave);
                em.end_direct(next);
            }

            Flow::Illegal => {
                // Real hardware wedges here; report it instead of running on.
                em.settle();
                em.asm.mov_rm_imm(State(OFF_PC), insn.addr as u32, 2);
                em.asm.mov_rm_imm(State(OFF_EXIT), EXIT_ILLEGAL as u32, 1);
                em.asm.ret();
            }
        }
        break;
    }

    em.finish();
    Block {
        start,
        end: pc,
        code: em.asm.code,
        links: em.links,
        instructions: count,
    }
}

/// The trampoline that gets from Rust into translated code and back.
///
/// Signature: `extern "win64" fn(state: *mut GbState, entry: *const u8)`.
/// It parks the state pointer in RBP, which every translated block relies on,
/// and leaves RSP arranged so a block's helper calls land 16-byte aligned.
pub fn build_trampoline() -> Vec<u8> {
    let mut asm = Asm::new();
    asm.push(RBP);
    asm.push(RBX);
    asm.alu_imm(Alu::Sub, Rm::R(RSP), 8, 8);
    asm.mov_reg(RBP, RCX, 8);
    asm.call_rm(Rm::R(RDX));
    asm.alu_imm(Alu::Add, Rm::R(RSP), 8, 8);
    asm.pop(RBX);
    asm.pop(RBP);
    asm.ret();
    asm.code
}
