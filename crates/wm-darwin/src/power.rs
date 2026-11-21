//! Panel brightness keys, volume keys and idle display sleep.
//!
//! Brightness is the SFDcp kext's backlight class device, read and written as
//! `kern.sf_backlight.*` sysctls. Display sleep is the renderer's business
//! (an atomic commit with the CRTC's ACTIVE property); this module decides
//! when, and which input events are spent on waking the display. The volume
//! keys are `volume`'s; they are taken out of the input here, next to the
//! brightness keys.
use crate::hid::Event;
use crate::volume::{self, CoreaudiodMixer, Mixer};
use std::cell::Cell;
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// evdev codes of the keys that step the brightness. The MacBook's built-in
/// keyboard (SFM1SPIHID) reports its top row as the plain keyboard-page
/// usages 0x3a (F1) and 0x3b (F2) whether or not Fn is held: Fn is a usage of
/// Apple's vendor top-case page, which the SFHID queue does not carry, and
/// nothing in the kernel path remaps the row.
pub const KEY_BRIGHTNESS_DOWN: u32 = 59;
pub const KEY_BRIGHTNESS_UP: u32 = 60;

/// The panel's backlight. Errors are errno values (`ENODEV` until the
/// display driver has registered it).
pub trait Backlight {
    fn get(&self, attribute: &str) -> Result<i32, i32>;
    fn set_brightness(&self, level: i32) -> Result<(), i32>;
}

/// `kern.sf_backlight.<attribute>` on SiliconForest; absent anywhere else.
pub struct SysctlBacklight;
impl Backlight for SysctlBacklight {
    #[cfg(target_os = "macos")]
    fn get(&self, attribute: &str) -> Result<i32, i32> {
        let name = std::ffi::CString::new(format!("kern.sf_backlight.{attribute}")).map_err(|_| libc::EINVAL)?;
        let mut value: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        // SAFETY: `value` and `size` describe one int; no new value is passed.
        let result = unsafe { libc::sysctlbyname(name.as_ptr(), (&mut value as *mut libc::c_int).cast(), &mut size, std::ptr::null_mut(), 0) };
        if result == 0 {
            Ok(value)
        } else {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
        }
    }
    #[cfg(target_os = "macos")]
    fn set_brightness(&self, level: i32) -> Result<(), i32> {
        let mut value: libc::c_int = level;
        // SAFETY: the new value is one int; nothing is read back.
        let result = unsafe {
            libc::sysctlbyname(
                c"kern.sf_backlight.brightness".as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                (&mut value as *mut libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>(),
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
        }
    }
    #[cfg(not(target_os = "macos"))]
    fn get(&self, _: &str) -> Result<i32, i32> {
        Err(libc::ENODEV)
    }
    #[cfg(not(target_os = "macos"))]
    fn set_brightness(&self, _: i32) -> Result<(), i32> {
        Err(libc::ENODEV)
    }
}

/// A backlight in memory with the kext's semantics, for tests.
pub struct FakeBacklight {
    pub max: i32,
    pub level: Cell<i32>,
}
impl Backlight for FakeBacklight {
    fn get(&self, attribute: &str) -> Result<i32, i32> {
        match attribute {
            "max_brightness" => Ok(self.max),
            "brightness" | "actual_brightness" => Ok(self.level.get()),
            "bl_power" => Ok(0),
            _ => Err(libc::ENOENT),
        }
    }
    fn set_brightness(&self, level: i32) -> Result<(), i32> {
        if !(0..=self.max).contains(&level) {
            return Err(libc::EINVAL);
        }
        self.level.set(level);
        Ok(())
    }
}

/// One key press: five percent of the range, as `sf-backlight up|down`.
/// Down stops at level 1, so the keys never switch the panel dark.
pub fn stepped(level: i32, max: i32, up: bool) -> i32 {
    let step = (max * 5 / 100).max(1);
    if up {
        (level + step).min(max)
    } else {
        (level - step).max(1)
    }
}

/// What became of an input event before the seat saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filtered {
    /// Deliver it as usual.
    Pass,
    /// Spent on waking the display; nobody else sees it.
    Wake,
    /// A brightness key: (level now, maximum).
    Brightness(i32, i32),
    /// A volume key: (percent now, muted).
    Volume(u8, bool),
    /// The release (or the rest) of something already spent.
    Spent,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PowerCounts {
    pub blanks: u32,
    pub wakes: u32,
    /// Input events spent on waking (the waking event and what belongs to it).
    pub swallowed: u32,
    pub brightness_steps: u32,
    pub volume_steps: u32,
}

pub struct Power {
    backlight: Box<dyn Backlight>,
    mixer: Box<dyn Mixer>,
    /// Blank the display after this long without input; `None`: never.
    pub(crate) idle: Option<Duration>,
    pub(crate) last_input: Instant,
    pub(crate) off: bool,
    pub(crate) wake_requested: bool,
    /// Tests: go through the blank state although the renderer has no display to power down.
    pub(crate) without_display: bool,
    pub(crate) unsupported: bool,
    spent_keys: HashSet<u32>,
    spent_pointer: bool,
    pub(crate) counts: PowerCounts,
    pub(crate) off_since: Option<Instant>,
}

/// `CHONKSTEP_DARWIN_IDLE_BLANK_MINUTES` (default 5, 0: never);
/// `CHONKSTEP_DARWIN_IDLE_BLANK_SECONDS` takes precedence when set.
pub fn idle_from_env() -> Result<Option<Duration>, String> {
    let parse = |name: &str, unit: u64| -> Result<Option<Option<Duration>>, String> {
        match std::env::var(name) {
            Err(_) => Ok(None),
            Ok(text) => {
                let n: u64 = text.parse().map_err(|_| format!("{name}={text:?}: expected a whole number (0: never)"))?;
                if n > 24 * 60 * 60 {
                    return Err(format!("{name}={text:?}: too long"));
                }
                Ok(Some((n > 0).then(|| Duration::from_secs(n * unit))))
            }
        }
    };
    if let Some(seconds) = parse("CHONKSTEP_DARWIN_IDLE_BLANK_SECONDS", 1)? {
        return Ok(seconds);
    }
    Ok(parse("CHONKSTEP_DARWIN_IDLE_BLANK_MINUTES", 60)?.unwrap_or(Some(Duration::from_secs(300))))
}

impl Power {
    pub fn new(idle: Option<Duration>) -> Self {
        Self {
            backlight: Box::new(SysctlBacklight),
            mixer: Box::new(CoreaudiodMixer),
            idle,
            last_input: Instant::now(),
            off: false,
            wake_requested: false,
            without_display: false,
            unsupported: false,
            spent_keys: HashSet::new(),
            spent_pointer: false,
            counts: PowerCounts::default(),
            off_since: None,
        }
    }
    pub fn set_backlight(&mut self, backlight: Box<dyn Backlight>) {
        self.backlight = backlight;
    }
    pub fn backlight(&self) -> &dyn Backlight {
        &*self.backlight
    }
    pub fn set_mixer(&mut self, mixer: Box<dyn Mixer>) {
        self.mixer = mixer;
    }
    pub fn mixer(&self) -> &dyn Mixer {
        &*self.mixer
    }
    /// Every input event passes here first. `plain`: no modifier is held.
    pub(crate) fn filter(&mut self, event: &Event, plain: bool) -> Filtered {
        self.last_input = Instant::now();
        match *event {
            Event::Key { evdev, down } => {
                if !down {
                    return if self.spent_keys.remove(&evdev) { Filtered::Spent } else { Filtered::Pass };
                }
                if self.off {
                    self.wake_requested = true;
                    self.spent_keys.insert(evdev);
                    self.counts.swallowed += 1;
                    return Filtered::Wake;
                }
                if plain && (evdev == KEY_BRIGHTNESS_DOWN || evdev == KEY_BRIGHTNESS_UP) {
                    // Without a backlight these are ordinary function keys.
                    let (Ok(level), Ok(max)) = (self.backlight.get("brightness"), self.backlight.get("max_brightness")) else {
                        return Filtered::Pass;
                    };
                    let next = stepped(level, max, evdev == KEY_BRIGHTNESS_UP);
                    self.spent_keys.insert(evdev);
                    if next != level {
                        if let Err(errno) = self.backlight.set_brightness(next) {
                            eprintln!("chonkstep-darwin: kern.sf_backlight.brightness={next} refused, errno {errno}");
                            return Filtered::Brightness(level, max);
                        }
                    }
                    self.counts.brightness_steps += 1;
                    return Filtered::Brightness(next, max);
                }
                if plain && volume::is_key(evdev) {
                    // Without the audio server these are ordinary function keys.
                    let Some(level) = volume::press(&*self.mixer, evdev) else {
                        return Filtered::Pass;
                    };
                    self.spent_keys.insert(evdev);
                    self.counts.volume_steps += 1;
                    return Filtered::Volume(volume::percent(level.volume), level.muted);
                }
                Filtered::Pass
            }
            Event::Pointer { buttons, .. } => {
                if self.off {
                    self.wake_requested = true;
                    // A press that wakes belongs to the wake until it is let go.
                    self.spent_pointer = buttons != 0;
                    self.counts.swallowed += 1;
                    return Filtered::Wake;
                }
                if self.spent_pointer {
                    if buttons != 0 {
                        return Filtered::Spent;
                    }
                    self.spent_pointer = false;
                }
                Filtered::Pass
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::Motion;
    fn key(evdev: u32, down: bool) -> Event {
        Event::Key { evdev, down }
    }
    fn pointer(buttons: u8) -> Event {
        Event::Pointer { motion: Motion::Relative(1, 0), buttons, wheel: 0 }
    }
    #[test]
    fn steps_are_five_percent_and_down_stops_at_one() {
        assert_eq!(stepped(250, 509, true), 275);
        assert_eq!(stepped(250, 509, false), 225);
        assert_eq!(stepped(500, 509, true), 509);
        assert_eq!(stepped(20, 509, false), 1);
        assert_eq!(stepped(1, 509, false), 1);
        assert_eq!(stepped(3, 10, true), 4);
    }
    #[test]
    fn brightness_keys_step_the_backlight_and_are_not_typed() {
        let mut power = Power::new(None);
        // No backlight (ENODEV): F1 and F2 are ordinary keys.
        assert_eq!(power.filter(&key(KEY_BRIGHTNESS_UP, true), true), Filtered::Pass);
        assert_eq!(power.filter(&key(KEY_BRIGHTNESS_UP, false), true), Filtered::Pass);
        power.set_backlight(Box::new(FakeBacklight { max: 509, level: Cell::new(300) }));
        assert_eq!(power.filter(&key(KEY_BRIGHTNESS_UP, true), true), Filtered::Brightness(325, 509));
        assert_eq!(power.filter(&key(KEY_BRIGHTNESS_UP, false), true), Filtered::Spent);
        assert_eq!(power.filter(&key(KEY_BRIGHTNESS_DOWN, true), true), Filtered::Brightness(300, 509));
        assert_eq!(power.filter(&key(KEY_BRIGHTNESS_DOWN, false), true), Filtered::Spent);
        assert_eq!(power.backlight().get("brightness"), Ok(300));
        // With a modifier held the key goes to the client.
        assert_eq!(power.filter(&key(KEY_BRIGHTNESS_DOWN, true), false), Filtered::Pass);
        assert_eq!(power.filter(&key(KEY_BRIGHTNESS_DOWN, false), false), Filtered::Pass);
        assert_eq!(power.counts.brightness_steps, 2);
    }
    #[test]
    fn volume_keys_move_the_mixer_and_are_not_typed() {
        use crate::volume::{FakeMixer, Level, KEY_MUTE, KEY_VOLUME_DOWN, KEY_VOLUME_UP};
        let mut power = Power::new(None);
        // No audio server: F10 to F12 are ordinary keys.
        for evdev in [KEY_MUTE, KEY_VOLUME_DOWN, KEY_VOLUME_UP] {
            assert_eq!(power.filter(&key(evdev, true), true), Filtered::Pass);
            assert_eq!(power.filter(&key(evdev, false), true), Filtered::Pass);
        }
        power.set_mixer(Box::new(FakeMixer::new(0.5, false)));
        assert_eq!(power.filter(&key(KEY_VOLUME_UP, true), true), Filtered::Volume(56, false));
        assert_eq!(power.filter(&key(KEY_VOLUME_UP, false), true), Filtered::Spent);
        assert_eq!(power.filter(&key(KEY_VOLUME_DOWN, true), true), Filtered::Volume(50, false));
        assert_eq!(power.filter(&key(KEY_VOLUME_DOWN, false), true), Filtered::Spent);
        assert_eq!(power.filter(&key(KEY_MUTE, true), true), Filtered::Volume(50, true));
        assert_eq!(power.filter(&key(KEY_MUTE, false), true), Filtered::Spent);
        // Quieter keeps the mute, louder ends it.
        assert_eq!(power.filter(&key(KEY_VOLUME_DOWN, true), true), Filtered::Volume(44, true));
        assert_eq!(power.filter(&key(KEY_VOLUME_DOWN, false), true), Filtered::Spent);
        assert_eq!(power.filter(&key(KEY_VOLUME_UP, true), true), Filtered::Volume(50, false));
        assert_eq!(power.filter(&key(KEY_VOLUME_UP, false), true), Filtered::Spent);
        assert_eq!(power.mixer().level(), Ok(Level { volume: 0.5, muted: false }));
        // With a modifier held the key goes to the client.
        assert_eq!(power.filter(&key(KEY_MUTE, true), false), Filtered::Pass);
        assert_eq!(power.filter(&key(KEY_MUTE, false), false), Filtered::Pass);
        assert_eq!(power.mixer().level(), Ok(Level { volume: 0.5, muted: false }));
        assert_eq!(power.counts.volume_steps, 5);
        // While the display sleeps a volume key wakes it like any other and changes nothing.
        power.off = true;
        assert_eq!(power.filter(&key(KEY_VOLUME_UP, true), true), Filtered::Wake);
        power.off = false;
        assert_eq!(power.filter(&key(KEY_VOLUME_UP, false), true), Filtered::Spent);
        assert_eq!(power.mixer().level(), Ok(Level { volume: 0.5, muted: false }));
    }
    #[test]
    fn the_event_that_wakes_the_display_is_spent_with_its_release() {
        let mut power = Power::new(Some(Duration::from_secs(1)));
        power.off = true;
        assert_eq!(power.filter(&key(30, true), true), Filtered::Wake);
        assert!(power.wake_requested);
        // More keys before the display is back are spent too.
        assert_eq!(power.filter(&key(31, true), true), Filtered::Wake);
        power.off = false;
        assert_eq!(power.filter(&key(30, false), true), Filtered::Spent);
        assert_eq!(power.filter(&key(31, false), true), Filtered::Spent);
        assert_eq!(power.filter(&key(30, true), true), Filtered::Pass);
        assert_eq!(power.filter(&key(30, false), true), Filtered::Pass);
        // A click that wakes: nothing of it is delivered until the button is up.
        power.off = true;
        assert_eq!(power.filter(&pointer(1), true), Filtered::Wake);
        power.off = false;
        assert_eq!(power.filter(&pointer(1), true), Filtered::Spent);
        assert_eq!(power.filter(&pointer(0), true), Filtered::Pass);
        assert_eq!(power.filter(&pointer(1), true), Filtered::Pass);
        // Plain motion that wakes leaves nothing behind.
        power.off = true;
        assert_eq!(power.filter(&pointer(0), true), Filtered::Wake);
        power.off = false;
        assert_eq!(power.filter(&pointer(1), true), Filtered::Pass);
        assert_eq!(power.counts.swallowed, 4);
    }
    #[test]
    fn idle_timeout_from_the_environment() {
        // Only the parser's arithmetic: the variables are not set in tests.
        assert_eq!(idle_from_env().unwrap(), Some(Duration::from_secs(300)));
    }
}
