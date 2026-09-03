//! A small x86-64 encoder — only the forms the translator actually emits.
//!
//! Everything produced here is position-independent: the only absolute values
//! that reach the buffer are immediates from the game's own instruction stream.
//! Machine state lives at `[rbp + offset]` and host services are reached through
//! a function table at `[rbp + helper offset]`, so a finished block runs correctly
//! wherever the loader happens to put it.

/// Register numbers, matching the hardware encoding.
pub const RAX: u8 = 0;
pub const RCX: u8 = 1;
pub const RDX: u8 = 2;
pub const RBX: u8 = 3;
pub const RSP: u8 = 4;
/// Pinned: points at the `GbState` the whole time translated code is running.
pub const RBP: u8 = 5;
pub const R8: u8 = 8;
pub const R9: u8 = 9;
pub const R10: u8 = 10;
pub const R11: u8 = 11;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Alu {
    Add = 0,
    Or = 1,
    Adc = 2,
    Sbb = 3,
    And = 4,
    Sub = 5,
    Xor = 6,
    Cmp = 7,
}

#[derive(Clone, Copy)]
pub enum Shift {
    Rol = 0,
    Ror = 1,
    Shl = 4,
    Shr = 5,
    Sar = 7,
}

/// x86 condition codes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Cc {
    B = 2,
    Ae = 3,
    E = 4,
    Ne = 5,
    Be = 6,
    A = 7,
    S = 8,
    Ns = 9,
}

/// An operand: a register, or memory at `[base + disp]`.
#[derive(Clone, Copy)]
pub enum Rm {
    R(u8),
    Mem { base: u8, disp: i32 },
}

/// `[rbp + disp]` — a field of the machine state.
#[allow(non_snake_case)]
pub const fn State(disp: i32) -> Rm {
    Rm::Mem { base: RBP, disp }
}

/// A jump whose destination is not known yet.
pub struct Patch {
    /// Offset of the rel32 field within the buffer.
    at: usize,
}

#[derive(Default)]
pub struct Asm {
    pub code: Vec<u8>,
}

impl Asm {
    pub fn new() -> Self {
        Asm {
            code: Vec::with_capacity(256),
        }
    }

    pub fn len(&self) -> usize {
        self.code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    fn byte(&mut self, b: u8) {
        self.code.push(b);
    }

    fn bytes(&mut self, b: &[u8]) {
        self.code.extend_from_slice(b);
    }

    /// Emit prefixes, opcode and ModRM for `opcode reg, rm` at the given width.
    /// `size` is 1, 2, 4 or 8 bytes.
    fn encode(&mut self, opcode: &[u8], reg: u8, rm: Rm, size: u8) {
        if size == 2 {
            self.byte(0x66);
        }

        let rm_reg = match rm {
            Rm::R(r) => r,
            Rm::Mem { base, .. } => base,
        };

        let mut rex = 0u8;
        if size == 8 {
            rex |= 0x48;
        }
        if reg >= 8 {
            rex |= 0x44;
        }
        if rm_reg >= 8 {
            rex |= 0x41;
        }
        // Byte operations on SPL/BPL/SIL/DIL need a REX prefix; the same
        // encodings without one mean AH/CH/DH/BH instead.
        if size == 1 && ((4..8).contains(&reg) || matches!(rm, Rm::R(r) if (4..8).contains(&r))) {
            rex |= 0x40;
        }
        if rex != 0 {
            self.byte(rex);
        }

        self.bytes(opcode);
        self.modrm(reg, rm);
    }

    fn modrm(&mut self, reg: u8, rm: Rm) {
        match rm {
            Rm::R(r) => self.byte(0xc0 | ((reg & 7) << 3) | (r & 7)),
            Rm::Mem { base, disp } => {
                // RSP/R12 as a base would need a SIB byte; the translator never
                // addresses through them.
                debug_assert!(base & 7 != 4, "base register requires a SIB byte");
                // RBP/R13 have no zero-displacement form, so always give them one.
                let needs_disp = disp != 0 || base & 7 == 5;
                if !needs_disp {
                    self.byte(((reg & 7) << 3) | (base & 7));
                } else if (-128..=127).contains(&disp) {
                    self.byte(0x40 | ((reg & 7) << 3) | (base & 7));
                    self.byte(disp as u8);
                } else {
                    self.byte(0x80 | ((reg & 7) << 3) | (base & 7));
                    self.bytes(&disp.to_le_bytes());
                }
            }
        }
    }

    // ---- moves -----------------------------------------------------------

    /// `mov rm, reg`
    pub fn mov_store(&mut self, rm: Rm, reg: u8, size: u8) {
        let op = if size == 1 { 0x88 } else { 0x89 };
        self.encode(&[op], reg, rm, size);
    }

    /// `mov reg, rm`
    pub fn mov_load(&mut self, reg: u8, rm: Rm, size: u8) {
        let op = if size == 1 { 0x8a } else { 0x8b };
        self.encode(&[op], reg, rm, size);
    }

    pub fn mov_reg(&mut self, dst: u8, src: u8, size: u8) {
        self.mov_store(Rm::R(dst), src, size);
    }

    pub fn mov_imm32(&mut self, reg: u8, value: u32) {
        if reg >= 8 {
            self.byte(0x41);
        }
        self.byte(0xb8 + (reg & 7));
        self.bytes(&value.to_le_bytes());
    }

    pub fn mov_imm64(&mut self, reg: u8, value: u64) {
        self.byte(0x48 | ((reg >= 8) as u8));
        self.byte(0xb8 + (reg & 7));
        self.bytes(&value.to_le_bytes());
    }

    /// `mov rm, imm`
    pub fn mov_rm_imm(&mut self, rm: Rm, value: u32, size: u8) {
        let op = if size == 1 { 0xc6 } else { 0xc7 };
        self.encode(&[op], 0, rm, size);
        match size {
            1 => self.byte(value as u8),
            2 => self.bytes(&(value as u16).to_le_bytes()),
            _ => self.bytes(&value.to_le_bytes()),
        }
    }

    /// `movzx r32, rm8` or `movzx r32, rm16`
    pub fn movzx(&mut self, reg: u8, rm: Rm, src_size: u8) {
        let op = if src_size == 1 { 0xb6 } else { 0xb7 };
        // The 0x0F escape goes after REX, which `encode` handles by taking the
        // whole opcode sequence.
        self.encode(&[0x0f, op], reg, rm, if src_size == 1 { 1 } else { 4 });
    }

    // ---- arithmetic ------------------------------------------------------

    /// `op rm, reg`
    pub fn alu_store(&mut self, op: Alu, rm: Rm, reg: u8, size: u8) {
        let base = (op as u8) * 8 + if size == 1 { 0x00 } else { 0x01 };
        self.encode(&[base], reg, rm, size);
    }

    /// `op reg, rm`
    pub fn alu_load(&mut self, op: Alu, reg: u8, rm: Rm, size: u8) {
        let base = (op as u8) * 8 + if size == 1 { 0x02 } else { 0x03 };
        self.encode(&[base], reg, rm, size);
    }

    pub fn alu_reg(&mut self, op: Alu, dst: u8, src: u8, size: u8) {
        self.alu_store(op, Rm::R(dst), src, size);
    }

    /// `op rm, imm`
    pub fn alu_imm(&mut self, op: Alu, rm: Rm, value: u32, size: u8) {
        if size == 1 {
            self.encode(&[0x80], op as u8, rm, 1);
            self.byte(value as u8);
        } else if (value as i32) >= -128 && (value as i32) <= 127 {
            self.encode(&[0x83], op as u8, rm, size);
            self.byte(value as u8);
        } else {
            self.encode(&[0x81], op as u8, rm, size);
            match size {
                2 => self.bytes(&(value as u16).to_le_bytes()),
                _ => self.bytes(&value.to_le_bytes()),
            }
        }
    }

    pub fn inc(&mut self, rm: Rm, size: u8) {
        let op = if size == 1 { 0xfe } else { 0xff };
        self.encode(&[op], 0, rm, size);
    }

    pub fn dec(&mut self, rm: Rm, size: u8) {
        let op = if size == 1 { 0xfe } else { 0xff };
        self.encode(&[op], 1, rm, size);
    }

    pub fn not(&mut self, rm: Rm, size: u8) {
        let op = if size == 1 { 0xf6 } else { 0xf7 };
        self.encode(&[op], 2, rm, size);
    }

    pub fn shift_imm(&mut self, kind: Shift, rm: Rm, amount: u8, size: u8) {
        let op = if size == 1 { 0xc0 } else { 0xc1 };
        self.encode(&[op], kind as u8, rm, size);
        self.byte(amount);
    }

    pub fn test(&mut self, rm: Rm, reg: u8, size: u8) {
        let op = if size == 1 { 0x84 } else { 0x85 };
        self.encode(&[op], reg, rm, size);
    }

    pub fn test_imm(&mut self, rm: Rm, value: u32, size: u8) {
        let op = if size == 1 { 0xf6 } else { 0xf7 };
        self.encode(&[op], 0, rm, size);
        match size {
            1 => self.byte(value as u8),
            2 => self.bytes(&(value as u16).to_le_bytes()),
            _ => self.bytes(&value.to_le_bytes()),
        }
    }

    pub fn setcc(&mut self, cc: Cc, rm: Rm) {
        self.encode(&[0x0f, 0x90 + cc as u8], 0, rm, 1);
    }

    /// `lea reg, [base + disp]`
    pub fn lea(&mut self, reg: u8, rm: Rm) {
        self.encode(&[0x8d], reg, rm, 8);
    }

    // ---- stack and control flow ------------------------------------------

    pub fn push(&mut self, reg: u8) {
        if reg >= 8 {
            self.byte(0x41);
        }
        self.byte(0x50 + (reg & 7));
    }

    pub fn pop(&mut self, reg: u8) {
        if reg >= 8 {
            self.byte(0x41);
        }
        self.byte(0x58 + (reg & 7));
    }

    pub fn ret(&mut self) {
        self.byte(0xc3);
    }

    /// `call rm` — used for the helper table entries at `[rbp + n]`.
    pub fn call_rm(&mut self, rm: Rm) {
        self.encode(&[0xff], 2, rm, 4);
    }

    pub fn jmp_rm(&mut self, rm: Rm) {
        self.encode(&[0xff], 4, rm, 4);
    }

    /// `jmp rel32` with the destination filled in later.
    pub fn jmp_placeholder(&mut self) -> Patch {
        self.byte(0xe9);
        let at = self.code.len();
        self.bytes(&[0; 4]);
        Patch { at }
    }

    pub fn jcc_placeholder(&mut self, cc: Cc) -> Patch {
        self.bytes(&[0x0f, 0x80 + cc as u8]);
        let at = self.code.len();
        self.bytes(&[0; 4]);
        Patch { at }
    }

    /// Point a previously emitted jump at the current end of the buffer.
    pub fn bind(&mut self, patch: Patch) {
        let target = self.code.len();
        let rel = (target as i64 - (patch.at as i64 + 4)) as i32;
        self.code[patch.at..patch.at + 4].copy_from_slice(&rel.to_le_bytes());
    }

    /// Point a jump at an arbitrary offset within this buffer.
    pub fn bind_to(&mut self, patch: Patch, target: usize) {
        let rel = (target as i64 - (patch.at as i64 + 4)) as i32;
        self.code[patch.at..patch.at + 4].copy_from_slice(&rel.to_le_bytes());
    }

    /// Where a pending jump's rel32 field sits, for cross-block linking.
    pub fn patch_site(patch: &Patch) -> usize {
        patch.at
    }

    pub fn into_patch(at: usize) -> Patch {
        Patch { at }
    }
}
