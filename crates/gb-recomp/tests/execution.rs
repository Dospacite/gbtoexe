//! End-to-end checks that translated x86-64 computes what the SM83 would have.
//!
//! Each test assembles a short program, converts it, runs it on the host CPU
//! and inspects the resulting machine state.

use gb_hw::Config;
use gb_recomp::machine::Emu;

/// Wrap a program in a minimal cartridge, entered at 0x0150.
fn cartridge(program: &[u8]) -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    rom[0x100..0x104].copy_from_slice(&[0x00, 0xc3, 0x50, 0x01]); // NOP; JP 0x0150
    rom[0x147] = 0x00; // ROM only, no mapper
    rom[0x150..0x150 + program.len()].copy_from_slice(program);
    let sum = rom[0x134..0x14d]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_sub(b).wrapping_sub(1));
    rom[0x14d] = sum;
    rom
}

/// Run until the program halts. Returns the machine so state can be inspected.
fn run(program: &[u8]) -> Emu {
    // A small arena keeps the test suite from reserving gigabytes.
    let mut emu = Emu::with_arena(cartridge(program), &Config::default(), 4 << 20).unwrap();
    for _ in 0..200_000 {
        if emu.state.halted != 0 || emu.stopped || emu.fault.is_some() {
            break;
        }
        emu.step();
    }
    assert!(emu.fault.is_none(), "faulted: {:?}", emu.fault);
    assert!(emu.state.halted != 0, "program never halted");
    emu
}

const HALT: u8 = 0x76;

#[test]
fn addition_sets_the_half_carry() {
    // LD A,0x0F ; ADD A,0x01
    let emu = run(&[0x3e, 0x0f, 0xc6, 0x01, HALT]);
    assert_eq!(emu.state.a, 0x10);
    assert_eq!(emu.state.f & 0x20, 0x20, "H should be set");
    assert_eq!(emu.state.f & 0x80, 0x00, "Z should be clear");
    assert_eq!(emu.state.f & 0x10, 0x00, "C should be clear");
}

#[test]
fn addition_carries_out_of_the_byte() {
    // LD A,0xFF ; ADD A,0x02
    let emu = run(&[0x3e, 0xff, 0xc6, 0x02, HALT]);
    assert_eq!(emu.state.a, 0x01);
    assert_eq!(emu.state.f & 0x10, 0x10, "C should be set");
    assert_eq!(emu.state.f & 0x20, 0x20, "H should be set");
}

#[test]
fn subtraction_borrows_and_sets_n() {
    // LD A,0x10 ; SUB 0x01
    let emu = run(&[0x3e, 0x10, 0xd6, 0x01, HALT]);
    assert_eq!(emu.state.a, 0x0f);
    assert_eq!(emu.state.f & 0x40, 0x40, "N should be set");
    assert_eq!(emu.state.f & 0x20, 0x20, "H should be set");
    assert_eq!(emu.state.f & 0x10, 0x00, "C should be clear");
}

#[test]
fn subtract_with_carry_handles_the_full_borrow() {
    // LD A,0x00 ; SCF ; SBC A,0xFF  -> 0x00 - 0xFF - 1 = 0x00 with a borrow
    let emu = run(&[0x3e, 0x00, 0x37, 0xde, 0xff, HALT]);
    assert_eq!(emu.state.a, 0x00);
    assert_eq!(emu.state.f & 0x10, 0x10, "C should be set");
    assert_eq!(emu.state.f & 0x80, 0x80, "Z should be set");
}

#[test]
fn compare_leaves_the_accumulator_alone() {
    // LD A,0x42 ; CP 0x42
    let emu = run(&[0x3e, 0x42, 0xfe, 0x42, HALT]);
    assert_eq!(emu.state.a, 0x42);
    assert_eq!(emu.state.f & 0x80, 0x80, "Z should be set");
}

#[test]
fn logic_ops_set_their_fixed_flags() {
    // LD A,0xF0 ; AND 0x0F  -> zero, and AND always sets H
    let emu = run(&[0x3e, 0xf0, 0xe6, 0x0f, HALT]);
    assert_eq!(emu.state.a, 0x00);
    assert_eq!(emu.state.f, 0xa0, "expected Z and H only");

    // LD A,0xF0 ; XOR 0xFF
    let emu = run(&[0x3e, 0xf0, 0xee, 0xff, HALT]);
    assert_eq!(emu.state.a, 0x0f);
    assert_eq!(emu.state.f, 0x00);
}

#[test]
fn decimal_adjust_corrects_bcd() {
    // LD A,0x09 ; ADD A,0x01 ; DAA  -> 0x10, not 0x0A
    let emu = run(&[0x3e, 0x09, 0xc6, 0x01, 0x27, HALT]);
    assert_eq!(emu.state.a, 0x10);

    // LD A,0x10 ; SUB 0x01 ; DAA  -> 0x09
    let emu = run(&[0x3e, 0x10, 0xd6, 0x01, 0x27, HALT]);
    assert_eq!(emu.state.a, 0x09);

    // 0x99 + 0x01 rolls over to 0x00 and carries.
    let emu = run(&[0x3e, 0x99, 0xc6, 0x01, 0x27, HALT]);
    assert_eq!(emu.state.a, 0x00);
    assert_eq!(emu.state.f & 0x10, 0x10, "C should be set");
    assert_eq!(emu.state.f & 0x80, 0x80, "Z should be set");
}

#[test]
fn increment_preserves_the_carry_flag() {
    // SCF ; LD B,0xFF ; INC B  -> B wraps to 0, C untouched
    let emu = run(&[0x37, 0x06, 0xff, 0x04, HALT]);
    assert_eq!(emu.state.b, 0x00);
    assert_eq!(emu.state.f & 0x10, 0x10, "C must survive INC");
    assert_eq!(emu.state.f & 0x80, 0x80, "Z should be set");
    assert_eq!(emu.state.f & 0x20, 0x20, "H should be set");
}

#[test]
fn sixteen_bit_add_sets_carry_from_bit_eleven() {
    // LD HL,0x0FFF ; LD BC,0x0001 ; ADD HL,BC
    let emu = run(&[0x21, 0xff, 0x0f, 0x01, 0x01, 0x00, 0x09, HALT]);
    assert_eq!(u16::from_le_bytes([emu.state.l, emu.state.h]), 0x1000);
    assert_eq!(emu.state.f & 0x20, 0x20, "H comes from bit 11");
    assert_eq!(emu.state.f & 0x10, 0x00, "no carry out of 16 bits");
}

#[test]
fn a_counted_loop_runs_the_right_number_of_times() {
    // LD B,10 ; LD A,0 : loop: INC A ; DEC B ; JR NZ,loop
    let emu = run(&[
        0x06, 0x0a, // LD B,10
        0x3e, 0x00, // LD A,0
        0x3c, // INC A
        0x05, // DEC B
        0x20, 0xfc, // JR NZ,-4
        HALT,
    ]);
    assert_eq!(emu.state.a, 10);
    assert_eq!(emu.state.b, 0);
}

#[test]
fn call_and_return_restore_the_stack() {
    // CALL 0x0160 ; HALT   /  0x0160: LD A,0x77 ; RET
    let mut program = vec![0xcd, 0x60, 0x01, HALT];
    program.resize(0x10, 0x00);
    program.extend_from_slice(&[0x3e, 0x77, 0xc9]);
    let emu = run(&program);
    assert_eq!(emu.state.a, 0x77);
    assert_eq!(
        emu.state.sp, 0xfffe,
        "stack pointer should be back where it started"
    );
}

#[test]
fn push_and_pop_round_trip_through_memory() {
    // LD BC,0x1234 ; PUSH BC ; POP DE
    let emu = run(&[0x01, 0x34, 0x12, 0xc5, 0xd1, HALT]);
    assert_eq!(emu.state.d, 0x12);
    assert_eq!(emu.state.e, 0x34);
    assert_eq!(emu.state.sp, 0xfffe);
}

#[test]
fn popping_af_drops_the_low_nibble() {
    // LD BC,0x12FF ; PUSH BC ; POP AF
    let emu = run(&[0x01, 0xff, 0x12, 0xc5, 0xf1, HALT]);
    assert_eq!(emu.state.a, 0x12);
    assert_eq!(emu.state.f, 0xf0, "the bottom four flag bits do not exist");
}

#[test]
fn work_ram_round_trips_a_byte() {
    // LD HL,0xC000 ; LD (HL),0x42 ; LD A,0x00 ; LD A,(HL)
    let emu = run(&[0x21, 0x00, 0xc0, 0x36, 0x42, 0x3e, 0x00, 0x7e, HALT]);
    assert_eq!(emu.state.a, 0x42);
}

#[test]
fn load_increment_walks_up_memory() {
    // Fill 0xC000..0xC004 with 0..4 using LD (HL+),A
    let emu = run(&[
        0x21, 0x00, 0xc0, // LD HL,0xC000
        0x3e, 0x00, // LD A,0
        0x06, 0x05, // LD B,5
        0x22, // LD (HL+),A
        0x3c, // INC A
        0x05, // DEC B
        0x20, 0xfb, // JR NZ,-5
        HALT,
    ]);
    assert_eq!(u16::from_le_bytes([emu.state.l, emu.state.h]), 0xc005);
    for i in 0..5u16 {
        assert_eq!(emu.host.mmu.peek(0xc000 + i), i as u8);
    }
}

#[test]
fn bit_test_reports_a_clear_bit() {
    // LD A,0x7F ; BIT 7,A
    let emu = run(&[0x3e, 0x7f, 0xcb, 0x7f, HALT]);
    assert_eq!(emu.state.f & 0x80, 0x80, "Z set because bit 7 is clear");
    assert_eq!(emu.state.f & 0x20, 0x20, "BIT always sets H");
    assert_eq!(emu.state.a, 0x7f, "BIT must not change the operand");
}

#[test]
fn rotate_through_carry_moves_one_bit_around() {
    // SCF ; LD A,0x00 ; RL A   -> carry rotates into bit 0
    let emu = run(&[0x37, 0x3e, 0x00, 0xcb, 0x17, HALT]);
    assert_eq!(emu.state.a, 0x01);
    assert_eq!(emu.state.f & 0x10, 0x00, "nothing fell out the top");
}

#[test]
fn swap_exchanges_the_nibbles() {
    // LD A,0xAB ; SWAP A
    let emu = run(&[0x3e, 0xab, 0xcb, 0x37, HALT]);
    assert_eq!(emu.state.a, 0xba);
    assert_eq!(emu.state.f, 0x00);
}

#[test]
fn arithmetic_shift_right_keeps_the_sign() {
    // LD A,0x81 ; SRA A
    let emu = run(&[0x3e, 0x81, 0xcb, 0x2f, HALT]);
    assert_eq!(emu.state.a, 0xc0);
    assert_eq!(emu.state.f & 0x10, 0x10, "bit 0 fell into the carry");
}

#[test]
fn set_and_reset_touch_only_one_bit() {
    // LD A,0x00 ; SET 3,A ; RES 0,A
    let emu = run(&[0x3e, 0x00, 0xcb, 0xdf, 0xcb, 0x87, HALT]);
    assert_eq!(emu.state.a, 0x08);
}

#[test]
fn restart_vectors_are_reachable() {
    // The RST 0x08 handler lives in the header area, so build it by hand.
    let mut rom = cartridge(&[0xcf, HALT]); // RST 0x08 ; HALT
    rom[0x08] = 0x3e; // LD A,0x5A
    rom[0x09] = 0x5a;
    rom[0x0a] = 0xc9; // RET
    let sum = rom[0x134..0x14d]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_sub(b).wrapping_sub(1));
    rom[0x14d] = sum;

    let mut emu = Emu::with_arena(rom, &Config::default(), 4 << 20).unwrap();
    for _ in 0..10_000 {
        if emu.state.halted != 0 {
            break;
        }
        emu.step();
    }
    assert_eq!(emu.state.a, 0x5a);
}

#[test]
fn a_jump_table_dispatches_through_the_inline_cache() {
    // LD HL,0x0170 ; JP (HL)   /  0x0170: LD A,0x99 ; HALT
    let mut program = vec![0x21, 0x70, 0x01, 0xe9];
    program.resize(0x20, 0x00);
    program.extend_from_slice(&[0x3e, 0x99, HALT]);
    let emu = run(&program);
    assert_eq!(emu.state.a, 0x99);
}

#[test]
fn code_copied_into_ram_runs() {
    // Copy "LD A,0x33 ; RET" to 0xC000, then CALL it. Nothing in the cartridge
    // image contains that block, so it can only come from translating RAM.
    let emu = run(&[
        0x21, 0x00, 0xc0, // LD HL,0xC000
        0x36, 0x3e, 0x23, // LD (HL),0x3E ; INC HL
        0x36, 0x33, 0x23, // LD (HL),0x33 ; INC HL
        0x36, 0xc9, // LD (HL),0xC9   (RET)
        0xcd, 0x00, 0xc0, // CALL 0xC000
        HALT,
    ]);
    assert_eq!(emu.state.a, 0x33);
}

#[test]
fn rewriting_ram_code_retranslates_it() {
    // Run a routine from RAM, overwrite it with a different one, run it again.
    // A stale translation would return the first value both times.
    let emu = run(&[
        0x21, 0x00, 0xc0, // LD HL,0xC000
        0x36, 0x3e, 0x23, // LD (HL),0x3E ; INC HL
        0x36, 0x11, 0x23, // LD (HL),0x11 ; INC HL
        0x36, 0xc9, // LD (HL),0xC9
        0xcd, 0x00, 0xc0, // CALL 0xC000     -> A = 0x11
        0x21, 0x01, 0xc0, // LD HL,0xC001
        0x36, 0x22, // LD (HL),0x22    rewrite the immediate
        0xcd, 0x00, 0xc0, // CALL 0xC000     -> A = 0x22
        HALT,
    ]);
    assert_eq!(emu.state.a, 0x22, "the rewritten routine should have run");
}

#[test]
fn timing_matches_the_hardware_cycle_counts() {
    // NOP is 4 cycles, LD A,d8 is 8, JP nn is 16. Entry costs NOP + JP = 20.
    let mut emu =
        Emu::with_arena(cartridge(&[0x00, 0x00, HALT]), &Config::default(), 4 << 20).unwrap();
    while emu.state.halted == 0 {
        emu.step();
    }
    // 20 to enter, two NOPs at 4 each, and the HALT's own 4.
    assert_eq!(emu.host.mmu.total_cycles(), 20 + 4 + 4 + 4);
}

#[test]
fn memory_access_timing_is_charged_correctly() {
    // LD HL,d16 (12) ; LD (HL),d8 (12) ; HALT (4), after the 20-cycle entry.
    let mut emu = Emu::with_arena(
        cartridge(&[0x21, 0x00, 0xc0, 0x36, 0x42, HALT]),
        &Config::default(),
        4 << 20,
    )
    .unwrap();
    while emu.state.halted == 0 {
        emu.step();
    }
    assert_eq!(emu.host.mmu.total_cycles(), 20 + 12 + 12 + 4);
}

#[test]
fn a_taken_branch_costs_more_than_one_not_taken() {
    // JR NZ over a HALT that is never reached: taken is 12, not taken 8.
    let taken = {
        let mut emu = Emu::with_arena(
            // LD A,1 (8) ; OR A (4, clears Z) ; JR NZ,+1 (12) ; HALT
            cartridge(&[0x3e, 0x01, 0xb7, 0x20, 0x01, 0x00, HALT]),
            &Config::default(),
            4 << 20,
        )
        .unwrap();
        while emu.state.halted == 0 {
            emu.step();
        }
        emu.host.mmu.total_cycles()
    };
    let not_taken = {
        let mut emu = Emu::with_arena(
            // LD A,0 (8) ; OR A (4, sets Z) ; JR NZ,+1 (8) ; NOP (4) ; HALT
            cartridge(&[0x3e, 0x00, 0xb7, 0x20, 0x01, 0x00, HALT]),
            &Config::default(),
            4 << 20,
        )
        .unwrap();
        while emu.state.halted == 0 {
            emu.step();
        }
        emu.host.mmu.total_cycles()
    };
    // The taken path skips a 4-cycle NOP but pays 4 more for the branch.
    assert_eq!(taken, 20 + 8 + 4 + 12 + 4);
    assert_eq!(not_taken, 20 + 8 + 4 + 8 + 4 + 4);
}

#[test]
fn an_illegal_opcode_is_reported_rather_than_run() {
    // 0xDD is not wired to anything; real hardware locks up on it.
    let mut emu = Emu::with_arena(cartridge(&[0xdd]), &Config::default(), 4 << 20).unwrap();
    for _ in 0..100 {
        emu.step();
        if emu.fault.is_some() {
            break;
        }
    }
    let fault = emu.fault.expect("should have reported the bad opcode");
    assert!(fault.contains("0x0150"), "should name the address: {fault}");
    assert!(fault.contains("0xdd"), "should name the opcode: {fault}");
}

#[test]
fn stop_halts_the_processor_without_reporting_a_fault() {
    // STOP with no speed switch armed parks the CPU until input arrives.
    let mut emu = Emu::with_arena(cartridge(&[0x10, 0x00]), &Config::default(), 4 << 20).unwrap();
    for _ in 0..100 {
        emu.step();
        if emu.stopped {
            break;
        }
    }
    assert!(emu.stopped, "STOP should have stopped the processor");
    assert!(emu.fault.is_none(), "STOP is not a fault: {:?}", emu.fault);
}
