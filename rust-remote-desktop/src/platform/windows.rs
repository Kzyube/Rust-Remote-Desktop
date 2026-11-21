use std::{
    collections::{HashMap, HashSet},
    mem::size_of,
    ptr::null_mut,
};

use anyhow::{Context, Result, ensure};
use windows_sys::Win32::{
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC,
        CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HBITMAP, HDC,
        HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
    },
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
            KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
            MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
            MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
        },
        WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, SetProcessDPIAware},
    },
};

use super::{
    CapturedFrame, PointerButton, RemoteInput, RemoteKey, rgba_length, validate_pointer,
    validate_scroll,
};

pub(super) struct NativeDesktop {
    capture: Option<CaptureSurface>,
    held_keys: HashMap<RemoteKey, u16>,
    held_buttons: HashSet<PointerButton>,
}

impl NativeDesktop {
    pub(super) fn new() -> Result<Self> {
        // The GUI may already have selected per-monitor DPI awareness. Failure
        // then means it was already configured and must not be overridden.
        unsafe {
            SetProcessDPIAware();
        }
        dimensions()?;
        Ok(Self {
            capture: None,
            held_keys: HashMap::new(),
            held_buttons: HashSet::new(),
        })
    }

    pub(super) fn capture(&mut self) -> Result<CapturedFrame> {
        let (width, height) = dimensions()?;
        if !self
            .capture
            .as_ref()
            .is_some_and(|surface| surface.width == width && surface.height == height)
        {
            self.capture = Some(CaptureSurface::new(width, height)?);
        }
        self.capture
            .as_mut()
            .context("Windows capture surface is missing")?
            .capture()
    }

    pub(super) fn inject(&mut self, event: RemoteInput) -> Result<()> {
        match event {
            RemoteInput::PointerMove { x, y } => {
                let (width, height) = dimensions()?;
                validate_pointer(x, y, width, height)?;
                send(mouse(
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    absolute_coordinate(x, width),
                    absolute_coordinate(y, height),
                    0,
                ))?;
            }
            RemoteInput::Button { button, pressed } => {
                if pressed != self.held_buttons.contains(&button) {
                    send(mouse(button_flag(button, pressed), 0, 0, 0))?;
                    if pressed {
                        self.held_buttons.insert(button);
                    } else {
                        self.held_buttons.remove(&button);
                    }
                }
            }
            RemoteInput::Key { key, pressed } => {
                if pressed && !self.held_keys.contains_key(&key) {
                    let code = virtual_key(key);
                    send(keyboard(key, code, true))?;
                    self.held_keys.insert(key, code);
                } else if !pressed && let Some(code) = self.held_keys.get(&key).copied() {
                    send(keyboard(key, code, false))?;
                    self.held_keys.remove(&key);
                }
            }
            RemoteInput::Scroll { vertical } => {
                validate_scroll(vertical)?;
                if vertical != 0 {
                    send(mouse(MOUSEEVENTF_WHEEL, 0, 0, (vertical * 120) as u32))?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn release_all(&mut self) -> Result<()> {
        let mut failure = None;
        for (key, code) in self.held_keys.clone() {
            match send(keyboard(key, code, false)) {
                Ok(()) => {
                    self.held_keys.remove(&key);
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        for button in self.held_buttons.clone() {
            match send(mouse(button_flag(button, false), 0, 0, 0)) {
                Ok(()) => {
                    self.held_buttons.remove(&button);
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        if let Some(error) = failure {
            Err(error)
        } else {
            Ok(())
        }
    }
}

fn dimensions() -> Result<(u32, u32)> {
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let dimensions = (u32::try_from(width)?, u32::try_from(height)?);
    rgba_length(dimensions.0, dimensions.1)?;
    Ok(dimensions)
}

fn absolute_coordinate(value: u32, extent: u32) -> i32 {
    if extent <= 1 {
        0
    } else {
        ((u64::from(value) * 65_535) / u64::from(extent - 1)) as i32
    }
}

fn send(input: INPUT) -> Result<()> {
    let sent = unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) };
    ensure!(
        sent == 1,
        "Windows rejected input (another integrity level or the secure desktop may be active): {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn mouse(flags: u32, x: i32, y: i32, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: x,
                dy: y,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn button_flag(button: PointerButton, pressed: bool) -> u32 {
    match (button, pressed) {
        (PointerButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (PointerButton::Left, false) => MOUSEEVENTF_LEFTUP,
        (PointerButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (PointerButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        (PointerButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (PointerButton::Right, false) => MOUSEEVENTF_RIGHTUP,
    }
}

fn keyboard(key: RemoteKey, code: u16, pressed: bool) -> INPUT {
    use RemoteKey::*;
    let extended = matches!(
        key,
        ArrowUp
            | ArrowDown
            | ArrowLeft
            | ArrowRight
            | Home
            | End
            | PageUp
            | PageDown
            | Delete
            | Insert
            | Meta
    );
    let flags = if extended { KEYEVENTF_EXTENDEDKEY } else { 0 }
        | if pressed { 0 } else { KEYEVENTF_KEYUP };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: code,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn virtual_key(key: RemoteKey) -> u16 {
    use RemoteKey::*;
    match key {
        A => 0x41,
        B => 0x42,
        C => 0x43,
        D => 0x44,
        E => 0x45,
        F => 0x46,
        G => 0x47,
        H => 0x48,
        I => 0x49,
        J => 0x4a,
        K => 0x4b,
        L => 0x4c,
        M => 0x4d,
        N => 0x4e,
        O => 0x4f,
        P => 0x50,
        Q => 0x51,
        R => 0x52,
        S => 0x53,
        T => 0x54,
        U => 0x55,
        V => 0x56,
        W => 0x57,
        X => 0x58,
        Y => 0x59,
        Z => 0x5a,
        Digit0 => 0x30,
        Digit1 => 0x31,
        Digit2 => 0x32,
        Digit3 => 0x33,
        Digit4 => 0x34,
        Digit5 => 0x35,
        Digit6 => 0x36,
        Digit7 => 0x37,
        Digit8 => 0x38,
        Digit9 => 0x39,
        Enter => 0x0d,
        Escape => 0x1b,
        Backspace => 0x08,
        Tab => 0x09,
        Space => 0x20,
        ArrowUp => 0x26,
        ArrowDown => 0x28,
        ArrowLeft => 0x25,
        ArrowRight => 0x27,
        Home => 0x24,
        End => 0x23,
        PageUp => 0x21,
        PageDown => 0x22,
        Delete => 0x2e,
        Insert => 0x2d,
        Shift => 0xa0,
        Control => 0xa2,
        Alt => 0xa4,
        Meta => 0x5b,
        F1 => 0x70,
        F2 => 0x71,
        F3 => 0x72,
        F4 => 0x73,
        F5 => 0x74,
        F6 => 0x75,
        F7 => 0x76,
        F8 => 0x77,
        F9 => 0x78,
        F10 => 0x79,
        F11 => 0x7a,
        F12 => 0x7b,
        Minus => 0xbd,
        Equal => 0xbb,
        LeftBracket => 0xdb,
        RightBracket => 0xdd,
        Backslash => 0xdc,
        Semicolon => 0xba,
        Quote => 0xde,
        Comma => 0xbc,
        Period => 0xbe,
        Slash => 0xbf,
        Backtick => 0xc0,
    }
}

struct CaptureSurface {
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    width: u32,
    height: u32,
}

impl CaptureSurface {
    fn new(width: u32, height: u32) -> Result<Self> {
        let length = rgba_length(width, height)?;
        let mut surface = Self {
            screen: null_mut(),
            memory: null_mut(),
            bitmap: null_mut(),
            previous: null_mut(),
            bits: null_mut(),
            width,
            height,
        };
        // Each successfully acquired handle is immediately owned by `surface`,
        // so later allocation failures still release the earlier resources.
        unsafe {
            surface.screen = GetDC(null_mut());
            ensure!(
                !surface.screen.is_null(),
                "GetDC failed: {}",
                std::io::Error::last_os_error()
            );
            surface.memory = CreateCompatibleDC(surface.screen);
            ensure!(
                !surface.memory.is_null(),
                "CreateCompatibleDC failed: {}",
                std::io::Error::last_os_error()
            );
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: i32::try_from(width)?,
                    biHeight: -i32::try_from(height)?,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    biSizeImage: u32::try_from(length)?,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = null_mut();
            surface.bitmap = CreateDIBSection(
                surface.screen,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                null_mut(),
                0,
            );
            ensure!(
                !surface.bitmap.is_null() && !bits.is_null(),
                "CreateDIBSection failed: {}",
                std::io::Error::last_os_error()
            );
            surface.bits = bits.cast();
            surface.previous = SelectObject(surface.memory, surface.bitmap);
            ensure!(
                !surface.previous.is_null() && surface.previous as isize != -1,
                "SelectObject failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(surface)
    }

    fn capture(&mut self) -> Result<CapturedFrame> {
        let length = rgba_length(self.width, self.height)?;
        let success = unsafe {
            BitBlt(
                self.memory,
                0,
                0,
                self.width as i32,
                self.height as i32,
                self.screen,
                0,
                0,
                SRCCOPY | CAPTUREBLT,
            )
        };
        ensure!(
            success != 0,
            "Windows screen capture failed: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            unsafe { GdiFlush() } != 0,
            "Windows capture could not synchronize its DIB pixels"
        );
        // The owned top-down 32bpp DIB allocates exactly this validated length.
        // BitBlt finished before we read it; there is no concurrent writer.
        let source = unsafe { std::slice::from_raw_parts(self.bits, length) };
        let mut rgba = Vec::with_capacity(length);
        for pixel in source.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
        Ok(CapturedFrame {
            width: self.width,
            height: self.height,
            rgba,
        })
    }
}

impl Drop for CaptureSurface {
    fn drop(&mut self) {
        unsafe {
            if !self.memory.is_null() && !self.previous.is_null() && self.previous as isize != -1 {
                SelectObject(self.memory, self.previous);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            if !self.memory.is_null() {
                DeleteDC(self.memory);
            }
            if !self.screen.is_null() {
                ReleaseDC(null_mut(), self.screen);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_pointer_coordinates_reach_both_edges() {
        assert_eq!(absolute_coordinate(0, 1920), 0);
        assert_eq!(absolute_coordinate(1919, 1920), 65535);
        assert_eq!(absolute_coordinate(0, 1), 0);
    }

    #[test]
    fn navigation_keys_are_extended_and_keyup_is_marked() {
        let input = keyboard(
            RemoteKey::ArrowLeft,
            virtual_key(RemoteKey::ArrowLeft),
            false,
        );
        let flags = unsafe { input.Anonymous.ki.dwFlags };
        assert_eq!(flags, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP);
        let input = keyboard(RemoteKey::A, virtual_key(RemoteKey::A), true);
        assert_eq!(unsafe { input.Anonymous.ki.dwFlags }, 0);
    }
}
