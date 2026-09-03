//! P1/JOYP. The register reads back inverted: a clear bit means pressed.

pub const IRQ_JOYPAD: u8 = 0x10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    Right,
    Left,
    Up,
    Down,
    A,
    B,
    Select,
    Start,
}

impl Button {
    pub const ALL: [Button; 8] = [
        Button::Right,
        Button::Left,
        Button::Up,
        Button::Down,
        Button::A,
        Button::B,
        Button::Select,
        Button::Start,
    ];

    fn bit(self) -> u8 {
        match self {
            Button::Right | Button::A => 0x01,
            Button::Left | Button::B => 0x02,
            Button::Up | Button::Select => 0x04,
            Button::Down | Button::Start => 0x08,
        }
    }

    fn is_action(self) -> bool {
        matches!(self, Button::A | Button::B | Button::Select | Button::Start)
    }
}

pub struct Joypad {
    directions: u8,
    actions: u8,
    /// Bits 4/5 of P1: which row the game selected.
    select: u8,
    pub irq: u8,
}

impl Joypad {
    pub fn new() -> Self {
        Joypad {
            directions: 0,
            actions: 0,
            select: 0x30,
            irq: 0,
        }
    }

    pub fn set(&mut self, button: Button, pressed: bool) {
        let (mask, target) = if button.is_action() {
            (button.bit(), &mut self.actions)
        } else {
            (button.bit(), &mut self.directions)
        };
        let before = *target;
        if pressed {
            *target |= mask;
        } else {
            *target &= !mask;
        }
        // A press only interrupts while its row is selected.
        if *target & !before != 0 {
            let row_selected = if button.is_action() {
                self.select & 0x20 == 0
            } else {
                self.select & 0x10 == 0
            };
            if row_selected {
                self.irq |= IRQ_JOYPAD;
            }
        }
    }

    pub fn read(&self) -> u8 {
        let mut low = 0x0f;
        if self.select & 0x10 == 0 {
            low &= !self.directions;
        }
        if self.select & 0x20 == 0 {
            low &= !self.actions;
        }
        0xc0 | self.select | low
    }

    pub fn write(&mut self, value: u8) {
        self.select = value & 0x30;
    }
}

impl Default for Joypad {
    fn default() -> Self {
        Self::new()
    }
}
