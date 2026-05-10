//! Wayland xdg-activation client.
//!
//! Nicotine is an X11 client (preview manager uses XComposite + XRender;
//! EVE itself runs through XWayland). On Wayland sessions our EWMH
//! `_NET_ACTIVE_WINDOW` ClientMessage tells KWin to focus a target X11
//! window, but KWin doesn't reliably propagate that to **Wayland surface
//! focus** for XWayland clients. The user-visible symptom: after cycling
//! to a new EVE client, the first click on it is consumed by KWin's
//! focus-on-click logic instead of being delivered to EVE.
//!
//! The Wayland-native way to grant focus authority across clients is
//! xdg-activation: client A requests a token from the compositor, hands
//! it to client B (or in our case, attaches it to client B's X11 window
//! via the `_NET_WM_ACTIVATION_TOKEN` property), and the compositor uses
//! the token to authorize the focus change at the Wayland level.
//!
//! This module opens a tiny single-purpose Wayland connection — no
//! surfaces, no input handling — just to mint tokens. The pure-Rust
//! `wayland-backend` keeps us off `libwayland-client.so`, preserving the
//! design property that made us drop `eframe`'s wayland feature in the
//! first place.

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry::WlRegistry,
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::{Event as TokenEvent, XdgActivationTokenV1},
    xdg_activation_v1::XdgActivationV1,
};

/// Max time we'll wait for the compositor to mint a token before giving
/// up and letting the caller fall back to a token-less EWMH activation.
/// The compositor round-trip is normally sub-millisecond on a healthy
/// system; 100ms is a generous ceiling that won't block cycling
/// perceptibly even on the worst case.
const TOKEN_TIMEOUT: Duration = Duration::from_millis(100);

/// Holds the Wayland connection + bound activation manager. Reused
/// across cycle activations; one connection at daemon startup, not
/// per-request.
pub struct XdgActivation {
    conn: Connection,
    activation: XdgActivationV1,
    queue_handle: QueueHandle<TokenState>,
    event_queue: std::sync::Mutex<wayland_client::EventQueue<TokenState>>,
}

impl XdgActivation {
    /// Open a Wayland connection and bind `xdg_activation_v1`. Returns
    /// `Err` if there's no Wayland session, the compositor doesn't
    /// advertise the protocol (older Plasma, niche compositors), or any
    /// other setup step fails. Callers should fall back to the
    /// token-less activation path on `Err` — every step here is
    /// best-effort.
    pub fn new() -> Result<Self> {
        let conn = Connection::connect_to_env().context("connect to Wayland display")?;
        let (globals, event_queue) =
            registry_queue_init::<TokenState>(&conn).context("init Wayland registry")?;
        let queue_handle = event_queue.handle();
        let activation: XdgActivationV1 = globals
            .bind(&queue_handle, 1..=1, ())
            .context("bind xdg_activation_v1 — compositor doesn't advertise it")?;
        Ok(Self {
            conn,
            activation,
            queue_handle,
            event_queue: std::sync::Mutex::new(event_queue),
        })
    }

    /// Synchronously request an activation token from the compositor.
    /// Blocks up to `TOKEN_TIMEOUT` for the `done` reply. We don't set
    /// `serial`/`seat`/`surface` on the request because we're an X11
    /// client without Wayland input events; modern KWin tolerates
    /// serial-less tokens and applies the user's focus-stealing-
    /// prevention level to decide whether to honor them. On default
    /// settings this should pass.
    pub fn request_token(&self) -> Result<String> {
        let mut state = TokenState { token: None };
        let request = self.activation.get_activation_token(&self.queue_handle, ());
        request.set_app_id("nicotine".to_string());
        request.commit();
        // Single connection means single event_queue; lock for the
        // duration of one token round-trip. Cycle activations are
        // serialized through this mutex, which is fine — they're
        // user-paced (mouse clicks / hotkeys) and the wait is short.
        let mut event_queue = self
            .event_queue
            .lock()
            .map_err(|_| anyhow::anyhow!("event queue mutex poisoned"))?;
        let _ = self.conn.flush();
        let deadline = Instant::now() + TOKEN_TIMEOUT;
        while state.token.is_none() {
            let now = Instant::now();
            if now >= deadline {
                anyhow::bail!("xdg_activation token request timed out");
            }
            // blocking_dispatch can sit indefinitely on a quiet
            // socket. Pump it manually with a poll-style read so we
            // can respect the deadline.
            event_queue
                .dispatch_pending(&mut state)
                .context("dispatch_pending")?;
            if state.token.is_some() {
                break;
            }
            event_queue
                .blocking_dispatch(&mut state)
                .context("blocking_dispatch")?;
        }
        state
            .token
            .ok_or_else(|| anyhow::anyhow!("compositor returned no token"))
    }
}

/// Per-request state captured by the `Done(token)` event handler.
/// Recreated for each `request_token` call so old tokens don't linger.
struct TokenState {
    token: Option<String>,
}

// The registry global itself doesn't deliver events we act on after
// the initial bind (registry_queue_init already drained the global
// list).
impl Dispatch<WlRegistry, GlobalListContents> for TokenState {
    fn event(
        _state: &mut Self,
        _registry: &WlRegistry,
        _event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// The activation manager itself emits no events.
impl Dispatch<XdgActivationV1, ()> for TokenState {
    fn event(
        _state: &mut Self,
        _proxy: &XdgActivationV1,
        _event: <XdgActivationV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// The token object emits `Done(token)` with the minted string when the
// compositor finishes processing our commit. Stash it on the state so
// `request_token` can return.
impl Dispatch<XdgActivationTokenV1, ()> for TokenState {
    fn event(
        state: &mut Self,
        proxy: &XdgActivationTokenV1,
        event: <XdgActivationTokenV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let TokenEvent::Done { token } = event {
            state.token = Some(token);
            // The token object is single-use from our perspective —
            // free its protocol resources right away.
            proxy.destroy();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_returns_err_when_wayland_display_unset() {
        // Save + clear WAYLAND_DISPLAY so the test doesn't depend on
        // the environment. Connection::connect_to_env should fail
        // cleanly with no env var pointing at a live socket.
        let saved = std::env::var_os("WAYLAND_DISPLAY");
        // SAFETY: tests run single-threaded by default in cargo test;
        // we restore the value below.
        unsafe {
            std::env::remove_var("WAYLAND_DISPLAY");
            std::env::remove_var("WAYLAND_SOCKET");
        }
        let result = XdgActivation::new();
        if let Some(v) = saved {
            unsafe {
                std::env::set_var("WAYLAND_DISPLAY", v);
            }
        }
        assert!(
            result.is_err(),
            "XdgActivation::new() should return Err when no Wayland connection is available"
        );
    }
}
