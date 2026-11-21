//! Optional self-hosted rendezvous and byte relay.
//!
//! Both endpoints make outbound TCP connections, join the same room in opposite
//! roles, and then run the existing end-to-end Noise handshake through the
//! returned sockets. The relay never receives the pairing secret or decrypts
//! screen/input messages. It does see IP addresses, a stable hash of the pairing
//! secret, timing, and traffic volume. Knowledge of that hash can disrupt a
//! room, but cannot authenticate a Noise session. This small relay provides no
//! account authentication, public-service abuse controls, or production SLA.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

const MAGIC: &[u8; 8] = b"RDRLY001";
const ROOM_DOMAIN: &[u8] = b"rust-remote-desktop/relay-room/1\0";
const PREFACE_BYTES: usize = MAGIC.len() + 1 + 32;
const MAX_PENDING_ROOMS: usize = 64;
const MAX_ACTIVE_PAIRS: usize = 32;
const PAIRED: u8 = 0;
const DUPLICATE_ROLE: u8 = 1;
const CAPACITY_REACHED: u8 = 2;
const INVALID_REQUEST: u8 = 3;
const ROOM_EXPIRED: u8 = 4;
const RELAY_STOPPING: u8 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Host,
    Viewer,
}

impl Role {
    fn wire(self) -> u8 {
        match self {
            Self::Host => 0,
            Self::Viewer => 1,
        }
    }

    fn from_wire(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Host),
            1 => Ok(Self::Viewer),
            _ => bail!("Unknown relay role"),
        }
    }
}

fn room_id(secret: &str) -> Result<[u8; 32]> {
    ensure!(
        secret.len() == 64 && secret.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "The pairing secret must contain exactly 64 hexadecimal characters"
    );
    let mut decoded = [0_u8; 32];
    for (index, byte) in decoded.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&secret[index * 2..index * 2 + 2], 16)
            .context("Invalid pairing secret")?;
    }
    let mut digest = Sha256::new();
    digest.update(ROOM_DOMAIN);
    digest.update(decoded);
    Ok(digest.finalize().into())
}

fn send_join_request(stream: &mut TcpStream, role: Role, secret: &str) -> Result<()> {
    let room = room_id(secret)?;
    let mut preface = [0_u8; PREFACE_BYTES];
    preface[..MAGIC.len()].copy_from_slice(MAGIC);
    preface[MAGIC.len()] = role.wire();
    preface[MAGIC.len() + 1..].copy_from_slice(&room);
    stream
        .write_all(&preface)
        .context("Send relay join request")
}

/// Join the relay, waiting at most 90 seconds for the opposite role. On success,
/// all relay metadata has been consumed and the socket is ready for Noise.
/// A GUI may cancel the wait by shutting down a clone of the original socket.
pub fn join(mut stream: TcpStream, role: Role, secret: &str) -> Result<TcpStream> {
    stream
        .set_nodelay(true)
        .context("Configure relay connection")?;
    stream.set_read_timeout(Some(Duration::from_secs(90)))?;
    stream.set_write_timeout(Some(Duration::from_secs(90)))?;
    send_join_request(&mut stream, role, secret)?;
    let mut reply = [0_u8; 1];
    stream
        .read_exact(&mut reply)
        .context("Wait for the other relay peer")?;
    match reply[0] {
        PAIRED => Ok(stream),
        DUPLICATE_ROLE => bail!("A peer with this role is already waiting in the relay room"),
        CAPACITY_REACHED => {
            bail!("The relay is at capacity or this room already has an active session")
        }
        INVALID_REQUEST => bail!("The relay rejected the join request"),
        ROOM_EXPIRED => bail!("The relay room expired while waiting for the other peer"),
        RELAY_STOPPING => bail!("The relay is stopping"),
        _ => bail!("The relay returned an unknown response"),
    }
}

#[derive(Clone, Copy)]
struct Timeouts {
    handshake: Duration,
    pending: Duration,
    idle: Duration,
    poll: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            handshake: Duration::from_secs(5),
            pending: Duration::from_secs(60),
            idle: Duration::from_secs(60),
            poll: Duration::from_millis(20),
        }
    }
}

struct PendingPeer {
    stream: TcpStream,
    role: Role,
    expires: Instant,
}

struct ActivePair {
    room: [u8; 32],
    host: TcpStream,
    viewer: TcpStream,
    workers: [JoinHandle<()>; 2],
}

impl ActivePair {
    fn start(room: [u8; 32], host: TcpStream, viewer: TcpStream, idle: Duration) -> Result<Self> {
        for stream in [&host, &viewer] {
            stream.set_nodelay(true)?;
            stream.set_read_timeout(Some(idle))?;
            stream.set_write_timeout(Some(idle))?;
        }
        // Create every descriptor before spawning so a clone failure cannot
        // leave an unowned forwarding thread.
        let host_reader = host.try_clone()?;
        let viewer_writer = viewer.try_clone()?;
        let viewer_reader = viewer.try_clone()?;
        let host_writer = host.try_clone()?;
        let outgoing = thread::Builder::new()
            .name("relay-host-to-viewer".into())
            .spawn(move || forward(host_reader, viewer_writer))
            .context("Start relay forwarding")?;
        let incoming = match thread::Builder::new()
            .name("relay-viewer-to-host".into())
            .spawn(move || forward(viewer_reader, host_writer))
        {
            Ok(worker) => worker,
            Err(error) => {
                let _ = host.shutdown(Shutdown::Both);
                let _ = viewer.shutdown(Shutdown::Both);
                let _ = outgoing.join();
                return Err(error).context("Start relay return forwarding");
            }
        };
        Ok(Self {
            room,
            host,
            viewer,
            workers: [outgoing, incoming],
        })
    }

    fn finished(&self) -> bool {
        self.workers.iter().any(JoinHandle::is_finished)
    }

    fn close_and_join(self) -> Result<()> {
        let _ = self.host.shutdown(Shutdown::Both);
        let _ = self.viewer.shutdown(Shutdown::Both);
        let [outgoing, incoming] = self.workers;
        let outgoing_ok = outgoing.join().is_ok();
        let incoming_ok = incoming.join().is_ok();
        ensure!(
            outgoing_ok && incoming_ok,
            "A relay forwarding worker panicked"
        );
        Ok(())
    }
}

fn forward(mut source: TcpStream, mut destination: TcpStream) {
    // io::copy uses a bounded buffer. No application payload is parsed here.
    let _ = io::copy(&mut source, &mut destination);
    // EOF, idle timeout, or one-direction failure closes both halves and wakes
    // the other owned worker instead of retaining a half-dead active pair.
    let _ = source.shutdown(Shutdown::Both);
    let _ = destination.shutdown(Shutdown::Both);
}

fn acknowledge(stream: &mut TcpStream, status: u8) -> Result<()> {
    stream.write_all(&[status]).context("Send relay response")
}

fn expire_pending(pending: &mut HashMap<[u8; 32], PendingPeer>) {
    let now = Instant::now();
    let expired: Vec<_> = pending
        .iter()
        .filter_map(|(room, peer)| (peer.expires <= now).then_some(*room))
        .collect();
    for room in expired {
        if let Some(mut peer) = pending.remove(&room) {
            let _ = acknowledge(&mut peer.stream, ROOM_EXPIRED);
            let _ = peer.stream.shutdown(Shutdown::Both);
        }
    }
}

fn read_preface(
    stream: &mut TcpStream,
    stop: &AtomicBool,
    timeout: Duration,
) -> Result<(Role, [u8; 32])> {
    let deadline = Instant::now() + timeout;
    let mut preface = [0_u8; PREFACE_BYTES];
    let mut received = 0;
    stream.set_write_timeout(Some(timeout))?;
    while received < preface.len() {
        ensure!(!stop.load(Ordering::Acquire), "Relay is stopping");
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "Relay join request timed out");
        stream.set_read_timeout(Some(remaining.min(Duration::from_millis(50))))?;
        match stream.read(&mut preface[received..]) {
            Ok(0) => bail!("Relay peer closed before joining"),
            Ok(length) => received += length,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error).context("Read relay join request"),
        }
    }
    ensure!(
        &preface[..MAGIC.len()] == MAGIC,
        "Invalid relay protocol magic"
    );
    let role = Role::from_wire(preface[MAGIC.len()])?;
    let room = preface[MAGIC.len() + 1..]
        .try_into()
        .expect("fixed-size room digest");
    Ok((role, room))
}

/// Serve only on the caller-supplied listener. Stop closes pending and active
/// sockets and joins every forwarding worker before this function returns.
pub fn run(listener: TcpListener, stop: Arc<AtomicBool>) -> Result<()> {
    run_with_timeouts(listener, stop, Timeouts::default())
}

fn run_with_timeouts(
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    timeouts: Timeouts,
) -> Result<()> {
    listener
        .set_nonblocking(true)
        .context("Configure relay listener")?;
    let mut pending = HashMap::<[u8; 32], PendingPeer>::new();
    let mut active = Vec::<ActivePair>::new();
    let result = relay_loop(&listener, &stop, timeouts, &mut pending, &mut active);

    // Keep cleanup outside the fallible admission loop so every return path
    // closes its resources, including listener failures and worker failures.
    for (_, mut peer) in pending.drain() {
        let _ = acknowledge(&mut peer.stream, RELAY_STOPPING);
        let _ = peer.stream.shutdown(Shutdown::Both);
    }
    let mut cleanup_error = None;
    for pair in active.drain(..) {
        if let Err(error) = pair.close_and_join() {
            cleanup_error.get_or_insert(error);
        }
    }
    result?;
    if let Some(error) = cleanup_error {
        return Err(error);
    }
    Ok(())
}

fn relay_loop(
    listener: &TcpListener,
    stop: &AtomicBool,
    timeouts: Timeouts,
    pending: &mut HashMap<[u8; 32], PendingPeer>,
    active: &mut Vec<ActivePair>,
) -> Result<()> {
    while !stop.load(Ordering::Acquire) {
        expire_pending(pending);
        let mut index = 0;
        while index < active.len() {
            if active[index].finished() {
                active.swap_remove(index).close_and_join()?;
            } else {
                index += 1;
            }
        }

        let mut stream = match listener.accept() {
            Ok((stream, _address)) => stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(timeouts.poll);
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("Accept relay connection"),
        };
        let (role, room) = match read_preface(&mut stream, stop, timeouts.handshake) {
            Ok(request) => request,
            Err(_) => {
                let _ = acknowledge(&mut stream, INVALID_REQUEST);
                let _ = stream.shutdown(Shutdown::Both);
                continue;
            }
        };
        // A slow preface may span a pending peer's deadline. Recheck before
        // pairing so that an expired room cannot be revived by that request.
        expire_pending(pending);
        if active.iter().any(|pair| pair.room == room) {
            let _ = acknowledge(&mut stream, CAPACITY_REACHED);
        } else if let Some(waiting) = pending.get(&room) {
            if waiting.role == role {
                // Never replace a rightful peer just because a duplicate joins.
                let _ = acknowledge(&mut stream, DUPLICATE_ROLE);
            } else if active.len() >= MAX_ACTIVE_PAIRS {
                let _ = acknowledge(&mut stream, CAPACITY_REACHED);
            } else {
                let mut waiting = pending.remove(&room).expect("room found above");
                if acknowledge(&mut waiting.stream, PAIRED).is_err()
                    || acknowledge(&mut stream, PAIRED).is_err()
                {
                    let _ = waiting.stream.shutdown(Shutdown::Both);
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                let (host, viewer) = if role == Role::Host {
                    (stream, waiting.stream)
                } else {
                    (waiting.stream, stream)
                };
                active.push(ActivePair::start(room, host, viewer, timeouts.idle)?);
            }
        } else if pending.len() >= MAX_PENDING_ROOMS {
            let _ = acknowledge(&mut stream, CAPACITY_REACHED);
        } else {
            pending.insert(
                room,
                PendingPeer {
                    stream,
                    role,
                    expires: Instant::now() + timeouts.pending,
                },
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport;
    use std::net::SocketAddr;

    struct Harness {
        address: SocketAddr,
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<Result<()>>>,
    }

    impl Harness {
        fn start(timeouts: Timeouts) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let server_stop = stop.clone();
            let worker = thread::spawn(move || run_with_timeouts(listener, server_stop, timeouts));
            Self {
                address,
                stop,
                worker: Some(worker),
            }
        }

        fn stop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap().unwrap();
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn test_timeouts() -> Timeouts {
        Timeouts {
            handshake: Duration::from_secs(2),
            pending: Duration::from_secs(2),
            idle: Duration::from_secs(2),
            poll: Duration::from_millis(2),
        }
    }

    #[test]
    fn two_outbound_connections_keep_noise_end_to_end() {
        let mut harness = Harness::start(test_timeouts());
        let secret = transport::new_pairing_secret().unwrap();
        let host_secret = secret.clone();
        let address = harness.address;
        let host = thread::spawn(move || {
            let socket = join(
                TcpStream::connect(address).unwrap(),
                Role::Host,
                &host_secret,
            )
            .unwrap();
            let mut encrypted = transport::accept(socket, &host_secret).unwrap();
            let payload: Vec<u8> = encrypted.receive().unwrap();
            encrypted.send(&payload).unwrap();
        });
        let socket = join(TcpStream::connect(address).unwrap(), Role::Viewer, &secret).unwrap();
        let mut encrypted = transport::connect(socket, &secret).unwrap();
        let payload = vec![42_u8; 90_000];
        encrypted.send(&payload).unwrap();
        assert_eq!(encrypted.receive::<Vec<u8>>().unwrap(), payload);
        host.join().unwrap();
        harness.stop();
    }

    #[test]
    fn duplicate_host_is_rejected_and_original_peer_is_preserved() {
        let mut harness = Harness::start(test_timeouts());
        let secret = transport::new_pairing_secret().unwrap();
        let mut original = TcpStream::connect(harness.address).unwrap();
        send_join_request(&mut original, Role::Host, &secret).unwrap();
        let error = join(
            TcpStream::connect(harness.address).unwrap(),
            Role::Host,
            &secret,
        )
        .unwrap_err();
        assert!(error.to_string().contains("already waiting"));
        let mut viewer = join(
            TcpStream::connect(harness.address).unwrap(),
            Role::Viewer,
            &secret,
        )
        .unwrap();
        let mut ack = [255_u8; 1];
        original.read_exact(&mut ack).unwrap();
        assert_eq!(ack, [PAIRED]);
        original.write_all(b"x").unwrap();
        viewer.read_exact(&mut ack).unwrap();
        assert_eq!(ack, [b'x']);
        harness.stop();
    }

    #[test]
    fn different_rooms_never_pair_and_pending_requests_expire() {
        let timeouts = Timeouts {
            pending: Duration::from_millis(100),
            ..test_timeouts()
        };
        let mut harness = Harness::start(timeouts);
        let address = harness.address;
        let host = thread::spawn(move || {
            let result = join(
                TcpStream::connect(address).unwrap(),
                Role::Host,
                &"00".repeat(32),
            );
            assert!(result.unwrap_err().to_string().contains("expired"));
        });
        let result = join(
            TcpStream::connect(address).unwrap(),
            Role::Viewer,
            &"01".repeat(32),
        );
        assert!(result.unwrap_err().to_string().contains("expired"));
        host.join().unwrap();
        harness.stop();
    }

    #[test]
    fn slow_preface_cannot_revive_an_expired_waiting_peer() {
        let timeouts = Timeouts {
            pending: Duration::from_millis(100),
            ..test_timeouts()
        };
        let mut harness = Harness::start(timeouts);
        let secret = transport::new_pairing_secret().unwrap();
        let mut host = TcpStream::connect(harness.address).unwrap();
        host.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        send_join_request(&mut host, Role::Host, &secret).unwrap();
        let duplicate = join(
            TcpStream::connect(harness.address).unwrap(),
            Role::Host,
            &secret,
        );
        assert!(duplicate.is_err());
        let mut viewer = TcpStream::connect(harness.address).unwrap();
        viewer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        viewer.write_all(&MAGIC[..3]).unwrap();
        thread::sleep(Duration::from_millis(150));
        viewer.write_all(&MAGIC[3..]).unwrap();
        viewer.write_all(&[Role::Viewer.wire()]).unwrap();
        viewer.write_all(&room_id(&secret).unwrap()).unwrap();
        let mut reply = [255_u8; 1];
        host.read_exact(&mut reply).unwrap();
        assert_eq!(reply, [ROOM_EXPIRED]);
        viewer.read_exact(&mut reply).unwrap();
        assert_eq!(reply, [ROOM_EXPIRED]);
        harness.stop();
    }

    #[test]
    fn stopping_relay_closes_active_sockets_and_joins_workers() {
        let mut harness = Harness::start(test_timeouts());
        let address = harness.address;
        let secret = transport::new_pairing_secret().unwrap();
        let host_secret = secret.clone();
        let host = thread::spawn(move || {
            join(
                TcpStream::connect(address).unwrap(),
                Role::Host,
                &host_secret,
            )
            .unwrap()
        });
        let mut viewer = join(TcpStream::connect(address).unwrap(), Role::Viewer, &secret).unwrap();
        let mut host = host.join().unwrap();
        harness.stop();
        for socket in [&mut viewer, &mut host] {
            socket
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let result = socket.read(&mut [0_u8; 1]);
            assert!(
                matches!(result, Ok(0))
                    || result.as_ref().is_err_and(|error| matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                    ))
            );
        }
    }

    #[test]
    fn stopping_relay_interrupts_incomplete_preface() {
        let mut harness = Harness::start(test_timeouts());
        let mut socket = TcpStream::connect(harness.address).unwrap();
        socket.write_all(&MAGIC[..3]).unwrap();
        // Wait until the socket has been accepted, then stop while its declared
        // join request is still incomplete. Cleanup must not wait for 5 seconds.
        thread::sleep(Duration::from_millis(20));
        let before = Instant::now();
        harness.stop();
        assert!(before.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn room_derivation_is_case_insensitive_and_does_not_echo_secrets() {
        assert_eq!(
            room_id(&"ab".repeat(32)).unwrap(),
            room_id(&"AB".repeat(32)).unwrap()
        );
        assert_ne!(room_id(&"ab".repeat(32)).unwrap(), [0xab_u8; 32]);
        let invalid = "this-is-private-invalid-input";
        assert!(!room_id(invalid).unwrap_err().to_string().contains(invalid));
    }
}
