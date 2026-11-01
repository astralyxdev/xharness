//! How the app reaches a server: a Unix socket on this machine, or TCP to
//! another machine, encrypted and authenticated with the server's key.
//!
//! The key is a shared secret the server makes (and prints); the app is
//! given it as a [`ServerKey`], which also says where the server is. Each
//! connection starts with a Noise handshake (`NNpsk0`: both sides prove
//! they have the key, and agree on fresh keys for this connection, so one
//! recorded connection can't be decrypted later). After it, every byte is
//! encrypted and authenticated (ChaCha20-Poly1305). A wrong key fails the
//! handshake's first message.

use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpStream, ToSocketAddrs},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use snow::{Builder, StatelessTransportState};

use crate::services::server::UnixStream;

const NOISE: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
/// Mixed into the handshake: an app and server of different protocols never
/// agree.
const PROLOGUE: &[u8] = b"xharness protocol 2";
const KEY_PREFIX: &str = "xhk1_";
const MAX_MESSAGE: usize = 65535;
const TAG: usize = 16;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

// Keys -----------------------------------------------------------------------------

/// Everything the app needs to reach a remote server: where it is, and its
/// secret. The server prints it as one string (`xhk1_…`) to paste into the
/// app.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerKey {
    /// What to call the server (its machine's name).
    pub name: String,
    pub host: String,
    pub port: u16,
    /// The shared secret: 32 bytes, base64url.
    pub secret: String,
}

impl ServerKey {
    /// A key with a new random secret.
    pub fn generate(name: &str, host: &str, port: u16) -> Result<Self, String> {
        let builder = Builder::new(NOISE.parse().map_err(|e| format!("{e:?}"))?);
        let random = builder.generate_keypair().map_err(|e| format!("{e:?}"))?.private;
        Ok(Self {
            name: name.to_string(),
            host: host.to_string(),
            port,
            secret: base64::encode(&random[..32]),
        })
    }

    /// The string to paste: `xhk1_` and the key as base64url JSON.
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("a key serializes");
        format!("{KEY_PREFIX}{}", base64::encode(&json))
    }

    /// Reads a pasted key (spaces and line breaks are ignored).
    pub fn decode(text: &str) -> Result<Self, String> {
        let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let body = text
            .strip_prefix(KEY_PREFIX)
            .ok_or("That isn't an XHarness server key (they start with xhk1_).")?;
        let json = base64::decode(body).ok_or("This key is damaged: copy it again.")?;
        let key: ServerKey = serde_json::from_slice(&json).map_err(|_| "This key is damaged: copy it again.")?;
        key.secret_bytes()?;
        Ok(key)
    }

    /// `host:port`.
    pub fn address(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    fn secret_bytes(&self) -> Result<[u8; 32], String> {
        base64::decode(&self.secret)
            .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
            .ok_or_else(|| "This key's secret is damaged: copy it again.".to_string())
    }
}

// Connections ------------------------------------------------------------------------

/// A connection to a server, local or remote.
pub enum Stream {
    Local(UnixStream),
    Remote(SecureStream),
}

impl Stream {
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(match self {
            Stream::Local(stream) => Stream::Local(stream.try_clone()?),
            Stream::Remote(stream) => Stream::Remote(stream.try_clone()?),
        })
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Stream::Local(stream) => stream.shutdown(how),
            Stream::Remote(stream) => stream.raw.shutdown(how),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Local(stream) => stream.read(buf),
            Stream::Remote(stream) => stream.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Local(stream) => stream.write(buf),
            Stream::Remote(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Local(stream) => stream.flush(),
            Stream::Remote(stream) => stream.flush(),
        }
    }
}

struct Sending {
    stream: TcpStream,
    nonce: u64,
}

struct Receiving {
    stream: TcpStream,
    nonce: u64,
    plain: Vec<u8>,
    at: usize,
}

/// A TCP connection after the handshake: what's written is encrypted, what's
/// read is decrypted and checked. Clones share it (one sends, another
/// receives), like a socket's.
pub struct SecureStream {
    state: Arc<StatelessTransportState>,
    sending: Arc<Mutex<Sending>>,
    receiving: Arc<Mutex<Receiving>>,
    raw: TcpStream,
}

impl SecureStream {
    fn new(stream: TcpStream, state: StatelessTransportState) -> io::Result<Self> {
        Ok(Self {
            state: Arc::new(state),
            sending: Arc::new(Mutex::new(Sending { stream: stream.try_clone()?, nonce: 0 })),
            receiving: Arc::new(Mutex::new(Receiving {
                stream: stream.try_clone()?,
                nonce: 0,
                plain: Vec::new(),
                at: 0,
            })),
            raw: stream,
        })
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            state: self.state.clone(),
            sending: self.sending.clone(),
            receiving: self.receiving.clone(),
            raw: self.raw.try_clone()?,
        })
    }
}

impl Read for SecureStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut receiving = self.receiving.lock().unwrap();
        while receiving.at >= receiving.plain.len() {
            let cipher = read_message(&mut receiving.stream)?;
            let mut plain = vec![0; cipher.len()];
            let len = self
                .state
                .read_message(receiving.nonce, &cipher, &mut plain)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "a message failed its check"))?;
            plain.truncate(len);
            receiving.nonce += 1;
            receiving.plain = plain;
            receiving.at = 0;
        }
        let n = buf.len().min(receiving.plain.len() - receiving.at);
        let at = receiving.at;
        buf[..n].copy_from_slice(&receiving.plain[at..at + n]);
        receiving.at += n;
        Ok(n)
    }
}

impl Write for SecureStream {
    /// Writes all of `buf` (in as many messages as it takes), so a frame
    /// written at once is never split by another thread's.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut sending = self.sending.lock().unwrap();
        let mut out = vec![0; MAX_MESSAGE];
        let mut bytes = Vec::with_capacity(buf.len() + buf.len() / (MAX_MESSAGE - TAG) * (TAG + 2) + TAG + 2);
        for chunk in buf.chunks(MAX_MESSAGE - TAG) {
            let len = self
                .state
                .write_message(sending.nonce, chunk, &mut out)
                .map_err(|e| io::Error::other(format!("{e:?}")))?;
            sending.nonce += 1;
            bytes.extend_from_slice(&(len as u16).to_be_bytes());
            bytes.extend_from_slice(&out[..len]);
        }
        sending.stream.write_all(&bytes)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sending.lock().unwrap().stream.flush()
    }
}

fn write_message(stream: &mut TcpStream, message: &[u8]) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(2 + message.len());
    bytes.extend_from_slice(&(message.len() as u16).to_be_bytes());
    bytes.extend_from_slice(message);
    stream.write_all(&bytes)
}

fn read_message(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len)?;
    let mut message = vec![0; u16::from_be_bytes(len) as usize];
    stream.read_exact(&mut message)?;
    Ok(message)
}

fn noise_error(error: snow::Error) -> io::Error {
    io::Error::other(format!("{error:?}"))
}

/// Connects to the server `key` describes and proves we have its key.
pub fn connect(key: &ServerKey) -> io::Result<SecureStream> {
    let secret = key.secret_bytes().map_err(io::Error::other)?;
    let address = key
        .address()
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("can't find {}", key.host)))?;
    let mut stream = TcpStream::connect_timeout(&address, HANDSHAKE_TIMEOUT)?;
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let mut handshake = Builder::new(NOISE.parse().map_err(noise_error)?)
        .prologue(PROLOGUE)
        .and_then(|b| b.psk(0, &secret))
        .and_then(|b| b.build_initiator())
        .map_err(noise_error)?;
    let mut buffer = vec![0u8; 1024];
    let len = handshake.write_message(&[], &mut buffer).map_err(noise_error)?;
    write_message(&mut stream, &buffer[..len])?;
    let reply = read_message(&mut stream)
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "the server didn't accept this key"))?;
    handshake
        .read_message(&reply, &mut buffer)
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "the server didn't accept this key"))?;
    stream.set_read_timeout(None)?;
    SecureStream::new(stream, handshake.into_stateless_transport_mode().map_err(noise_error)?)
}

/// The server's side of [`connect`]: fails unless the app has `secret`.
pub fn accept(mut stream: TcpStream, key: &ServerKey) -> io::Result<SecureStream> {
    let secret = key.secret_bytes().map_err(io::Error::other)?;
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let mut handshake = Builder::new(NOISE.parse().map_err(noise_error)?)
        .prologue(PROLOGUE)
        .and_then(|b| b.psk(0, &secret))
        .and_then(|b| b.build_responder())
        .map_err(noise_error)?;
    let mut buffer = vec![0u8; 1024];
    let first = read_message(&mut stream)?;
    handshake
        .read_message(&first, &mut buffer)
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "a connection with the wrong key"))?;
    let len = handshake.write_message(&[], &mut buffer).map_err(noise_error)?;
    write_message(&mut stream, &buffer[..len])?;
    stream.set_read_timeout(None)?;
    SecureStream::new(stream, handshake.into_stateless_transport_mode().map_err(noise_error)?)
}

/// base64url without padding (RFC 4648 §5), for keys.
mod base64 {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 4 / 3 + 3);
        for chunk in bytes.chunks(3) {
            let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
            for i in 0..=chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        out
    }

    pub fn decode(text: &str) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(text.len() * 3 / 4);
        let (mut bits, mut count) = (0u32, 0u32);
        for c in text.bytes() {
            let value = ALPHABET.iter().position(|&a| a == c)? as u32;
            bits = bits << 6 | value;
            count += 6;
            if count >= 8 {
                count -= 8;
                out.push((bits >> count) as u8);
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn keys_round_trip_through_their_string() {
        assert_eq!(base64::decode(&base64::encode(b"any bytes \xff\x00")).unwrap(), b"any bytes \xff\x00");
        let key = ServerKey::generate("build-box", "10.0.0.7", 38888).unwrap();
        let text = key.encode();
        assert!(text.starts_with("xhk1_"));
        // Pasted with a line break in it.
        let pasted = format!("  {}\n{}  ", &text[..20], &text[20..]);
        assert_eq!(ServerKey::decode(&pasted).unwrap(), key);
        assert_eq!(key.address(), "10.0.0.7:38888");
        assert!(ServerKey::decode("hello").is_err());
        assert!(ServerKey::decode("xhk1_damaged!").is_err());
        assert_ne!(ServerKey::generate("a", "h", 1).unwrap().secret, key.secret, "secrets are random");
    }

    /// Both ends talk over the encrypted stream; the wrong key is turned
    /// away at the handshake.
    #[test]
    fn only_the_right_key_gets_a_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let key = ServerKey::generate("test", "127.0.0.1", port).unwrap();
        let server_key = key.clone();
        let server = std::thread::spawn(move || {
            // The right key: echo, uppercased (with a message bigger than
            // one Noise message).
            let (stream, _) = listener.accept().unwrap();
            let mut secure = accept(stream, &server_key).unwrap();
            let mut buffer = vec![0u8; 200_000];
            secure.read_exact(&mut buffer).unwrap();
            secure.write_all(&buffer.to_ascii_uppercase()).unwrap();
            // The wrong key.
            let (stream, _) = listener.accept().unwrap();
            accept(stream, &server_key).is_err()
        });

        let mut client = connect(&key).unwrap();
        let message: Vec<u8> = (0..200_000).map(|i| b"abcdefgh"[i % 8]).collect();
        let mut writer = client.try_clone().unwrap();
        writer.write_all(&message).unwrap();
        let mut reply = vec![0u8; message.len()];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(reply, message.to_ascii_uppercase());

        let wrong = ServerKey { secret: ServerKey::generate("x", "127.0.0.1", port).unwrap().secret, ..key };
        assert!(connect(&wrong).is_err());
        assert!(server.join().unwrap(), "the server turned the wrong key away");
    }
}
