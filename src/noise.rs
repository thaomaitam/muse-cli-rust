//! Noise XX transport (writer B).
//!
//! Mirrors Python `gateway.py`: pattern `Noise_XX_25519_AESGCM_SHA256`,
//! initiator role, a fresh random 32-byte static key per connection, and
//! empty payloads for all three handshake messages. The three-message
//! handshake sequence itself lives in `gateway.rs`, using this API.
//!
//! No `unsafe` is used anywhere here; key material is held in ordinary
//! owned buffers and dropped with the session.

use rand::{rngs::OsRng, RngCore};
use snow::{Builder, HandshakeState, TransportState};

use crate::errors::CliError;

const PATTERN: &str = "Noise_XX_25519_AESGCM_SHA256";

enum NoiseState {
    Handshake(HandshakeState),
    Transport(TransportState),
}

/// One Noise session. Created as initiator by [`NoiseSession::new_initiator`];
/// [`NoiseSession::new_responder`] exists only for loopback tests (the
/// Python client is always the initiator against the real server).
pub struct NoiseSession {
    state: Option<NoiseState>,
}

fn noise_error(err: snow::Error) -> CliError {
    CliError::io(format!("noise error: {err}"))
}

impl NoiseSession {
    /// Fresh initiator with a fresh random 32-byte static key.
    pub fn new_initiator() -> Result<Self, CliError> {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        let params: snow::params::NoiseParams = PATTERN
            .parse()
            .map_err(|e| CliError::io(format!("noise pattern error: {e}")))?;
        let handshake = Builder::new(params)
            .local_private_key(&key)
            .build_initiator()
            .map_err(noise_error)?;
        Ok(Self {
            state: Some(NoiseState::Handshake(handshake)),
        })
    }

    /// Responder counterpart, for in-memory loopback tests only.
    /// Not used against the real gateway (the client is always initiator).
    pub fn new_responder() -> Result<Self, CliError> {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        let params: snow::params::NoiseParams = PATTERN
            .parse()
            .map_err(|e| CliError::io(format!("noise pattern error: {e}")))?;
        let handshake = Builder::new(params)
            .local_private_key(&key)
            .build_responder()
            .map_err(noise_error)?;
        Ok(Self {
            state: Some(NoiseState::Handshake(handshake)),
        })
    }

    fn take_state(&mut self) -> Result<NoiseState, CliError> {
        self.state
            .take()
            .ok_or_else(|| CliError::io("noise session is in an invalid state".to_string()))
    }

    /// Write one handshake message with an empty payload. Converts to
    /// transport mode when this write finishes the handshake (the
    /// initiator's third message is the finishing one in Noise XX).
    pub fn write_handshake_message(&mut self) -> Result<Vec<u8>, CliError> {
        match self.take_state()? {
            NoiseState::Handshake(mut state) => {
                let mut buf = vec![0u8; 65535];
                let len = state.write_message(b"", &mut buf).map_err(noise_error)?;
                buf.truncate(len);
                if state.is_handshake_finished() {
                    let transport = state.into_transport_mode().map_err(noise_error)?;
                    self.state = Some(NoiseState::Transport(transport));
                } else {
                    self.state = Some(NoiseState::Handshake(state));
                }
                Ok(buf)
            }
            other => {
                self.state = Some(other);
                Err(CliError::value(
                    "noise handshake already finished".to_string(),
                ))
            }
        }
    }

    /// Read one handshake message; converts to transport mode once the
    /// handshake finishes.
    pub fn read_handshake_message(&mut self, msg: &[u8]) -> Result<(), CliError> {
        match self.take_state()? {
            NoiseState::Handshake(mut state) => {
                let mut buf = vec![0u8; 65535];
                state.read_message(msg, &mut buf).map_err(noise_error)?;
                if state.is_handshake_finished() {
                    let transport = state.into_transport_mode().map_err(noise_error)?;
                    self.state = Some(NoiseState::Transport(transport));
                } else {
                    self.state = Some(NoiseState::Handshake(state));
                }
                Ok(())
            }
            other => {
                self.state = Some(other);
                Err(CliError::value(
                    "cannot read a handshake message after the handshake finished".to_string(),
                ))
            }
        }
    }

    pub fn handshake_finished(&self) -> bool {
        matches!(self.state, Some(NoiseState::Transport(_)))
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, CliError> {
        match self.take_state()? {
            NoiseState::Transport(mut state) => {
                let mut buf = vec![0u8; plaintext.len() + 32];
                let len = state
                    .write_message(plaintext, &mut buf)
                    .map_err(noise_error)?;
                buf.truncate(len);
                self.state = Some(NoiseState::Transport(state));
                Ok(buf)
            }
            other => {
                self.state = Some(other);
                Err(CliError::value(
                    "noise transport is not ready (handshake not finished)".to_string(),
                ))
            }
        }
    }

    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, CliError> {
        match self.take_state()? {
            NoiseState::Transport(mut state) => {
                let mut buf = vec![0u8; ciphertext.len()];
                let len = state
                    .read_message(ciphertext, &mut buf)
                    .map_err(noise_error)?;
                buf.truncate(len);
                self.state = Some(NoiseState::Transport(state));
                Ok(buf)
            }
            other => {
                self.state = Some(other);
                Err(CliError::value(
                    "noise transport is not ready (handshake not finished)".to_string(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initiator_and_responder_complete_empty_payload_handshake() {
        let mut initiator = NoiseSession::new_initiator().unwrap();
        let mut responder = NoiseSession::new_responder().unwrap();
        assert!(!initiator.handshake_finished());
        assert!(!responder.handshake_finished());

        let msg1 = initiator.write_handshake_message().unwrap();
        responder.read_handshake_message(&msg1).unwrap();
        let msg2 = responder.write_handshake_message().unwrap();
        initiator.read_handshake_message(&msg2).unwrap();
        let msg3 = initiator.write_handshake_message().unwrap();
        responder.read_handshake_message(&msg3).unwrap();

        assert!(initiator.handshake_finished());
        assert!(responder.handshake_finished());
    }

    #[test]
    fn transport_encrypt_decrypt_roundtrips_both_directions() {
        let mut initiator = NoiseSession::new_initiator().unwrap();
        let mut responder = NoiseSession::new_responder().unwrap();
        let msg1 = initiator.write_handshake_message().unwrap();
        responder.read_handshake_message(&msg1).unwrap();
        let msg2 = responder.write_handshake_message().unwrap();
        initiator.read_handshake_message(&msg2).unwrap();
        let msg3 = initiator.write_handshake_message().unwrap();
        responder.read_handshake_message(&msg3).unwrap();

        let ciphertext = initiator.encrypt(b"hello gateway").unwrap();
        assert_eq!(responder.decrypt(&ciphertext).unwrap(), b"hello gateway");
        let back = responder.encrypt(b"reply").unwrap();
        assert_eq!(initiator.decrypt(&back).unwrap(), b"reply");
    }

    #[test]
    fn encrypt_before_handshake_finished_is_an_error_not_a_panic() {
        let mut session = NoiseSession::new_initiator().unwrap();
        assert!(session.encrypt(b"x").is_err());
        assert!(session.decrypt(b"x").is_err());
    }
}
