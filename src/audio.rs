//! Background audio worker for the logo easter-egg jingle.
//!
//! The OS audio handle (`MixerDeviceSink`) is `!Send`, so it lives entirely
//! on this worker thread; the rest of the app only holds the channel `Sender`.

use std::io::Cursor;
use std::sync::mpsc::{channel, Sender};

/// The jingle, embedded so it ships with the binary (like the fonts/icon).
const JINGLE: &[u8] = include_bytes!("../assets/nicotinecountry.mp3");

/// Spawn the audio worker. Returns a sender — send `()` to play the jingle
/// from the start, restarting it if it's already playing.
///
/// Best-effort: if no output device opens (or decoding fails) the commands
/// are dropped without affecting the rest of the app.
pub fn spawn() -> Sender<()> {
    let (tx, rx) = channel::<()>();
    std::thread::spawn(move || {
        let handle = match rodio::DeviceSinkBuilder::open_default_sink() {
            Ok(handle) => handle,
            Err(e) => {
                eprintln!("audio: could not open output device ({e}); logo jingle disabled");
                return;
            }
        };

        // Holding the current Player keeps its sound alive; replacing it
        // drops (and thus stops) the previous one — exactly the
        // restart-on-re-click behavior we want.
        let mut current: Option<rodio::Player> = None;
        while rx.recv().is_ok() {
            match rodio::play(handle.mixer(), Cursor::new(JINGLE)) {
                Ok(player) => {
                    current.replace(player);
                }
                Err(e) => eprintln!("audio: playback failed ({e})"),
            }
        }
    });
    tx
}
