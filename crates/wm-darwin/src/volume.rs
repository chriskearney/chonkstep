//! The volume keys: level and mute of the default output device.
//!
//! The level belongs to coreaudiod, SiliconForest's audio server, which applies
//! it in the codec for the headphones and in its mix for the speakers. The
//! keys talk to it directly over its Mach service (the device table and the
//! set message of the tree's `SFAudioServer.h`): no process is started for a
//! key press and nothing but libSystem is linked. Without coreaudiod, or
//! without an output device, the keys stay ordinary function keys.
use std::cell::Cell;

/// evdev codes of F10, F11 and F12: mute, quieter, louder, as printed on the
/// MacBook's top row (which reports plain function keys, see `power`).
pub const KEY_MUTE: u32 = 68;
pub const KEY_VOLUME_DOWN: u32 = 87;
pub const KEY_VOLUME_UP: u32 = 88;

/// One key press moves the level by a sixteenth of the range.
pub const STEP: f32 = 1.0 / 16.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    /// 0.0 to 1.0.
    pub volume: f32,
    pub muted: bool,
}

/// The default output device's level. Errors are errno values (`ENOENT`
/// without the audio server, `ENODEV` without an output device).
pub trait Mixer {
    fn level(&self) -> Result<Level, i32>;
    fn set_volume(&self, volume: f32) -> Result<(), i32>;
    fn set_muted(&self, muted: bool) -> Result<(), i32>;
    /// The output device's name as the audio server reports it ("MacBook
    /// Pro Speakers", "Headphones"), when it has one.
    fn device_name(&self) -> Option<String> {
        None
    }
}

pub fn is_key(evdev: u32) -> bool {
    matches!(evdev, KEY_MUTE | KEY_VOLUME_DOWN | KEY_VOLUME_UP)
}

/// The level as people read it.
pub fn percent(volume: f32) -> u8 {
    (volume.clamp(0.0, 1.0) * 100.0 + 0.5) as u8
}

/// What one press of a volume key asks for. Louder also ends a mute; quieter
/// keeps it.
pub fn stepped(level: Level, evdev: u32) -> Level {
    match evdev {
        KEY_MUTE => Level { muted: !level.muted, ..level },
        KEY_VOLUME_UP => Level { volume: (level.volume + STEP).min(1.0), muted: false },
        KEY_VOLUME_DOWN => Level { volume: (level.volume - STEP).max(0.0), ..level },
        _ => level,
    }
}

/// One press of a volume key: what the mixer holds afterwards. `None` when
/// there is no mixer to ask (the key is then an ordinary one).
pub fn press(mixer: &dyn Mixer, evdev: u32) -> Option<Level> {
    let before = mixer.level().ok()?;
    let wanted = stepped(before, evdev);
    let mut result = Ok(());
    if wanted.volume != before.volume {
        result = mixer.set_volume(wanted.volume);
    }
    if result.is_ok() && wanted.muted != before.muted {
        result = mixer.set_muted(wanted.muted);
    }
    if let Err(errno) = result {
        eprintln!("chonkstep-darwin: the audio server refused volume {} muted {}: errno {errno}", percent(wanted.volume), wanted.muted);
    }
    // What the server holds now, whatever became of the request.
    Some(mixer.level().unwrap_or(before))
}

/// coreaudiod on SiliconForest; absent anywhere else.
pub struct CoreaudiodMixer;

#[cfg(target_os = "macos")]
mod mach {
    //! The two requests of coreaudiod's protocol the keys need, as
    //! `src/Frameworks/CoreAudio/SFAudioServer.h` of the SiliconForest tree
    //! lays them out.
    use std::ffi::{c_char, c_int, c_uint};
    use std::mem::size_of;

    type Port = c_uint;
    const SERVICE: &std::ffi::CStr = c"org.siliconforest.audio.coreaudiod";
    const GET_STATE: c_int = 0x5044_4102;
    const GET_STATE_REPLY: c_int = 0x5044_4103;
    const SET: c_int = 0x5044_4106;
    const SET_REPLY: c_int = 0x5044_4107;
    pub const SET_VOLUME: u32 = 4;
    pub const SET_MUTE: u32 = 5;
    const MAX_DEVICES: usize = 16;
    const HAS_OUTPUT: u32 = 1;

    const COPY_SEND: u32 = 19;
    const MAKE_SEND_ONCE: u32 = 21;
    const SEND_MSG: c_int = 0x1;
    const RCV_MSG: c_int = 0x2;
    const SEND_TIMEOUT: c_int = 0x10;
    const RCV_TIMEOUT: c_int = 0x100;
    const RIGHT_RECEIVE: c_uint = 1;
    /// The server answers at once or is not there: a key press never waits longer.
    const TIMEOUT_MS: c_uint = 300;
    const TRAILER: usize = 68;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Header {
        bits: u32,
        size: u32,
        remote: Port,
        local: Port,
        voucher: Port,
        id: c_int,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Device {
        id: u32,
        flags: u32,
        sample_rate: u32,
        output_channels: u32,
        input_channels: u32,
        latency_frames: u32,
        volume: f32,
        mute: u32,
        uid: [u8; 128],
        name: [u8; 64],
        manufacturer: [u8; 64],
    }
    #[repr(C)]
    struct StateReply {
        header: Header,
        ndr: [u8; 8],
        status: i32,
        generation: u32,
        default_output: u32,
        default_system_output: u32,
        default_input: u32,
        count: u32,
        devices: [Device; MAX_DEVICES],
        trailer: [u8; TRAILER],
    }
    #[repr(C)]
    struct SetRequest {
        header: Header,
        what: u32,
        device: u32,
        value: u32,
        scalar: f32,
        // The reply (header, NDR record, status) and its trailer land here too.
        room: [u8; 16 + TRAILER],
    }
    const SET_REQUEST_SIZE: u32 = (size_of::<Header>() + 16) as u32;

    extern "C" {
        static bootstrap_port: Port;
        static mach_task_self_: Port;
        fn bootstrap_look_up(bootstrap: Port, name: *const c_char, service: *mut Port) -> c_int;
        fn mach_port_allocate(task: Port, right: c_uint, name: *mut Port) -> c_int;
        fn mach_port_mod_refs(task: Port, name: Port, right: c_uint, delta: c_int) -> c_int;
        fn mach_port_deallocate(task: Port, name: Port) -> c_int;
        fn mach_msg(message: *mut Header, option: c_int, send_size: u32, receive_size: u32, receive_name: Port, timeout: c_uint, notify: Port) -> c_int;
    }

    /// One request and its reply in `message` (`total` bytes of room, the
    /// first `send` of them the request). Errno values.
    ///
    /// # Safety
    /// `message` points to `total` writable bytes that begin with a `Header`.
    unsafe fn call(message: *mut Header, id: c_int, send: u32, total: u32) -> Result<(), i32> {
        let task = mach_task_self_;
        let mut server: Port = 0;
        if bootstrap_look_up(bootstrap_port, SERVICE.as_ptr(), &mut server) != 0 || server == 0 {
            return Err(libc::ENOENT);
        }
        let mut reply: Port = 0;
        if mach_port_allocate(task, RIGHT_RECEIVE, &mut reply) != 0 {
            mach_port_deallocate(task, server);
            return Err(libc::ENOMEM);
        }
        *message = Header { bits: COPY_SEND | (MAKE_SEND_ONCE << 8), size: send, remote: server, local: reply, voucher: 0, id };
        let result = mach_msg(message, SEND_MSG | RCV_MSG | SEND_TIMEOUT | RCV_TIMEOUT, send, total, reply, TIMEOUT_MS, 0);
        mach_port_mod_refs(task, reply, RIGHT_RECEIVE, -1);
        mach_port_deallocate(task, server);
        if result == 0 { Ok(()) } else { Err(libc::EIO) }
    }

    /// (device id, volume, muted) of the default output.
    pub fn default_output() -> Result<(u32, f32, bool), i32> {
        // SAFETY: StateReply is plain data for which all-zero is valid; the
        // kernel writes at most its size into it.
        let state = unsafe {
            let mut state: Box<StateReply> = Box::new(std::mem::zeroed());
            call(&mut state.header, GET_STATE, size_of::<Header>() as u32, size_of::<StateReply>() as u32)?;
            state
        };
        if state.header.id != GET_STATE_REPLY || state.status != 0 {
            return Err(libc::EIO);
        }
        let count = (state.count as usize).min(MAX_DEVICES);
        state.devices[..count]
            .iter()
            .find(|d| d.id == state.default_output && d.id != 0 && d.flags & HAS_OUTPUT != 0)
            .map(|d| (d.id, d.volume, d.mute != 0))
            .ok_or(libc::ENODEV)
    }

    /// The name of the default output device.
    pub fn default_output_name() -> Result<String, i32> {
        // SAFETY: as in `default_output`.
        let state = unsafe {
            let mut state: Box<StateReply> = Box::new(std::mem::zeroed());
            call(&mut state.header, GET_STATE, size_of::<Header>() as u32, size_of::<StateReply>() as u32)?;
            state
        };
        if state.header.id != GET_STATE_REPLY || state.status != 0 {
            return Err(libc::EIO);
        }
        let count = (state.count as usize).min(MAX_DEVICES);
        state.devices[..count]
            .iter()
            .find(|d| d.id == state.default_output && d.id != 0 && d.flags & HAS_OUTPUT != 0)
            .map(|d| {
                let end = d.name.iter().position(|b| *b == 0).unwrap_or(d.name.len());
                String::from_utf8_lossy(&d.name[..end]).into_owned()
            })
            .filter(|name| !name.is_empty())
            .ok_or(libc::ENODEV)
    }

    pub fn set(what: u32, device: u32, value: u32, scalar: f32) -> Result<(), i32> {
        let mut request = SetRequest { header: Header::default(), what, device, value, scalar, room: [0; 16 + TRAILER] };
        // SAFETY: SetRequest begins with a Header and has room for the reply.
        unsafe { call(&mut request.header, SET, SET_REQUEST_SIZE, size_of::<SetRequest>() as u32)? };
        // The reply's status follows its header and NDR record: where `value` was.
        if request.header.id != SET_REPLY || request.value != 0 {
            return Err(libc::EIO);
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Mixer for CoreaudiodMixer {
    fn level(&self) -> Result<Level, i32> {
        mach::default_output().map(|(_, volume, muted)| Level { volume, muted })
    }
    fn set_volume(&self, volume: f32) -> Result<(), i32> {
        let (device, _, _) = mach::default_output()?;
        mach::set(mach::SET_VOLUME, device, 0, volume)
    }
    fn set_muted(&self, muted: bool) -> Result<(), i32> {
        let (device, _, _) = mach::default_output()?;
        mach::set(mach::SET_MUTE, device, u32::from(muted), 0.0)
    }
    fn device_name(&self) -> Option<String> {
        mach::default_output_name().ok()
    }
}
#[cfg(not(target_os = "macos"))]
impl Mixer for CoreaudiodMixer {
    fn level(&self) -> Result<Level, i32> {
        Err(libc::ENOENT)
    }
    fn set_volume(&self, _: f32) -> Result<(), i32> {
        Err(libc::ENOENT)
    }
    fn set_muted(&self, _: bool) -> Result<(), i32> {
        Err(libc::ENOENT)
    }
}

/// A mixer in memory with the server's semantics, for tests.
pub struct FakeMixer {
    pub level: Cell<Level>,
    pub name: Option<&'static str>,
}
impl FakeMixer {
    pub fn new(volume: f32, muted: bool) -> Self {
        Self { level: Cell::new(Level { volume, muted }), name: None }
    }
}
impl Mixer for FakeMixer {
    fn level(&self) -> Result<Level, i32> {
        Ok(self.level.get())
    }
    fn set_volume(&self, volume: f32) -> Result<(), i32> {
        self.level.set(Level { volume: volume.clamp(0.0, 1.0), ..self.level.get() });
        Ok(())
    }
    fn set_muted(&self, muted: bool) -> Result<(), i32> {
        self.level.set(Level { muted, ..self.level.get() });
        Ok(())
    }
    fn device_name(&self) -> Option<String> {
        self.name.map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_step_is_a_sixteenth_and_stays_in_range() {
        let at = |volume, muted| Level { volume, muted };
        assert_eq!(stepped(at(0.5, false), KEY_VOLUME_UP), at(0.5625, false));
        assert_eq!(stepped(at(0.5, false), KEY_VOLUME_DOWN), at(0.4375, false));
        assert_eq!(stepped(at(0.98, false), KEY_VOLUME_UP), at(1.0, false));
        assert_eq!(stepped(at(0.03, false), KEY_VOLUME_DOWN), at(0.0, false));
        assert_eq!(percent(0.5625), 56);
        assert_eq!(percent(1.0), 100);
        assert_eq!(percent(0.0), 0);
    }
    #[test]
    fn mute_toggles_and_louder_ends_it() {
        let at = |volume, muted| Level { volume, muted };
        assert_eq!(stepped(at(0.5, false), KEY_MUTE), at(0.5, true));
        assert_eq!(stepped(at(0.5, true), KEY_MUTE), at(0.5, false));
        assert_eq!(stepped(at(0.5, true), KEY_VOLUME_UP), at(0.5625, false));
        assert_eq!(stepped(at(0.5, true), KEY_VOLUME_DOWN), at(0.4375, true));
    }
    #[test]
    fn a_press_reports_what_the_mixer_holds() {
        let mixer = FakeMixer::new(0.5, false);
        assert_eq!(press(&mixer, KEY_VOLUME_UP), Some(Level { volume: 0.5625, muted: false }));
        assert_eq!(press(&mixer, KEY_MUTE), Some(Level { volume: 0.5625, muted: true }));
        assert_eq!(press(&mixer, KEY_VOLUME_UP), Some(Level { volume: 0.625, muted: false }));
        assert_eq!(mixer.level(), Ok(Level { volume: 0.625, muted: false }));
        // No audio server: nothing to report, the key is an ordinary one.
        assert_eq!(press(&CoreaudiodMixer, KEY_VOLUME_UP), None);
    }
}
