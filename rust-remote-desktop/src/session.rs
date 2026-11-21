//! Screen sharing begins only after the local host approves a request.

use std::{
    io::Cursor,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use image::{ExtendedColorType, ImageReader, Limits, codecs::jpeg::JpegEncoder};

use crate::{
    platform::{CapturedFrame, Desktop, RemoteInput},
    protocol::{
        ClientMessage, MAX_INPUT_BATCH, MAX_JPEG_BYTES, ServerMessage, VERSION,
        validate_dimensions, validate_hello,
    },
    transport::EncryptedConnection,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Decline,
    View,
    Control,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub name: String,
    pub request_control: bool,
}

pub trait ScreenSource {
    fn capture(&mut self) -> Result<CapturedFrame>;
    fn inject(&mut self, input: RemoteInput) -> Result<()>;
    fn release_all(&mut self) -> Result<()>;
}

impl ScreenSource for Desktop {
    fn capture(&mut self) -> Result<CapturedFrame> {
        Desktop::capture(self)
    }
    fn inject(&mut self, input: RemoteInput) -> Result<()> {
        Desktop::inject(self, input)
    }
    fn release_all(&mut self) -> Result<()> {
        Desktop::release_all(self)
    }
}

struct ReleaseOnExit<'a, S: ScreenSource>(&'a mut S);
impl<S: ScreenSource> Drop for ReleaseOnExit<'_, S> {
    fn drop(&mut self) {
        let _ = self.0.release_all();
    }
}

pub fn read_request(connection: &mut EncryptedConnection) -> Result<Request> {
    match connection.receive()? {
        ClientMessage::Hello {
            version,
            name,
            request_control,
        } => {
            validate_hello(version, &name)?;
            Ok(Request {
                name,
                request_control,
            })
        }
        _ => bail!("The first message must be a greeting"),
    }
}

pub fn answer_request(
    connection: &mut EncryptedConnection,
    request: &Request,
    permission: Permission,
) -> Result<bool> {
    if permission == Permission::Decline {
        connection.send(&ServerMessage::Rejected {
            reason: "The host declined or the request expired".into(),
        })?;
        return Ok(false);
    }
    let control = permission == Permission::Control && request.request_control;
    connection.send(&ServerMessage::Accepted {
        version: VERSION,
        control,
    })?;
    connection.set_timeout(Duration::from_secs(10))?;
    Ok(true)
}

pub fn serve<S: ScreenSource>(
    connection: &mut EncryptedConnection,
    source: &mut S,
    control: bool,
) -> Result<()> {
    let guard = ReleaseOnExit(source);
    let mut previous = Vec::new();
    let mut previous_size = (0, 0);
    let mut sequence = 0_u64;
    let mut next_capture = Instant::now();
    loop {
        match connection.receive()? {
            ClientMessage::Disconnect => return Ok(()),
            ClientMessage::Hello { .. } => bail!("Greeting was repeated inside an active session"),
            ClientMessage::Poll {
                inputs,
                release_inputs,
            } => {
                ensure!(
                    inputs.len() <= MAX_INPUT_BATCH,
                    "Too many input events in one poll"
                );
                ensure!(
                    control || inputs.is_empty(),
                    "This session allows viewing only"
                );
                // The host owns the capture rate, including when a viewer polls aggressively.
                std::thread::sleep(next_capture.saturating_duration_since(Instant::now()));
                next_capture = Instant::now() + Duration::from_millis(67);
                if release_inputs {
                    guard.0.release_all()?;
                }
                for input in inputs {
                    guard.0.inject(input)?;
                }
                let frame = guard.0.capture()?;
                ensure!(
                    frame.rgba.len() == validate_dimensions(frame.width, frame.height)?,
                    "Capture returned an invalid pixel buffer"
                );
                if previous_size == (frame.width, frame.height) && previous == frame.rgba {
                    connection.send(&ServerMessage::Unchanged { sequence })?;
                    continue;
                }
                sequence = sequence.checked_add(1).context("Frame sequence overflow")?;
                let jpeg = encode_frame(&frame)?;
                previous_size = (frame.width, frame.height);
                previous = frame.rgba;
                connection.send(&ServerMessage::Frame {
                    sequence,
                    width: frame.width,
                    height: frame.height,
                    jpeg,
                })?;
            }
        }
    }
}

pub fn encode_frame(frame: &CapturedFrame) -> Result<Vec<u8>> {
    ensure!(
        frame.rgba.len() == validate_dimensions(frame.width, frame.height)?,
        "Invalid captured frame"
    );
    let rgb: Vec<u8> = frame
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| pixel[..3].iter().copied())
        .collect();
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 70).encode(
        &rgb,
        frame.width,
        frame.height,
        ExtendedColorType::Rgb8,
    )?;
    ensure!(
        jpeg.len() <= MAX_JPEG_BYTES,
        "JPEG frame exceeds the protocol limit"
    );
    Ok(jpeg)
}

pub fn decode_frame(width: u32, height: u32, jpeg: &[u8]) -> Result<CapturedFrame> {
    let expected = validate_dimensions(width, height)?;
    ensure!(
        jpeg.len() <= MAX_JPEG_BYTES,
        "JPEG frame exceeds the protocol limit"
    );
    let mut reader = ImageReader::with_format(Cursor::new(jpeg), image::ImageFormat::Jpeg);
    let mut limits = Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .context("Unable to decode remote JPEG frame")?
        .to_rgba8();
    ensure!(
        decoded.width() == width && decoded.height() == height,
        "JPEG dimensions differ from the frame header"
    );
    let rgba = decoded.into_raw();
    ensure!(
        rgba.len() == expected,
        "Decoded frame length does not match its dimensions"
    );
    Ok(CapturedFrame {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        platform::{RemoteInput, RemoteKey},
        transport,
    };
    use std::{
        net::{TcpListener, TcpStream},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
    };

    struct FakeScreen {
        released: Arc<AtomicBool>,
        injected: Arc<AtomicUsize>,
    }
    impl ScreenSource for FakeScreen {
        fn capture(&mut self) -> Result<CapturedFrame> {
            Ok(CapturedFrame {
                width: 8,
                height: 8,
                rgba: [64, 128, 192, 255].repeat(64),
            })
        }
        fn inject(&mut self, _input: RemoteInput) -> Result<()> {
            self.injected.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn release_all(&mut self) -> Result<()> {
            self.released.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn encoded_frame_round_trips_and_rejects_false_dimensions() {
        let frame = CapturedFrame {
            width: 8,
            height: 8,
            rgba: [64, 128, 192, 255].repeat(64),
        };
        let jpeg = encode_frame(&frame).unwrap();
        assert_eq!(decode_frame(8, 8, &jpeg).unwrap().rgba.len(), 256);
        assert!(decode_frame(7, 8, &jpeg).is_err());
        assert!(decode_frame(8192, 8192, &jpeg).is_err());
    }

    #[test]
    fn authenticated_viewer_gets_frames_and_view_only_rejects_input() {
        let secret = transport::new_pairing_secret().unwrap();
        let host_secret = secret.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let injected = Arc::new(AtomicUsize::new(0));
        let source = FakeScreen {
            released: released.clone(),
            injected: injected.clone(),
        };
        let host = thread::spawn(move || {
            let mut connection =
                transport::accept(listener.accept().unwrap().0, &host_secret).unwrap();
            let request = read_request(&mut connection).unwrap();
            answer_request(&mut connection, &request, Permission::View).unwrap();
            serve(&mut connection, &mut { source }, false)
        });
        let mut viewer = transport::connect(TcpStream::connect(address).unwrap(), &secret).unwrap();
        viewer
            .send(&ClientMessage::Hello {
                version: VERSION,
                name: "test viewer".into(),
                request_control: true,
            })
            .unwrap();
        assert!(matches!(
            viewer.receive::<ServerMessage>().unwrap(),
            ServerMessage::Accepted { control: false, .. }
        ));
        viewer
            .send(&ClientMessage::Poll {
                inputs: vec![],
                release_inputs: false,
            })
            .unwrap();
        let ServerMessage::Frame {
            width,
            height,
            jpeg,
            ..
        } = viewer.receive().unwrap()
        else {
            panic!("Expected first frame")
        };
        assert_eq!(decode_frame(width, height, &jpeg).unwrap().rgba.len(), 256);
        viewer
            .send(&ClientMessage::Poll {
                inputs: vec![],
                release_inputs: false,
            })
            .unwrap();
        assert!(matches!(
            viewer.receive::<ServerMessage>().unwrap(),
            ServerMessage::Unchanged { .. }
        ));
        viewer
            .send(&ClientMessage::Poll {
                inputs: vec![RemoteInput::Key {
                    key: RemoteKey::A,
                    pressed: true,
                }],
                release_inputs: false,
            })
            .unwrap();
        assert!(host.join().unwrap().is_err());
        assert_eq!(injected.load(Ordering::SeqCst), 0);
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn approved_control_releases_held_input_after_socket_failure() {
        let secret = transport::new_pairing_secret().unwrap();
        let host_secret = secret.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let injected = Arc::new(AtomicUsize::new(0));
        let source = FakeScreen {
            released: released.clone(),
            injected: injected.clone(),
        };
        let host = thread::spawn(move || {
            let mut connection =
                transport::accept(listener.accept().unwrap().0, &host_secret).unwrap();
            let request = read_request(&mut connection).unwrap();
            answer_request(&mut connection, &request, Permission::Control).unwrap();
            serve(&mut connection, &mut { source }, true)
        });
        let mut viewer = transport::connect(TcpStream::connect(address).unwrap(), &secret).unwrap();
        viewer
            .send(&ClientMessage::Hello {
                version: VERSION,
                name: "controller".into(),
                request_control: true,
            })
            .unwrap();
        assert!(matches!(
            viewer.receive::<ServerMessage>().unwrap(),
            ServerMessage::Accepted { control: true, .. }
        ));
        viewer
            .send(&ClientMessage::Poll {
                inputs: vec![RemoteInput::Key {
                    key: RemoteKey::A,
                    pressed: true,
                }],
                release_inputs: false,
            })
            .unwrap();
        assert!(matches!(
            viewer.receive::<ServerMessage>().unwrap(),
            ServerMessage::Frame { .. }
        ));
        assert_eq!(injected.load(Ordering::SeqCst), 1);
        assert!(!released.load(Ordering::SeqCst));
        drop(viewer);
        assert!(host.join().unwrap().is_err());
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn explicit_release_can_precede_a_remote_shortcut_in_the_same_poll() {
        let secret = transport::new_pairing_secret().unwrap();
        let host_secret = secret.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let injected = Arc::new(AtomicUsize::new(0));
        let source = FakeScreen {
            released: released.clone(),
            injected: injected.clone(),
        };
        let host = thread::spawn(move || {
            let mut connection =
                transport::accept(listener.accept().unwrap().0, &host_secret).unwrap();
            let request = read_request(&mut connection).unwrap();
            answer_request(&mut connection, &request, Permission::Control).unwrap();
            serve(&mut connection, &mut { source }, true)
        });
        let mut viewer = transport::connect(TcpStream::connect(address).unwrap(), &secret).unwrap();
        viewer
            .send(&ClientMessage::Hello {
                version: VERSION,
                name: "controller".into(),
                request_control: true,
            })
            .unwrap();
        let _: ServerMessage = viewer.receive().unwrap();
        viewer
            .send(&ClientMessage::Poll {
                inputs: vec![
                    RemoteInput::Key {
                        key: RemoteKey::Control,
                        pressed: true,
                    },
                    RemoteInput::Key {
                        key: RemoteKey::V,
                        pressed: true,
                    },
                    RemoteInput::Key {
                        key: RemoteKey::V,
                        pressed: false,
                    },
                    RemoteInput::Key {
                        key: RemoteKey::Control,
                        pressed: false,
                    },
                ],
                release_inputs: true,
            })
            .unwrap();
        assert!(matches!(
            viewer.receive::<ServerMessage>().unwrap(),
            ServerMessage::Frame { .. }
        ));
        assert!(released.load(Ordering::SeqCst));
        assert_eq!(injected.load(Ordering::SeqCst), 4);
        viewer.send(&ClientMessage::Disconnect).unwrap();
        assert!(host.join().unwrap().is_ok());
    }

    #[test]
    fn decline_never_constructs_or_captures_a_desktop() {
        let secret = transport::new_pairing_secret().unwrap();
        let host_secret = secret.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let host = thread::spawn(move || {
            let mut connection =
                transport::accept(listener.accept().unwrap().0, &host_secret).unwrap();
            let request = read_request(&mut connection).unwrap();
            assert!(!answer_request(&mut connection, &request, Permission::Decline).unwrap());
        });
        let mut viewer = transport::connect(TcpStream::connect(address).unwrap(), &secret).unwrap();
        viewer
            .send(&ClientMessage::Hello {
                version: VERSION,
                name: "test viewer".into(),
                request_control: false,
            })
            .unwrap();
        assert!(matches!(
            viewer.receive::<ServerMessage>().unwrap(),
            ServerMessage::Rejected { .. }
        ));
        host.join().unwrap();
    }
}
