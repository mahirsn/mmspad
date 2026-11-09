//! Virtual tablet device implementation using uinput.

use crate::events::*;
use mmspad_protocol::InputEvent;
use evdev::{
    uinput::{VirtualDevice, VirtualDeviceBuilder},
    AbsInfo, AbsoluteAxisType, AttributeSet, BusType, EventType, InputEvent as EvdevEvent, InputId,
    Key, MiscType, PropType, Synchronization, UinputAbsSetup,
};
use std::time::{Duration, Instant};
use thiserror::Error;
use tracing::{debug, info};

/// The tablet screen's size in mm (Galaxy Tab S9: 11" at 16:10), used as the touchpad size.
const TOUCHPAD_MM: (i32, i32) = (237, 148);

/// Configuration for the virtual tablet device identity.
#[derive(Debug, Clone, PartialEq)]
pub struct TabletDescriptor {
    pub name: String,
    pub vid: u16,
    pub pid: u16,
}

impl Default for TabletDescriptor {
    fn default() -> Self {
        Self {
            // Use generic name, no VID/PID (let kernel assign defaults)
            name: "mmspad Pen".to_string(),
            vid: 0x0000,
            pid: 0x0000,
        }
    }
}

impl TabletDescriptor {
    // Presets removed as we are standardizing on "mmspad Pen"
}

/// Errors that can occur when creating or using a virtual tablet.
#[derive(Debug, Error)]
pub enum TabletError {
    #[error("failed to create virtual device: {0}")]
    DeviceCreation(#[source] std::io::Error),

    #[error("failed to emit event: {0}")]
    EventEmit(#[source] std::io::Error),

    #[error("invalid touch slot: {slot} (max {max})")]
    InvalidSlot { slot: u8, max: u8 },
}

/// A virtual input system that separates Pen and Touch into two devices.
pub struct VirtualTablet {
    pen_device: VirtualDevice,
    touch_device: VirtualDevice,
    stylus_state: StylusState,
    in_proximity: bool,
    tool_announced: bool, // Track if we've sent the tool serial (only once per session)
    last_pen_activity: Option<Instant>,
    touch_slots: Vec<TouchSlot>,
    current_slot: u8,
    /// Touch changes not yet reported. A touch frame is every finger at one moment: the
    /// tablet sends one event per finger with the same timestamp, and reporting them as
    /// separate frames makes the other fingers look like they jumped.
    touch_dirty: bool,
    touch_frame: Option<u64>,
    /// The pending frame has a finger going down or up.
    touch_edge: bool,
    /// Touch events of the frame being built. evdev's `emit` ends every call with
    /// SYN_REPORT, so a frame has to go out in one call: split, libinput reads each slot's
    /// update as a new frame microseconds later, a huge speed, and drops it as a jump.
    touch_out: Vec<EvdevEvent>,
}

impl VirtualTablet {
    /// Create a new virtual tablet system (Pen + Touchpad).
    pub fn new(descriptor: &TabletDescriptor) -> Result<Self, TabletError> {
        info!(
            "Creating virtual tablet devices as '{}' (VID: {:04x}, PID: {:04x})",
            descriptor.name, descriptor.vid, descriptor.pid
        );

        // --- Device 1: Pen (Stylus) ---
        // 65535 / 200mm = ~327 resolution.
        let resolution = 400;

        let abs_x = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_X,
            AbsInfo::new(0, 0, ABS_MAX, 0, 0, resolution),
        );
        let abs_y = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_Y,
            AbsInfo::new(0, 0, ABS_MAX, 0, 0, resolution),
        );
        let abs_pressure = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_PRESSURE,
            AbsInfo::new(0, 0, PRESSURE_MAX, 0, 0, 0),
        );
        let abs_tilt_x = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_TILT_X,
            AbsInfo::new(0, -TILT_MAX, TILT_MAX, 0, 0, 0),
        );
        let abs_tilt_y = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_TILT_Y,
            AbsInfo::new(0, -TILT_MAX, TILT_MAX, 0, 0, 0),
        );
        // Add ABS_DISTANCE (0-255) for hover detection support
        let abs_distance = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_DISTANCE,
            AbsInfo::new(0, 0, 255, 0, 0, 0),
        );

        // Add MSC_SERIAL to identify the tool
        let mut msc_supported = AttributeSet::<MiscType>::new();
        msc_supported.insert(MiscType::MSC_SERIAL);

        let mut pen_keys = AttributeSet::<Key>::new();
        pen_keys.insert(Key::BTN_TOOL_PEN);
        pen_keys.insert(Key::BTN_TOOL_RUBBER); // Eraser support
        pen_keys.insert(Key::BTN_TOUCH); // Often needed for pen contact
        pen_keys.insert(Key::BTN_STYLUS);
        pen_keys.insert(Key::BTN_STYLUS2);

        // Pen Properties: DIRECT + POINTER (OpenTabletDriver style)
        // DIRECT: Indicates direct-input device where coordinates map 1:1 to screen
        // POINTER: Indicates device has pointer capability
        // OTD sets both for their "Artist Tablet" mode
        let mut pen_props = AttributeSet::<PropType>::new();
        pen_props.insert(PropType::DIRECT);
        pen_props.insert(PropType::POINTER);

        // Use BUS_VIRTUAL since this is a virtual device (OTD style)
        // This is more honest than pretending to be a USB device
        let pen_id = InputId::new(BusType::BUS_VIRTUAL, descriptor.vid, descriptor.pid, 0x0100);

        let pen_device = VirtualDeviceBuilder::new()
            .map_err(TabletError::DeviceCreation)?
            .name(&descriptor.name)
            .input_id(pen_id)
            .with_keys(&pen_keys)
            .map_err(TabletError::DeviceCreation)?
            .with_properties(&pen_props)
            .map_err(TabletError::DeviceCreation)?
            .with_absolute_axis(&abs_x)
            .map_err(TabletError::DeviceCreation)?
            .with_absolute_axis(&abs_y)
            .map_err(TabletError::DeviceCreation)?
            .with_absolute_axis(&abs_pressure)
            .map_err(TabletError::DeviceCreation)?
            .with_absolute_axis(&abs_tilt_x)
            .map_err(TabletError::DeviceCreation)?
            .with_absolute_axis(&abs_tilt_y)
            .map_err(TabletError::DeviceCreation)?
            .with_absolute_axis(&abs_distance)
            .map_err(TabletError::DeviceCreation)?
            .with_msc(&msc_supported)
            .map_err(TabletError::DeviceCreation)?
            .build()
            .map_err(TabletError::DeviceCreation)?;

        // --- Device 2: Touchpad (Multitouch) ---
        // libinput sizes a touchpad from range / resolution (units per mm) and scales pointer
        // speed by it, so the axes carry the tablet screen's real size: Galaxy Tab S9,
        // 11" 16:10, about 237 x 148 mm.
        let (pad_res_x, pad_res_y) = (ABS_MAX / TOUCHPAD_MM.0, ABS_MAX / TOUCHPAD_MM.1);
        let pad_x = AbsInfo::new(0, 0, ABS_MAX, 0, 0, pad_res_x);
        let pad_y = AbsInfo::new(0, 0, ABS_MAX, 0, 0, pad_res_y);
        let mt_slot = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_MT_SLOT,
            AbsInfo::new(0, 0, (MT_SLOTS - 1) as i32, 0, 0, 0),
        );
        let mt_tracking_id = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_MT_TRACKING_ID,
            AbsInfo::new(0, 0, 65535, 0, 0, 0),
        );
        let mt_position_x = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_MT_POSITION_X,
            pad_x,
        );
        let mt_position_y = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_MT_POSITION_Y,
            pad_y,
        );
        // A touchpad also needs the single-touch axes and the finger-count keys; with them
        // (and no DIRECT property) libinput treats it as one: the pointer moves, a tap clicks,
        // two fingers scroll, the compositor's touchpad settings apply.
        let pad_abs_x = UinputAbsSetup::new(AbsoluteAxisType::ABS_X, pad_x);
        let pad_abs_y = UinputAbsSetup::new(AbsoluteAxisType::ABS_Y, pad_y);

        let mut touch_keys = AttributeSet::<Key>::new();
        for key in [
            Key::BTN_LEFT,
            Key::BTN_TOUCH,
            Key::BTN_TOOL_FINGER,
            Key::BTN_TOOL_DOUBLETAP,
            Key::BTN_TOOL_TRIPLETAP,
            Key::BTN_TOOL_QUADTAP,
        ] {
            touch_keys.insert(key);
        }

        let mut touch_props = AttributeSet::<PropType>::new();
        touch_props.insert(PropType::POINTER);
        // No physical buttons: a clickpad, so libinput does not look for a right button.
        touch_props.insert(PropType::BUTTONPAD);

        // Use the same VID/PID as the Pen device so they are associated as the same physical hardware.
        let touch_id = InputId::new(BusType::BUS_USB, descriptor.vid, descriptor.pid, 0x0101);

        let touch_device = VirtualDeviceBuilder::new()
            .map_err(TabletError::DeviceCreation)?
            .name("mmspad Touchpad")
            .input_id(touch_id)
            .with_keys(&touch_keys)
            .map_err(TabletError::DeviceCreation)?
            .with_properties(&touch_props)
            .map_err(TabletError::DeviceCreation)?
            .with_absolute_axis(&mt_slot)
            .map_err(TabletError::DeviceCreation)?
            .with_absolute_axis(&mt_tracking_id)
            .map_err(TabletError::DeviceCreation)?
            .with_absolute_axis(&mt_position_x)
            .map_err(TabletError::DeviceCreation)?
            .with_absolute_axis(&mt_position_y)
            .map_err(TabletError::DeviceCreation)?
            .with_absolute_axis(&pad_abs_x)
            .map_err(TabletError::DeviceCreation)?
            .with_absolute_axis(&pad_abs_y)
            .map_err(TabletError::DeviceCreation)?
            .build()
            .map_err(TabletError::DeviceCreation)?;

        info!("Virtual tablet devices created successfully");

        Ok(Self {
            pen_device,
            touch_device,
            stylus_state: StylusState::default(),
            in_proximity: false,
            tool_announced: false,
            last_pen_activity: None,
            touch_slots: (0..MT_SLOTS).map(|_| TouchSlot::default()).collect(),
            current_slot: 0,
            touch_dirty: false,
            touch_frame: None,
            touch_edge: false,
            touch_out: Vec::new(),
        })
    }


    /// Process an input event sent at `timestamp`. Touch events are gathered into one frame
    /// and reported on `flush()`, which the server calls when its input goes quiet.
    ///
    /// The tablet sends a finger's past samples too, and over USB they arrive in bursts:
    /// reported one by one they would reach libinput microseconds apart, which it reads as
    /// an impossible speed and drops as a jump. So moves from different moments merge into
    /// the newest position; only a finger going down or up, which has to be seen on its
    /// own, closes the frame first.
    pub fn process_input(&mut self, timestamp: u64, event: &InputEvent) -> Result<(), TabletError> {
        let edge = matches!(event, InputEvent::TouchDown { .. } | InputEvent::TouchUp { .. });
        let touch = edge || matches!(event, InputEvent::TouchMove { .. });
        let new_moment = self.touch_frame.is_some_and(|t| t != timestamp);
        if !touch || (new_moment && (edge || self.touch_edge)) {
            self.flush()?;
        }
        if touch {
            self.touch_frame = Some(timestamp);
            self.touch_edge |= edge;
        }
        self.apply(event)
    }

    /// Report the pending touch frame, if any.
    pub fn flush(&mut self) -> Result<(), TabletError> {
        self.touch_frame = None;
        self.touch_edge = false;
        if !std::mem::take(&mut self.touch_dirty) {
            return Ok(());
        }
        self.sync_touch_state_to_os()
    }

    fn apply(&mut self, event: &InputEvent) -> Result<(), TabletError> {
        // Simple Palm Rejection:
        // If the pen has been active recently (hovering or touching), ignore all touch input.
        // This prevents palm touches while writing.
        const PALM_REJECTION_TIMEOUT: Duration = Duration::from_millis(1000);

        match event {
            InputEvent::StylusMove { .. }
            | InputEvent::StylusDown { .. }
            | InputEvent::StylusUp
            | InputEvent::StylusLeave
            | InputEvent::StylusButton { .. } => {
                // If this is the FIRST pen event after a while, we should forcibly release any touches
                // to prevent "stuck" clicks if the user rests their palm while a finger was still down.
                let was_active = self
                    .last_pen_activity
                    .is_some_and(|t| t.elapsed() < PALM_REJECTION_TIMEOUT);
                self.last_pen_activity = Some(Instant::now());

                if !was_active {
                    // Force release all active touches
                    self.release_all_touches()?;
                }

                match event {
                    InputEvent::StylusMove { .. } => self.stylus_move(event),
                    InputEvent::StylusDown { .. } => self.stylus_down(event),
                    InputEvent::StylusUp => self.stylus_up(),
                    InputEvent::StylusLeave => self.leave_proximity(),
                    InputEvent::StylusButton { button, pressed } => {
                        self.stylus_button(*button, *pressed)
                    }
                    _ => Ok(()),
                }
            }
            InputEvent::TouchDown { id, x, y } => {
                if let Some(last) = self.last_pen_activity {
                    if last.elapsed() < PALM_REJECTION_TIMEOUT {
                        debug!("Touch suppressed (Palm Rejection)");
                        return Ok(());
                    }
                }
                self.touch_down(*id, *x, *y)
            }
            InputEvent::TouchMove { id, x, y } => {
                if let Some(last) = self.last_pen_activity {
                    if last.elapsed() < PALM_REJECTION_TIMEOUT {
                        // If we are here, it means we might have suppressed the Down event,
                        // so this Move event refers to a non-existent slot.
                        return Ok(());
                    }
                }
                self.touch_move(*id, *x, *y)
            }
            InputEvent::TouchUp { id } => {
                // Always process Up to clean up slots, even if we are in rejection mode.
                // This ensures our internal state tracks reality as much as possible,
                // although the "force release" above handles the OS side.
                self.touch_up(*id)
            }
        }
    }

    /// Force release all active touch slots.
    fn release_all_touches(&mut self) -> Result<(), TabletError> {
        if !self.touch_slots.iter().any(|s| s.active) {
            return Ok(());
        }
        info!("Palm Rejection triggered: releasing active touches");
        for slot in &mut self.touch_slots {
            slot.active = false;
        }
        self.touch_dirty = false;
        self.sync_touch_state_to_os()
    }

    fn stylus_move(&mut self, event: &InputEvent) -> Result<(), TabletError> {
        self.stylus_state.update_from_event(event);

        // Ensure proximity is established first
        if !self.in_proximity {
            self.enter_proximity()?;
        }

        self.emit_stylus_position()?;
        self.sync_pen()
    }

    fn stylus_down(&mut self, event: &InputEvent) -> Result<(), TabletError> {
        debug!("Stylus down");
        self.stylus_state.update_from_event(event);

        // Ensure proximity is established first (and synced!)
        if !self.in_proximity {
            self.enter_proximity()?;
        }

        // Force minimum pressure to 1 to ensure Krita registers the stroke start.
        // If pressure is 0, apps often treat it as a hover even if BTN_TOUCH is 1.
        let pressure = self.stylus_state.pressure.max(1);

        // Send BTN_TOUCH=1 and Pressure/Position in the same frame.
        self.emit_pen(&[
            EvdevEvent::new(EventType::KEY, Key::BTN_TOOL_PEN.0, 1), // Re-affirm tool
            EvdevEvent::new(EventType::KEY, Key::BTN_TOUCH.0, 1),
            EvdevEvent::new(
                EventType::ABSOLUTE,
                AbsoluteAxisType::ABS_X.0,
                self.stylus_state.x,
            ),
            EvdevEvent::new(
                EventType::ABSOLUTE,
                AbsoluteAxisType::ABS_Y.0,
                self.stylus_state.y,
            ),
            EvdevEvent::new(
                EventType::ABSOLUTE,
                AbsoluteAxisType::ABS_PRESSURE.0,
                pressure,
            ),
            // Distance = 0 for touch
            EvdevEvent::new(EventType::ABSOLUTE, AbsoluteAxisType::ABS_DISTANCE.0, 0),
            EvdevEvent::new(
                EventType::ABSOLUTE,
                AbsoluteAxisType::ABS_TILT_X.0,
                self.stylus_state.tilt_x,
            ),
            EvdevEvent::new(
                EventType::ABSOLUTE,
                AbsoluteAxisType::ABS_TILT_Y.0,
                self.stylus_state.tilt_y,
            ),
        ])?;
        self.sync_pen()
        // We stay in_proximity = true because the pen is likely still hovering
    }

    fn stylus_up(&mut self) -> Result<(), TabletError> {
        debug!("Stylus up");
        self.stylus_state.is_down = false;
        self.stylus_state.pressure = 0;

        self.emit_pen(&[
            EvdevEvent::new(EventType::KEY, Key::BTN_TOUCH.0, 0),
            EvdevEvent::new(EventType::ABSOLUTE, AbsoluteAxisType::ABS_PRESSURE.0, 0),
        ])?;
        self.sync_pen()
        // We stay in_proximity = true because the pen is likely still hovering
    }

    /// The pen left hover range: report it, so the cursor goes back to the pointer.
    ///
    /// libinput takes a pen that never reports leaving for one that cannot, and then
    /// ends its proximity itself after 50 ms without events: a pen held still while
    /// hovering went out and back in at every small move, and the cursor jumped between
    /// the pen and the pointer. One real leave turns that off.
    fn leave_proximity(&mut self) -> Result<(), TabletError> {
        if !self.in_proximity || self.stylus_state.is_down {
            return Ok(());
        }
        self.emit_pen(&[
            EvdevEvent::new(EventType::ABSOLUTE, AbsoluteAxisType::ABS_DISTANCE.0, 0),
            EvdevEvent::new(EventType::KEY, Key::BTN_TOOL_PEN.0, 0),
        ])?;
        self.in_proximity = false;
        Ok(())
    }

    fn enter_proximity(&mut self) -> Result<(), TabletError> {
        // A fixed serial, sent once: each new serial makes the compositor create another
        // tablet tool (zwp_tablet_v2), and apps such as Krita track tools by it.
        const PEN_SERIAL: i32 = 0x12345678;
        let first = !self.tool_announced;
        let at = [
            EvdevEvent::new(EventType::ABSOLUTE, AbsoluteAxisType::ABS_X.0, self.stylus_state.x),
            EvdevEvent::new(EventType::ABSOLUTE, AbsoluteAxisType::ABS_Y.0, self.stylus_state.y),
            EvdevEvent::new(EventType::ABSOLUTE, AbsoluteAxisType::ABS_DISTANCE.0, 10), // hovering
        ];
        let tool = EvdevEvent::new(EventType::KEY, Key::BTN_TOOL_PEN.0, 1);
        let mut events = vec![tool];
        if first {
            events.push(EvdevEvent::new(EventType::MISC, MiscType::MSC_SERIAL.0, PEN_SERIAL));
            self.tool_announced = true;
            info!("Announcing tablet tool with serial {:08x}", PEN_SERIAL);
        }
        events.extend(at);
        self.emit_pen(&events)?;
        if first {
            // One real leave right away, so libinput never ends proximity on its own (see
            // leave_proximity), not even before the pen first leaves range.
            self.emit_pen(&[EvdevEvent::new(EventType::KEY, Key::BTN_TOOL_PEN.0, 0)])?;
            let mut back = vec![tool];
            back.extend(at);
            self.emit_pen(&back)?;
        }
        self.in_proximity = true;
        Ok(())
    }

    fn emit_stylus_position(&mut self) -> Result<(), TabletError> {
        // Just emit position/pressure updates.
        // We DO NOT re-emit BTN_TOOL_PEN or MSC_SERIAL here to keep the event stream clean.

        // IMPORTANT: Force pressure to 0 when hovering (!is_down) to prevent false strokes.
        // This is a safety net in case the client sends non-zero pressure during hover.
        // Krita and other apps interpret any pressure > 0 as drawing intent.
        let pressure = if self.stylus_state.is_down {
            self.stylus_state.pressure
        } else {
            0
        };

        self.emit_pen(&[
            EvdevEvent::new(
                EventType::ABSOLUTE,
                AbsoluteAxisType::ABS_X.0,
                self.stylus_state.x,
            ),
            EvdevEvent::new(
                EventType::ABSOLUTE,
                AbsoluteAxisType::ABS_Y.0,
                self.stylus_state.y,
            ),
            EvdevEvent::new(
                EventType::ABSOLUTE,
                AbsoluteAxisType::ABS_PRESSURE.0,
                pressure,
            ),
            // If touching, Distance is 0. If hovering, it's > 0 (e.g., 10).
            EvdevEvent::new(
                EventType::ABSOLUTE,
                AbsoluteAxisType::ABS_DISTANCE.0,
                if self.stylus_state.is_down { 0 } else { 10 },
            ),
            EvdevEvent::new(
                EventType::ABSOLUTE,
                AbsoluteAxisType::ABS_TILT_X.0,
                self.stylus_state.tilt_x,
            ),
            EvdevEvent::new(
                EventType::ABSOLUTE,
                AbsoluteAxisType::ABS_TILT_Y.0,
                self.stylus_state.tilt_y,
            ),
        ])
    }

    fn stylus_button(&mut self, button: u8, pressed: bool) -> Result<(), TabletError> {
        debug!(
            "Stylus button {} {}",
            button,
            if pressed { "pressed" } else { "released" }
        );
        let key = match button {
            0 => Key::BTN_STYLUS,
            1 => Key::BTN_STYLUS2,
            _ => return Ok(()), // Ignore unknown buttons
        };

        // Ensure proximity is established first (this will announce the tool if needed)
        if !self.in_proximity {
            self.enter_proximity()?;
        }

        // Just send the button state - no need to re-send BTN_TOOL_PEN or MSC_SERIAL
        self.emit_pen(&[EvdevEvent::new(
            EventType::KEY,
            key.0,
            if pressed { 1 } else { 0 },
        )])?;
        self.sync_pen()
    }

    fn find_slot_for_id(&mut self, id: u32) -> Option<u8> {
        self.touch_slots
            .iter()
            .position(|s| s.active && s.tracking_id == id as i32)
            .map(|i| i as u8)
    }

    fn find_free_slot(&mut self) -> Option<u8> {
        self.touch_slots
            .iter()
            .position(|s| !s.active)
            .map(|i| i as u8)
    }

    fn touch_down(&mut self, id: u32, x: f32, y: f32) -> Result<(), TabletError> {
        // A finger already down under this id never got its up (the tablet lost it):
        // lift it, or it stays on the touchpad and every gesture counts one finger more.
        if let Some(stale) = self.find_slot_for_id(id) {
            self.touch_slots[stale as usize].active = false;
        }
        let slot_idx = match self.find_free_slot() {
            Some(s) => s,
            None => return Ok(()), // No free slots, ignore
        };

        debug!(
            "Touch down (Internal): id={} slot={} pos=({}, {})",
            id, slot_idx, x, y
        );

        let abs_x = normalize_to_abs(x, ABS_MAX);
        let abs_y = normalize_to_abs(y, ABS_MAX);

        self.touch_slots[slot_idx as usize] = TouchSlot {
            tracking_id: id as i32,
            x: abs_x,
            y: abs_y,
            active: true,
            reported_to_os: false,
        };
        self.touch_dirty = true;
        Ok(())
    }

    fn touch_move(&mut self, id: u32, x: f32, y: f32) -> Result<(), TabletError> {
        let slot_idx = match self.find_slot_for_id(id) {
            Some(s) => s,
            None => return Ok(()), // Unknown touch, ignore
        };

        let abs_x = normalize_to_abs(x, ABS_MAX);
        let abs_y = normalize_to_abs(y, ABS_MAX);

        self.touch_slots[slot_idx as usize].x = abs_x;
        self.touch_slots[slot_idx as usize].y = abs_y;
        self.touch_dirty = true;
        Ok(())
    }

    fn touch_up(&mut self, id: u32) -> Result<(), TabletError> {
        let slot_idx = match self.find_slot_for_id(id) {
            Some(s) => s,
            None => return Ok(()), // Unknown touch, ignore
        };

        debug!("Touch up (Internal): id={} slot={}", id, slot_idx);

        self.touch_slots[slot_idx as usize].active = false;

        // We do NOT reset reported_to_os here, because sync_touch_state_to_os
        // needs to see that it WAS reported so it can correctly send the Lift event (Tracking ID -1).
        self.touch_dirty = true;
        Ok(())
    }

    /// Sync the internal touch slot state to the OS
    fn sync_touch_state_to_os(&mut self) -> Result<(), TabletError> {
        let mut something_changed = false;

        // Iterate all slots and sync changes
        for i in 0..MT_SLOTS {
            // Extract state needed for decision
            let (is_active_internal, tracking_id, x, y, reported) = {
                let s = &self.touch_slots[i as usize];
                (s.active, s.tracking_id, s.x, s.y, s.reported_to_os)
            };

            let want_active_os = is_active_internal;

            if !reported && want_active_os {
                // HIDDEN -> SHOWING
                self.select_slot(i)?;
                self.emit_touch(&[
                    EvdevEvent::new(
                        EventType::ABSOLUTE,
                        AbsoluteAxisType::ABS_MT_TRACKING_ID.0,
                        tracking_id,
                    ),
                    EvdevEvent::new(
                        EventType::ABSOLUTE,
                        AbsoluteAxisType::ABS_MT_POSITION_X.0,
                        x,
                    ),
                    EvdevEvent::new(
                        EventType::ABSOLUTE,
                        AbsoluteAxisType::ABS_MT_POSITION_Y.0,
                        y,
                    ),
                ])?;
                self.touch_slots[i as usize].reported_to_os = true;
                something_changed = true;
            } else if reported && !want_active_os {
                // SHOWING -> HIDDEN
                self.select_slot(i)?;
                self.emit_touch(&[EvdevEvent::new(
                    EventType::ABSOLUTE,
                    AbsoluteAxisType::ABS_MT_TRACKING_ID.0,
                    -1,
                )])?;
                self.touch_slots[i as usize].reported_to_os = false;
                something_changed = true;
            } else if reported && want_active_os {
                // SHOWING -> SHOWING (Update Position)
                self.select_slot(i)?;
                self.emit_touch(&[
                    EvdevEvent::new(
                        EventType::ABSOLUTE,
                        AbsoluteAxisType::ABS_MT_POSITION_X.0,
                        x,
                    ),
                    EvdevEvent::new(
                        EventType::ABSOLUTE,
                        AbsoluteAxisType::ABS_MT_POSITION_Y.0,
                        y,
                    ),
                ])?;
                something_changed = true;
            }
        }

        if something_changed {
            let reported: Vec<&TouchSlot> = self.touch_slots.iter().filter(|s| s.reported_to_os).collect();
            let count = reported.len();
            // Single-touch axes follow the first finger, as a real touchpad's do.
            let mut events: Vec<EvdevEvent> = reported
                .first()
                .map(|s| {
                    vec![
                        EvdevEvent::new(EventType::ABSOLUTE, AbsoluteAxisType::ABS_X.0, s.x),
                        EvdevEvent::new(EventType::ABSOLUTE, AbsoluteAxisType::ABS_Y.0, s.y),
                    ]
                })
                .unwrap_or_default();
            events.extend([
                EvdevEvent::new(EventType::KEY, Key::BTN_TOUCH.0, (count > 0) as i32),
                EvdevEvent::new(EventType::KEY, Key::BTN_TOOL_FINGER.0, (count == 1) as i32),
                EvdevEvent::new(EventType::KEY, Key::BTN_TOOL_DOUBLETAP.0, (count == 2) as i32),
                EvdevEvent::new(EventType::KEY, Key::BTN_TOOL_TRIPLETAP.0, (count == 3) as i32),
                EvdevEvent::new(EventType::KEY, Key::BTN_TOOL_QUADTAP.0, (count >= 4) as i32),
            ]);
            self.emit_touch(&events)?;
            self.sync_touch()?;
        }

        Ok(())
    }

    fn select_slot(&mut self, slot: u8) -> Result<(), TabletError> {
        if slot >= MT_SLOTS {
            return Err(TabletError::InvalidSlot {
                slot,
                max: MT_SLOTS - 1,
            });
        }

        if self.current_slot != slot {
            self.current_slot = slot;
            self.emit_touch(&[EvdevEvent::new(
                EventType::ABSOLUTE,
                AbsoluteAxisType::ABS_MT_SLOT.0,
                slot as i32,
            )])?;
        }
        Ok(())
    }

    fn emit_pen(&mut self, events: &[EvdevEvent]) -> Result<(), TabletError> {
        tracing::debug!("Emitting pen events: {:?}", events);
        self.pen_device.emit(events).map_err(TabletError::EventEmit)
    }

    fn sync_pen(&mut self) -> Result<(), TabletError> {
        self.emit_pen(&[EvdevEvent::new(
            EventType::SYNCHRONIZATION,
            Synchronization::SYN_REPORT.0,
            0,
        )])
    }

    fn emit_touch(&mut self, events: &[EvdevEvent]) -> Result<(), TabletError> {
        self.touch_out.extend_from_slice(events);
        Ok(())
    }

    /// Send the built frame; `emit` adds the SYN_REPORT.
    fn sync_touch(&mut self) -> Result<(), TabletError> {
        let events = std::mem::take(&mut self.touch_out);
        tracing::debug!("Touch frame: {:?}", events);
        self.touch_device.emit(&events).map_err(TabletError::EventEmit)
    }
}
