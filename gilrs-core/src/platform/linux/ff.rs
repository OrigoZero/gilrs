// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use std::fs::File;
use std::io::{Error as IoError, Result as IoResult, Write};
use std::os::unix::io::AsRawFd;
use std::{mem, slice};

use super::ioctl::{
    self, ff_condition_effect, ff_constant_effect, ff_effect, ff_envelope, ff_replay,
    ff_rumble_effect, input_event,
};
use crate::{ForceCondition, ForceEffect, ForceKind};
use nix::errno::Errno;
use std::time::Duration;

#[derive(Debug)]
pub struct Device {
    effect: i16,
    file: File,
}

impl Device {
    pub(crate) fn new(path: &str) -> IoResult<Self> {
        let file = File::create(path)?;
        let mut effect = ff_effect {
            type_: FF_RUMBLE,
            id: -1,
            direction: 0,
            trigger: Default::default(),
            replay: Default::default(),
            u: Default::default(),
        };

        #[allow(clippy::unnecessary_mut_passed)]
        let res = unsafe { ioctl::eviocsff(file.as_raw_fd(), &mut effect) };

        if res.is_err() {
            Err(IoError::other("Failed to create effect"))
        } else {
            Ok(Device {
                effect: effect.id,
                file,
            })
        }
    }

    pub fn set_ff_state(&mut self, strong: u16, weak: u16, min_duration: Duration) {
        let duration = min_duration.as_secs() * 1000 + u64::from(min_duration.subsec_millis());
        let duration = if duration > u64::from(u16::MAX) {
            u16::MAX
        } else {
            duration as u16
        };

        let mut effect = ff_effect {
            type_: FF_RUMBLE,
            id: self.effect,
            direction: 0,
            trigger: Default::default(),
            replay: ff_replay {
                delay: 0,
                length: duration,
            },
            u: Default::default(),
        };

        unsafe {
            let rumble = &mut effect.u as *mut _ as *mut ff_rumble_effect;
            (*rumble).strong_magnitude = strong;
            (*rumble).weak_magnitude = weak;

            if let Err(err) = ioctl::eviocsff(self.file.as_raw_fd(), &effect) {
                error!(
                    "Failed to modify effect of gamepad {:?}, error: {}",
                    self.file, err
                );

                return;
            }
        };

        let time = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let ev = input_event {
            type_: EV_FF,
            code: self.effect as u16,
            value: 1,
            time,
        };

        let size = mem::size_of::<input_event>();
        let s = unsafe { slice::from_raw_parts(&ev as *const _ as *const u8, size) };

        match self.file.write(s) {
            Ok(s) if s == size => (),
            Ok(_) => unreachable!(),
            Err(e) => error!("Failed to set ff state: {}", e),
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        let effect = self.effect as ::libc::c_ulong;
        #[cfg(not(target_os = "linux"))]
        let effect = self.effect as ::libc::c_int;

        if let Err(err) = unsafe { ioctl::eviocrmff(self.file.as_raw_fd(), effect) } {
            if err != Errno::ENODEV {
                error!(
                    "Failed to remove effect of gamepad {:?}: {}",
                    self.file, err
                )
            }
        };
    }
}

/// A device opened for force-feedback effects on its own axes, holding the
/// one effect of each kind it plays. An effect plays until it is stopped,
/// replaced or the device is dropped.
#[derive(Debug)]
pub struct ForceDevice {
    file: File,
    /// The id the kernel gave the effect of each kind, in
    /// [`ForceKind::ALL`] order, while one is uploaded.
    ids: [Option<i16>; 4],
}

fn kind_slot(kind: ForceKind) -> usize {
    match kind {
        ForceKind::Constant => 0,
        ForceKind::Spring => 1,
        ForceKind::Damper => 2,
        ForceKind::Friction => 3,
    }
}

fn unit_u16(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * f32::from(u16::MAX)).round() as u16
}

fn signed_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

fn condition(c: &ForceCondition) -> ff_condition_effect {
    ff_condition_effect {
        right_saturation: unit_u16(c.saturation),
        left_saturation: unit_u16(c.saturation),
        right_coeff: signed_i16(c.coefficient),
        left_coeff: signed_i16(c.coefficient),
        deadband: unit_u16(c.deadband),
        center: signed_i16(c.center),
    }
}

/// The kernel effect `effect` describes, under `id` (`-1` for a new one).
/// Every effect plays with no end of its own (a replay length of `0`).
fn kernel_effect(effect: &ForceEffect, id: i16) -> ff_effect {
    let mut out = ff_effect {
        type_: 0,
        id,
        direction: 0,
        trigger: Default::default(),
        replay: ff_replay {
            length: 0,
            delay: 0,
        },
        u: Default::default(),
    };
    match effect {
        ForceEffect::Constant(level) => {
            out.type_ = FF_CONSTANT;
            // The kernel's force model takes a constant effect's push along
            // the first axis as sin(direction) * level, so a direction of a
            // quarter turn carries the level onto that axis unchanged.
            out.direction = 0x4000;
            let constant = ff_constant_effect {
                level: signed_i16(*level),
                envelope: ff_envelope {
                    attack_length: 0,
                    attack_level: 0,
                    fade_length: 0,
                    fade_level: 0,
                },
            };
            unsafe {
                std::ptr::write_unaligned(&mut out.u as *mut _ as *mut ff_constant_effect, constant);
            }
        }
        ForceEffect::Spring(c) | ForceEffect::Damper(c) | ForceEffect::Friction(c) => {
            out.type_ = match effect.kind() {
                ForceKind::Spring => FF_SPRING,
                ForceKind::Damper => FF_DAMPER,
                _ => FF_FRICTION,
            };
            // One condition per axis: the device's first two axes take the
            // same shape.
            let both = [condition(c), condition(c)];
            unsafe {
                std::ptr::write_unaligned(
                    &mut out.u as *mut _ as *mut [ff_condition_effect; 2],
                    both,
                );
            }
        }
    }
    out
}

impl ForceDevice {
    pub(crate) fn open(path: &str) -> IoResult<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        Ok(ForceDevice {
            file,
            ids: [None; 4],
        })
    }

    pub fn play(&mut self, effect: ForceEffect) -> IoResult<()> {
        let slot = kind_slot(effect.kind());
        let existing = self.ids[slot];
        let mut kernel = kernel_effect(&effect, existing.unwrap_or(-1));
        // The kernel writes the id it assigns a new effect back into `id`.
        #[allow(clippy::unnecessary_mut_passed)]
        unsafe { ioctl::eviocsff(self.file.as_raw_fd(), &mut kernel) }.map_err(IoError::from)?;
        if existing.is_none() {
            self.ids[slot] = Some(kernel.id);
            self.write_play(kernel.id, 1)?;
        }
        Ok(())
    }

    pub fn stop(&mut self, kind: ForceKind) -> IoResult<()> {
        let Some(id) = self.ids[kind_slot(kind)].take() else {
            return Ok(());
        };
        self.remove(id)
    }

    fn write_play(&mut self, id: i16, value: i32) -> IoResult<()> {
        let ev = input_event {
            type_: EV_FF,
            code: id as u16,
            value,
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        };
        let size = mem::size_of::<input_event>();
        let s = unsafe { slice::from_raw_parts(&ev as *const _ as *const u8, size) };
        let written = self.file.write(s)?;
        if written != size {
            return Err(IoError::other(format!(
                "the device took {written} of the {size} bytes that start an effect"
            )));
        }
        Ok(())
    }

    fn remove(&mut self, id: i16) -> IoResult<()> {
        #[cfg(target_os = "linux")]
        let effect = id as ::libc::c_ulong;
        #[cfg(not(target_os = "linux"))]
        let effect = id as ::libc::c_int;
        match unsafe { ioctl::eviocrmff(self.file.as_raw_fd(), effect) } {
            Ok(_) | Err(Errno::ENODEV) => Ok(()),
            Err(err) => Err(IoError::from(err)),
        }
    }
}

impl Drop for ForceDevice {
    fn drop(&mut self) {
        for slot in 0..self.ids.len() {
            if let Some(id) = self.ids[slot].take() {
                if let Err(err) = self.remove(id) {
                    error!("Failed to remove force effect of {:?}: {}", self.file, err);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conditions(effect: &ff_effect) -> [ff_condition_effect; 2] {
        unsafe { std::ptr::read_unaligned(&effect.u as *const _ as *const [ff_condition_effect; 2]) }
    }

    #[test]
    fn a_constant_force_carries_its_level_onto_the_first_axis() {
        let e = kernel_effect(&ForceEffect::Constant(-0.5), -1);
        assert_eq!(e.type_, FF_CONSTANT);
        assert_eq!(e.direction, 0x4000);
        assert_eq!(e.replay.length, 0);
        let c = unsafe { std::ptr::read_unaligned(&e.u as *const _ as *const ff_constant_effect) };
        assert_eq!(c.level, -16384);
    }

    #[test]
    fn a_spring_states_the_same_condition_on_both_axes() {
        let shape = ForceCondition {
            coefficient: 1.0,
            saturation: 0.5,
            center: -0.25,
            deadband: 0.1,
        };
        let e = kernel_effect(&ForceEffect::Spring(shape), 3);
        assert_eq!(e.type_, FF_SPRING);
        assert_eq!(e.id, 3);
        let [x, y] = conditions(&e);
        for axis in [x, y] {
            assert_eq!(axis.right_coeff, i16::MAX);
            assert_eq!(axis.left_coeff, i16::MAX);
            assert_eq!(axis.right_saturation, 32768);
            assert_eq!(axis.center, -8192);
            assert_eq!(axis.deadband, 6554);
        }
        assert_eq!(kernel_effect(&ForceEffect::Damper(shape), -1).type_, FF_DAMPER);
        assert_eq!(kernel_effect(&ForceEffect::Friction(shape), -1).type_, FF_FRICTION);
    }
}

const EV_FF: u16 = 0x15;
const FF_RUMBLE: u16 = 0x50;
const FF_CONSTANT: u16 = 0x52;
const FF_SPRING: u16 = 0x53;
const FF_FRICTION: u16 = 0x54;
const FF_DAMPER: u16 = 0x55;
