//! The Windows front-end: one window, a keyboard, and a sound card.
//!
//! Written straight against the Win32 API so a converted game depends on
//! nothing but the operating system.

#![allow(non_snake_case, non_camel_case_types)]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use gb_hw::{Button, SCREEN_H, SCREEN_W};

type HWND = *mut core::ffi::c_void;
type HDC = *mut core::ffi::c_void;
type HINSTANCE = *mut core::ffi::c_void;
type LRESULT = isize;
type WPARAM = usize;
type LPARAM = isize;

#[repr(C)]
struct WNDCLASSW {
    style: u32,
    lpfnWndProc: Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>,
    cbClsExtra: i32,
    cbWndExtra: i32,
    hInstance: HINSTANCE,
    hIcon: *mut core::ffi::c_void,
    hCursor: *mut core::ffi::c_void,
    hbrBackground: *mut core::ffi::c_void,
    lpszMenuName: *const u16,
    lpszClassName: *const u16,
}

#[repr(C)]
struct POINT {
    x: i32,
    y: i32,
}

#[repr(C)]
struct MSG {
    hwnd: HWND,
    message: u32,
    wParam: WPARAM,
    lParam: LPARAM,
    time: u32,
    pt: POINT,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct RECT {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct BITMAPINFOHEADER {
    biSize: u32,
    biWidth: i32,
    biHeight: i32,
    biPlanes: u16,
    biBitCount: u16,
    biCompression: u32,
    biSizeImage: u32,
    biXPelsPerMeter: i32,
    biYPelsPerMeter: i32,
    biClrUsed: u32,
    biClrImportant: u32,
}

#[repr(C)]
struct WAVEFORMATEX {
    wFormatTag: u16,
    nChannels: u16,
    nSamplesPerSec: u32,
    nAvgBytesPerSec: u32,
    nBlockAlign: u16,
    wBitsPerSample: u16,
    cbSize: u16,
}

#[repr(C)]
struct WAVEHDR {
    lpData: *mut u8,
    dwBufferLength: u32,
    dwBytesRecorded: u32,
    dwUser: usize,
    dwFlags: u32,
    dwLoops: u32,
    lpNext: *mut WAVEHDR,
    reserved: usize,
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassW(class: *const WNDCLASSW) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class: *const u16,
        window: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: HWND,
        menu: *mut core::ffi::c_void,
        instance: HINSTANCE,
        param: *mut core::ffi::c_void,
    ) -> HWND;
    fn DefWindowProcW(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT;
    fn ShowWindow(hwnd: HWND, cmd: i32) -> i32;
    fn PeekMessageW(msg: *mut MSG, hwnd: HWND, min: u32, max: u32, remove: u32) -> i32;
    fn TranslateMessage(msg: *const MSG) -> i32;
    fn DispatchMessageW(msg: *const MSG) -> LRESULT;
    fn GetDC(hwnd: HWND) -> HDC;
    fn ReleaseDC(hwnd: HWND, dc: HDC) -> i32;
    fn GetClientRect(hwnd: HWND, rect: *mut RECT) -> i32;
    fn PostQuitMessage(code: i32);
    fn LoadCursorW(instance: HINSTANCE, name: usize) -> *mut core::ffi::c_void;
    fn AdjustWindowRect(rect: *mut RECT, style: u32, menu: i32) -> i32;
    fn MessageBoxW(hwnd: HWND, text: *const u16, caption: *const u16, kind: u32) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn StretchDIBits(
        dc: HDC,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        src_x: i32,
        src_y: i32,
        src_w: i32,
        src_h: i32,
        bits: *const core::ffi::c_void,
        info: *const BITMAPINFOHEADER,
        usage: u32,
        rop: u32,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(name: *const u16) -> HINSTANCE;
    fn Sleep(ms: u32);
    fn QueryPerformanceCounter(count: *mut i64) -> i32;
    fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
}

#[link(name = "winmm")]
extern "system" {
    fn waveOutOpen(
        out: *mut *mut core::ffi::c_void,
        device: u32,
        format: *const WAVEFORMATEX,
        callback: usize,
        instance: usize,
        flags: u32,
    ) -> u32;
    fn waveOutPrepareHeader(h: *mut core::ffi::c_void, hdr: *mut WAVEHDR, size: u32) -> u32;
    fn waveOutUnprepareHeader(h: *mut core::ffi::c_void, hdr: *mut WAVEHDR, size: u32) -> u32;
    fn waveOutWrite(h: *mut core::ffi::c_void, hdr: *mut WAVEHDR, size: u32) -> u32;
    fn waveOutReset(h: *mut core::ffi::c_void) -> u32;
    fn waveOutClose(h: *mut core::ffi::c_void) -> u32;
}

const WS_OVERLAPPEDWINDOW: u32 = 0x00cf_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const CW_USEDEFAULT: i32 = -2147483648;
const SW_SHOW: i32 = 5;
const PM_REMOVE: u32 = 1;
const WM_DESTROY: u32 = 0x0002;
const WM_CLOSE: u32 = 0x0010;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
const WM_QUIT: u32 = 0x0012;
const DIB_RGB_COLORS: u32 = 0;
const SRCCOPY: u32 = 0x00cc_0020;
const IDC_ARROW: usize = 32512;
const WAVE_FORMAT_PCM: u16 = 1;
const WHDR_DONE: u32 = 0x0000_0001;
const MB_ICONERROR: u32 = 0x0000_0010;

/// Which buttons are held, as a bitmask the window procedure can update.
static KEYS: AtomicU32 = AtomicU32::new(0);
static QUIT: AtomicBool = AtomicBool::new(false);
/// Held to run the game as fast as the host allows.
static TURBO: AtomicBool = AtomicBool::new(false);

/// Virtual key codes, paired with the button each one drives.
const BINDINGS: [(u32, u8); 12] = [
    (0x27, 0), // Right
    (0x25, 1), // Left
    (0x26, 2), // Up
    (0x28, 3), // Down
    (0x58, 4), // X -> A
    (0x5a, 5), // Z -> B
    (0x53, 4), // S -> A, for keyboards where Z and X are awkward
    (0x41, 5), // A -> B
    (0x08, 6), // Backspace -> Select
    (0x10, 6), // Shift -> Select
    (0x0d, 7), // Enter -> Start
    (0x20, 7), // Space -> Start
];

const BUTTONS: [Button; 8] = [
    Button::Right,
    Button::Left,
    Button::Up,
    Button::Down,
    Button::A,
    Button::B,
    Button::Select,
    Button::Start,
];

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CLOSE | WM_DESTROY => {
            QUIT.store(true, Ordering::Relaxed);
            PostQuitMessage(0);
            0
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let vk = w as u32;
            if vk == 0x1b && down {
                QUIT.store(true, Ordering::Relaxed);
            }
            if vk == 0x09 {
                TURBO.store(down, Ordering::Relaxed);
            }
            let mut mask = 0u32;
            for (code, bit) in BINDINGS {
                if code == vk {
                    mask |= 1 << bit;
                }
            }
            if mask != 0 {
                if down {
                    KEYS.fetch_or(mask, Ordering::Relaxed);
                } else {
                    KEYS.fetch_and(!mask, Ordering::Relaxed);
                }
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Show a message box. Used for errors, since the runtime has no console.
pub fn report(title: &str, message: &str) {
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(message).as_ptr(),
            wide(title).as_ptr(),
            MB_ICONERROR,
        );
    }
}

pub struct Window {
    hwnd: HWND,
    info: BITMAPINFOHEADER,
    /// The framebuffer, converted to the byte order StretchDIBits expects.
    pixels: Vec<u32>,
}

impl Window {
    pub fn new(title: &str, scale: u32) -> Option<Window> {
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let class_name = wide("gbtoexe.window");

            let class = WNDCLASSW {
                style: 0x0002 | 0x0001, // CS_HREDRAW | CS_VREDRAW
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: std::ptr::null_mut(),
                hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                hbrBackground: 6 as *mut _, // COLOR_WINDOWTEXT + 1: a black ground
                lpszMenuName: std::ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };
            RegisterClassW(&class);

            // Size the frame so the client area is an exact multiple of the LCD.
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: (SCREEN_W as u32 * scale) as i32,
                bottom: (SCREEN_H as u32 * scale) as i32,
            };
            AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, 0);

            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                wide(title).as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                rect.right - rect.left,
                rect.bottom - rect.top,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null_mut(),
            );
            if hwnd.is_null() {
                return None;
            }
            ShowWindow(hwnd, SW_SHOW);

            Some(Window {
                hwnd,
                info: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: SCREEN_W as i32,
                    // Negative height means the first row is the top one.
                    biHeight: -(SCREEN_H as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                pixels: vec![0; SCREEN_W * SCREEN_H],
            })
        }
    }

    /// Drain the message queue. Returns false once the user has closed the game.
    pub fn pump(&mut self) -> bool {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT {
                    QUIT.store(true, Ordering::Relaxed);
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        !QUIT.load(Ordering::Relaxed)
    }

    pub fn buttons(&self) -> [bool; 8] {
        let keys = KEYS.load(Ordering::Relaxed);
        let mut held = [false; 8];
        for (i, slot) in held.iter_mut().enumerate() {
            *slot = keys & (1 << i) != 0;
        }
        held
    }

    pub fn turbo(&self) -> bool {
        TURBO.load(Ordering::Relaxed)
    }

    /// Scale the frame to the window, keeping the aspect ratio if asked.
    pub fn present(&mut self, frame: &[u32], keep_aspect: bool) {
        self.pixels.copy_from_slice(frame);
        unsafe {
            let mut rect = RECT::default();
            GetClientRect(self.hwnd, &mut rect);
            let (mut w, mut h) = (rect.right - rect.left, rect.bottom - rect.top);
            let (mut x, mut y) = (0, 0);
            if keep_aspect && w > 0 && h > 0 {
                let scale = (w / SCREEN_W as i32).min(h / SCREEN_H as i32).max(1);
                let (fit_w, fit_h) = (SCREEN_W as i32 * scale, SCREEN_H as i32 * scale);
                x = (w - fit_w) / 2;
                y = (h - fit_h) / 2;
                w = fit_w;
                h = fit_h;
            }

            let dc = GetDC(self.hwnd);
            StretchDIBits(
                dc,
                x,
                y,
                w,
                h,
                0,
                0,
                SCREEN_W as i32,
                SCREEN_H as i32,
                self.pixels.as_ptr() as *const _,
                &self.info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
            ReleaseDC(self.hwnd, dc);
        }
    }
}

pub fn button_for(index: usize) -> Button {
    BUTTONS[index]
}

// ---- audio ---------------------------------------------------------------

const AUDIO_BUFFERS: usize = 4;

/// Sound output over waveOut. Missing or unusable audio hardware is not fatal:
/// the game simply plays silently.
pub struct Audio {
    handle: *mut core::ffi::c_void,
    headers: Vec<Box<WAVEHDR>>,
    storage: Vec<Vec<i16>>,
    next: usize,
    frames_per_buffer: usize,
}

impl Audio {
    pub fn new(sample_rate: u32) -> Option<Audio> {
        // A fifth of a second of latency, split across the buffer ring.
        let frames_per_buffer = (sample_rate as usize / 50).max(256);

        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: 2,
            nSamplesPerSec: sample_rate,
            nAvgBytesPerSec: sample_rate * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        let mut handle: *mut core::ffi::c_void = std::ptr::null_mut();
        let result = unsafe { waveOutOpen(&mut handle, 0xffff_ffff, &format, 0, 0, 0) };
        if result != 0 || handle.is_null() {
            return None;
        }

        let mut storage = Vec::with_capacity(AUDIO_BUFFERS);
        let mut headers = Vec::with_capacity(AUDIO_BUFFERS);
        for _ in 0..AUDIO_BUFFERS {
            let mut buffer = vec![0i16; frames_per_buffer * 2];
            let mut header = Box::new(WAVEHDR {
                lpData: buffer.as_mut_ptr() as *mut u8,
                dwBufferLength: (buffer.len() * 2) as u32,
                dwBytesRecorded: 0,
                dwUser: 0,
                // Start marked done so the first refill can claim every buffer.
                dwFlags: WHDR_DONE,
                dwLoops: 0,
                lpNext: std::ptr::null_mut(),
                reserved: 0,
            });
            unsafe {
                waveOutPrepareHeader(handle, &mut *header, std::mem::size_of::<WAVEHDR>() as u32);
            }
            header.dwFlags |= WHDR_DONE;
            storage.push(buffer);
            headers.push(header);
        }

        Some(Audio {
            handle,
            headers,
            storage,
            next: 0,
            frames_per_buffer,
        })
    }

    /// Hand over as many queued samples as there are free buffers for.
    /// Anything that does not fit is dropped, which keeps latency from growing
    /// without bound if the host runs ahead.
    pub fn submit(&mut self, queue: &mut Vec<f32>) {
        let needed = self.frames_per_buffer * 2;
        while queue.len() >= needed {
            let slot = self.next;
            if self.headers[slot].dwFlags & WHDR_DONE == 0 {
                break; // still playing; leave the rest queued
            }
            for (i, sample) in queue.drain(..needed).enumerate() {
                self.storage[slot][i] = (sample.clamp(-1.0, 1.0) * 32_000.0) as i16;
            }
            self.headers[slot].dwFlags &= !WHDR_DONE;
            self.headers[slot].dwBufferLength = (needed * 2) as u32;
            unsafe {
                waveOutWrite(
                    self.handle,
                    &mut *self.headers[slot],
                    std::mem::size_of::<WAVEHDR>() as u32,
                );
            }
            self.next = (self.next + 1) % AUDIO_BUFFERS;
        }

        // If the game has run far ahead, throw away the backlog rather than
        // drift further and further behind the picture.
        if queue.len() > needed * AUDIO_BUFFERS * 2 {
            let excess = queue.len() - needed * AUDIO_BUFFERS;
            queue.drain(..excess);
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        unsafe {
            waveOutReset(self.handle);
            for header in &mut self.headers {
                waveOutUnprepareHeader(
                    self.handle,
                    &mut **header,
                    std::mem::size_of::<WAVEHDR>() as u32,
                );
            }
            waveOutClose(self.handle);
        }
    }
}

// ---- pacing --------------------------------------------------------------

/// A monotonic clock for holding the game to the right speed.
pub struct Clock {
    frequency: i64,
    next_frame: i64,
}

impl Clock {
    pub fn new() -> Clock {
        let mut frequency = 0i64;
        let mut now = 0i64;
        unsafe {
            QueryPerformanceFrequency(&mut frequency);
            QueryPerformanceCounter(&mut now);
        }
        Clock {
            frequency: frequency.max(1),
            next_frame: now,
        }
    }

    fn now(&self) -> i64 {
        let mut value = 0i64;
        unsafe { QueryPerformanceCounter(&mut value) };
        value
    }

    /// Wait until the next frame is due. `frame_seconds` is the LCD's period.
    pub fn wait(&mut self, frame_seconds: f64) {
        let period = (self.frequency as f64 * frame_seconds) as i64;
        self.next_frame += period;
        let now = self.now();

        // If we have fallen more than a few frames behind, give up on catching
        // up rather than sprinting through a backlog.
        if now > self.next_frame + period * 4 {
            self.next_frame = now;
            return;
        }
        while self.now() < self.next_frame {
            let remaining = self.next_frame - self.now();
            let ms = remaining * 1000 / self.frequency;
            if ms > 2 {
                unsafe { Sleep((ms - 1) as u32) };
            }
        }
    }

    pub fn resync(&mut self) {
        self.next_frame = self.now();
    }
}
