//! Audio processing unit: two pulse channels, a wavetable channel and a noise
//! channel, mixed and resampled to the host's rate.

const DUTY_TABLE: [[u8; 8]; 4] = [
    [0, 0, 0, 0, 0, 0, 0, 1],
    [1, 0, 0, 0, 0, 0, 0, 1],
    [1, 0, 0, 0, 0, 1, 1, 1],
    [0, 1, 1, 1, 1, 1, 1, 0],
];

/// Noise clock divisors, indexed by the low three bits of NR43.
const NOISE_DIVISOR: [u32; 8] = [8, 16, 32, 48, 64, 80, 96, 112];

const CPU_HZ: f64 = 4_194_304.0;

/// Volume envelope shared by the pulse and noise channels.
#[derive(Default, Clone)]
struct Envelope {
    initial: u8,
    increasing: bool,
    period: u8,
    volume: u8,
    timer: u8,
    enabled: bool,
}

impl Envelope {
    fn write(&mut self, value: u8) {
        self.initial = value >> 4;
        self.increasing = value & 0x08 != 0;
        self.period = value & 0x07;
    }

    fn byte(&self) -> u8 {
        (self.initial << 4) | ((self.increasing as u8) << 3) | self.period
    }

    fn trigger(&mut self) {
        self.volume = self.initial;
        self.timer = if self.period == 0 { 8 } else { self.period };
        self.enabled = true;
    }

    fn step(&mut self) {
        if self.period == 0 || !self.enabled {
            return;
        }
        self.timer = self.timer.saturating_sub(1);
        if self.timer == 0 {
            self.timer = self.period;
            if self.increasing && self.volume < 15 {
                self.volume += 1;
            } else if !self.increasing && self.volume > 0 {
                self.volume -= 1;
            } else {
                self.enabled = false;
            }
        }
    }

    /// A DAC with all of volume/direction zero is powered down, silencing the channel.
    fn dac_on(&self) -> bool {
        self.initial != 0 || self.increasing
    }
}

#[derive(Default, Clone)]
struct LengthCounter {
    value: u16,
    enabled: bool,
    max: u16,
}

impl LengthCounter {
    fn new(max: u16) -> Self {
        LengthCounter {
            value: 0,
            enabled: false,
            max,
        }
    }

    fn reload(&mut self, written: u16) {
        self.value = self.max - written;
    }

    /// Returns true when the counter just ran out and the channel must stop.
    fn step(&mut self) -> bool {
        if self.enabled && self.value > 0 {
            self.value -= 1;
            return self.value == 0;
        }
        false
    }
}

#[derive(Clone)]
struct Pulse {
    enabled: bool,
    duty: u8,
    phase: usize,
    freq: u16,
    timer: i32,
    envelope: Envelope,
    length: LengthCounter,

    // Sweep unit; only channel 1 has one.
    has_sweep: bool,
    sweep_period: u8,
    sweep_negate: bool,
    sweep_shift: u8,
    sweep_timer: u8,
    sweep_shadow: u16,
    sweep_enabled: bool,
    /// Set once a negate-mode calculation has happened, per the sweep quirk.
    sweep_negated: bool,
}

impl Pulse {
    fn new(has_sweep: bool) -> Self {
        Pulse {
            enabled: false,
            duty: 2,
            phase: 0,
            freq: 0,
            timer: 0,
            envelope: Envelope::default(),
            length: LengthCounter::new(64),
            has_sweep,
            sweep_period: 0,
            sweep_negate: false,
            sweep_shift: 0,
            sweep_timer: 0,
            sweep_shadow: 0,
            sweep_enabled: false,
            sweep_negated: false,
        }
    }

    fn step(&mut self, t: u32) {
        self.timer -= t as i32;
        while self.timer <= 0 {
            self.timer += (2048 - self.freq as i32) * 4;
            self.phase = (self.phase + 1) & 7;
        }
    }

    fn sample(&self) -> f32 {
        if !self.enabled || !self.envelope.dac_on() {
            return 0.0;
        }
        let level = DUTY_TABLE[self.duty as usize][self.phase] * self.envelope.volume;
        level as f32 / 15.0
    }

    fn trigger(&mut self) {
        self.enabled = true;
        if self.length.value == 0 {
            self.length.value = self.length.max;
        }
        self.timer = (2048 - self.freq as i32) * 4;
        self.envelope.trigger();

        if self.has_sweep {
            self.sweep_shadow = self.freq;
            self.sweep_timer = if self.sweep_period == 0 {
                8
            } else {
                self.sweep_period
            };
            self.sweep_enabled = self.sweep_period != 0 || self.sweep_shift != 0;
            self.sweep_negated = false;
            if self.sweep_shift != 0 && self.next_sweep_freq() > 2047 {
                self.enabled = false;
            }
        }
        if !self.envelope.dac_on() {
            self.enabled = false;
        }
    }

    fn next_sweep_freq(&mut self) -> u16 {
        let delta = self.sweep_shadow >> self.sweep_shift;
        if self.sweep_negate {
            self.sweep_negated = true;
            self.sweep_shadow.wrapping_sub(delta)
        } else {
            self.sweep_shadow.wrapping_add(delta)
        }
    }

    fn step_sweep(&mut self) {
        if !self.has_sweep || !self.sweep_enabled {
            return;
        }
        self.sweep_timer = self.sweep_timer.saturating_sub(1);
        if self.sweep_timer != 0 {
            return;
        }
        self.sweep_timer = if self.sweep_period == 0 {
            8
        } else {
            self.sweep_period
        };
        if self.sweep_period == 0 {
            return;
        }

        let new_freq = self.next_sweep_freq();
        if new_freq > 2047 {
            self.enabled = false;
        } else if self.sweep_shift != 0 {
            self.sweep_shadow = new_freq;
            self.freq = new_freq;
            // The overflow check runs a second time, discarding the result.
            if self.next_sweep_freq() > 2047 {
                self.enabled = false;
            }
        }
    }
}

#[derive(Clone)]
struct Wave {
    enabled: bool,
    dac_on: bool,
    volume_shift: u8,
    freq: u16,
    timer: i32,
    position: usize,
    length: LengthCounter,
    ram: [u8; 16],
    sample_buffer: u8,
}

impl Wave {
    fn new() -> Self {
        Wave {
            enabled: false,
            dac_on: false,
            volume_shift: 0,
            freq: 0,
            timer: 0,
            position: 0,
            length: LengthCounter::new(256),
            ram: [0; 16],
            sample_buffer: 0,
        }
    }

    fn step(&mut self, t: u32) {
        self.timer -= t as i32;
        while self.timer <= 0 {
            self.timer += (2048 - self.freq as i32) * 2;
            self.position = (self.position + 1) & 31;
            let byte = self.ram[self.position / 2];
            self.sample_buffer = if self.position.is_multiple_of(2) {
                byte >> 4
            } else {
                byte & 0x0f
            };
        }
    }

    fn sample(&self) -> f32 {
        if !self.enabled || !self.dac_on {
            return 0.0;
        }
        let level = match self.volume_shift {
            0 => 0,
            1 => self.sample_buffer,
            2 => self.sample_buffer >> 1,
            _ => self.sample_buffer >> 2,
        };
        level as f32 / 15.0
    }

    fn trigger(&mut self) {
        self.enabled = self.dac_on;
        if self.length.value == 0 {
            self.length.value = self.length.max;
        }
        self.timer = (2048 - self.freq as i32) * 2;
        self.position = 0;
    }
}

#[derive(Clone)]
struct Noise {
    enabled: bool,
    lfsr: u16,
    shift: u8,
    width7: bool,
    divisor: u8,
    timer: i32,
    envelope: Envelope,
    length: LengthCounter,
}

impl Noise {
    fn new() -> Self {
        Noise {
            enabled: false,
            lfsr: 0x7fff,
            shift: 0,
            width7: false,
            divisor: 0,
            timer: 0,
            envelope: Envelope::default(),
            length: LengthCounter::new(64),
        }
    }

    fn period(&self) -> i32 {
        (NOISE_DIVISOR[(self.divisor & 7) as usize] << self.shift.min(15)) as i32
    }

    fn step(&mut self, t: u32) {
        if self.shift >= 14 {
            return; // frequencies this high never clock the shift register
        }
        self.timer -= t as i32;
        while self.timer <= 0 {
            self.timer += self.period().max(1);
            let bit = (self.lfsr ^ (self.lfsr >> 1)) & 1;
            self.lfsr = (self.lfsr >> 1) | (bit << 14);
            if self.width7 {
                self.lfsr = (self.lfsr & !0x40) | (bit << 6);
            }
        }
    }

    fn sample(&self) -> f32 {
        if !self.enabled || !self.envelope.dac_on() {
            return 0.0;
        }
        let level = if self.lfsr & 1 == 0 {
            self.envelope.volume
        } else {
            0
        };
        level as f32 / 15.0
    }

    fn trigger(&mut self) {
        self.enabled = true;
        if self.length.value == 0 {
            self.length.value = self.length.max;
        }
        self.timer = self.period().max(1);
        self.lfsr = 0x7fff;
        self.envelope.trigger();
        if !self.envelope.dac_on() {
            self.enabled = false;
        }
    }
}

pub struct Apu {
    ch1: Pulse,
    ch2: Pulse,
    ch3: Wave,
    ch4: Noise,

    power: bool,
    nr50: u8,
    nr51: u8,

    /// 512 Hz frame sequencer, clocked off the same divider DIV uses.
    seq_step: u8,
    seq_timer: u32,

    /// Fractional accumulator for resampling to the host rate.
    sample_timer: f64,
    cycles_per_sample: f64,
    /// Interleaved stereo, drained by the front-end.
    pub buffer: Vec<f32>,
    buffer_cap: usize,

    /// Simple one-pole high-pass, standing in for the DMG's DC-blocking capacitor.
    hp_left: f32,
    hp_right: f32,
}

impl Apu {
    pub fn new(sample_rate: u32) -> Self {
        Apu {
            ch1: Pulse::new(true),
            ch2: Pulse::new(false),
            ch3: Wave::new(),
            ch4: Noise::new(),
            power: true,
            nr50: 0x77,
            nr51: 0xf3,
            seq_step: 0,
            seq_timer: 0,
            sample_timer: 0.0,
            cycles_per_sample: CPU_HZ / sample_rate as f64,
            buffer: Vec::with_capacity(8192),
            buffer_cap: sample_rate as usize / 4 * 2,
            hp_left: 0.0,
            hp_right: 0.0,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.cycles_per_sample = CPU_HZ / sample_rate as f64;
        self.buffer_cap = sample_rate as usize / 4 * 2;
    }

    /// In CGB double-speed mode the APU still runs at the normal rate, so the
    /// caller passes normal-speed cycles here.
    pub fn step(&mut self, t: u32) {
        if self.power {
            self.ch1.step(t);
            self.ch2.step(t);
            self.ch3.step(t);
            self.ch4.step(t);

            self.seq_timer += t;
            while self.seq_timer >= 8192 {
                self.seq_timer -= 8192;
                self.step_sequencer();
            }
        }

        self.sample_timer += t as f64;
        while self.sample_timer >= self.cycles_per_sample {
            self.sample_timer -= self.cycles_per_sample;
            self.emit_sample();
        }
    }

    fn step_sequencer(&mut self) {
        match self.seq_step {
            0 | 4 => self.step_lengths(),
            2 | 6 => {
                self.step_lengths();
                self.ch1.step_sweep();
            }
            7 => {
                self.ch1.envelope.step();
                self.ch2.envelope.step();
                self.ch4.envelope.step();
            }
            _ => {}
        }
        self.seq_step = (self.seq_step + 1) & 7;
    }

    fn step_lengths(&mut self) {
        if self.ch1.length.step() {
            self.ch1.enabled = false;
        }
        if self.ch2.length.step() {
            self.ch2.enabled = false;
        }
        if self.ch3.length.step() {
            self.ch3.enabled = false;
        }
        if self.ch4.length.step() {
            self.ch4.enabled = false;
        }
    }

    fn emit_sample(&mut self) {
        // Drop samples rather than grow without bound if nobody is consuming.
        if self.buffer.len() >= self.buffer_cap {
            return;
        }

        let channels = [
            self.ch1.sample(),
            self.ch2.sample(),
            self.ch3.sample(),
            self.ch4.sample(),
        ];

        let mut left = 0.0;
        let mut right = 0.0;
        for (i, s) in channels.iter().enumerate() {
            if self.nr51 & (0x10 << i) != 0 {
                left += s;
            }
            if self.nr51 & (1 << i) != 0 {
                right += s;
            }
        }

        let left_vol = ((self.nr50 >> 4) & 7) as f32 / 7.0;
        let right_vol = (self.nr50 & 7) as f32 / 7.0;
        let left = left * 0.25 * left_vol;
        let right = right * 0.25 * right_vol;

        // One-pole high-pass at roughly 20 Hz to remove the DC step.
        const ALPHA: f32 = 0.999;
        let out_l = left - self.hp_left;
        let out_r = right - self.hp_right;
        self.hp_left = left - out_l * ALPHA;
        self.hp_right = right - out_r * ALPHA;

        self.buffer.push(out_l);
        self.buffer.push(out_r);
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xff10 => {
                0x80 | (self.ch1.sweep_period << 4)
                    | ((self.ch1.sweep_negate as u8) << 3)
                    | self.ch1.sweep_shift
            }
            0xff11 => 0x3f | (self.ch1.duty << 6),
            0xff12 => self.ch1.envelope.byte(),
            0xff13 => 0xff,
            0xff14 => 0xbf | ((self.ch1.length.enabled as u8) << 6),
            0xff16 => 0x3f | (self.ch2.duty << 6),
            0xff17 => self.ch2.envelope.byte(),
            0xff18 => 0xff,
            0xff19 => 0xbf | ((self.ch2.length.enabled as u8) << 6),
            0xff1a => 0x7f | ((self.ch3.dac_on as u8) << 7),
            0xff1b => 0xff,
            0xff1c => 0x9f | (self.ch3.volume_shift << 5),
            0xff1d => 0xff,
            0xff1e => 0xbf | ((self.ch3.length.enabled as u8) << 6),
            0xff20 => 0xff,
            0xff21 => self.ch4.envelope.byte(),
            0xff22 => (self.ch4.shift << 4) | ((self.ch4.width7 as u8) << 3) | self.ch4.divisor,
            0xff23 => 0xbf | ((self.ch4.length.enabled as u8) << 6),
            0xff24 => self.nr50,
            0xff25 => self.nr51,
            0xff26 => {
                0x70 | ((self.power as u8) << 7)
                    | (self.ch1.enabled as u8)
                    | ((self.ch2.enabled as u8) << 1)
                    | ((self.ch3.enabled as u8) << 2)
                    | ((self.ch4.enabled as u8) << 3)
            }
            0xff30..=0xff3f => self.ch3.ram[(addr - 0xff30) as usize],
            _ => 0xff,
        }
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        // With the APU powered down only NR52 and wave RAM accept writes.
        if !self.power && !matches!(addr, 0xff26 | 0xff30..=0xff3f) {
            return;
        }

        match addr {
            0xff10 => {
                self.ch1.sweep_period = (value >> 4) & 7;
                let negate = value & 0x08 != 0;
                // Leaving negate mode after a calculation disables the channel.
                if self.ch1.sweep_negate && !negate && self.ch1.sweep_negated {
                    self.ch1.enabled = false;
                }
                self.ch1.sweep_negate = negate;
                self.ch1.sweep_shift = value & 7;
            }
            0xff11 => {
                self.ch1.duty = value >> 6;
                self.ch1.length.reload((value & 0x3f) as u16);
            }
            0xff12 => {
                self.ch1.envelope.write(value);
                if !self.ch1.envelope.dac_on() {
                    self.ch1.enabled = false;
                }
            }
            0xff13 => self.ch1.freq = (self.ch1.freq & 0x700) | value as u16,
            0xff14 => {
                self.ch1.freq = (self.ch1.freq & 0xff) | ((value as u16 & 7) << 8);
                self.ch1.length.enabled = value & 0x40 != 0;
                if value & 0x80 != 0 {
                    self.ch1.trigger();
                }
            }

            0xff16 => {
                self.ch2.duty = value >> 6;
                self.ch2.length.reload((value & 0x3f) as u16);
            }
            0xff17 => {
                self.ch2.envelope.write(value);
                if !self.ch2.envelope.dac_on() {
                    self.ch2.enabled = false;
                }
            }
            0xff18 => self.ch2.freq = (self.ch2.freq & 0x700) | value as u16,
            0xff19 => {
                self.ch2.freq = (self.ch2.freq & 0xff) | ((value as u16 & 7) << 8);
                self.ch2.length.enabled = value & 0x40 != 0;
                if value & 0x80 != 0 {
                    self.ch2.trigger();
                }
            }

            0xff1a => {
                self.ch3.dac_on = value & 0x80 != 0;
                if !self.ch3.dac_on {
                    self.ch3.enabled = false;
                }
            }
            0xff1b => self.ch3.length.reload(value as u16),
            0xff1c => self.ch3.volume_shift = (value >> 5) & 3,
            0xff1d => self.ch3.freq = (self.ch3.freq & 0x700) | value as u16,
            0xff1e => {
                self.ch3.freq = (self.ch3.freq & 0xff) | ((value as u16 & 7) << 8);
                self.ch3.length.enabled = value & 0x40 != 0;
                if value & 0x80 != 0 {
                    self.ch3.trigger();
                }
            }

            0xff20 => self.ch4.length.reload((value & 0x3f) as u16),
            0xff21 => {
                self.ch4.envelope.write(value);
                if !self.ch4.envelope.dac_on() {
                    self.ch4.enabled = false;
                }
            }
            0xff22 => {
                self.ch4.shift = value >> 4;
                self.ch4.width7 = value & 0x08 != 0;
                self.ch4.divisor = value & 7;
            }
            0xff23 => {
                self.ch4.length.enabled = value & 0x40 != 0;
                if value & 0x80 != 0 {
                    self.ch4.trigger();
                }
            }

            0xff24 => self.nr50 = value,
            0xff25 => self.nr51 = value,
            0xff26 => {
                let on = value & 0x80 != 0;
                if !on && self.power {
                    self.reset();
                }
                self.power = on;
            }
            0xff30..=0xff3f => self.ch3.ram[(addr - 0xff30) as usize] = value,
            _ => {}
        }
    }

    /// Powering the APU off clears every register except wave RAM.
    fn reset(&mut self) {
        let wave_ram = self.ch3.ram;
        self.ch1 = Pulse::new(true);
        self.ch2 = Pulse::new(false);
        self.ch3 = Wave::new();
        self.ch3.ram = wave_ram;
        self.ch4 = Noise::new();
        self.nr50 = 0;
        self.nr51 = 0;
        self.seq_step = 0;
    }

    pub fn drain(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.buffer);
    }
}
