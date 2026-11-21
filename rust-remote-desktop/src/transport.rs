//! Authenticated, encrypted messages for our own remote desktop protocol.
//!
//! A fresh Noise NNpsk0 handshake authenticates possession of an out-of-band,
//! 256-bit pairing secret and derives ephemeral X25519 transport keys. The
//! pairing secret is never sent over the connection. This is not AnyDesk's
//! protocol. Every application message starts with an encrypted length record;
//! subsequent encrypted records carry its serialized bytes in order. Any
//! framing, authentication, decoding, or I/O failure closes the connection.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Serialize, de::DeserializeOwned};
use snow::{Builder, HandshakeState, TransportState, params::NoiseParams};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

const NOISE_PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"rust-remote-desktop/transport/1";
const PSK_BYTES: usize = 32;
const AUTH_TAG_BYTES: usize = 16;
const MAX_PLAINTEXT_RECORD: usize = 60 * 1024;
const MAX_CIPHERTEXT_RECORD: usize = MAX_PLAINTEXT_RECORD + AUTH_TAG_BYTES;
const MAX_HANDSHAKE_RECORD: usize = 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(8);
const CONSENT_TIMEOUT: Duration = Duration::from_secs(90);

/// Generate a 256-bit pairing secret, represented as 64 hexadecimal characters.
/// Share this secret with the intended controller through a trusted channel.
pub fn new_pairing_secret() -> Result<String> {
    let mut bytes = [0_u8; PSK_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow!("Could not obtain secure operating-system randomness: {error}"))?;
    let mut encoded = String::with_capacity(PSK_BYTES * 2);
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(encoded, "{byte:02x}");
    }
    Ok(encoded)
}

fn parse_secret(secret: &str) -> Result<[u8; PSK_BYTES]> {
    ensure!(
        secret.len() == PSK_BYTES * 2 && secret.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "The pairing secret must contain exactly 64 hexadecimal characters"
    );
    let mut bytes = [0_u8; PSK_BYTES];
    for (index, chunk) in secret.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let nibble = |byte: u8| match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => unreachable!("ASCII hexadecimal was checked above"),
        };
        bytes[index] = (nibble(chunk[0]) << 4) | nibble(chunk[1]);
    }
    Ok(bytes)
}

/// Negotiate the host side. No application message is admitted before the
/// controller proves possession of the pairing secret.
pub fn accept(stream: TcpStream, secret: &str) -> Result<EncryptedConnection> {
    negotiate(stream, secret, false)
}

/// Negotiate the controller side and authenticate the host's possession of the
/// same out-of-band secret. There is no unencrypted fallback.
pub fn connect(stream: TcpStream, secret: &str) -> Result<EncryptedConnection> {
    negotiate(stream, secret, true)
}

fn negotiate(mut stream: TcpStream, secret: &str, initiator: bool) -> Result<EncryptedConnection> {
    let secret = parse_secret(secret)?;
    stream.set_nodelay(true).context("Configure connection")?;
    set_stream_timeout(&stream, HANDSHAKE_TIMEOUT)?;
    let parameters: NoiseParams = NOISE_PATTERN.parse().context("Parse Noise protocol")?;
    let builder = Builder::new(parameters)
        .prologue(PROLOGUE)
        .context("Set Noise application identity")?
        .psk(0, &secret)
        .context("Set Noise pairing secret")?;
    let mut handshake = if initiator {
        builder.build_initiator()
    } else {
        builder.build_responder()
    }
    .context("Create Noise handshake")?;

    let mut incoming = Vec::new();
    if initiator {
        write_handshake(&mut stream, &mut handshake)?;
        read_handshake(&mut stream, &mut handshake, &mut incoming)?;
    } else {
        read_handshake(&mut stream, &mut handshake, &mut incoming)?;
        write_handshake(&mut stream, &mut handshake)?;
    }
    ensure!(
        handshake.is_handshake_finished(),
        "Incomplete authenticated handshake"
    );

    let connection = EncryptedConnection {
        stream,
        noise: handshake
            .into_transport_mode()
            .context("Complete Noise handshake")?,
        outgoing: vec![0_u8; MAX_CIPHERTEXT_RECORD],
        incoming: Vec::new(),
        plaintext: vec![0_u8; MAX_PLAINTEXT_RECORD],
        closed: false,
    };
    // The GUI may need time to approve the authenticated incoming request.
    connection.set_timeout(CONSENT_TIMEOUT)?;
    Ok(connection)
}

fn write_handshake(stream: &mut TcpStream, handshake: &mut HandshakeState) -> Result<()> {
    let mut message = [0_u8; MAX_HANDSHAKE_RECORD];
    let length = handshake
        .write_message(&[], &mut message)
        .context("Create authenticated handshake message")?;
    write_wire_record(stream, &message[..length])
}

fn read_handshake(
    stream: &mut TcpStream,
    handshake: &mut HandshakeState,
    incoming: &mut Vec<u8>,
) -> Result<()> {
    let length = read_wire_record(stream, incoming, MAX_HANDSHAKE_RECORD)?;
    let mut payload = [0_u8; MAX_HANDSHAKE_RECORD];
    let payload_length = handshake
        .read_message(&incoming[..length], &mut payload)
        .context("Pairing authentication failed")?;
    ensure!(payload_length == 0, "Unexpected handshake payload");
    Ok(())
}

fn set_stream_timeout(stream: &TcpStream, timeout: Duration) -> Result<()> {
    ensure!(!timeout.is_zero(), "Connection timeout must be nonzero");
    stream
        .set_read_timeout(Some(timeout))
        .context("Set receive timeout")?;
    stream
        .set_write_timeout(Some(timeout))
        .context("Set send timeout")?;
    Ok(())
}

// Outer record lengths delimit AEAD ciphertext, but are never trusted as an
// application-message length. Changing a boundary invalidates its AEAD tag.
fn write_wire_record(stream: &mut TcpStream, ciphertext: &[u8]) -> Result<()> {
    let length = u16::try_from(ciphertext.len()).context("Encrypted record is too large")?;
    stream
        .write_all(&length.to_be_bytes())
        .context("Send encrypted record header")?;
    stream
        .write_all(ciphertext)
        .context("Send encrypted record")?;
    Ok(())
}

fn read_wire_record(stream: &mut TcpStream, buffer: &mut Vec<u8>, limit: usize) -> Result<usize> {
    let mut header = [0_u8; 2];
    stream
        .read_exact(&mut header)
        .context("Receive encrypted record header")?;
    let length = usize::from(u16::from_be_bytes(header));
    ensure!(
        (AUTH_TAG_BYTES..=limit).contains(&length),
        "Encrypted record length is outside the supported range"
    );
    // Validate the declared size before resizing or reading the record buffer.
    buffer.resize(length, 0);
    stream
        .read_exact(buffer)
        .context("Receive encrypted record")?;
    Ok(length)
}

/// One ordered, bidirectional Noise session. It owns its socket and cipher
/// states and must be driven by a single connection worker.
pub struct EncryptedConnection {
    stream: TcpStream,
    noise: TransportState,
    outgoing: Vec<u8>,
    incoming: Vec<u8>,
    plaintext: Vec<u8>,
    closed: bool,
}

impl EncryptedConnection {
    pub fn send<T: Serialize>(&mut self, value: &T) -> Result<()> {
        self.ensure_open()?;
        let result = (|| {
            let bytes = postcard::to_allocvec(value).context("Serialize remote desktop message")?;
            ensure!(
                bytes.len() <= MAX_MESSAGE_BYTES,
                "Remote desktop message exceeds 16 MiB"
            );
            let length =
                u32::try_from(bytes.len()).context("Message length cannot be represented")?;
            self.write_encrypted_record(&length.to_be_bytes())?;
            for chunk in bytes.chunks(MAX_PLAINTEXT_RECORD) {
                self.write_encrypted_record(chunk)?;
            }
            Ok(())
        })();
        self.close_on_error(result)
    }

    pub fn receive<T: DeserializeOwned>(&mut self) -> Result<T> {
        self.ensure_open()?;
        let result = (|| {
            let header_length = self.read_encrypted_record()?;
            ensure!(header_length == 4, "Invalid encrypted message header");
            let length =
                u32::from_be_bytes(self.plaintext[..4].try_into().expect("four-byte header"));
            let length = usize::try_from(length).context("Message length cannot be represented")?;
            // This check precedes allocation of the aggregate message buffer.
            ensure!(
                length <= MAX_MESSAGE_BYTES,
                "Remote desktop message exceeds 16 MiB"
            );
            let mut bytes = Vec::with_capacity(length);
            while bytes.len() < length {
                let chunk_length = self.read_encrypted_record()?;
                ensure!(
                    chunk_length > 0 && chunk_length <= length - bytes.len(),
                    "Encrypted message has an invalid fragment length"
                );
                bytes.extend_from_slice(&self.plaintext[..chunk_length]);
            }
            let (value, trailing) =
                postcard::take_from_bytes(&bytes).context("Decode remote desktop message")?;
            ensure!(
                trailing.is_empty(),
                "Remote desktop message has trailing data"
            );
            Ok(value)
        })();
        self.close_on_error(result)
    }

    /// Set both read and write deadlines. The application should shorten the
    /// initial consent deadline after the host approves a session.
    pub fn set_timeout(&self, timeout: Duration) -> Result<()> {
        set_stream_timeout(&self.stream, timeout)
    }

    /// A socket clone for a GUI stop button. Calling shutdown on the clone
    /// immediately interrupts a blocked worker without sharing cipher state.
    pub fn shutdown_handle(&self) -> std::io::Result<TcpStream> {
        self.stream.try_clone()
    }

    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed {
            bail!("The authenticated connection is closed");
        }
        Ok(())
    }

    fn close_on_error<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.closed = true;
            self.shutdown();
        }
        result
    }

    fn write_encrypted_record(&mut self, plaintext: &[u8]) -> Result<()> {
        ensure!(
            plaintext.len() <= MAX_PLAINTEXT_RECORD,
            "Plaintext record is too large"
        );
        let length = self
            .noise
            .write_message(plaintext, &mut self.outgoing)
            .context("Encrypt remote desktop record")?;
        write_wire_record(&mut self.stream, &self.outgoing[..length])
    }

    fn read_encrypted_record(&mut self) -> Result<usize> {
        let length = read_wire_record(&mut self.stream, &mut self.incoming, MAX_CIPHERTEXT_RECORD)?;
        self.noise
            .read_message(&self.incoming[..length], &mut self.plaintext)
            .context("Encrypted record authentication failed")
    }
}

impl Drop for EncryptedConnection {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn connected_pair() -> (EncryptedConnection, EncryptedConnection) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let secret = new_pairing_secret().unwrap();
        let server_secret = secret.clone();
        let server = thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            accept(socket, &server_secret).unwrap()
        });
        let controller = connect(TcpStream::connect(address).unwrap(), &secret).unwrap();
        let host = server.join().unwrap();
        (controller, host)
    }

    #[test]
    fn large_message_roundtrips_across_multiple_authenticated_records() {
        let (mut controller, mut host) = connected_pair();
        let payload: Vec<u8> = (0..(MAX_PLAINTEXT_RECORD * 3 + 17))
            .map(|value| value as u8)
            .collect();
        let expected = payload.clone();
        let server = thread::spawn(move || {
            let received: Vec<u8> = host.receive().unwrap();
            assert_eq!(received, expected);
            host.send(&received).unwrap();
        });
        controller.send(&payload).unwrap();
        let echoed: Vec<u8> = controller.receive().unwrap();
        assert_eq!(echoed, payload);
        server.join().unwrap();
    }

    #[test]
    fn mismatched_pairing_secrets_cannot_establish_a_session() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            assert!(accept(socket, &"00".repeat(32)).is_err());
        });
        assert!(connect(TcpStream::connect(address).unwrap(), &"01".repeat(32)).is_err());
        server.join().unwrap();
    }

    #[test]
    fn corrupted_ciphertext_closes_the_connection() {
        let (mut controller, mut host) = connected_pair();
        let serialized = postcard::to_allocvec(&"hello").unwrap();
        controller
            .write_encrypted_record(&(serialized.len() as u32).to_be_bytes())
            .unwrap();
        let length = controller
            .noise
            .write_message(&serialized, &mut controller.outgoing)
            .unwrap();
        controller.outgoing[0] ^= 0x80;
        write_wire_record(&mut controller.stream, &controller.outgoing[..length]).unwrap();
        assert!(host.receive::<String>().is_err());
        assert!(host.closed);
        assert!(host.receive::<String>().is_err());
    }

    #[test]
    fn oversized_authenticated_message_header_is_rejected_before_buffer_allocation() {
        let (mut controller, mut host) = connected_pair();
        controller
            .write_encrypted_record(&((MAX_MESSAGE_BYTES + 1) as u32).to_be_bytes())
            .unwrap();
        let error = host.receive::<Vec<u8>>().unwrap_err();
        assert!(error.to_string().contains("exceeds 16 MiB"));
        assert_eq!(host.incoming.len(), 4 + AUTH_TAG_BYTES);
        assert!(host.closed);
    }

    #[test]
    fn oversized_ciphertext_record_is_rejected_before_record_allocation() {
        let (mut controller, mut host) = connected_pair();
        controller
            .stream
            .write_all(&u16::MAX.to_be_bytes())
            .unwrap();
        assert!(host.receive::<Vec<u8>>().is_err());
        assert_eq!(host.incoming.capacity(), 0);
        assert!(host.closed);
    }

    #[test]
    fn truncated_message_is_never_delivered() {
        let (mut controller, mut host) = connected_pair();
        controller
            .write_encrypted_record(&100_u32.to_be_bytes())
            .unwrap();
        controller.write_encrypted_record(&[1, 2, 3]).unwrap();
        controller.shutdown();
        assert!(host.receive::<Vec<u8>>().is_err());
        assert!(host.closed);
    }

    #[test]
    fn secret_validation_does_not_echo_secret_input() {
        let invalid = "private-invalid-secret";
        let error = parse_secret(invalid).unwrap_err();
        assert!(!error.to_string().contains(invalid));
        assert!(parse_secret(&"Aa".repeat(32)).is_ok());
        let first = new_pairing_secret().unwrap();
        let second = new_pairing_secret().unwrap();
        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
    }
}
