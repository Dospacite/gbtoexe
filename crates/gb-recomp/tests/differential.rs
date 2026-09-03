//! Differential testing of the recompiler against a reference interpreter.
//!
//! The interpreter here exists only as an oracle: it is deliberately written
//! straight from the instruction set documentation, with no shared code with the
//! translator, so agreement between the two is real evidence. Every opcode is
//! run from several randomised starting states and the resulting registers,
//! flags and memory are compared byte for byte.

use gb_hw::{Config, Mmu};
use gb_recomp::machine::Emu;

const FLAG_Z: u8 = 0x80;
const FLAG_N: u8 = 0x40;
const FLAG_H: u8 = 0x20;
const FLAG_C: u8 = 0x10;

/// Registers, in the order the comparison reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Regs {
    a: u8,
    f: u8,
    b: u8,
    c: u8,
    d: u8,
    e: u8,
    h: u8,
    l: u8,
    sp: u16,
}

/// A plainly written SM83 interpreter used only to check the translator.
struct Reference<'a> {
    r: Regs,
    pc: u16,
    mmu: &'a mut Mmu,
}

impl Reference<'_> {
    fn hl(&self) -> u16 {
        u16::from_be_bytes([self.r.h, self.r.l])
    }
    fn bc(&self) -> u16 {
        u16::from_be_bytes([self.r.b, self.r.c])
    }
    fn de(&self) -> u16 {
        u16::from_be_bytes([self.r.d, self.r.e])
    }
    fn set_hl(&mut self, v: u16) {
        self.r.h = (v >> 8) as u8;
        self.r.l = v as u8;
    }

    fn flag(&self, m: u8) -> bool {
        self.r.f & m != 0
    }
    fn set_flag(&mut self, m: u8, on: bool) {
        if on {
            self.r.f |= m
        } else {
            self.r.f &= !m
        }
    }

    fn fetch(&mut self) -> u8 {
        let v = self.mmu.read(self.pc);
        self.pc = self.pc.wrapping_add(1);
        v
    }
    fn fetch16(&mut self) -> u16 {
        let lo = self.fetch();
        let hi = self.fetch();
        u16::from_le_bytes([lo, hi])
    }

    fn get_r(&mut self, i: u8) -> u8 {
        match i {
            0 => self.r.b,
            1 => self.r.c,
            2 => self.r.d,
            3 => self.r.e,
            4 => self.r.h,
            5 => self.r.l,
            6 => self.mmu.read(self.hl()),
            _ => self.r.a,
        }
    }

    fn set_r(&mut self, i: u8, v: u8) {
        match i {
            0 => self.r.b = v,
            1 => self.r.c = v,
            2 => self.r.d = v,
            3 => self.r.e = v,
            4 => self.r.h = v,
            5 => self.r.l = v,
            6 => {
                let addr = self.hl();
                self.mmu.write(addr, v)
            }
            _ => self.r.a = v,
        }
    }

    fn alu(&mut self, op: u8, v: u8) {
        let a = self.r.a;
        match op {
            0 | 1 => {
                let carry = if op == 1 { self.flag(FLAG_C) as u16 } else { 0 };
                let sum = a as u16 + v as u16 + carry;
                self.r.a = sum as u8;
                self.r.f = 0;
                self.set_flag(FLAG_Z, self.r.a == 0);
                self.set_flag(FLAG_H, (a & 0xf) as u16 + (v & 0xf) as u16 + carry > 0xf);
                self.set_flag(FLAG_C, sum > 0xff);
            }
            2 | 3 | 7 => {
                let carry = if op == 3 { self.flag(FLAG_C) as i16 } else { 0 };
                let diff = a as i16 - v as i16 - carry;
                let result = diff as u8;
                let half = (a & 0xf) as i16 - (v & 0xf) as i16 - carry;
                self.r.f = FLAG_N;
                self.set_flag(FLAG_Z, result == 0);
                self.set_flag(FLAG_H, half < 0);
                self.set_flag(FLAG_C, diff < 0);
                if op != 7 {
                    self.r.a = result;
                }
            }
            4 => {
                self.r.a &= v;
                self.r.f = FLAG_H;
                self.set_flag(FLAG_Z, self.r.a == 0);
            }
            5 => {
                self.r.a ^= v;
                self.r.f = 0;
                self.set_flag(FLAG_Z, self.r.a == 0);
            }
            _ => {
                self.r.a |= v;
                self.r.f = 0;
                self.set_flag(FLAG_Z, self.r.a == 0);
            }
        }
    }

    fn rotate(&mut self, kind: u8, v: u8) -> (u8, bool) {
        match kind {
            0 => (v.rotate_left(1), v & 0x80 != 0),
            1 => (v.rotate_right(1), v & 1 != 0),
            2 => ((v << 1) | self.flag(FLAG_C) as u8, v & 0x80 != 0),
            3 => ((v >> 1) | ((self.flag(FLAG_C) as u8) << 7), v & 1 != 0),
            4 => (v << 1, v & 0x80 != 0),
            5 => (((v as i8) >> 1) as u8, v & 1 != 0),
            6 => (v.rotate_right(4), false),
            _ => (v >> 1, v & 1 != 0),
        }
    }

    fn step(&mut self) {
        let op = self.fetch();
        match op {
            0x00 => {}
            0x01 | 0x11 | 0x21 | 0x31 => {
                let v = self.fetch16();
                match op {
                    0x01 => {
                        self.r.b = (v >> 8) as u8;
                        self.r.c = v as u8;
                    }
                    0x11 => {
                        self.r.d = (v >> 8) as u8;
                        self.r.e = v as u8;
                    }
                    0x21 => self.set_hl(v),
                    _ => self.r.sp = v,
                }
            }
            0x02 => {
                let addr = self.bc();
                self.mmu.write(addr, self.r.a)
            }
            0x12 => {
                let addr = self.de();
                self.mmu.write(addr, self.r.a)
            }
            0x22 => {
                let addr = self.hl();
                self.mmu.write(addr, self.r.a);
                self.set_hl(addr.wrapping_add(1));
            }
            0x32 => {
                let addr = self.hl();
                self.mmu.write(addr, self.r.a);
                self.set_hl(addr.wrapping_sub(1));
            }
            0x0a => {
                let addr = self.bc();
                self.r.a = self.mmu.read(addr)
            }
            0x1a => {
                let addr = self.de();
                self.r.a = self.mmu.read(addr)
            }
            0x2a => {
                let addr = self.hl();
                self.r.a = self.mmu.read(addr);
                self.set_hl(addr.wrapping_add(1));
            }
            0x3a => {
                let addr = self.hl();
                self.r.a = self.mmu.read(addr);
                self.set_hl(addr.wrapping_sub(1));
            }
            0x03 | 0x13 | 0x23 | 0x33 | 0x0b | 0x1b | 0x2b | 0x3b => {
                let delta: u16 = if op & 0xf == 3 { 1 } else { 0xffff };
                match op >> 4 {
                    0 => {
                        let v = self.bc().wrapping_add(delta);
                        self.r.b = (v >> 8) as u8;
                        self.r.c = v as u8;
                    }
                    1 => {
                        let v = self.de().wrapping_add(delta);
                        self.r.d = (v >> 8) as u8;
                        self.r.e = v as u8;
                    }
                    2 => {
                        let v = self.hl().wrapping_add(delta);
                        self.set_hl(v);
                    }
                    _ => self.r.sp = self.r.sp.wrapping_add(delta),
                }
            }
            0x09 | 0x19 | 0x29 | 0x39 => {
                let operand = match op >> 4 {
                    0 => self.bc(),
                    1 => self.de(),
                    2 => self.hl(),
                    _ => self.r.sp,
                };
                let hl = self.hl();
                let sum = hl as u32 + operand as u32;
                self.set_flag(FLAG_N, false);
                self.set_flag(FLAG_H, (hl & 0xfff) + (operand & 0xfff) > 0xfff);
                self.set_flag(FLAG_C, sum > 0xffff);
                self.set_hl(sum as u16);
            }
            0x04 | 0x0c | 0x14 | 0x1c | 0x24 | 0x2c | 0x34 | 0x3c => {
                let i = op >> 3;
                let v = self.get_r(i);
                let r = v.wrapping_add(1);
                self.set_flag(FLAG_Z, r == 0);
                self.set_flag(FLAG_N, false);
                self.set_flag(FLAG_H, v & 0xf == 0xf);
                self.set_r(i, r);
            }
            0x05 | 0x0d | 0x15 | 0x1d | 0x25 | 0x2d | 0x35 | 0x3d => {
                let i = op >> 3;
                let v = self.get_r(i);
                let r = v.wrapping_sub(1);
                self.set_flag(FLAG_Z, r == 0);
                self.set_flag(FLAG_N, true);
                self.set_flag(FLAG_H, v & 0xf == 0);
                self.set_r(i, r);
            }
            0x06 | 0x0e | 0x16 | 0x1e | 0x26 | 0x2e | 0x36 | 0x3e => {
                let v = self.fetch();
                self.set_r(op >> 3, v);
            }
            0x07 | 0x0f | 0x17 | 0x1f => {
                let (r, carry) = self.rotate((op >> 3) & 3, self.r.a);
                self.r.a = r;
                self.r.f = if carry { FLAG_C } else { 0 };
            }
            0x27 => {
                let mut adjust = 0u8;
                let mut carry = self.flag(FLAG_C);
                if self.flag(FLAG_H) || (!self.flag(FLAG_N) && self.r.a & 0xf > 9) {
                    adjust |= 0x06;
                }
                if carry || (!self.flag(FLAG_N) && self.r.a > 0x99) {
                    adjust |= 0x60;
                    carry = true;
                }
                self.r.a = if self.flag(FLAG_N) {
                    self.r.a.wrapping_sub(adjust)
                } else {
                    self.r.a.wrapping_add(adjust)
                };
                self.set_flag(FLAG_Z, self.r.a == 0);
                self.set_flag(FLAG_H, false);
                self.set_flag(FLAG_C, carry);
            }
            0x2f => {
                self.r.a = !self.r.a;
                self.set_flag(FLAG_N, true);
                self.set_flag(FLAG_H, true);
            }
            0x37 => {
                self.set_flag(FLAG_N, false);
                self.set_flag(FLAG_H, false);
                self.set_flag(FLAG_C, true);
            }
            0x3f => {
                let c = self.flag(FLAG_C);
                self.set_flag(FLAG_N, false);
                self.set_flag(FLAG_H, false);
                self.set_flag(FLAG_C, !c);
            }
            0x08 => {
                let addr = self.fetch16();
                self.mmu.write(addr, self.r.sp as u8);
                self.mmu.write(addr.wrapping_add(1), (self.r.sp >> 8) as u8);
            }
            0x40..=0x7f => {
                let v = self.get_r(op & 7);
                self.set_r((op >> 3) & 7, v);
            }
            0x80..=0xbf => {
                let v = self.get_r(op & 7);
                self.alu((op >> 3) & 7, v);
            }
            0xc6 | 0xce | 0xd6 | 0xde | 0xe6 | 0xee | 0xf6 | 0xfe => {
                let v = self.fetch();
                self.alu((op >> 3) & 7, v);
            }
            0xc1 | 0xd1 | 0xe1 | 0xf1 => {
                let lo = self.mmu.read(self.r.sp);
                let hi = self.mmu.read(self.r.sp.wrapping_add(1));
                self.r.sp = self.r.sp.wrapping_add(2);
                match op >> 4 {
                    0xc => {
                        self.r.c = lo;
                        self.r.b = hi;
                    }
                    0xd => {
                        self.r.e = lo;
                        self.r.d = hi;
                    }
                    0xe => {
                        self.r.l = lo;
                        self.r.h = hi;
                    }
                    _ => {
                        self.r.f = lo & 0xf0;
                        self.r.a = hi;
                    }
                }
            }
            0xc5 | 0xd5 | 0xe5 | 0xf5 => {
                let (hi, lo) = match op >> 4 {
                    0xc => (self.r.b, self.r.c),
                    0xd => (self.r.d, self.r.e),
                    0xe => (self.r.h, self.r.l),
                    _ => (self.r.a, self.r.f),
                };
                self.r.sp = self.r.sp.wrapping_sub(1);
                self.mmu.write(self.r.sp, hi);
                self.r.sp = self.r.sp.wrapping_sub(1);
                self.mmu.write(self.r.sp, lo);
            }
            0xe0 => {
                let off = self.fetch();
                self.mmu.write(0xff00 | off as u16, self.r.a);
            }
            0xf0 => {
                let off = self.fetch();
                self.r.a = self.mmu.read(0xff00 | off as u16);
            }
            0xe2 => {
                let addr = 0xff00 | self.r.c as u16;
                self.mmu.write(addr, self.r.a)
            }
            0xf2 => {
                let addr = 0xff00 | self.r.c as u16;
                self.r.a = self.mmu.read(addr)
            }
            0xea => {
                let addr = self.fetch16();
                self.mmu.write(addr, self.r.a);
            }
            0xfa => {
                let addr = self.fetch16();
                self.r.a = self.mmu.read(addr);
            }
            0xe8 | 0xf8 => {
                let offset = self.fetch() as i8 as u16;
                let sp = self.r.sp;
                self.r.f = 0;
                self.set_flag(FLAG_H, (sp & 0xf) + (offset & 0xf) > 0xf);
                self.set_flag(FLAG_C, (sp & 0xff) + (offset & 0xff) > 0xff);
                let result = sp.wrapping_add(offset);
                if op == 0xe8 {
                    self.r.sp = result;
                } else {
                    self.set_hl(result);
                }
            }
            0xf9 => self.r.sp = self.hl(),
            0xcb => {
                let cb = self.fetch();
                let i = cb & 7;
                let bit = (cb >> 3) & 7;
                let v = self.get_r(i);
                match cb >> 6 {
                    0 => {
                        let (r, carry) = self.rotate(bit, v);
                        self.r.f = 0;
                        self.set_flag(FLAG_Z, r == 0);
                        self.set_flag(FLAG_C, carry);
                        self.set_r(i, r);
                    }
                    1 => {
                        self.set_flag(FLAG_Z, v & (1 << bit) == 0);
                        self.set_flag(FLAG_N, false);
                        self.set_flag(FLAG_H, true);
                    }
                    2 => self.set_r(i, v & !(1 << bit)),
                    _ => self.set_r(i, v | (1 << bit)),
                }
            }
            other => panic!("the oracle was handed opcode {other:#04x}"),
        }
    }
}

// ---- harness -------------------------------------------------------------

/// A tiny reproducible generator, so a failure can be replayed exactly.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0 >> 17
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

/// Memory the comparison covers: work RAM plus high RAM.
const WATCHED: [(u16, u16); 2] = [(0xc000, 0xe000), (0xff80, 0xffff)];

fn cartridge(program: &[u8]) -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    rom[0x100..0x104].copy_from_slice(&[0x00, 0xc3, 0x50, 0x01]);
    rom[0x147] = 0x00;
    rom[0x150..0x150 + program.len()].copy_from_slice(program);
    let sum = rom[0x134..0x14d]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_sub(b).wrapping_sub(1));
    rom[0x14d] = sum;
    rom
}

/// Build the instruction bytes for `op`, choosing immediates that keep every
/// memory access inside RAM so the two runs see the same values.
fn encode(op: u8, cb: u8, rng: &mut Rng) -> Vec<u8> {
    let mut bytes = vec![op];
    match op {
        0xcb => bytes.push(cb),
        // 16-bit immediates that are used as addresses must land in work RAM.
        0x08 | 0xea | 0xfa => {
            let addr = 0xc200 + (rng.next() as u16 & 0x7f);
            bytes.extend_from_slice(&addr.to_le_bytes());
        }
        0x01 | 0x11 | 0x21 | 0x31 => {
            let value = 0xc300u16.wrapping_add(rng.next() as u16 & 0x7f);
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        // The high-page forms must reach high RAM, not the hardware registers.
        0xe0 | 0xf0 => bytes.push(0x80 | (rng.byte() & 0x7e)),
        0x06 | 0x0e | 0x16 | 0x1e | 0x26 | 0x2e | 0x36 | 0x3e | 0xc6 | 0xce | 0xd6 | 0xde
        | 0xe6 | 0xee | 0xf6 | 0xfe | 0xe8 | 0xf8 => bytes.push(rng.byte()),
        _ => {}
    }
    bytes
}

/// Registers to start from. Pointer registers are aimed at work RAM.
fn seed_regs(rng: &mut Rng) -> Regs {
    Regs {
        a: rng.byte(),
        f: rng.byte() & 0xf0,
        b: rng.byte(),
        // C doubles as a high-page pointer, so keep it out of the registers.
        c: 0x80 | (rng.byte() & 0x7e),
        d: 0xc4,
        e: rng.byte(),
        h: 0xc5,
        l: rng.byte(),
        sp: 0xdf00 + (rng.next() as u16 & 0x7e),
    }
}

fn watched_memory(mmu: &Mmu) -> Vec<u8> {
    let mut out = Vec::new();
    for (start, end) in WATCHED {
        for addr in start..end {
            out.push(mmu.peek(addr));
        }
    }
    out
}

fn fill_ram(mmu: &mut Mmu, rng: &mut Rng) {
    for addr in 0xc000u16..0xc800 {
        mmu.write(addr, rng.byte());
    }
    for addr in 0xdf00u16..0xdfff {
        mmu.write(addr, rng.byte());
    }
    for addr in 0xff80u16..0xffff {
        mmu.write(addr, rng.byte());
    }
}

/// Opcodes the harness cannot compare one instruction at a time: control flow
/// changes where execution goes, and HALT/STOP/EI/DI act on machine state whose
/// timing the oracle does not model. All of those have their own tests.
fn is_comparable(op: u8) -> bool {
    !matches!(
        op,
        0x10 | 0x76
            | 0xf3
            | 0xfb
            | 0x18
            | 0x20
            | 0x28
            | 0x30
            | 0x38
            | 0xc0
            | 0xc2
            | 0xc3
            | 0xc4
            | 0xc7
            | 0xc8
            | 0xc9
            | 0xca
            | 0xcc
            | 0xcd
            | 0xcf
            | 0xd0
            | 0xd2
            | 0xd4
            | 0xd7
            | 0xd8
            | 0xd9
            | 0xda
            | 0xdc
            | 0xdf
            | 0xe7
            | 0xe9
            | 0xef
            | 0xf7
            | 0xff
            | 0xd3
            | 0xdb
            | 0xdd
            | 0xe3
            | 0xe4
            | 0xeb
            | 0xec
            | 0xed
            | 0xf4
            | 0xfc
            | 0xfd
    )
}

/// Run one instruction both ways and compare everything observable.
fn compare(op: u8, cb: u8, seed: u64) -> Result<(), String> {
    let mut rng = Rng(seed);
    let instruction = encode(op, cb, &mut rng);
    let regs = seed_regs(&mut rng);
    let ram_seed = rng.0;

    let mut program = instruction.clone();
    program.push(0x76); // HALT, so the block ends and the run stops

    let config = Config::default();
    let rom = cartridge(&program);

    // The translated run.
    let mut emu = Emu::with_arena(rom.clone(), &config, 4 << 20).unwrap();
    fill_ram(&mut emu.host.mmu, &mut Rng(ram_seed));
    emu.state.a = regs.a;
    emu.state.f = regs.f;
    emu.state.b = regs.b;
    emu.state.c = regs.c;
    emu.state.d = regs.d;
    emu.state.e = regs.e;
    emu.state.h = regs.h;
    emu.state.l = regs.l;
    emu.state.sp = regs.sp;
    emu.state.pc = 0x0150;
    for _ in 0..64 {
        if emu.state.halted != 0 {
            break;
        }
        emu.step();
    }
    let translated = Regs {
        a: emu.state.a,
        f: emu.state.f,
        b: emu.state.b,
        c: emu.state.c,
        d: emu.state.d,
        e: emu.state.e,
        h: emu.state.h,
        l: emu.state.l,
        sp: emu.state.sp,
    };
    let translated_ram = watched_memory(&emu.host.mmu);

    // The reference run, over an identical machine.
    let mut oracle_emu = Emu::with_arena(rom, &config, 1 << 20).unwrap();
    fill_ram(&mut oracle_emu.host.mmu, &mut Rng(ram_seed));
    let mut reference = Reference {
        r: regs,
        pc: 0x0150,
        mmu: &mut oracle_emu.host.mmu,
    };
    reference.step();
    let expected = reference.r;
    let expected_ram = watched_memory(&oracle_emu.host.mmu);

    let name = if op == 0xcb {
        format!("CB {cb:02X}")
    } else {
        format!("{op:02X}")
    };

    if translated != expected {
        return Err(format!(
            "opcode {name} (seed {seed}) produced {translated:02x?}\n\
             but the reference produced          {expected:02x?}\n\
             starting from {regs:02x?}"
        ));
    }
    if translated_ram != expected_ram {
        let at = translated_ram
            .iter()
            .zip(&expected_ram)
            .position(|(a, b)| a != b)
            .unwrap();
        return Err(format!(
            "opcode {name} (seed {seed}) wrote {:#04x} where the reference wrote {:#04x} \
             (byte {at} of the watched memory)",
            translated_ram[at], expected_ram[at]
        ));
    }
    Ok(())
}

#[test]
fn every_plain_opcode_matches_the_reference() {
    let mut failures = Vec::new();
    for op in 0u16..=0xff {
        let op = op as u8;
        if op == 0xcb || !is_comparable(op) {
            continue;
        }
        for seed in 0..12u64 {
            if let Err(message) = compare(op, 0, seed * 7919 + op as u64) {
                failures.push(message);
                break; // one report per opcode is enough to act on
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} opcodes disagree:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn every_cb_prefixed_opcode_matches_the_reference() {
    let mut failures = Vec::new();
    for cb in 0u16..=0xff {
        for seed in 0..6u64 {
            if let Err(message) = compare(0xcb, cb as u8, seed * 104_729 + cb as u64) {
                failures.push(message);
                break;
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} CB opcodes disagree:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
