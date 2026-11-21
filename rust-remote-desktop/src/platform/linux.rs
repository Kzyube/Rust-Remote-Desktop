use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail, ensure};
use x11rb::{
    connection::Connection,
    protocol::{
        xproto::{
            BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, ImageFormat, ImageOrder,
            KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, VisualClass,
        },
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
};

use super::{
    CapturedFrame, PointerButton, RemoteInput, RemoteKey, rgba_length, validate_pointer,
    validate_scroll,
};

#[derive(Clone, Copy)]
struct PixelFormat {
    bits_per_pixel: u8,
    scanline_pad: u8,
    little_endian: bool,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
}

impl PixelFormat {
    fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.bits_per_pixel, 16 | 24 | 32),
            "Unsupported X11 pixel width"
        );
        ensure!(
            matches!(self.scanline_pad, 8 | 16 | 32),
            "Unsupported X11 scanline alignment"
        );
        let masks = [self.red_mask, self.green_mask, self.blue_mask];
        for mask in masks {
            ensure!(mask != 0, "X11 channel mask is empty");
            let normalized = mask >> mask.trailing_zeros();
            ensure!(
                normalized == u32::MAX || (normalized & (normalized + 1)) == 0,
                "Noncontiguous X11 color masks are unsupported"
            );
            if self.bits_per_pixel < 32 {
                ensure!(
                    mask < (1_u32 << self.bits_per_pixel),
                    "X11 mask exceeds pixel width"
                );
            }
        }
        ensure!(
            (self.red_mask & self.green_mask) == 0
                && (self.red_mask & self.blue_mask) == 0
                && (self.green_mask & self.blue_mask) == 0,
            "X11 color masks overlap"
        );
        Ok(())
    }

    fn convert(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
        self.validate()?;
        let output_len = rgba_length(width, height)?;
        let width = usize::try_from(width)?;
        let height = usize::try_from(height)?;
        let row_bits = width
            .checked_mul(usize::from(self.bits_per_pixel))
            .context("X11 scanline size overflow")?;
        let pad = usize::from(self.scanline_pad);
        let stride = row_bits
            .checked_add(pad - 1)
            .and_then(|value| (value / pad).checked_mul(pad / 8))
            .context("X11 scanline alignment overflow")?;
        ensure!(
            data.len()
                >= stride
                    .checked_mul(height)
                    .context("X11 image size overflow")?,
            "X11 returned a truncated image"
        );
        let bytes_per_pixel = usize::from(self.bits_per_pixel / 8);
        let channel = |pixel: u32, mask: u32| -> u8 {
            let shift = mask.trailing_zeros();
            let value = u64::from((pixel & mask) >> shift);
            let maximum = u64::from(mask >> shift);
            ((value * 255 + maximum / 2) / maximum) as u8
        };
        let mut rgba = vec![0; output_len];
        for row in 0..height {
            for column in 0..width {
                let source = row * stride + column * bytes_per_pixel;
                let bytes = &data[source..source + bytes_per_pixel];
                let pixel = if self.little_endian {
                    bytes
                        .iter()
                        .enumerate()
                        .fold(0_u32, |value, (index, byte)| {
                            value | (u32::from(*byte) << (index * 8))
                        })
                } else {
                    bytes
                        .iter()
                        .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte))
                };
                let target = (row * width + column) * 4;
                rgba[target..target + 4].copy_from_slice(&[
                    channel(pixel, self.red_mask),
                    channel(pixel, self.green_mask),
                    channel(pixel, self.blue_mask),
                    255,
                ]);
            }
        }
        Ok(rgba)
    }
}

pub(super) struct NativeDesktop {
    connection: RustConnection,
    root: u32,
    depth: u8,
    visual: u32,
    format: PixelFormat,
    keycodes: HashMap<u32, u8>,
    held_keys: HashMap<RemoteKey, u8>,
    held_buttons: HashSet<u8>,
}

impl NativeDesktop {
    pub(super) fn new() -> Result<Self> {
        if std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind.eq_ignore_ascii_case("wayland"))
        {
            bail!(
                "Linux hosting currently requires an X11 session. Wayland capture and input need a portal adapter."
            );
        }
        let (connection, screen_number) = x11rb::connect(None).context(
            "Cannot connect to the X11 display; run inside your graphical desktop session",
        )?;
        let setup = connection.setup();
        let screen = setup
            .roots
            .get(screen_number)
            .context("X11 screen is missing")?;
        let root = screen.root;
        let depth = screen.root_depth;
        let visual = screen.root_visual;
        let visual_info = screen
            .allowed_depths
            .iter()
            .flat_map(|depth| &depth.visuals)
            .find(|item| item.visual_id == visual)
            .context("X11 root visual was not advertised")?;
        ensure!(
            visual_info.class == VisualClass::TRUE_COLOR,
            "Only X11 TrueColor visuals are supported"
        );
        let pixmap = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == depth)
            .context("X11 root pixel format was not advertised")?;
        ensure!(
            setup.image_byte_order == ImageOrder::LSB_FIRST
                || setup.image_byte_order == ImageOrder::MSB_FIRST,
            "Unsupported X11 image byte order"
        );
        let format = PixelFormat {
            bits_per_pixel: pixmap.bits_per_pixel,
            scanline_pad: pixmap.scanline_pad,
            little_endian: setup.image_byte_order == ImageOrder::LSB_FIRST,
            red_mask: visual_info.red_mask,
            green_mask: visual_info.green_mask,
            blue_mask: visual_info.blue_mask,
        };
        format.validate()?;
        rgba_length(
            u32::from(screen.width_in_pixels),
            u32::from(screen.height_in_pixels),
        )?;
        connection
            .xtest_get_version(2, 2)?
            .reply()
            .context("X11 XTEST extension is required for input control")?;
        let minimum = setup.min_keycode;
        let count = setup
            .max_keycode
            .checked_sub(minimum)
            .and_then(|count| count.checked_add(1))
            .context("Invalid X11 keyboard map range")?;
        let mapping = connection.get_keyboard_mapping(minimum, count)?.reply()?;
        ensure!(
            mapping.keysyms_per_keycode > 0,
            "X11 keyboard map has no keysyms"
        );
        let mut keycodes = HashMap::new();
        for (index, symbols) in mapping
            .keysyms
            .chunks(usize::from(mapping.keysyms_per_keycode))
            .enumerate()
        {
            // Use only the unshifted first group. Shift/Alt are transmitted as
            // separate held modifiers; searching higher groups would require
            // injecting additional implicit modifiers.
            if let Some(symbol) = symbols.first().filter(|symbol| **symbol != 0) {
                keycodes
                    .entry(*symbol)
                    .or_insert(minimum + u8::try_from(index)?);
            }
        }
        Ok(Self {
            connection,
            root,
            depth,
            visual,
            format,
            keycodes,
            held_keys: HashMap::new(),
            held_buttons: HashSet::new(),
        })
    }

    fn dimensions(&self) -> Result<(u32, u32)> {
        let geometry = self.connection.get_geometry(self.root)?.reply()?;
        let dimensions = (u32::from(geometry.width), u32::from(geometry.height));
        rgba_length(dimensions.0, dimensions.1)?;
        Ok(dimensions)
    }

    pub(super) fn capture(&mut self) -> Result<CapturedFrame> {
        let (width, height) = self.dimensions()?;
        let image = self
            .connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                0,
                0,
                u16::try_from(width)?,
                u16::try_from(height)?,
                u32::MAX,
            )?
            .reply()?;
        ensure!(
            image.depth == self.depth && image.visual == self.visual,
            "X11 image visual changed; start a new session"
        );
        Ok(CapturedFrame {
            width,
            height,
            rgba: self.format.convert(&image.data, width, height)?,
        })
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.connection
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)?
            .check()?;
        self.connection.flush()?;
        Ok(())
    }

    pub(super) fn inject(&mut self, event: RemoteInput) -> Result<()> {
        match event {
            RemoteInput::PointerMove { x, y } => {
                let (width, height) = self.dimensions()?;
                validate_pointer(x, y, width, height)?;
                self.fake(MOTION_NOTIFY_EVENT, 0, i16::try_from(x)?, i16::try_from(y)?)?;
            }
            RemoteInput::Button { button, pressed } => {
                let detail = match button {
                    PointerButton::Left => 1,
                    PointerButton::Middle => 2,
                    PointerButton::Right => 3,
                };
                if pressed && !self.held_buttons.contains(&detail) {
                    self.fake(BUTTON_PRESS_EVENT, detail, 0, 0)?;
                    self.held_buttons.insert(detail);
                } else if !pressed && self.held_buttons.contains(&detail) {
                    self.fake(BUTTON_RELEASE_EVENT, detail, 0, 0)?;
                    self.held_buttons.remove(&detail);
                }
            }
            RemoteInput::Key { key, pressed } => {
                if pressed && !self.held_keys.contains_key(&key) {
                    let code = *self.keycodes.get(&keysym(key)).context(
                        "This key is not available in the host's current X11 keyboard layout",
                    )?;
                    self.fake(KEY_PRESS_EVENT, code, 0, 0)?;
                    self.held_keys.insert(key, code);
                } else if !pressed && let Some(code) = self.held_keys.get(&key).copied() {
                    self.fake(KEY_RELEASE_EVENT, code, 0, 0)?;
                    self.held_keys.remove(&key);
                }
            }
            RemoteInput::Scroll { vertical } => {
                validate_scroll(vertical)?;
                let button = if vertical > 0 { 4 } else { 5 };
                for _ in 0..vertical.unsigned_abs() {
                    self.fake(BUTTON_PRESS_EVENT, button, 0, 0)?;
                    self.fake(BUTTON_RELEASE_EVENT, button, 0, 0)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn release_all(&mut self) -> Result<()> {
        let mut failure = None;
        for (key, code) in self.held_keys.clone() {
            match self.fake(KEY_RELEASE_EVENT, code, 0, 0) {
                Ok(()) => {
                    self.held_keys.remove(&key);
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        for code in self.held_buttons.clone() {
            match self.fake(BUTTON_RELEASE_EVENT, code, 0, 0) {
                Ok(()) => {
                    self.held_buttons.remove(&code);
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

fn keysym(key: RemoteKey) -> u32 {
    use RemoteKey::*;
    match key {
        A => b'a' as u32,
        B => b'b' as u32,
        C => b'c' as u32,
        D => b'd' as u32,
        E => b'e' as u32,
        F => b'f' as u32,
        G => b'g' as u32,
        H => b'h' as u32,
        I => b'i' as u32,
        J => b'j' as u32,
        K => b'k' as u32,
        L => b'l' as u32,
        M => b'm' as u32,
        N => b'n' as u32,
        O => b'o' as u32,
        P => b'p' as u32,
        Q => b'q' as u32,
        R => b'r' as u32,
        S => b's' as u32,
        T => b't' as u32,
        U => b'u' as u32,
        V => b'v' as u32,
        W => b'w' as u32,
        X => b'x' as u32,
        Y => b'y' as u32,
        Z => b'z' as u32,
        Digit0 => b'0' as u32,
        Digit1 => b'1' as u32,
        Digit2 => b'2' as u32,
        Digit3 => b'3' as u32,
        Digit4 => b'4' as u32,
        Digit5 => b'5' as u32,
        Digit6 => b'6' as u32,
        Digit7 => b'7' as u32,
        Digit8 => b'8' as u32,
        Digit9 => b'9' as u32,
        Enter => 0xff0d,
        Escape => 0xff1b,
        Backspace => 0xff08,
        Tab => 0xff09,
        Space => 0x20,
        ArrowUp => 0xff52,
        ArrowDown => 0xff54,
        ArrowLeft => 0xff51,
        ArrowRight => 0xff53,
        Home => 0xff50,
        End => 0xff57,
        PageUp => 0xff55,
        PageDown => 0xff56,
        Delete => 0xffff,
        Insert => 0xff63,
        Shift => 0xffe1,
        Control => 0xffe3,
        Alt => 0xffe9,
        Meta => 0xffeb,
        F1 => 0xffbe,
        F2 => 0xffbf,
        F3 => 0xffc0,
        F4 => 0xffc1,
        F5 => 0xffc2,
        F6 => 0xffc3,
        F7 => 0xffc4,
        F8 => 0xffc5,
        F9 => 0xffc6,
        F10 => 0xffc7,
        F11 => 0xffc8,
        F12 => 0xffc9,
        Minus => b'-' as u32,
        Equal => b'=' as u32,
        LeftBracket => b'[' as u32,
        RightBracket => b']' as u32,
        Backslash => b'\\' as u32,
        Semicolon => b';' as u32,
        Quote => b'\'' as u32,
        Comma => b',' as u32,
        Period => b'.' as u32,
        Slash => b'/' as u32,
        Backtick => b'`' as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(bits_per_pixel: u8, little_endian: bool) -> PixelFormat {
        PixelFormat {
            bits_per_pixel,
            scanline_pad: 32,
            little_endian,
            red_mask: 0xff0000,
            green_mask: 0xff00,
            blue_mask: 0xff,
        }
    }

    #[test]
    fn converts_little_endian_bgrx_without_using_padding_as_alpha() {
        let data = [3, 2, 1, 0, 255, 128, 64, 17];
        assert_eq!(
            format(32, true).convert(&data, 2, 1).unwrap(),
            [1, 2, 3, 255, 64, 128, 255, 255]
        );
    }

    #[test]
    fn converts_big_endian_24_bit_rows_with_scanline_padding() {
        let data = [1, 2, 3, 99, 4, 5, 6, 88];
        assert_eq!(
            format(24, false).convert(&data, 1, 2).unwrap(),
            [1, 2, 3, 255, 4, 5, 6, 255]
        );
        assert!(format(24, false).convert(&data[..7], 1, 2).is_err());
    }

    #[test]
    fn scales_rgb565_channels_and_ignores_row_padding() {
        let format = PixelFormat {
            bits_per_pixel: 16,
            scanline_pad: 32,
            little_endian: true,
            red_mask: 0xf800,
            green_mask: 0x07e0,
            blue_mask: 0x001f,
        };
        assert_eq!(
            format.convert(&[0x00, 0xf8, 99, 99], 1, 1).unwrap(),
            [255, 0, 0, 255]
        );
        assert_eq!(
            format.convert(&[0xff, 0xff, 0, 0], 1, 1).unwrap(),
            [255, 255, 255, 255]
        );
    }

    #[test]
    fn rejects_overlapping_masks_and_unsupported_pixel_width() {
        let mut bad = format(32, true);
        bad.red_mask = bad.green_mask;
        assert!(bad.convert(&[0; 4], 1, 1).is_err());
        bad = format(8, true);
        assert!(bad.convert(&[0; 4], 1, 1).is_err());
    }
}
