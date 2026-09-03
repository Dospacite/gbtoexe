//! Finding the code in a cartridge before it runs.
//!
//! Recursive descent from the hardware's entry points follows every jump and
//! call whose destination is written into the instruction. What it cannot follow
//! — computed jumps, and calls into whichever bank happens to be mapped — is left
//! to the runtime, which translates those the first time the game goes there.

use std::collections::HashSet;

use crate::decode::{decode, Flow};
use crate::exec::BlockKey;

/// Where the hardware can start executing without being told to: the cartridge
/// entry point, the eight restart vectors, and the five interrupt vectors.
pub const ENTRY_POINTS: [u16; 14] = [
    0x0100, 0x0000, 0x0008, 0x0010, 0x0018, 0x0020, 0x0028, 0x0030, 0x0038, 0x0040, 0x0048, 0x0050,
    0x0058, 0x0060,
];

#[derive(Debug, Clone)]
pub struct Report {
    /// Every block worth translating ahead of time.
    pub blocks: Vec<BlockKey>,
    /// Addresses in the switchable window that some bank calls into.
    pub shared_entries: usize,
    /// True if the budget ran out before discovery settled.
    pub truncated: bool,
}

/// Read a byte as it would appear with `bank` selected in 0x4000-0x7FFF.
pub fn peek(rom: &[u8], bank: u16, addr: u16) -> u8 {
    let index = match addr {
        0x0000..=0x3fff => addr as usize,
        0x4000..=0x7fff => bank as usize * 0x4000 + (addr as usize - 0x4000),
        // Nothing outside the cartridge can be read ahead of time.
        _ => return 0xff,
    };
    rom.get(index).copied().unwrap_or(0xff)
}

fn region(addr: u16) -> u8 {
    match addr {
        0x0000..=0x3fff => 0,
        0x4000..=0x7fff => 1,
        _ => 2,
    }
}

/// Walk one basic block, reporting where it ends and where it can go next.
fn scan_block(rom: &[u8], bank: u16, start: u16) -> (u16, Vec<u16>) {
    let mut pc = start;
    let mut successors = Vec::new();
    // Matches the translator's own cap so discovery and translation agree.
    const MAX_INSTRUCTIONS: usize = 2048;

    for _ in 0..MAX_INSTRUCTIONS {
        let insn = decode(pc, &|a| peek(rom, bank, a));
        pc = pc.wrapping_add(insn.len as u16);

        match insn.flow {
            Flow::Normal => {
                if region(pc) != region(start) {
                    successors.push(pc);
                    return (pc, successors);
                }
                continue;
            }
            Flow::Jump {
                target,
                conditional,
            } => {
                successors.push(target);
                if conditional {
                    successors.push(pc);
                }
            }
            Flow::Call { target, .. } => {
                successors.push(target);
                successors.push(pc);
            }
            Flow::Rst { target } => {
                successors.push(target);
                successors.push(pc);
            }
            Flow::Ret { conditional, .. } => {
                if conditional {
                    successors.push(pc);
                }
            }
            // The destination only exists at run time.
            Flow::JumpIndirect | Flow::Illegal => {}
            Flow::Halt | Flow::Stop => successors.push(pc),
        }
        return (pc, successors);
    }
    (pc, successors)
}

/// Decide whether a speculative entry point really holds code in this bank.
///
/// Bank 0 calls into the switchable window at addresses that are only valid for
/// whichever bank is mapped at the time, and there is no way to know statically
/// which that is. So each candidate is tried against every bank, and this is
/// what rejects the ones that land in data: a run of bytes that is not code
/// almost always decodes into an opcode the hardware does not have, or rambles
/// on without ever reaching a return or a jump.
fn looks_like_code(rom: &[u8], bank: u16, addr: u16) -> bool {
    const HORIZON: usize = 32;
    let mut pc = addr;

    for _ in 0..HORIZON {
        let insn = decode(pc, &|a| peek(rom, bank, a));
        if insn.flow == Flow::Illegal {
            return false;
        }
        // Reaching a return or a jump means this reads as a real routine.
        if insn.flow.ends_block() {
            return !matches!(insn.flow, Flow::Stop);
        }
        pc = pc.wrapping_add(insn.len as u16);
        if region(pc) != region(addr) {
            return false;
        }
    }
    // Thirty-two instructions of straight-line code with no branch in sight is
    // not how games are written; this is data that happens to decode.
    false
}

/// Descend from `seeds` within one bank, adding what it finds to `found`.
fn descend(
    rom: &[u8],
    bank: u16,
    seeds: &[u16],
    found: &mut HashSet<BlockKey>,
    shared: &mut HashSet<u16>,
    budget: &mut usize,
) -> bool {
    let mut work: Vec<u16> = seeds.to_vec();
    let mut truncated = false;

    while let Some(addr) = work.pop() {
        // RAM-resident code is not in the file and cannot be found from here.
        if region(addr) == 2 {
            continue;
        }
        // In the fixed window the bank is irrelevant, so key those blocks once.
        let key = BlockKey {
            bank: if region(addr) == 0 { 0 } else { bank },
            addr,
        };
        if !found.insert(key) {
            continue;
        }
        if *budget == 0 {
            truncated = true;
            break;
        }
        *budget -= 1;

        let (_, successors) = scan_block(rom, bank, addr);
        for target in successors {
            if region(target) == 1 {
                // Which bank this belongs to is a runtime question; remember the
                // address so every bank gets a chance at it.
                shared.insert(target);
            }
            if region(target) != 2 {
                work.push(target);
            }
        }
    }
    truncated
}

/// Find everything in `rom` worth translating before the game starts.
///
/// `budget` caps the number of blocks, so a cartridge whose data happens to
/// decode as plausible code cannot make conversion run away.
pub fn discover(rom: &[u8], budget: usize) -> Report {
    discover_with(rom, budget, 4096)
}

/// As `discover`, but with an explicit per-bank allowance for speculative work.
pub fn discover_with(rom: &[u8], budget: usize, per_bank: usize) -> Report {
    let banks = (rom.len() / 0x4000).max(2) as u16;
    let mut found: HashSet<BlockKey> = HashSet::new();
    let mut shared: HashSet<u16> = HashSet::new();
    let mut remaining = budget;

    // The fixed window first: everything here is reachable whatever is mapped.
    let mut truncated = descend(
        rom,
        1,
        &ENTRY_POINTS,
        &mut found,
        &mut shared,
        &mut remaining,
    );

    // Banked code is entered at addresses the fixed window calls into. A game
    // switches the bank and then calls a known address, so the same entry point
    // is usually valid in many banks; the ones where it is not decode as
    // nonsense and get rejected.
    let shared_entries = shared.len();
    let mut seeds: Vec<u16> = shared.iter().copied().collect();
    seeds.sort_unstable();

    for bank in 1..banks {
        // A zero allowance means the caller only wants the fixed window.
        if truncated || per_bank == 0 {
            break;
        }
        let before = found.len();
        let viable: Vec<u16> = seeds
            .iter()
            .copied()
            .filter(|&addr| looks_like_code(rom, bank, addr))
            .collect();
        if viable.is_empty() {
            continue;
        }
        // Bank-local discovery is speculative, so give each bank a fixed
        // allowance. Anything missed is translated when the game gets there.
        let mut allowance = per_bank.min(remaining);
        let mut bank_shared = HashSet::new();
        descend(
            rom,
            bank,
            &viable,
            &mut found,
            &mut bank_shared,
            &mut allowance,
        );
        let spent = found.len() - before;
        remaining = remaining.saturating_sub(spent);
        if remaining == 0 {
            truncated = true;
        }
    }

    let mut blocks: Vec<BlockKey> = found.into_iter().collect();
    blocks.sort_unstable();

    Report {
        blocks,
        shared_entries,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rom_with(program: &[(u16, &[u8])]) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        for &(addr, bytes) in program {
            rom[addr as usize..addr as usize + bytes.len()].copy_from_slice(bytes);
        }
        rom
    }

    #[test]
    fn discovery_follows_a_call_and_its_return() {
        // 0x0100: CALL 0x0200 ; HALT      0x0200: NOP ; RET
        let rom = rom_with(&[(0x0100, &[0xcd, 0x00, 0x02, 0x76]), (0x0200, &[0x00, 0xc9])]);
        let report = discover(&rom, 1000);
        assert!(report.blocks.iter().any(|b| b.addr == 0x0100));
        assert!(report.blocks.iter().any(|b| b.addr == 0x0200));
        assert!(!report.truncated);
    }

    #[test]
    fn both_sides_of_a_conditional_branch_are_found() {
        // 0x0100: JR NZ,+2 ; HALT ; LD A,1 ; HALT
        let rom = rom_with(&[(0x0100, &[0x20, 0x01, 0x76, 0x3e, 0x01, 0x76])]);
        let report = discover(&rom, 1000);
        let addrs: Vec<u16> = report.blocks.iter().map(|b| b.addr).collect();
        assert!(addrs.contains(&0x0102), "fall-through not found");
        assert!(addrs.contains(&0x0103), "branch target not found");
    }

    #[test]
    fn a_computed_jump_stops_discovery_without_failing() {
        // JP (HL) leads somewhere only the running game knows.
        let rom = rom_with(&[(0x0100, &[0xe9])]);
        let report = discover(&rom, 1000);
        assert!(report.blocks.iter().any(|b| b.addr == 0x0100));
        assert!(!report.truncated);
    }

    #[test]
    fn the_budget_stops_a_runaway() {
        // A ROM of zeroes decodes as an endless run of NOPs in every bank.
        let rom = vec![0u8; 0x8000];
        let report = discover(&rom, 4);
        assert!(report.truncated);
        assert!(report.blocks.len() <= 8);
    }

    #[test]
    fn banked_entry_points_are_tried_in_every_bank() {
        let mut rom = vec![0u8; 0x10000]; // four banks
                                          // Bank 0 calls into the switchable window.
        rom[0x0100..0x0104].copy_from_slice(&[0xcd, 0x00, 0x40, 0x76]);
        // Bank 1 and bank 2 both answer at 0x4000 with different code.
        rom[0x4000..0x4003].copy_from_slice(&[0x3e, 0x01, 0xc9]);
        rom[0x8000..0x8003].copy_from_slice(&[0x3e, 0x02, 0xc9]);

        let report = discover(&rom, 1000);
        assert!(report
            .blocks
            .iter()
            .any(|b| b.bank == 1 && b.addr == 0x4000));
        assert!(report
            .blocks
            .iter()
            .any(|b| b.bank == 2 && b.addr == 0x4000));
    }

    #[test]
    fn data_that_decodes_as_an_illegal_opcode_is_rejected() {
        let mut rom = vec![0u8; 0x10000];
        rom[0x0100..0x0104].copy_from_slice(&[0xcd, 0x00, 0x40, 0x76]);
        rom[0x4000..0x4003].copy_from_slice(&[0x3e, 0x01, 0xc9]);
        // Bank 2's 0x4000 is data that starts with an opcode the CPU lacks.
        rom[0x8000..0x8003].copy_from_slice(&[0xdd, 0xdd, 0xdd]);

        let report = discover(&rom, 1000);
        assert!(!report
            .blocks
            .iter()
            .any(|b| b.bank == 2 && b.addr == 0x4000));
    }
}
