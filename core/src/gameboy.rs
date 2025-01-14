use std::cell::RefCell;

use crate::{
    disassembler::Trace,
    save_state::{LoadStateError, SaveState, SaveStateHeader},
};

pub mod cartridge;
pub mod cpu;
pub mod ppu;
pub mod sound_controller;
pub mod timer;

use self::{
    cartridge::Cartridge, cpu::Cpu, ppu::Ppu, sound_controller::SoundController, timer::Timer,
};

/// The offset between `clock_count` and the serial transfer clock, in cycles. This is choose
/// arbitrarily, in a way that pass the serial_boot_sclk_align_dmg_abc_mgb test.
const SERIAL_OFFSET: u64 = 8;

pub struct GameBoy {
    pub trace: RefCell<Trace>,
    pub cpu: Cpu,
    pub cartridge: Cartridge,
    /// C000-DFFF: Work RAM
    pub wram: [u8; 0x2000],
    /// FF80-FFFE: High RAM
    pub hram: [u8; 0x7F],
    pub boot_rom: Option<[u8; 0x100]>,
    pub boot_rom_active: bool,
    pub clock_count: u64,
    pub timer: Timer,
    pub sound: RefCell<SoundController>,
    pub ppu: RefCell<Ppu>,
    /// FF00: P1
    pub joypad_io: u8,
    /// JoyPad state. 0 bit means pressed.
    /// From bit 7 to 0, the order is: Start, Select, B, A, Down, Up, Left, Right
    pub joypad: u8,
    /// FF01: SB
    pub serial_data: u8,
    /// FF02: SC
    pub serial_control: u8,
    /// The instant, in 2^13 Hz clock count (T-clock count >> 9), in which the first bit of current
    /// serial transfer was send. It is 0 if there is no transfer happening.
    pub serial_transfer_started: u64,
    #[cfg(not(target_arch = "wasm32"))]
    pub serial_transfer_callback: Option<Box<dyn FnMut(u8) + Send>>,
    #[cfg(target_arch = "wasm32")]
    pub serial_transfer_callback: Option<Box<dyn FnMut(u8)>>,
    /// FF0F: Interrupt Flag (IF)
    /// - bit 0: VBlank
    /// - bit 1: STAT
    /// - bit 2: Timer
    /// - bit 3: Serial
    /// - bit 4: Joypad
    pub interrupt_flag: u8,
    /// FF46: DMA register
    pub dma: u8,
    /// FFFF: Interrupt Enabled (IE). Same scheme as `interrupt_flag`.
    pub interrupt_enabled: u8,

    /// This trigger control if in the next interpret the `v_blank` callback will be called.
    pub v_blank_trigger: bool,
    /// A callback that is called after a VBlank. This is called at the
    #[cfg(not(target_arch = "wasm32"))]
    pub v_blank: Option<Box<dyn FnMut(&mut GameBoy) + Send>>,
    #[cfg(target_arch = "wasm32")]
    pub v_blank: Option<Box<dyn FnMut(&mut GameBoy)>>,
}

impl std::fmt::Debug for GameBoy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO: derive Debug for fields when the time arrive.
        f.debug_struct("GameBoy")
            // .field("trace", &self.trace)
            .field("cpu", &self.cpu)
            // .field("cartridge", &self.cartridge)
            .field("wram", &self.wram)
            .field("hram", &self.hram)
            .field("boot_rom", &self.boot_rom)
            .field("boot_rom_active", &self.boot_rom_active)
            .field("clock_count", &self.clock_count)
            .field("timer", &self.timer)
            // .field("sound", &self.sound)
            // .field("ppu", &self.ppu)
            .field("joypad", &self.joypad)
            .field("joypad_io", &self.joypad_io)
            // .field("serial_transfer", &self.serial_transfer)
            // .field("v_blank", &self.v_blank)
            .finish()
    }
}

impl Eq for GameBoy {}
impl PartialEq for GameBoy {
    fn eq(&self, other: &Self) -> bool {
        // self.trace == other.trace &&
        self.cpu == other.cpu
            && self.cartridge == other.cartridge
            && self.wram == other.wram
            && self.hram == other.hram
            && self.boot_rom == other.boot_rom
            && self.boot_rom_active == other.boot_rom_active
            && self.clock_count == other.clock_count
            && self.timer == other.timer
            && self.sound == other.sound
            && self.ppu == other.ppu
            && self.joypad_io == other.joypad_io
            && self.joypad == other.joypad
            && self.serial_data == other.serial_data
            && self.serial_control == other.serial_control
            // && self.serial_transfer == other.serial_transfer
            && self.interrupt_flag == other.interrupt_flag
            && self.interrupt_enabled == other.interrupt_enabled
        // && self.v_blank == other.v_blank
    }
}
crate::save_state!(GameBoy, self, data {
    SaveStateHeader::new();
    // self.trace;
    self.cpu;
    self.cartridge;
    self.wram;
    self.hram;
    // self.boot_rom;
    self.clock_count;
    self.timer;

    self.sound.borrow_mut();
    self.ppu.borrow_mut();

    self.joypad_io;
    self.joypad;
    self.serial_data;
    self.serial_control;
    self.serial_transfer_started;
    // self.serial_transfer;
    self.interrupt_flag;
    self.dma;
    self.interrupt_enabled;

    bitset [self.boot_rom_active, self.v_blank_trigger];
    // self.v_blank;
});
impl GameBoy {
    pub fn new(boot_rom: Option<[u8; 0x100]>, cartridge: Cartridge) -> Self {
        let mut this = Self {
            trace: RefCell::new(Trace::new()),
            cpu: Cpu::default(),
            cartridge,
            wram: [0; 0x2000],
            hram: [0; 0x7F],
            boot_rom,
            boot_rom_active: true,
            clock_count: 0,
            timer: Timer::new(),
            sound: RefCell::new(SoundController::default()),
            ppu: Ppu::default().into(),

            joypad: 0xFF,
            joypad_io: 0x00,
            serial_data: 0,
            serial_control: 0x7E,
            serial_transfer_started: 0,
            serial_transfer_callback: Some(Box::new(|c| {
                eprint!("{}", c as char);
            })),
            interrupt_flag: 0,
            dma: 0xff,
            interrupt_enabled: 0,
            v_blank_trigger: false,
            v_blank: None,
        };

        if this.boot_rom.is_none() {
            this.reset_after_boot();
        }

        this
    }

    /// call the `v_blank` callback
    pub fn call_v_blank_callback(&mut self) {
        if let Some(mut v_blank) = self.v_blank.take() {
            v_blank(self);
            self.v_blank = Some(v_blank);
        }
    }

    /// Reset the gameboy to its stating state.
    pub fn reset(&mut self) {
        if self.boot_rom.is_none() {
            self.reset_after_boot();
            return;
        }
        // TODO: Maybe I should reset the cartridge
        self.cpu = Cpu::default();
        self.wram = [0; 0x2000];
        self.hram = [0; 0x7F];
        self.boot_rom_active = true;
        self.clock_count = 0;
        self.timer = Timer::new();
        self.sound = RefCell::new(SoundController::default());
        self.ppu = Ppu::default().into();
        self.joypad = 0xFF;
        self.joypad_io = 0x00;
    }

    /// Reset the gameboy to its state after disabling the boot.
    pub fn reset_after_boot(&mut self) {
        self.cpu = Cpu {
            a: 0x01,
            f: cpu::Flags(0xb0),
            b: 0x00,
            c: 0x13,
            d: 0x00,
            e: 0xd8,
            h: 0x01,
            l: 0x4d,
            sp: 0xfffe,
            pc: 0x0100,
            ime: cpu::ImeState::Disabled,
            state: cpu::CpuState::Running,
        };

        self.wram = [0; 0x2000];
        self.hram = [0; 0x7F];
        self.hram[0x7a..=0x7c].copy_from_slice(&[0x39, 0x01, 0x2e]);

        self.boot_rom_active = false;
        self.clock_count = 23_440_324;
        self.ppu.borrow_mut().reset_after_boot();

        self.joypad = 0xFF;

        self.joypad_io = 0xCF;
        self.serial_data = 0x00;
        self.serial_control = 0x7E;
        self.timer = Timer {
            div: 0xabcc,
            tima: 0x00,
            tma: 0x00,
            tac: 0xf8,
            last_counter_bit: false,
            last_clock_count: self.clock_count,
            loading: 0,
        };
        self.interrupt_flag = 0xE1;
        self.sound
            .borrow_mut()
            .load_state(&mut &include_bytes!("../after_boot/sound.sav")[..])
            .unwrap();
    }

    pub fn read(&self, mut address: u16) -> u8 {
        if self.boot_rom_active {
            if address < 0x100 {
                let boot_rom = self
                    .boot_rom
                    .expect("the boot rom is only actived when there is one");
                return boot_rom[address as usize];
            }
        }
        if (0xE000..=0xFDFF).contains(&address) {
            address -= 0x2000;
        }
        match address {
            // Cartridge ROM
            0x0000..=0x7FFF => self.cartridge.read(address),
            // Video RAM
            0x8000..=0x9FFF => Ppu::read_vram(self, address),
            // Cartridge RAM
            0xA000..=0xBFFF => self.cartridge.read(address),
            // Work RAM
            0xC000..=0xDFFF => self.wram[address as usize - 0xC000],
            // ECHO RAM
            0xE000..=0xFDFF => unreachable!(),
            // Sprite Attribute table
            0xFE00..=0xFE9F => Ppu::read_oam(self, address),
            // Not Usable
            0xFEA0..=0xFEFF => 0xff,
            // I/O registers
            0xFF00..=0xFF7F => self.read_io(address as u8),
            // Hight RAM
            0xFF80..=0xFFFE => self.hram[address as usize - 0xFF80],
            // IE Register
            0xFFFF => self.read_io(address as u8),
        }
    }

    pub fn write(&mut self, mut address: u16, value: u8) {
        if (0xE000..=0xFDFF).contains(&address) {
            address -= 0x2000;
        }

        match address {
            // Cartridge ROM
            0x0000..=0x7FFF => self.cartridge.write(address, value),
            // Video RAM
            0x8000..=0x9FFF => Ppu::write_vram(self, address, value),
            // Cartridge RAM
            0xA000..=0xBFFF => self.cartridge.write(address, value),
            // Work RAM
            0xC000..=0xDFFF => self.wram[address as usize - 0xC000] = value,
            // ECHO RAM
            0xE000..=0xFDFF => unreachable!(),
            // Sprite Attribute table
            0xFE00..=0xFE9F => Ppu::write_oam(self, address, value),
            // Not Usable
            0xFEA0..=0xFEFF => {}
            // I/O registers
            0xFF00..=0xFF7F => self.write_io(address as u8, value),
            // Hight RAM
            0xFF80..=0xFFFE => self.hram[address as usize - 0xFF80] = value,
            // IE Register
            0xFFFF => self.write_io(address as u8, value),
        }
    }

    /// Advance the clock by 'count' cycles
    pub fn tick(&mut self, count: u8) {
        self.clock_count += count as u64;

        // ppu
        let (v_blank_interrupt, stat_interrupt) = Ppu::update(self);
        if stat_interrupt {
            self.interrupt_flag |= 1 << 1;
        }
        if v_blank_interrupt {
            self.interrupt_flag |= 1 << 0;
            self.v_blank_trigger = true;
        }

        // timer
        if self.timer.update(self.clock_count) {
            self.interrupt_flag |= 1 << 2;
        }

        // serial
        if self.serial_transfer_started != 0
            && self.serial_transfer_started + 7 < (self.clock_count + SERIAL_OFFSET) >> 9
        {
            // interrupt
            self.interrupt_flag |= 1 << 3;
            // clear tranfer flag bit
            self.serial_control &= !0x80;
            self.serial_transfer_started = 0;
        }
    }

    pub fn read16(&self, address: u16) -> u16 {
        u16::from_le_bytes([self.read(address), self.read(address.wrapping_add(1))])
    }

    pub fn write16(&mut self, address: u16, value: u16) {
        let [a, b] = value.to_le_bytes();
        self.write(address, a);
        self.write(address.wrapping_add(1), b);
    }

    fn write_io(&mut self, address: u8, value: u8) {
        match address {
            0x00 => self.joypad_io = 0b1100_1111 | (value & 0x30), // JOYPAD
            0x01 => self.serial_data = value,
            0x02 => {
                self.serial_control = value | 0x7E;
                if value & 0x81 == 0x81 {
                    // serial transfer is aligned to a 8192Hz (2^13 Hz) clock.
                    self.serial_transfer_started = (self.clock_count + SERIAL_OFFSET) >> 9;
                    let data = self.serial_data;
                    self.serial_transfer_callback.as_mut().map(|x| x(data));
                }
            }
            0x03 => {}
            0x04..=0x07 => self.timer.write(address, value),
            0x08..=0x0e => {}
            0x0f => self.interrupt_flag = value,
            0x10..=0x14 | 0x16..=0x1e | 0x20..=0x26 | 0x30..=0x3f => {
                self.sound
                    .borrow_mut()
                    .write(self.clock_count, address, value)
            }
            0x15 => {}
            0x1f => {}
            0x27..=0x2f => {}
            0x40..=0x45 => Ppu::write(self, address, value),
            0x46 => {
                // DMA Transfer
                Ppu::start_dma(self, value);
            }
            0x47..=0x4b => Ppu::write(self, address, value),
            0x4c..=0x4f => {}
            0x50 => {
                if self.boot_rom_active && value & 0b1 != 0 {
                    self.boot_rom_active = false;
                    self.cpu.pc = 0x100;
                }
            }
            0x51..=0x7f => {}
            0x80..=0xfe => self.hram[address as usize - 0x80] = value,
            0xff => self.interrupt_enabled = value,
        }
    }

    fn read_io(&self, address: u8) -> u8 {
        match address {
            0x00 => {
                // JOYPAD
                let v = self.joypad_io & 0x30;
                let mut r = v | 0b1100_0000;
                if v & 0x10 != 0 {
                    r |= (self.joypad >> 4) & 0x0F;
                }
                if v & 0x20 != 0 {
                    r |= self.joypad & 0x0F;
                }
                if v == 0 {
                    r |= 0x0F;
                }
                r
            }
            0x01 => self.serial_data,
            0x02 => self.serial_control,
            0x03 => 0xff,
            0x04..=0x07 => self.timer.read(address),
            0x08..=0x0e => 0xff,
            0x0f => self.interrupt_flag | 0xE0,
            0x10..=0x14 | 0x16..=0x1e | 0x20..=0x26 | 0x30..=0x3f => {
                self.sound.borrow_mut().read(self.clock_count, address)
            }
            0x15 => 0xff,
            0x1f => 0xff,
            0x27..=0x2f => 0xff,
            0x40..=0x45 => Ppu::read(self, address),
            0x46 => self.dma,
            0x47..=0x4b => Ppu::read(self, address),
            0x4c => 0xff,
            0x4d => 0xff,
            0x4e..=0x4f => 0xff,
            0x50 => 0xff,
            0x51..=0x7F => 0xff,
            0x80..=0xfe => {
                // high RAM, IF flag and IE flag
                self.hram[address as usize - 0x80]
            }
            0xff => self.interrupt_enabled,
        }
    }
}
