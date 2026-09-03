//! DIV/TIMA. Both are views onto one 16-bit counter, which is why writing DIV
//! can itself clock TIMA: the falling edge that drives it comes from a counter bit.

pub const IRQ_TIMER: u8 = 0x04;

pub struct Timer {
    /// The 16-bit system counter; DIV is its high byte.
    counter: u16,
    tima: u8,
    tma: u8,
    tac: u8,
    /// Previous state of the multiplexed counter bit, for edge detection.
    last_edge: bool,
    /// TIMA's reload is four cycles late, and reads see 0 in the gap.
    reload_delay: u8,
    pub irq: u8,
}

impl Timer {
    pub fn new() -> Self {
        Timer {
            counter: 0xabcc,
            tima: 0,
            tma: 0,
            tac: 0xf8,
            last_edge: false,
            reload_delay: 0,
            irq: 0,
        }
    }

    fn selected_bit(&self) -> u16 {
        match self.tac & 3 {
            0 => 1 << 9,
            1 => 1 << 3,
            2 => 1 << 5,
            _ => 1 << 7,
        }
    }

    fn edge(&self) -> bool {
        self.tac & 4 != 0 && self.counter & self.selected_bit() != 0
    }

    pub fn step(&mut self, t: u32) {
        for _ in 0..t {
            if self.reload_delay > 0 {
                self.reload_delay -= 1;
                if self.reload_delay == 0 {
                    self.tima = self.tma;
                    self.irq |= IRQ_TIMER;
                }
            }
            self.counter = self.counter.wrapping_add(1);
            self.detect_edge();
        }
    }

    fn detect_edge(&mut self) {
        let now = self.edge();
        if self.last_edge && !now {
            self.increment_tima();
        }
        self.last_edge = now;
    }

    fn increment_tima(&mut self) {
        let (v, overflow) = self.tima.overflowing_add(1);
        self.tima = v;
        if overflow {
            self.reload_delay = 4;
        }
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xff04 => (self.counter >> 8) as u8,
            0xff05 => {
                if self.reload_delay > 0 {
                    0
                } else {
                    self.tima
                }
            }
            0xff06 => self.tma,
            0xff07 => self.tac | 0xf8,
            _ => 0xff,
        }
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0xff04 => {
                self.counter = 0;
                self.detect_edge();
            }
            0xff05 => {
                // A write during the reload window cancels the reload.
                self.tima = value;
                self.reload_delay = 0;
            }
            0xff06 => self.tma = value,
            0xff07 => {
                self.tac = value & 7;
                self.detect_edge();
            }
            _ => {}
        }
    }
}

impl Default for Timer {
    fn default() -> Self {
        Self::new()
    }
}
