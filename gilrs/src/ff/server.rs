// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use super::effect_source::{DistanceModel, EffectSource, EffectState, Magnitude};
use super::time::{Repeat, Ticks, TICK_DURATION};

use std::ops::{Deref, DerefMut};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::gamepad::GamepadId;
use crate::Event;
use gilrs_core::FfDevice;

use vec_map::VecMap;

#[derive(Debug)]
pub(crate) enum Message {
    Create {
        id: usize,
        effect: Box<EffectSource>,
    },
    HandleCloned {
        id: usize,
    },
    HandleDropped {
        id: usize,
    },
    Play {
        id: usize,
    },
    Stop {
        id: usize,
    },
    Open {
        id: usize,
        device: FfDevice,
    },
    Close {
        id: usize,
    },
    SetListenerPosition {
        id: usize,
        position: [f32; 3],
    },
    SetGamepads {
        id: usize,
        gamepads: VecMap<()>,
    },
    AddGamepad {
        id: usize,
        gamepad_id: GamepadId,
    },
    SetRepeat {
        id: usize,
        repeat: Repeat,
    },
    SetDistanceModel {
        id: usize,
        model: DistanceModel,
    },
    SetPosition {
        id: usize,
        position: [f32; 3],
    },
    SetGain {
        id: usize,
        gain: f32,
    },
}

pub(crate) enum FfMessage {
    EffectCompleted { event: Event },
}

impl Message {
    // Whether to use trace level logging or debug
    fn use_trace_level(&self) -> bool {
        use self::Message::*;

        matches!(
            self,
            &SetListenerPosition { .. } | &HandleCloned { .. } | &HandleDropped { .. }
        )
    }
}

#[derive(Debug)]
struct Device {
    inner: FfDevice,
    position: [f32; 3],
}

struct Effect {
    source: EffectSource,
    /// Number of created effect's handles.
    count: usize,
}

impl Effect {
    fn inc(&mut self) -> usize {
        self.count += 1;
        self.count
    }

    fn dec(&mut self) -> usize {
        self.count -= 1;
        self.count
    }
}

impl From<EffectSource> for Effect {
    fn from(source: EffectSource) -> Self {
        Effect { source, count: 1 }
    }
}

impl Deref for Effect {
    type Target = EffectSource;

    fn deref(&self) -> &Self::Target {
        &self.source
    }
}

impl DerefMut for Effect {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.source
    }
}

impl From<FfDevice> for Device {
    fn from(inner: FfDevice) -> Self {
        Device {
            inner,
            position: [0.0, 0.0, 0.0],
        }
    }
}

/// Apply `ev` to the server's effects and devices, at `tick`.
fn handle_message(ev: Message, effects: &mut VecMap<Effect>, devices: &mut VecMap<Device>, tick: Ticks) {
    if ev.use_trace_level() {
        trace!("New ff event: {:?}", ev);
    } else {
        debug!("New ff event: {:?}", ev);
    }

    match ev {
        Message::Create { id, effect } => {
            effects.insert(id, (*effect).into());
        }
        Message::Play { id } => {
            if let Some(effect) = effects.get_mut(id) {
                effect.source.state = EffectState::Playing { since: tick }
            } else {
                error!("{:?} with wrong ID", ev);
            }
        }
        Message::Stop { id } => {
            if let Some(effect) = effects.get_mut(id) {
                effect.source.state = EffectState::Stopped
            } else {
                error!("{:?} with wrong ID", ev);
            }
        }
        Message::Open { id, device } => {
            devices.insert(id, device.into());
        }
        Message::Close { id } => {
            devices.remove(id);
        }
        Message::SetListenerPosition { id, position } => {
            if let Some(device) = devices.get_mut(id) {
                device.position = position;
            } else {
                error!("{:?} with wrong ID", ev);
            }
        }
        Message::HandleCloned { id } => {
            if let Some(effect) = effects.get_mut(id) {
                effect.inc();
            } else {
                error!("{:?} with wrong ID", ev);
            }
        }
        Message::HandleDropped { id } => {
            let mut drop = false;
            if let Some(effect) = effects.get_mut(id) {
                if effect.dec() == 0 {
                    drop = true;
                }
            } else {
                error!("{:?} with wrong ID", ev);
            }

            if drop {
                effects.remove(id);
            }
        }
        Message::SetGamepads { id, gamepads } => {
            if let Some(eff) = effects.get_mut(id) {
                eff.source.devices = gamepads;
            } else {
                error!("Invalid effect id {} when changing gamepads.", id);
            }
        }
        Message::AddGamepad { id, gamepad_id } => {
            if let Some(eff) = effects.get_mut(id) {
                eff.source.devices.insert(gamepad_id.0, ());
            } else {
                error!("Invalid effect id {} when changing gamepads.", id);
            }
        }
        Message::SetRepeat { id, repeat } => {
            if let Some(eff) = effects.get_mut(id) {
                eff.source.repeat = repeat;
            } else {
                error!("Invalid effect id {} when changing repeat mode.", id);
            }
        }
        Message::SetDistanceModel { id, model } => {
            if let Some(eff) = effects.get_mut(id) {
                eff.source.distance_model = model;
            } else {
                error!("Invalid effect id {} when changing distance model.", id);
            }
        }
        Message::SetPosition { id, position } => {
            if let Some(eff) = effects.get_mut(id) {
                eff.source.position = position;
            } else {
                error!("Invalid effect id {}.", id);
            }
        }
        Message::SetGain { id, gain } => {
            if let Some(eff) = effects.get_mut(id) {
                eff.source.gain = gain;
            } else {
                error!("Invalid effect id {} when changing effect gain.", id);
            }
        }
    }
}

/// Whether an effect is playing on a device that is open, which is what a
/// tick of the server drives. With none, a tick sets nothing but magnitudes
/// of zero, so the server waits for its next message instead.
fn plays_on_open_device(effects: &VecMap<Effect>, open: impl Fn(usize) -> bool) -> bool {
    effects.values().any(|effect| {
        matches!(effect.source.state, EffectState::Playing { .. })
            && effect.source.devices.keys().any(&open)
    })
}

/// The ticks that have passed in `elapsed`, which the server's clock advances
/// by after it waited for a message.
fn ticks_in(elapsed: Duration) -> u32 {
    (elapsed.as_millis() / u128::from(TICK_DURATION)).min(u128::from(u32::MAX)) as u32
}

pub(crate) fn run(tx: Sender<FfMessage>, rx: Receiver<Message>) {
    let mut effects = VecMap::<Effect>::new();
    let mut devices = VecMap::<Device>::new();
    let sleep_dur = Duration::from_millis(TICK_DURATION.into());
    let mut tick = Ticks(0);
    let mut completion_events = Vec::<Event>::new();

    loop {
        let t1 = Instant::now();
        while let Ok(ev) = rx.try_recv() {
            handle_message(ev, &mut effects, &mut devices, tick);
        }

        combine_and_play(&mut effects, &mut devices, tick, &mut completion_events);
        completion_events.iter().for_each(|ev| {
            let _ = tx.send(FfMessage::EffectCompleted { event: *ev });
        });
        completion_events.clear();

        // With nothing playing on an open device the tick above has set every
        // open device's magnitude to zero, and the next one would do the
        // same: the server sleeps until a message arrives, and its clock
        // then advances by the ticks that passed while it slept.
        if !plays_on_open_device(&effects, |id| devices.contains_key(id)) {
            let waited = Instant::now();
            match rx.recv() {
                Ok(ev) => {
                    tick.0 = tick.0.saturating_add(ticks_in(waited.elapsed())).saturating_add(1);
                    handle_message(ev, &mut effects, &mut devices, tick);
                    continue;
                }
                // Every sender is gone with the `Gilrs` that held them.
                Err(_) => return,
            }
        }

        let dur = Instant::now().duration_since(t1);
        if dur > sleep_dur {
            // TODO: Should we add dur - sleep_dur to next iteration's dur?
            warn!(
                "One iteration of a force feedback loop took more than {}ms!",
                TICK_DURATION
            );
        } else {
            thread::sleep(sleep_dur - dur);
        }
        tick.inc();
    }
}

pub(crate) fn init(enable: bool) -> (Sender<Message>, Receiver<FfMessage>) {
    let (tx, _rx) = mpsc::channel();
    let (_tx2, rx2) = mpsc::channel();

    // Skip setup if not enabled
    if !enable {
        return (tx, rx2);
    }

    // Wasm doesn't support threads and force feedback
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::Builder::new()
        .name("gilrs".to_owned())
        .spawn(move || run(_tx2, _rx))
        .expect("failed to spawn thread");

    (tx, rx2)
}

fn combine_and_play(
    effects: &mut VecMap<Effect>,
    devices: &mut VecMap<Device>,
    tick: Ticks,
    completion_events: &mut Vec<Event>,
) {
    for (dev_id, dev) in devices {
        let mut magnitude = Magnitude::zero();
        for (_, ref mut effect) in effects.iter_mut() {
            if effect.devices.contains_key(dev_id) {
                magnitude += effect.combine_base_effects(tick, dev.position);
                completion_events.extend(effect.flush_completion_events());
            }
        }
        trace!(
            "({:?}) Setting ff state of {:?} to {:?}",
            tick,
            dev,
            magnitude
        );
        dev.inner.set_ff_state(
            magnitude.strong,
            magnitude.weak,
            Duration::from_millis(u64::from(TICK_DURATION) * 2),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effect_on(gamepad: usize, state: EffectState) -> Effect {
        let mut on = VecMap::new();
        on.insert(gamepad, ());
        let mut source =
            EffectSource::new(Vec::new(), on, Repeat::Infinitely, DistanceModel::None, [0.0; 3], 1.0);
        source.state = state;
        source.into()
    }

    /// A tick drives a playing effect on an open device, and only that: a
    /// stopped effect, or one whose devices are all closed, leaves the server
    /// nothing to tick for.
    #[test]
    fn the_server_ticks_only_for_an_effect_playing_on_an_open_device() {
        let mut effects = VecMap::new();
        assert!(!plays_on_open_device(&effects, |_| true), "no effect");
        effects.insert(0, effect_on(3, EffectState::Stopped));
        assert!(!plays_on_open_device(&effects, |_| true), "a stopped effect");
        effects.insert(1, effect_on(4, EffectState::Playing { since: Ticks(0) }));
        assert!(!plays_on_open_device(&effects, |id| id == 3), "playing on a closed device");
        assert!(plays_on_open_device(&effects, |id| id == 4), "playing on an open device");
    }

    #[test]
    fn the_clock_advances_by_the_ticks_a_wait_took() {
        assert_eq!(ticks_in(Duration::from_millis(0)), 0);
        assert_eq!(ticks_in(Duration::from_millis(49)), 0);
        assert_eq!(ticks_in(Duration::from_millis(150)), 3);
    }

    /// The context switches the thread whose `/proc` directory is `task` has made.
    #[cfg(target_os = "linux")]
    fn context_switches(task: &std::path::Path) -> u64 {
        let status = std::fs::read_to_string(task.join("status")).expect("the thread's status reads");
        status
            .lines()
            .filter(|line| line.contains("ctxt_switches:"))
            .filter_map(|line| line.split_whitespace().last()?.parse::<u64>().ok())
            .sum()
    }

    /// With no gamepad and no effect, the force-feedback server sleeps until
    /// a message arrives: over a second it wakes about once, where a server
    /// that ticks every 50 ms wakes twenty times.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_idle_server_sleeps_until_a_message_arrives() {
        let (to_server, from_engine) = mpsc::channel::<Message>();
        let (to_engine, _from_server) = mpsc::channel::<FfMessage>();
        let (tell, task) = mpsc::channel();
        let server = thread::spawn(move || {
            // `/proc/thread-self` links to `<pid>/task/<tid>`, relative to `/proc`.
            let me = std::fs::read_link("/proc/thread-self").expect("the thread names itself");
            tell.send(std::path::Path::new("/proc").join(me)).unwrap();
            run(to_engine, from_engine);
        });
        let task = task.recv().unwrap();
        thread::sleep(Duration::from_millis(200));
        for _ in 0..2 {
            let before = context_switches(&task);
            thread::sleep(Duration::from_secs(1));
            let woke = context_switches(&task) - before;
            assert!(woke <= 2, "the idle server woke {woke} times in a second");
        }
        drop(to_server);
        server.join().expect("the server ends once its senders are gone");
    }
}
