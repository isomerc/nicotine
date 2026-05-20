//! X11 event handling: top-level dispatch plus the three drag-related
//! handlers (ButtonPress, MotionNotify, ButtonRelease) that own all
//! click-to-activate and drag-to-position behavior for both per-client
//! previews and the list window.

use anyhow::Result;

use x11rb::protocol::damage::ConnectionExt as _;
use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt as _, EventMask, GrabMode};
use x11rb::protocol::Event;

use crate::preview_common::{snap_position, DRAG_THRESHOLD_PX, SNAP_THRESHOLD_PX};

use super::{DragTarget, MutexExt as _, PreviewManager};

impl PreviewManager {
    pub(super) fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            // Focus changes flip the chrome highlight on whichever
            // preview matches (or used to match) the new active window.
            // A new window becoming active is also the most likely
            // moment for us to get covered (KWin can raise the
            // newly-focused EVE window above us), so restack right away.
            Event::PropertyNotify(ev) if ev.atom == self.atoms.net_active_window => {
                self.update_active()?;
                self.restack_topmost();
            }
            // The WM updates _NET_CLIENT_LIST whenever a top-level
            // window is mapped or destroyed. Use it as the signal to
            // re-enumerate EVE windows; running the enumeration's sync
            // get_property round-trips on every 100ms tick caused
            // ~10Hz judder, so it's now event-driven.
            Event::PropertyNotify(ev) if ev.atom == self.atoms.net_client_list => {
                self.needs_window_scan = true;
            }
            // EVE drew a frame. Composite + present right here on the
            // event arrival to minimize damage→display latency. The
            // dirty flag is left as a backstop for newly-created
            // previews (whose first paint can't be triggered by a
            // damage event yet) and for the Expose path below.
            Event::DamageNotify(ev) => {
                let key = self
                    .previews
                    .values_mut()
                    .find(|p| p.damage_id == ev.damage)
                    .map(|p| {
                        p.dirty = true;
                        p.character_name.clone()
                    });
                if let Some(k) = key {
                    if let Err(e) = self.paint_preview_now(&k) {
                        eprintln!("paint_preview_now({}): {}", k, e);
                    }
                }
                // damage_subtract clears the accumulated damage region
                // and re-arms the notify. Without this we get exactly
                // one DamageNotify per damage_create.
                let _ = self
                    .conn
                    .damage_subtract(ev.damage, x11rb::NONE, x11rb::NONE);
            }
            // The X server (or compositor on first map) cleared our
            // window content; present a fresh full frame.
            // paint_preview_now bundles chrome + composite + present so
            // the window content is consistent. List window paints
            // itself directly via XRender.
            Event::Expose(ev) => {
                let key = self
                    .previews
                    .values()
                    .find(|p| p.window == ev.window)
                    .map(|p| p.character_name.clone());
                if let Some(k) = key {
                    let _ = self.paint_preview_now(&k);
                } else if self
                    .list_window
                    .as_ref()
                    .is_some_and(|l| l.window == ev.window)
                {
                    self.paint_list()?;
                }
            }
            // Drag lifecycle: press → motion → release. Same code paths
            // for previews and the list window — both drag identically.
            Event::ButtonPress(ev) => self.handle_button_press(ev)?,
            Event::MotionNotify(ev) => self.handle_motion_notify(ev)?,
            Event::ButtonRelease(ev) => self.handle_button_release(ev)?,
            // Our own configure_window calls send these back to us;
            // currently informational only. Phase-5 fancy positioning
            // (e.g., snap-back-from-offscreen) would update preview.x/y
            // here.
            Event::ConfigureNotify(_) => {}
            _ => {}
        }
        Ok(())
    }

    /// Left-click-drag start. Grab the pointer (unless positions are
    /// locked) so motion + release events flow back to us even if the
    /// cursor leaves the window mid-drag. Previews and the list window
    /// take the same path.
    fn handle_button_press(&mut self, ev: x11rb::protocol::xproto::ButtonPressEvent) -> Result<()> {
        if ev.detail != 1 {
            return Ok(());
        }
        let positions_locked = self.live.lock_recover().positions_locked;
        let target_window = ev.event;

        let target = self
            .previews
            .iter()
            .find_map(|(n, p)| (p.window == target_window).then(|| DragTarget::Preview(n.clone())))
            .or_else(|| {
                self.list_window
                    .as_ref()
                    .and_then(|l| (l.window == target_window).then_some(DragTarget::List))
            });
        let Some(target) = target else {
            return Ok(());
        };

        let origin_window: (i32, i32) = match &target {
            DragTarget::Preview(name) => self
                .previews
                .get(name)
                .map(|p| (p.x as i32, p.y as i32))
                .unwrap_or((0, 0)),
            DragTarget::List => self
                .list_window
                .as_ref()
                .map(|l| (l.x as i32, l.y as i32))
                .unwrap_or((0, 0)),
        };

        let mut grabbed = false;
        if !positions_locked {
            let cookie = self.conn.grab_pointer(
                false,
                target_window,
                EventMask::POINTER_MOTION | EventMask::BUTTON_RELEASE,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                x11rb::NONE,
                x11rb::NONE,
                ev.time,
            )?;
            if let Ok(reply) = cookie.reply() {
                grabbed = matches!(reply.status, x11rb::protocol::xproto::GrabStatus::SUCCESS);
            }
        }

        match target {
            DragTarget::Preview(name) => {
                if let Some(preview) = self.previews.get_mut(&name) {
                    preview.drag.active = true;
                    preview.drag.dragged = false;
                    preview.drag.origin_screen = (ev.root_x as i32, ev.root_y as i32);
                    preview.drag.origin_window = origin_window;
                    preview.grabbed = grabbed;
                }
            }
            DragTarget::List => {
                if let Some(list) = self.list_window.as_mut() {
                    list.drag.active = true;
                    list.drag.dragged = false;
                    list.drag.origin_screen = (ev.root_x as i32, ev.root_y as i32);
                    list.drag.origin_window = origin_window;
                    list.grabbed = grabbed;
                }
            }
        }
        Ok(())
    }

    /// Drag in progress (only delivered while we hold the grab). Move
    /// the dragged window with optional snap-to-dock against other
    /// previews + the list window.
    fn handle_motion_notify(
        &mut self,
        ev: x11rb::protocol::xproto::MotionNotifyEvent,
    ) -> Result<()> {
        let positions_locked = self.live.lock_recover().positions_locked;
        if positions_locked {
            return Ok(());
        }
        let target = self
            .previews
            .iter()
            .find_map(|(n, p)| p.drag.active.then(|| DragTarget::Preview(n.clone())))
            .or_else(|| {
                self.list_window
                    .as_ref()
                    .and_then(|l| l.drag.active.then_some(DragTarget::List))
            });
        let Some(target) = target else {
            return Ok(());
        };

        let (origin_screen, origin_window, width, height, window_id) = match &target {
            DragTarget::Preview(name) => {
                let p = self.previews.get(name).unwrap();
                (
                    p.drag.origin_screen,
                    p.drag.origin_window,
                    p.width,
                    p.height,
                    p.window,
                )
            }
            DragTarget::List => {
                let l = self.list_window.as_ref().unwrap();
                (
                    l.drag.origin_screen,
                    l.drag.origin_window,
                    l.width,
                    l.height,
                    l.window,
                )
            }
        };
        let dx = ev.root_x as i32 - origin_screen.0;
        let dy = ev.root_y as i32 - origin_screen.1;
        let proposed_x = origin_window.0 + dx;
        let proposed_y = origin_window.1 + dy;
        let crossed_threshold = dx.abs() > DRAG_THRESHOLD_PX || dy.abs() > DRAG_THRESHOLD_PX;
        if !crossed_threshold {
            return Ok(());
        }
        let others = self.collect_other_rects(&target);
        let (snapped_x, snapped_y) = snap_position(
            proposed_x,
            proposed_y,
            width as i32,
            height as i32,
            &others,
            SNAP_THRESHOLD_PX,
        );
        let new_x = snapped_x.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let new_y = snapped_y.clamp(i16::MIN as i32, i16::MAX as i32) as i16;

        match target {
            DragTarget::Preview(name) => {
                if let Some(preview) = self.previews.get_mut(&name) {
                    preview.drag.dragged = true;
                    preview.x = new_x;
                    preview.y = new_y;
                }
            }
            DragTarget::List => {
                if let Some(list) = self.list_window.as_mut() {
                    list.drag.dragged = true;
                    list.x = new_x;
                    list.y = new_y;
                }
            }
        }
        let _ = self.conn.configure_window(
            window_id,
            &ConfigureWindowAux::new().x(new_x as i32).y(new_y as i32),
        );
        Ok(())
    }

    /// End of click-or-drag.
    ///
    /// - Preview: drag → persist per-character position. Click → EWMH
    ///   `_NET_ACTIVE_WINDOW` ClientMessage so the source EVE
    ///   foregrounds (bypasses the compositor-native id space so it
    ///   works the same on KWin / Sway / Hyprland).
    /// - List: drag → persist `list_position`. Click → no-op (rows
    ///   aren't clickable; matches Windows).
    fn handle_button_release(
        &mut self,
        ev: x11rb::protocol::xproto::ButtonReleaseEvent,
    ) -> Result<()> {
        if ev.detail != 1 {
            return Ok(());
        }
        let target_window = ev.event;
        let target = self
            .previews
            .iter()
            .find_map(|(n, p)| {
                (p.window == target_window && p.drag.active).then(|| DragTarget::Preview(n.clone()))
            })
            .or_else(|| {
                self.list_window.as_ref().and_then(|l| {
                    (l.window == target_window && l.drag.active).then_some(DragTarget::List)
                })
            });
        let Some(target) = target else {
            return Ok(());
        };

        match target {
            DragTarget::Preview(name) => {
                let (was_dragged, was_grabbed, source_id, x, y, character_name) = {
                    let p = self.previews.get(&name).unwrap();
                    (
                        p.drag.dragged,
                        p.grabbed,
                        p.source_id,
                        p.x,
                        p.y,
                        p.character_name.clone(),
                    )
                };
                if was_grabbed {
                    let _ = self.conn.ungrab_pointer(ev.time);
                }
                if was_dragged {
                    let mut positions = self.positions.lock_recover();
                    positions.set(character_name, x as i32, y as i32);
                    positions.save();
                } else {
                    let _ = self.activate_source(source_id);
                }
                if let Some(preview) = self.previews.get_mut(&name) {
                    preview.drag.active = false;
                    preview.drag.dragged = false;
                    preview.grabbed = false;
                }
            }
            DragTarget::List => {
                let (was_dragged, was_grabbed, x, y) = {
                    let l = self.list_window.as_ref().unwrap();
                    (l.drag.dragged, l.grabbed, l.x, l.y)
                };
                if was_grabbed {
                    let _ = self.conn.ungrab_pointer(ev.time);
                }
                if was_dragged {
                    let mut positions = self.positions.lock_recover();
                    positions.list_position = Some((x as i32, y as i32));
                    positions.save();
                }
                if let Some(list) = self.list_window.as_mut() {
                    list.drag.active = false;
                    list.drag.dragged = false;
                    list.grabbed = false;
                }
            }
        }
        Ok(())
    }
}
