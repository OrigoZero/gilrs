// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use super::FfDevice;
use crate::native_ev_codes as nec;
use crate::{utils, AxisInfo, Event, EventType, PlatformError, PowerInfo};

#[cfg(feature = "serde-serialize")]
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};
use std::{mem, panic, thread};
use uuid::Uuid;
use windows::core::HSTRING;
use windows::Devices::Power::BatteryReport;
use windows::Gaming::Input::{
    GameControllerSwitchPosition, Gamepad as WgiGamepad, GamepadButtons, GamepadReading,
    RawGameController,
};
use windows::System::Power::BatteryStatus;

const SDL_HARDWARE_BUS_USB: u32 = 0x03;
// const SDL_HARDWARE_BUS_BLUETOOTH: u32 = 0x05;

// The general consensus is that standard xbox controllers poll at ~125 hz which
// means 8 ms between updates.
// Seems like a good target for how often we update the background thread.
const EVENT_THREAD_SLEEP_TIME: u64 = 8;
// Battery info only needs to be polled sparingly: once per second.
const ITERATIONS_TO_CHECK_BATTERY: u64 = 125;

const WGI_TO_GILRS_BUTTON_MAP: [(GamepadButtons, crate::EvCode); 14] = [
    (GamepadButtons::DPadUp, nec::BTN_DPAD_UP),
    (GamepadButtons::DPadDown, nec::BTN_DPAD_DOWN),
    (GamepadButtons::DPadLeft, nec::BTN_DPAD_LEFT),
    (GamepadButtons::DPadRight, nec::BTN_DPAD_RIGHT),
    (GamepadButtons::Menu, nec::BTN_START),
    (GamepadButtons::View, nec::BTN_SELECT),
    (GamepadButtons::LeftThumbstick, nec::BTN_LTHUMB),
    (GamepadButtons::RightThumbstick, nec::BTN_RTHUMB),
    (GamepadButtons::LeftShoulder, nec::BTN_LT),
    (GamepadButtons::RightShoulder, nec::BTN_RT),
    (GamepadButtons::A, nec::BTN_SOUTH),
    (GamepadButtons::B, nec::BTN_EAST),
    (GamepadButtons::X, nec::BTN_WEST),
    (GamepadButtons::Y, nec::BTN_NORTH),
];

/// This is similar to `gilrs_core::Event` but with a `non_roamable_id`.
#[derive(Debug)]
struct WgiEvent {
    non_roamable_id: HSTRING,
    event: WgiEventType,
    pub time: SystemTime,
}

impl WgiEvent {
    fn new(non_roamable_id: HSTRING, event: WgiEventType) -> Self {
        let time = utils::time_now();
        WgiEvent {
            non_roamable_id,
            event,
            time,
        }
    }
}

#[derive(Debug)]
enum WgiEventType {
    Connected(RawGameController),
    Standard(EventType),
}

impl From<EventType> for WgiEventType {
    fn from(value: EventType) -> Self {
        Self::Standard(value)
    }
}

#[derive(Debug)]
pub struct Gilrs {
    gamepads: Vec<Gamepad>,
    rx: Receiver<WgiEvent>,
    join_handle: Option<JoinHandle<()>>,
    stop_tx: Sender<()>,
}

impl Gilrs {
    pub(crate) fn new() -> Result<Self, PlatformError> {
        let raw_game_controllers = RawGameController::RawGameControllers()
            .map_err(|e| PlatformError::Other(Box::new(e)))?;
        let count = raw_game_controllers
            .Size()
            .map_err(|e| PlatformError::Other(Box::new(e)))?;
        // Don't use RawGameControllers.into_iter() here as it
        // triggers a crash when the app is run through Steam.
        // https://gitlab.com/gilrs-project/gilrs/-/issues/132
        let gamepads: Vec<_> = (0..count)
            .filter_map(|i| {
                let controller = raw_game_controllers.GetAt(i).ok()?;
                let non_roamable_id = controller.NonRoamableId().ok()?;
                Some((controller, non_roamable_id))
            })
            .enumerate()
            .map(|(id, (controller, non_roamable_id))| {
                Gamepad::new(id as u32, controller, non_roamable_id)
            })
            .collect();

        let (tx, rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel();
        let join_handle = Some(Self::spawn_thread(tx, stop_rx, &gamepads));
        Ok(Gilrs {
            gamepads,
            rx,
            join_handle,
            stop_tx,
        })
    }

    fn spawn_thread(
        tx: Sender<WgiEvent>,
        stop_rx: Receiver<()>,
        gamepads: &[Gamepad],
    ) -> JoinHandle<()> {
        let mut old_controllers =
            HashSet::from_iter(gamepads.iter().map(|g| g.non_roamable_id.clone()));

        std::thread::Builder::new()
            .name("gilrs".to_owned())
            .spawn(move || {
                // To avoid allocating every update, store old and new readings for every controller
                // and swap their memory. Also tracks last reported power state per controller.
                let mut readings: HashMap<HSTRING, (Reading, Reading, PowerInfo)> = HashMap::new();
                // We will also swap the memory of old and new controller ID's.
                let mut new_controllers: HashSet<HSTRING> = Default::default();
                let mut last_failed_get_id: Option<Instant> = None;
                let mut counter: u64 = 0;

                loop {
                    match stop_rx.try_recv() {
                        Ok(_) => break,
                        Err(TryRecvError::Disconnected) => {
                            warn!("stop_rx channel disconnected prematurely");
                            break;
                        }
                        Err(TryRecvError::Empty) => {}
                    }

                    // Don't use RawGameControllers.into_iter() here as it
                    // triggers a crash when the app is run through Steam.
                    // https://gitlab.com/gilrs-project/gilrs/-/issues/132
                    let Ok(raw_game_controllers) = RawGameController::RawGameControllers() else {
                        // Try again later
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    };
                    let Ok(count) = raw_game_controllers.Size() else {
                        // Try again later
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    };
                    let controllers = (0..count).filter_map(|i| {
                        let controller = raw_game_controllers.GetAt(i).ok()?;
                        match controller.NonRoamableId() {
                            Ok(non_roamable_id) => Some((controller, non_roamable_id)),
                            Err(e) => {
                                if last_failed_get_id.map_or(true, |x| x.elapsed().as_secs() > 59) {
                                    error!(
                                        "Failed to get gamepad id: {e}! Skipping reading events \
                                         for this gamepad."
                                    );
                                    last_failed_get_id = Some(Instant::now());
                                }
                                None
                            }
                        }
                    });

                    new_controllers.clear();

                    for (controller, non_roamable_id) in controllers {
                        new_controllers.insert(non_roamable_id.clone());

                        // Find readings for this controller or insert new ones.
                        let (old_reading, new_reading, last_power) =
                            readings.entry(non_roamable_id.clone()).or_insert_with(|| {
                                let reading = match WgiGamepad::FromGameController(&controller) {
                                    Ok(wgi_gamepad) => {
                                        Reading::Gamepad(wgi_gamepad.GetCurrentReading().unwrap())
                                    }
                                    _ => Reading::Raw(RawGamepadReading::new(&controller).unwrap()),
                                };
                                (reading.clone(), reading, read_power_info(&controller))
                            });

                        // Make last update's reading the old reading and get a new one.
                        std::mem::swap(old_reading, new_reading);
                        if let Err(e) = new_reading.update(&controller) {
                            if e.code().is_err() {
                                error!("Reading::update() function failed with {e}");
                            }
                        }

                        if old_controllers.contains(&non_roamable_id) {
                            if counter % ITERATIONS_TO_CHECK_BATTERY == 0 {
                                let current = read_power_info(&controller);
                                if current != *last_power {
                                    *last_power = current;
                                    _ = tx.send(WgiEvent::new(
                                        non_roamable_id.clone(),
                                        EventType::BatteryChanged(current).into(),
                                    ));
                                }
                            }
                        } else {
                            _ = tx.send(WgiEvent::new(
                                non_roamable_id.clone(),
                                WgiEventType::Connected(controller.clone()),
                            ));
                            _ = tx.send(WgiEvent::new(
                                non_roamable_id.clone(),
                                EventType::BatteryChanged(read_power_info(&controller)).into(),
                            ));
                        }

                        // Skip if this is the same reading as the last one.
                        if old_reading.time() == new_reading.time() {
                            continue;
                        }

                        Reading::send_events_for_differences(
                            old_reading,
                            new_reading,
                            non_roamable_id,
                            &tx,
                        );
                    }

                    for controller in old_controllers.difference(&new_controllers) {
                        _ = tx.send(WgiEvent::new(
                            controller.clone(),
                            EventType::Disconnected.into(),
                        ));
                    }

                    mem::swap(&mut old_controllers, &mut new_controllers);
                    counter = counter.wrapping_add(1);
                    thread::sleep(Duration::from_millis(EVENT_THREAD_SLEEP_TIME));
                }
            })
            .expect("failed to spawn thread")
    }

    pub(crate) fn next_event(&mut self) -> Option<Event> {
        self.rx
            .try_recv()
            .ok()
            .map(|wgi_event: WgiEvent| self.handle_event(wgi_event))
    }

    pub(crate) fn next_event_blocking(&mut self, timeout: Option<Duration>) -> Option<Event> {
        if let Some(timeout) = timeout {
            self.rx
                .recv_timeout(timeout)
                .ok()
                .map(|wgi_event: WgiEvent| self.handle_event(wgi_event))
        } else {
            self.rx
                .recv()
                .ok()
                .map(|wgi_event: WgiEvent| self.handle_event(wgi_event))
        }
    }

    fn handle_event(&mut self, wgi_event: WgiEvent) -> Event {
        let id = self
            .gamepads
            .iter()
            .position(|gamepad| wgi_event.non_roamable_id == gamepad.non_roamable_id);

        match id {
            Some(id) => match wgi_event.event {
                WgiEventType::Connected(..) => {
                    self.gamepads[id].is_connected = true;
                    Event {
                        id,
                        event: EventType::Connected,
                        time: wgi_event.time,
                    }
                }
                WgiEventType::Standard(event) => match event {
                    EventType::Disconnected => {
                        self.gamepads[id].is_connected = false;
                        Event {
                            id,
                            event,
                            time: wgi_event.time,
                        }
                    }
                    _ => Event {
                        id,
                        event,
                        time: wgi_event.time,
                    },
                },
            },
            None => match wgi_event.event {
                WgiEventType::Connected(raw_game_controller) => {
                    self.gamepads.push(Gamepad::new(
                        self.gamepads.len() as u32,
                        raw_game_controller,
                        wgi_event.non_roamable_id,
                    ));
                    Event {
                        id: self.gamepads.len() - 1,
                        event: EventType::Connected,
                        time: wgi_event.time,
                    }
                }
                WgiEventType::Standard(other) => panic!(
                    "Newly connected controllers must use WgiEventType::Connected. Found {:?} instead.",
                    other
                ),
            },
        }
    }

    pub fn gamepad(&self, id: usize) -> Option<&Gamepad> {
        self.gamepads.get(id)
    }

    pub fn last_gamepad_hint(&self) -> usize {
        self.gamepads.len()
    }
}

impl Drop for Gilrs {
    fn drop(&mut self) {
        if let Err(e) = self.stop_tx.send(()) {
            warn!("Failed to send stop signal to thread: {e:?}");
        }
        if let Err(e) = self.join_handle.take().unwrap().join() {
            warn!("Failed to join thread: {e:?}");
        }
    }
}

fn read_power_info(controller: &RawGameController) -> PowerInfo {
    read_power_info_err(controller).unwrap_or(PowerInfo::Unknown)
}

fn read_power_info_err(controller: &RawGameController) -> windows::core::Result<PowerInfo> {
    if !controller.IsWireless()? {
        return Ok(PowerInfo::Wired);
    }
    let report: BatteryReport = controller.TryGetBatteryReport()?;
    let status: BatteryStatus = report.Status()?;

    let power_info = match status {
        BatteryStatus::Discharging | BatteryStatus::Charging => {
            let full = report.FullChargeCapacityInMilliwattHours()?.GetInt32()? as f32;
            let remaining = report.RemainingCapacityInMilliwattHours()?.GetInt32()? as f32;
            let percent: u8 = ((remaining / full) * 100.0) as u8;
            match status {
                _ if percent == 100 => PowerInfo::Charged,
                BatteryStatus::Discharging => PowerInfo::Discharging(percent),
                BatteryStatus::Charging => PowerInfo::Charging(percent),
                _ => unreachable!(),
            }
        }
        BatteryStatus::NotPresent => PowerInfo::Wired,
        BatteryStatus::Idle => PowerInfo::Charged,
        BatteryStatus(_) => PowerInfo::Unknown,
    };
    Ok(power_info)
}

#[derive(Debug, Clone)]
struct RawGamepadReading {
    axes: Vec<f64>,
    buttons: Vec<bool>,
    switches: Vec<GameControllerSwitchPosition>,
    time: u64,
}

impl RawGamepadReading {
    fn new(raw_game_controller: &RawGameController) -> windows::core::Result<Self> {
        let axis_count = raw_game_controller.AxisCount()? as usize;
        let button_count = raw_game_controller.ButtonCount()? as usize;
        let switch_count = raw_game_controller.SwitchCount()? as usize;
        let mut new = Self {
            axes: vec![0.0; axis_count],
            buttons: vec![false; button_count],
            switches: vec![GameControllerSwitchPosition::default(); switch_count],
            time: 0,
        };
        new.time = raw_game_controller.GetCurrentReading(
            &mut new.buttons,
            &mut new.switches,
            &mut new.axes,
        )?;
        Ok(new)
    }

    fn update(&mut self, raw_game_controller: &RawGameController) -> windows::core::Result<()> {
        self.time = raw_game_controller.GetCurrentReading(
            &mut self.buttons,
            &mut self.switches,
            &mut self.axes,
        )?;
        Ok(())
    }
}

/// Treats switches like a two axes similar to a Directional pad.
/// Returns a tuple containing the values of the x and y axis.
/// Value's range is -1 to 1.
fn direction_from_switch(switch: GameControllerSwitchPosition) -> (i32, i32) {
    match switch {
        GameControllerSwitchPosition::Up => (0, 1),
        GameControllerSwitchPosition::Down => (0, -1),
        GameControllerSwitchPosition::Right => (1, 0),
        GameControllerSwitchPosition::Left => (-1, 0),
        GameControllerSwitchPosition::UpLeft => (-1, 1),
        GameControllerSwitchPosition::UpRight => (1, 1),
        GameControllerSwitchPosition::DownLeft => (-1, -1),
        GameControllerSwitchPosition::DownRight => (1, -1),
        _ => (0, 0),
    }
}

#[derive(Clone)]
enum Reading {
    Raw(RawGamepadReading),
    Gamepad(GamepadReading),
}

impl Reading {
    fn time(&self) -> u64 {
        match self {
            Reading::Raw(r) => r.time,
            Reading::Gamepad(r) => r.Timestamp,
        }
    }

    fn update(&mut self, controller: &RawGameController) -> windows::core::Result<()> {
        match self {
            Reading::Raw(raw_reading) => {
                raw_reading.update(controller)?;
            }
            Reading::Gamepad(gamepad_reading) => {
                let gamepad: WgiGamepad = WgiGamepad::FromGameController(controller)?;
                *gamepad_reading = gamepad.GetCurrentReading()?;
            }
        }
        Ok(())
    }

    fn send_events_for_differences(
        old: &Self,
        new: &Self,
        non_roamable_id: HSTRING,
        tx: &Sender<WgiEvent>,
    ) {
        match (old, new) {
            // WGI RawGameController
            (Reading::Raw(old), Reading::Raw(new)) => {
                // Axis changes
                for index in 0..new.axes.len() {
                    if old.axes.get(index) != new.axes.get(index) {
                        // https://github.com/libsdl-org/SDL/blob/6af17369ca773155bd7f39b8801725c4a6d52e4f/src/joystick/windows/SDL_windows_gaming_input.c#L863
                        let value = ((new.axes[index] * 65535.0) - 32768.0) as i32;
                        let event_type = EventType::AxisValueChanged(
                            value,
                            crate::EvCode(EvCode {
                                kind: EvCodeKind::Axis,
                                index: index as u32,
                            }),
                        );
                        _ = tx.send(WgiEvent::new(non_roamable_id.clone(), event_type.into()));
                    }
                }

                for index in 0..new.buttons.len() {
                    if old.buttons.get(index) != new.buttons.get(index) {
                        let event_type = match new.buttons[index] {
                            true => EventType::ButtonPressed(crate::EvCode(EvCode {
                                kind: EvCodeKind::Button,
                                index: index as u32,
                            })),
                            false => EventType::ButtonReleased(crate::EvCode(EvCode {
                                kind: EvCodeKind::Button,
                                index: index as u32,
                            })),
                        };
                        _ = tx.send(WgiEvent::new(non_roamable_id.clone(), event_type.into()));
                    }
                }

                for index in 0..old.switches.len() {
                    let (old_x, old_y) = direction_from_switch(old.switches[index]);
                    let (new_x, new_y) = direction_from_switch(new.switches[index]);
                    if old_x != new_x {
                        let event_type = EventType::AxisValueChanged(
                            new_x,
                            crate::EvCode(EvCode {
                                kind: EvCodeKind::Switch,
                                index: (index * 2) as u32,
                            }),
                        );
                        _ = tx.send(WgiEvent::new(non_roamable_id.clone(), event_type.into()));
                    }
                    if old_y != new_y {
                        let event_type = EventType::AxisValueChanged(
                            -new_y,
                            crate::EvCode(EvCode {
                                kind: EvCodeKind::Switch,
                                index: (index * 2) as u32 + 1,
                            }),
                        );
                        _ = tx.send(WgiEvent::new(non_roamable_id.clone(), event_type.into()));
                    }
                }
            }
            // WGI Gamepad
            (Reading::Gamepad(old), Reading::Gamepad(new)) => {
                #[rustfmt::skip]
                let axes = [
                    (new.LeftTrigger, old.LeftTrigger, nec::AXIS_LT2, 1.0),
                    (new.RightTrigger, old.RightTrigger, nec::AXIS_RT2, 1.0),
                    (new.LeftThumbstickX, old.LeftThumbstickX, nec::AXIS_LSTICKX, 1.0),
                    (new.LeftThumbstickY, old.LeftThumbstickY, nec::AXIS_LSTICKY, -1.0),
                    (new.RightThumbstickX, old.RightThumbstickX, nec::AXIS_RSTICKX, 1.0),
                    (new.RightThumbstickY, old.RightThumbstickY, nec::AXIS_RSTICKY, -1.0),
                ];
                for (new, old, code, multiplier) in axes {
                    if new != old {
                        _ = tx.send(WgiEvent::new(
                            non_roamable_id.clone(),
                            EventType::AxisValueChanged(
                                (multiplier * new * i32::MAX as f64) as i32,
                                code,
                            )
                            .into(),
                        ));
                    }
                }

                for (current_button, ev_code) in WGI_TO_GILRS_BUTTON_MAP {
                    if (new.Buttons & current_button) != (old.Buttons & current_button) {
                        _ = match new.Buttons & current_button != GamepadButtons::None {
                            true => tx.send(WgiEvent::new(
                                non_roamable_id.clone(),
                                EventType::ButtonPressed(ev_code).into(),
                            )),
                            false => tx.send(WgiEvent::new(
                                non_roamable_id.clone(),
                                EventType::ButtonReleased(ev_code).into(),
                            )),
                        };
                    }
                }
            }
            (a, b) => {
                warn!(
                    "WGI Controller changed from gamepad: {} to gamepad: {}. Could not compare \
                     last update.",
                    a.is_gamepad(),
                    b.is_gamepad()
                );
                #[cfg(debug_assertions)]
                panic!(
                    "Controllers shouldn't change type between updates, likely programmer error"
                );
            }
        }
    }

    fn is_gamepad(&self) -> bool {
        matches!(self, Reading::Gamepad(_))
    }
}

#[derive(Debug)]
pub struct Gamepad {
    id: u32,
    name: String,
    uuid: Uuid,
    is_connected: bool,
    /// This is the generic controller handle without any mappings
    /// https://learn.microsoft.com/en-us/uwp/api/windows.gaming.input.rawgamecontroller
    raw_game_controller: RawGameController,
    /// An ID for this device that will survive disconnects and restarts.
    /// [NonRoamableIds](https://learn.microsoft.com/en-us/uwp/api/windows.gaming.input.rawgamecontroller.nonroamableid)
    ///
    /// Changes if plugged into a different port and is not the same between different applications
    /// or PCs.
    non_roamable_id: HSTRING,
    /// If the controller has a [Gamepad](https://learn.microsoft.com/en-us/uwp/api/windows.gaming.input.gamepad?view=winrt-22621)
    /// mapping, this is used to access the mapped values.
    wgi_gamepad: Option<WgiGamepad>,
    axes: Option<Vec<EvCode>>,
    buttons: Option<Vec<EvCode>>,
}

impl Gamepad {
    fn new(id: u32, raw_game_controller: RawGameController, non_roamable_id: HSTRING) -> Gamepad {
        let is_connected = true;

        // See if we can cast this to a windows definition of a gamepad
        let wgi_gamepad = WgiGamepad::FromGameController(&raw_game_controller).ok();
        let name = match raw_game_controller.DisplayName() {
            Ok(hstring) => hstring.to_string_lossy(),
            Err(_) => "unknown".to_string(),
        };

        let uuid = match wgi_gamepad.is_some() {
            true => Uuid::nil(),
            false => {
                let vendor_id = raw_game_controller.HardwareVendorId().unwrap_or(0).to_be();
                let product_id = raw_game_controller.HardwareProductId().unwrap_or(0).to_be();
                let version = 0;

                // SDL uses the SDL_HARDWARE_BUS_BLUETOOTH bustype for IsWireless devices:
                // https://github.com/libsdl-org/SDL/blob/294ccba0a23b37fffef62189423444f93732e565/src/joystick/windows/SDL_windows_gaming_input.c#L335-L338
                // In my testing though, it caused my controllers to not find mappings.
                // SDL only uses their WGI implementation for UWP apps so I guess it hasn't been
                // used enough for people to submit mappings with the different bustype.
                let bustype = SDL_HARDWARE_BUS_USB.to_be();

                Uuid::from_fields(
                    bustype,
                    vendor_id,
                    0,
                    &[
                        (product_id >> 8) as u8,
                        product_id as u8,
                        0,
                        0,
                        (version >> 8) as u8,
                        version as u8,
                        0,
                        0,
                    ],
                )
            }
        };

        let mut gamepad = Gamepad {
            id,
            name,
            uuid,
            is_connected,
            raw_game_controller,
            non_roamable_id,
            wgi_gamepad,
            axes: None,
            buttons: None,
        };

        if gamepad.wgi_gamepad.is_none() {
            gamepad.collect_axes_and_buttons();
        }

        gamepad
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn uuid(&self) -> Uuid {
        self.uuid
    }

    pub fn vendor_id(&self) -> Option<u16> {
        self.raw_game_controller.HardwareVendorId().ok()
    }

    pub fn product_id(&self) -> Option<u16> {
        self.raw_game_controller.HardwareProductId().ok()
    }

    pub fn is_connected(&self) -> bool {
        self.is_connected
    }

    pub fn power_info(&self) -> PowerInfo {
        read_power_info(&self.raw_game_controller)
    }

    pub fn is_ff_supported(&self) -> bool {
        self.wgi_gamepad.is_some()
            && self
                .raw_game_controller
                .ForceFeedbackMotors()
                .ok()
                .map(|motors| motors.First())
                .is_some()
    }

    pub fn ff_device(&self) -> Option<FfDevice> {
        Some(FfDevice::new(self.id, self.wgi_gamepad.clone()))
    }

    pub fn buttons(&self) -> &[EvCode] {
        match &self.buttons {
            None => &native_ev_codes::BUTTONS,
            Some(buttons) => buttons,
        }
    }

    pub fn axes(&self) -> &[EvCode] {
        match &self.axes {
            None => &native_ev_codes::AXES,
            Some(axes) => axes,
        }
    }

    pub(crate) fn axis_info(&self, nec: EvCode) -> Option<&AxisInfo> {
        // If it isn't a Windows "Gamepad" then return what we want SDL mappings to be able to use
        if self.wgi_gamepad.is_none() {
            return match nec.kind {
                EvCodeKind::Button => None,
                EvCodeKind::Axis => Some(&AxisInfo {
                    min: i16::MIN as i32,
                    max: i16::MAX as i32,
                    deadzone: None,
                }),
                EvCodeKind::Switch => Some(&AxisInfo {
                    min: -1,
                    max: 1,
                    deadzone: None,
                }),
            };
        }

        // For Windows Gamepads, the triggers are 0.0 to 1.0 and the thumbsticks are -1.0 to 1.0
        // https://learn.microsoft.com/en-us/uwp/api/windows.gaming.input.gamepadreading#fields
        // Since Gilrs processes axis data as integers, the input has already been multiplied by
        // i32::MAX in the joy_value method.
        match nec {
            native_ev_codes::AXIS_LT2 | native_ev_codes::AXIS_RT2 => Some(&AxisInfo {
                min: 0,
                max: i32::MAX,
                deadzone: None,
            }),
            _ => Some(&AxisInfo {
                min: i32::MIN,
                max: i32::MAX,
                deadzone: None,
            }),
        }
    }

    fn collect_axes_and_buttons(&mut self) {
        let axis_count = self.raw_game_controller.AxisCount().unwrap() as u32;
        let button_count = self.raw_game_controller.ButtonCount().unwrap() as u32;
        let switch_count = self.raw_game_controller.SwitchCount().unwrap() as u32;
        self.buttons = Some(
            (0..button_count)
                .map(|index| EvCode {
                    kind: EvCodeKind::Button,
                    index,
                })
                .collect(),
        );
        self.axes = Some(
            (0..axis_count)
                .map(|index| EvCode {
                    kind: EvCodeKind::Axis,
                    index,
                })
                .chain(
                    // Treat switches as two axes
                    (0..switch_count).flat_map(|index| {
                        [
                            EvCode {
                                kind: EvCodeKind::Switch,
                                index: index * 2,
                            },
                            EvCode {
                                kind: EvCodeKind::Switch,
                                index: (index * 2) + 1,
                            },
                        ]
                    }),
                )
                .collect(),
        );
    }
}

#[cfg_attr(feature = "serde-serialize", derive(Serialize, Deserialize))]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum EvCodeKind {
    Button = 0,
    Axis,
    Switch,
}

impl Display for EvCodeKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            EvCodeKind::Button => "Button",
            EvCodeKind::Axis => "Axis",
            EvCodeKind::Switch => "Switch",
        }
        .fmt(f)
    }
}

#[cfg_attr(feature = "serde-serialize", derive(Serialize, Deserialize))]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EvCode {
    kind: EvCodeKind,
    index: u32,
}

impl EvCode {
    pub fn into_u32(self) -> u32 {
        ((self.kind as u32) << 16) | self.index
    }
}

impl Display for EvCode {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        write!(f, "{}({})", self.kind, self.index)
    }
}

pub mod native_ev_codes {
    use super::{EvCode, EvCodeKind};

    pub const AXIS_LSTICKX: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 0,
    };
    pub const AXIS_LSTICKY: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 1,
    };
    pub const AXIS_RSTICKX: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 2,
    };
    pub const AXIS_LT2: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 3,
    };
    pub const AXIS_RT2: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 4,
    };
    pub const AXIS_RSTICKY: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 5,
    };
    pub const AXIS_RT: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 6,
    };
    pub const AXIS_LT: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 7,
    };
    pub const AXIS_LEFTZ: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 8,
    };
    pub const AXIS_RIGHTZ: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 9,
    };

    pub const AXIS_DPADX: EvCode = EvCode {
        kind: EvCodeKind::Switch,
        index: 0,
    };
    pub const AXIS_DPADY: EvCode = EvCode {
        kind: EvCodeKind::Switch,
        index: 1,
    };

    pub const BTN_WEST: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 0,
    };
    pub const BTN_SOUTH: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 1,
    };
    pub const BTN_EAST: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 2,
    };
    pub const BTN_NORTH: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 3,
    };
    pub const BTN_LT: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 4,
    };
    pub const BTN_RT: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 5,
    };
    pub const BTN_LT2: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 6,
    };
    pub const BTN_RT2: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 7,
    };
    pub const BTN_SELECT: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 8,
    };
    pub const BTN_START: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 9,
    };
    pub const BTN_LTHUMB: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 10,
    };
    pub const BTN_RTHUMB: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 11,
    };
    pub const BTN_MODE: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 12,
    };
    pub const BTN_C: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 13,
    };
    pub const BTN_Z: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 14,
    };

    // The DPad for DS4 controllers is a hat/switch that gets mapped to the DPad native event
    // code buttons. These "buttons" don't exist on the DS4 controller, so it doesn't matter
    // what the index is, but if it overlaps with an existing button it will send the event
    // for the overlapping button as a dpad button instead of unknown.
    // By using a large index it should avoid this.
    pub const BTN_DPAD_UP: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: u32::MAX - 3,
    };
    pub const BTN_DPAD_RIGHT: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: u32::MAX - 2,
    };
    pub const BTN_DPAD_DOWN: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: u32::MAX - 1,
    };
    pub const BTN_DPAD_LEFT: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: u32::MAX,
    };

    pub(super) static BUTTONS: [EvCode; 14] = [
        BTN_WEST,
        BTN_SOUTH,
        BTN_EAST,
        BTN_NORTH,
        BTN_LT,
        BTN_RT,
        BTN_SELECT,
        BTN_START,
        BTN_LTHUMB,
        BTN_RTHUMB,
        BTN_DPAD_UP,
        BTN_DPAD_RIGHT,
        BTN_DPAD_DOWN,
        BTN_DPAD_LEFT,
    ];

    pub(super) static AXES: [EvCode; 6] = [
        AXIS_LSTICKX,
        AXIS_LSTICKY,
        AXIS_RSTICKX,
        AXIS_LT2,
        AXIS_RT2,
        AXIS_RSTICKY,
    ];
}
