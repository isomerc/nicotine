//! Wayland pointer-focus nudge (KWin / GNOME, XWayland clients).
//!
//! On Wayland, KWin (and Mutter) only re-evaluate *which surface receives
//! pointer input* when the pointer actually moves. Cycling raises and
//! keyboard-focuses the next EVE client, but pointer focus stays on the
//! window that was on top before — so the user's first click after a cycle
//! lands on the now-hidden previous client until they jiggle the mouse.
//! The classic stacked-clients multibox workflow (hover a fixed UI element,
//! then cycle → click → cycle → click) breaks.
//!
//! There is no client-side Wayland API to move the pointer over a surface
//! we don't own (`wp_pointer_warp_v1` only warps within the caller's own
//! surface). The one mechanism that works is to feed the compositor a real
//! motion event: we register a virtual pointer through uinput and, just
//! after activating a client, emit a *net-zero* relative move (+1px then
//! -1px). KWin processes the motion, re-evaluates the surface under the
//! cursor — now the freshly-raised client — and the next click lands. The
//! cursor ends on the exact pixel it started on and the two events take
//! microseconds.
//!
//! The nudge fires from a dedicated thread a few ms after activation so it
//! reaches the compositor *after* the raise it is correcting, and never on
//! the cycle hot path. Everything here is best-effort: if `/dev/uinput`
//! isn't writable we log once and do nothing, leaving cycling unaffected.

use anyhow::{Context, Result};
use evdev::{
    uinput::{VirtualDevice, VirtualDeviceBuilder},
    AttributeSet, EventType, InputEvent, Key, RelativeAxisType,
};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Delay between scheduling a nudge and emitting it. Long enough that KWin
/// has processed the (already-flushed) raise ClientMessage the nudge is
/// correcting, far shorter than human click latency (~100ms+) so it never
/// affects the workflow.
const NUDGE_DELAY: Duration = Duration::from_millis(5);

pub struct PointerNudger {
    // A Mutex makes the struct `Sync` (mpsc::Sender is `Send` but not
    // `Sync`), which the `WindowManager: Send + Sync` bound requires. The
    // send is cheap and the lock is uncontended in practice.
    tx: Mutex<Sender<()>>,
}

impl PointerNudger {
    /// Register the virtual pointer and spawn its emit thread. Returns
    /// `Err` if `/dev/uinput` can't be opened (missing permission, or the
    /// `uinput` module isn't loaded) — callers treat that as "no nudge"
    /// and carry on with plain activation.
    pub fn new() -> Result<Self> {
        let device = build_device().context("create uinput virtual pointer")?;
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || run(device, rx));
        Ok(Self { tx: Mutex::new(tx) })
    }

    /// Request a pointer-focus nudge shortly after the latest activation.
    /// Non-blocking — just signals the emit thread.
    pub fn schedule(&self) {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(());
        }
    }
}

/// Schedule a nudge through a lazily-initialized nudger cell. The device is
/// created on first use so one-shot CLI commands that never activate a
/// window don't spawn (and immediately tear down) a uinput device. A
/// cached `None` means uinput was unavailable; we don't retry every cycle.
pub fn schedule_nudge(cell: &OnceLock<Option<PointerNudger>>) {
    let nudger = cell.get_or_init(|| match PointerNudger::new() {
        Ok(nudger) => {
            println!("Pointer-focus nudge active (virtual pointer registered)");
            Some(nudger)
        }
        Err(e) => {
            eprintln!(
                "Pointer-focus nudge unavailable ({e:?}); the first click after a \
                 cycle may need a mouse move on Wayland. Ensure your user can write \
                 /dev/uinput (input group + a udev rule on most distros)."
            );
            None
        }
    });
    if let Some(nudger) = nudger {
        nudger.schedule();
    }
}

fn build_device() -> Result<VirtualDevice> {
    let mut axes = AttributeSet::<RelativeAxisType>::new();
    axes.insert(RelativeAxisType::REL_X);
    axes.insert(RelativeAxisType::REL_Y);
    // A lone BTN_LEFT (never emitted) makes libinput classify the device as
    // a mouse, so its relative motion actually drives the cursor. It carries
    // neither BTN_SIDE nor BTN_EXTRA, so our own mouse listener's autodetect
    // skips it — no feedback loop.
    let mut keys = AttributeSet::<Key>::new();
    keys.insert(Key::BTN_LEFT);
    let device = VirtualDeviceBuilder::new()?
        .name(b"nicotine-pointer-nudge")
        .with_keys(&keys)?
        .with_relative_axes(&axes)?
        .build()?;
    Ok(device)
}

fn run(mut device: VirtualDevice, rx: Receiver<()>) {
    while rx.recv().is_ok() {
        std::thread::sleep(NUDGE_DELAY);
        // Coalesce a burst: drain activations that queued during the sleep
        // so rapid cycling emits roughly one nudge per NUDGE_DELAY instead
        // of one per cycle. The final cycle still gets its trailing nudge.
        while rx.try_recv().is_ok() {}
        if let Err(e) = emit_nudge(&mut device) {
            eprintln!("pointer nudge emit failed: {e}");
        }
    }
}

/// Net-zero relative move: +1px then -1px, each in its own event frame
/// (`emit` appends a SYN_REPORT per call). Two frames = two position
/// changes the compositor acts on; the cursor returns to the starting pixel.
fn emit_nudge(device: &mut VirtualDevice) -> std::io::Result<()> {
    device.emit(&[InputEvent::new(
        EventType::RELATIVE,
        RelativeAxisType::REL_X.0,
        1,
    )])?;
    device.emit(&[InputEvent::new(
        EventType::RELATIVE,
        RelativeAxisType::REL_X.0,
        -1,
    )])?;
    Ok(())
}
