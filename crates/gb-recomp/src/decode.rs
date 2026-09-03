//! SM83 instruction decoding — enough to know how long an instruction is and
//! where control goes next. Semantics live in the translator.

/// How an instruction moves the program counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Falls through to the next instruction.
    Normal,
    /// `JP nn` / `JR e`, absolute target known at translation time.
    Jump {
        target: u16,
        conditional: bool,
    },
    /// `CALL nn`.
    Call {
        target: u16,
        conditional: bool,
    },
    /// `RST n`.
    Rst {
        target: u16,
    },
    /// `RET`, `RETI`.
    Ret {
        conditional: bool,
        enable_interrupts: bool,
    },
    /// `JP HL` — the destination is only known while running.
    JumpIndirect,
    Halt,
    Stop,
    /// An opcode the hardware does not implement.
    Illegal,
}

impl Flow {
    /// Whether the instruction after this one is reachable by falling through.
    pub fn falls_through(self) -> bool {
        match self {
            Flow::Normal | Flow::Halt | Flow::Stop => true,
            Flow::Jump { conditional, .. } => conditional,
            Flow::Call { .. } => true,
            Flow::Rst { .. } => true,
            Flow::Ret { conditional, .. } => conditional,
            Flow::JumpIndirect | Flow::Illegal => false,
        }
    }

    /// Whether a block must end here.
    pub fn ends_block(self) -> bool {
        !matches!(self, Flow::Normal)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Insn {
    pub addr: u16,
    pub opcode: u8,
    /// Second byte of a `CB`-prefixed instruction.
    pub cb: u8,
    pub imm8: u8,
    pub imm16: u16,
    pub len: u8,
    pub flow: Flow,
}

/// Instruction length in bytes, indexed by opcode. Zero marks an opcode the
/// hardware locks up on; the decoder treats those as one byte and illegal.
#[rustfmt::skip]
const LENGTHS: [u8; 256] = [
    1, 3, 1, 1, 1, 1, 2, 1, 3, 1, 1, 1, 1, 1, 2, 1,
    2, 3, 1, 1, 1, 1, 2, 1, 2, 1, 1, 1, 1, 1, 2, 1,
    2, 3, 1, 1, 1, 1, 2, 1, 2, 1, 1, 1, 1, 1, 2, 1,
    2, 3, 1, 1, 1, 1, 2, 1, 2, 1, 1, 1, 1, 1, 2, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 3, 3, 3, 1, 2, 1, 1, 1, 3, 2, 3, 3, 2, 1,
    1, 1, 3, 1, 3, 1, 2, 1, 1, 1, 3, 1, 3, 1, 2, 1,
    2, 1, 1, 1, 1, 1, 2, 1, 2, 1, 3, 1, 1, 1, 2, 1,
    2, 1, 1, 1, 1, 1, 2, 1, 2, 1, 3, 1, 1, 1, 2, 1,
];

pub const ILLEGAL: [u8; 11] = [
    0xd3, 0xdb, 0xdd, 0xe3, 0xe4, 0xeb, 0xec, 0xed, 0xf4, 0xfc, 0xfd,
];

pub fn is_illegal(opcode: u8) -> bool {
    ILLEGAL.contains(&opcode)
}

/// Decode the instruction at `addr`, reading bytes through `fetch`.
pub fn decode(addr: u16, fetch: &impl Fn(u16) -> u8) -> Insn {
    let opcode = fetch(addr);
    let len = LENGTHS[opcode as usize];
    let imm8 = if len >= 2 {
        fetch(addr.wrapping_add(1))
    } else {
        0
    };
    let imm16 = if len >= 3 {
        u16::from_le_bytes([imm8, fetch(addr.wrapping_add(2))])
    } else {
        0
    };
    let cb = if opcode == 0xcb { imm8 } else { 0 };
    let next = addr.wrapping_add(len as u16);

    let flow = if is_illegal(opcode) {
        Flow::Illegal
    } else {
        match opcode {
            0x18 => Flow::Jump {
                target: next.wrapping_add(imm8 as i8 as u16),
                conditional: false,
            },
            0x20 | 0x28 | 0x30 | 0x38 => Flow::Jump {
                target: next.wrapping_add(imm8 as i8 as u16),
                conditional: true,
            },
            0xc3 => Flow::Jump {
                target: imm16,
                conditional: false,
            },
            0xc2 | 0xca | 0xd2 | 0xda => Flow::Jump {
                target: imm16,
                conditional: true,
            },
            0xcd => Flow::Call {
                target: imm16,
                conditional: false,
            },
            0xc4 | 0xcc | 0xd4 | 0xdc => Flow::Call {
                target: imm16,
                conditional: true,
            },
            0xc9 => Flow::Ret {
                conditional: false,
                enable_interrupts: false,
            },
            0xd9 => Flow::Ret {
                conditional: false,
                enable_interrupts: true,
            },
            0xc0 | 0xc8 | 0xd0 | 0xd8 => Flow::Ret {
                conditional: true,
                enable_interrupts: false,
            },
            0xc7 | 0xcf | 0xd7 | 0xdf | 0xe7 | 0xef | 0xf7 | 0xff => Flow::Rst {
                target: (opcode & 0x38) as u16,
            },
            0xe9 => Flow::JumpIndirect,
            0x76 => Flow::Halt,
            0x10 => Flow::Stop,
            _ => Flow::Normal,
        }
    };

    Insn {
        addr,
        opcode,
        cb,
        imm8,
        imm16,
        len: len.max(1),
        flow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(bytes: &[u8]) -> Insn {
        let owned = bytes.to_vec();
        decode(0x1000, &move |a: u16| {
            owned.get((a - 0x1000) as usize).copied().unwrap_or(0)
        })
    }

    #[test]
    fn nop_is_one_byte_and_falls_through() {
        let insn = at(&[0x00]);
        assert_eq!(insn.len, 1);
        assert_eq!(insn.flow, Flow::Normal);
    }

    #[test]
    fn absolute_jump_target_is_read_little_endian() {
        let insn = at(&[0xc3, 0x50, 0x01]);
        assert_eq!(insn.len, 3);
        assert_eq!(
            insn.flow,
            Flow::Jump {
                target: 0x0150,
                conditional: false
            }
        );
    }

    #[test]
    fn relative_jump_is_measured_from_the_next_instruction() {
        // JR -2 at 0x1000 is the classic one-instruction spin loop.
        let insn = at(&[0x18, 0xfe]);
        assert_eq!(
            insn.flow,
            Flow::Jump {
                target: 0x1000,
                conditional: false
            }
        );
    }

    #[test]
    fn conditional_forms_are_marked_conditional() {
        assert!(matches!(
            at(&[0x28, 0x05]).flow,
            Flow::Jump {
                conditional: true,
                ..
            }
        ));
        assert!(matches!(
            at(&[0xc4, 0, 0]).flow,
            Flow::Call {
                conditional: true,
                ..
            }
        ));
        assert!(matches!(
            at(&[0xc0]).flow,
            Flow::Ret {
                conditional: true,
                ..
            }
        ));
    }

    #[test]
    fn rst_targets_its_vector() {
        assert_eq!(at(&[0xff]).flow, Flow::Rst { target: 0x38 });
        assert_eq!(at(&[0xc7]).flow, Flow::Rst { target: 0x00 });
    }

    #[test]
    fn cb_prefixed_instructions_carry_their_second_byte() {
        let insn = at(&[0xcb, 0x7f]);
        assert_eq!(insn.len, 2);
        assert_eq!(insn.cb, 0x7f);
    }

    #[test]
    fn unimplemented_opcodes_are_flagged() {
        assert_eq!(at(&[0xdd]).flow, Flow::Illegal);
        assert!(is_illegal(0xed));
    }

    #[test]
    fn every_opcode_has_a_plausible_length() {
        for (op, &len) in LENGTHS.iter().enumerate() {
            assert!((1..=3).contains(&len), "opcode {op:#04x} has length {len}");
        }
    }
}
