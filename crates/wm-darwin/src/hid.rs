//! SiliconForest SFHIDEventQueue byte ABI, not Linux evdev and not host injection.
//! Native queues are destructive, have 64 slots, and do not support kqueue.
use std::io::{self, Read};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion { Relative(i8,i8), Absolute(u16,u16) }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Key { evdev:u32, down:bool },
    Pointer { motion:Motion, buttons:u8, wheel:i8 },
}
#[derive(Default)]
pub struct Decoder {
    keyboard_sequence:Option<u32>, mouse_sequence:Option<u32>,
    keys:std::collections::HashSet<u8>, mouse_buttons:std::collections::HashMap<u8,u8>,
}
fn sequence(last:&mut Option<u32>, next:u32)->Result<(),String>{
    if last.is_some_and(|last|last.wrapping_add(1)!=next){return Err("SFHID sequence discontinuity: queue overflow or competing reader; input state is lost".into());}
    *last=Some(next);Ok(())
}
impl Decoder {
    pub fn keyboard(&mut self,bytes:&[u8])->Result<Option<Event>,String>{
        if bytes.len()!=8 || bytes[5]>1 || bytes[6..]!=[0,0]{return Err("malformed SFHID keyboard record".into());}
        sequence(&mut self.keyboard_sequence,u32::from_le_bytes(bytes[..4].try_into().unwrap()))?;
        let usage=bytes[4];let down=bytes[5]!=0;
        let Some(evdev)=usage_to_evdev(usage) else {return Err(format!("unsupported HID keyboard usage {usage:#04x}"));};
        // Clients repeat from Wayland repeat_info; duplicate down must not toggle locks.
        if down {if !self.keys.insert(usage){return Ok(None);}}
        else if !self.keys.remove(&usage){return Ok(None);}
        Ok(Some(Event::Key{evdev,down}))
    }
    pub fn mouse(&mut self,bytes:&[u8])->Result<Event,String>{
        if bytes.len()!=12 || bytes[5]&!7!=0 || bytes[9]&!1!=0 {return Err("malformed SFHID pointer record".into());}
        sequence(&mut self.mouse_sequence,u32::from_le_bytes(bytes[..4].try_into().unwrap()))?;
        let motion=if bytes[9]==1 {Motion::Absolute(u16::from_le_bytes(bytes[6..8].try_into().unwrap()),u16::from_le_bytes(bytes[10..12].try_into().unwrap()))}
            else {if bytes[10..]!=[0,0]{return Err("unexpected relative pointer reserved bytes".into());}Motion::Relative(bytes[6] as i8,bytes[7] as i8)};
        self.mouse_buttons.insert(bytes[4],bytes[5]);
        let buttons=self.mouse_buttons.values().fold(0,|all,mask|all|mask);
        Ok(Event::Pointer{motion,buttons,wheel:bytes[8] as i8})
    }
}

/// USB HID keyboard usage -> evdev numbering required by wl_keyboard/XKB.
/// Mirrors the pinned SiliconForest backend's common keyboard table and covers
/// navigation, keypad and F13-F24 as actual keycodes, never an ASCII injection.
pub fn usage_to_evdev(usage:u8)->Option<u32>{
    const LETTERS:[u32;26]=[30,48,46,32,18,33,34,35,23,36,37,38,50,49,24,25,16,19,31,20,22,47,17,45,21,44];
    Some(match usage {
        0x04..=0x1d=>LETTERS[(usage-4) as usize],0x1e..=0x26=>(usage-0x1e+2) as u32,0x27=>11,
        0x28=>28,0x29=>1,0x2a=>14,0x2b=>15,0x2c=>57,0x2d=>12,0x2e=>13,0x2f=>26,0x30=>27,
        0x31|0x32=>43,0x33=>39,0x34=>40,0x35=>41,0x36=>51,0x37=>52,0x38=>53,0x39=>58,
        0x3a..=0x43=>(usage-0x3a+59) as u32,0x44=>87,0x45=>88,0x46=>99,0x47=>70,0x48=>119,
        0x49=>110,0x4a=>102,0x4b=>104,0x4c=>111,0x4d=>107,0x4e=>109,0x4f=>106,0x50=>105,
        0x51=>108,0x52=>103,0x53=>69,0x54=>98,0x55=>55,0x56=>74,0x57=>78,0x58=>96,
        0x59=>79,0x5a=>80,0x5b=>81,0x5c=>75,0x5d=>76,0x5e=>77,0x5f=>71,0x60=>72,0x61=>73,
        0x62=>82,0x63=>83,0x64=>86,0x65=>127,0x67=>117,0x68..=0x73=>(usage-0x68+183) as u32,
        0xe0=>29,0xe1=>42,0xe2=>56,0xe3=>125,0xe4=>97,0xe5=>54,0xe6=>100,0xe7=>126,_=>return None,
    })
}
fn read_record<const N:usize>(reader:&mut impl Read)->Result<Option<[u8;N]>,String>{
    let mut bytes=[0;N];
    loop {match reader.read(&mut bytes){
        Ok(n) if n==N=>return Ok(Some(bytes)),
        Ok(n)=>return Err(format!("SFHID short record: {n}, expected {N}")),
        Err(e) if e.kind()==io::ErrorKind::WouldBlock=>return Ok(None),
        Err(e) if e.kind()==io::ErrorKind::Interrupted=>continue,
        Err(e)=>return Err(format!("SFHID read failed: {e}")),
    }}
}

const EXPECTED_CLI: &str = "expected --run --socket ABSOLUTE_PATH [--input sfhid|sfhid-keyboard]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Off,
    Both,
    KeyboardOnly,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
    Help,
    Run { socket: String, input: InputMode },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeatCaps {
    pub keyboard: bool,
    pub pointer: bool,
}

pub fn usage_text() -> &'static str {
    "Chonkstep experimental Darwin CPU backend\n\
Usage: chonkstep-darwin --run --socket /private/runtime/wayland-0 [--input sfhid|sfhid-keyboard]\n\
Requires a caller-owned mode-0700 runtime directory and native SFGOP. \
SFHID input is opt-in. sfhid opens both native character devices and advertises keyboard and pointer. \
sfhid-keyboard opens only /dev/usb_hid_kbd and advertises keyboard with no pointer. \
Default is input off. No GPU, X11 or privileged IPC is advertised. \
A seat capability is not native input."
}

pub fn parse_cli(args: &[String]) -> Result<Cli, String> {
    if args.is_empty() || args == ["--help"] {
        return Ok(Cli::Help);
    }
    if args.len() < 3 || args[0] != "--run" || args[1] != "--socket" {
        return Err(EXPECTED_CLI.into());
    }
    let input = if args.len() == 3 {
        InputMode::Off
    } else if args.len() == 5 && args[3] == "--input" {
        match args[4].as_str() {
            "sfhid" => InputMode::Both,
            "sfhid-keyboard" => InputMode::KeyboardOnly,
            _ => return Err(EXPECTED_CLI.into()),
        }
    } else {
        return Err(EXPECTED_CLI.into());
    };
    Ok(Cli::Run { socket: args[2].clone(), input })
}

/// Wayland seat bits for a selected mode. Off advertises nothing.
/// Both advertises keyboard and pointer. Keyboard-only advertises keyboard.
pub fn advertised_caps(mode: InputMode) -> SeatCaps {
    match mode {
        InputMode::Off => SeatCaps { keyboard: false, pointer: false },
        InputMode::Both => SeatCaps { keyboard: true, pointer: true },
        InputMode::KeyboardOnly => SeatCaps { keyboard: true, pointer: false },
    }
}

/// Reject events the selected seat did not advertise, and malformed decoded events.
pub fn accept_for_seat(caps: SeatCaps, event: &Event) -> Result<(), String> {
    match event {
        Event::Key { evdev, .. } if *evdev == 0 || *evdev > 0x2ff => Err("invalid decoded input event".into()),
        Event::Key { .. } if !caps.keyboard => Err("keyboard event rejected; seat has no keyboard".into()),
        Event::Pointer { .. } if !caps.pointer => Err("pointer event rejected; seat has no pointer".into()),
        Event::Pointer { buttons, .. } if *buttons & !7 != 0 => Err("invalid decoded input event".into()),
        _ => Ok(()),
    }
}

#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
fn open_read_nofollow(path: &str) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| format!("{path}: {e}"))
}

#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
fn require_character_device(file: std::fs::File, path: &str) -> Result<std::fs::File, String> {
    use std::os::unix::fs::FileTypeExt;
    if !file.metadata().map_err(|e| e.to_string())?.file_type().is_char_device() {
        return Err(format!("{path} is not a character device"));
    }
    Ok(file)
}

#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
fn open_character_queue(path: &str) -> Result<std::fs::File, String> {
    require_character_device(open_read_nofollow(path)?, path)
}

/// Open the keyboard queue first. Mouse is opened only for Both, and only
/// after the keyboard open succeeds. A failing later open drops the keyboard
/// file, which releases its console grab.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
fn opened_queues(
    mode: InputMode,
    open_keyboard: impl FnOnce() -> Result<std::fs::File, String>,
    open_mouse: impl FnOnce() -> Result<std::fs::File, String>,
) -> Result<(std::fs::File, Option<std::fs::File>), String> {
    match mode {
        InputMode::Off => Err("input off does not open SFHID".into()),
        InputMode::KeyboardOnly => Ok((open_keyboard()?, None)),
        InputMode::Both => {
            let keyboard = open_keyboard()?;
            // A mouse-open failure returns here and drops `keyboard`, releasing its grab.
            let mouse = open_mouse()?;
            Ok((keyboard, Some(mouse)))
        }
    }
}

/// Holds the keyboard character device for its console-grab lifetime, plus the
/// mouse device only when both queues were requested. No custom path or
/// userspace pipe is accepted as a native device.
pub struct NativeHid {
    keyboard: std::fs::File,
    mouse: Option<std::fs::File>,
    decoder: Decoder,
}
impl NativeHid {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn acquire(mode: InputMode) -> Result<Self, String> {
        let (keyboard, mouse) = opened_queues(
            mode,
            || open_character_queue("/dev/usb_hid_kbd"),
            || open_character_queue("/dev/usb_hid_mouse"),
        )?;
        Self::discard_until_idle(keyboard, mouse)
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub fn open() -> Result<Self, String> {
        Self::acquire(InputMode::Both)
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub fn open_keyboard() -> Result<Self, String> {
        Self::acquire(InputMode::KeyboardOnly)
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    pub fn open() -> Result<Self, String> {
        Err("native SFHID requires aarch64-apple-darwin".into())
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    pub fn open_keyboard() -> Result<Self, String> {
        Err("native SFHID requires aarch64-apple-darwin".into())
    }
    pub fn seat_caps(&self) -> SeatCaps {
        SeatCaps { keyboard: true, pointer: self.mouse.is_some() }
    }
    pub fn qualification_note(&self) -> &'static str {
        if self.mouse.is_some() {
            "SFHID queues opened; keyboard and pointer event delivery still requires live qualification"
        } else {
            "SFHID keyboard queue opened; pointer is not advertised; keyboard event delivery still requires live qualification"
        }
    }
    // The queues retain pre-open events. Discard the bounded backlog before
    // clients are connected; the operator must release keys before launch.
    #[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
    fn discard_until_idle(keyboard: std::fs::File, mouse: Option<std::fs::File>) -> Result<Self, String> {
        let mut input = Self { keyboard, mouse, decoder: Decoder::default() };
        for _ in 0..128 {
            let k = read_record::<8>(&mut input.keyboard)?;
            let m = if let Some(mouse) = input.mouse.as_mut() {
                read_record::<12>(mouse)?
            } else {
                None
            };
            if k.is_none() && m.is_none() {
                return Ok(input);
            }
        }
        Err("SFHID queues did not reach an idle boundary during acquisition".into())
    }
    pub fn drain(&mut self) -> Result<Vec<Event>, String> {
        let mut events = Vec::new();
        for _ in 0..64 {
            let k = read_record::<8>(&mut self.keyboard)?;
            let m = if let Some(mouse) = self.mouse.as_mut() {
                read_record::<12>(mouse)?
            } else {
                None
            };
            if k.is_none() && m.is_none() {
                break;
            }
            if let Some(k) = k {
                if let Some(event) = self.decoder.keyboard(&k)? {
                    events.push(event);
                }
            }
            if let Some(m) = m {
                events.push(self.decoder.mouse(&m)?);
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(seq:u32,usage:u8,down:u8)->[u8;8]{let mut b=[0;8];b[..4].copy_from_slice(&seq.to_le_bytes());b[4]=usage;b[5]=down;b}
    #[test]fn keys_modifiers_wrap_repeat_and_loss(){
        let mut d=Decoder::default();
        assert_eq!(d.keyboard(&key(u32::MAX,0xe1,1)).unwrap(),Some(Event::Key{evdev:42,down:true}));
        assert_eq!(d.keyboard(&key(0,4,1)).unwrap(),Some(Event::Key{evdev:30,down:true}));
        assert_eq!(d.keyboard(&key(1,4,1)).unwrap(),None);
        assert_eq!(d.keyboard(&key(2,4,0)).unwrap(),Some(Event::Key{evdev:30,down:false}));
        assert!(d.keyboard(&key(4,0xe1,0)).is_err());
        assert!(Decoder::default().keyboard(&key(0,4,2)).is_err());
        assert!(Decoder::default().keyboard(&key(0,0xff,1)).is_err());
        assert!(read_record::<8>(&mut io::Cursor::new(vec![0;7])).is_err());
    }
    #[test]fn absolute_fraction_signed_motion_and_per_device_buttons(){
        let mut d=Decoder::default();let mut b=[0;12];b[4]=2;b[5]=1;b[6..8].copy_from_slice(&65535u16.to_le_bytes());b[9]=1;b[10..].copy_from_slice(&32768u16.to_le_bytes());
        assert_eq!(d.mouse(&b).unwrap(),Event::Pointer{motion:Motion::Absolute(65535,32768),buttons:1,wheel:0});
        b=[0;12];b[..4].copy_from_slice(&1u32.to_le_bytes());b[4]=3;b[5]=2;b[6]=255;b[7]=128;b[8]=255;
        assert_eq!(d.mouse(&b).unwrap(),Event::Pointer{motion:Motion::Relative(-1,-128),buttons:3,wheel:-1});
        b[..4].copy_from_slice(&2u32.to_le_bytes());b[9]=0x80;assert!(d.mouse(&b).is_err());
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }
    #[test]
    fn cli_modes_and_advertised_caps() {
        assert_eq!(parse_cli(&[]).unwrap(), Cli::Help);
        assert_eq!(parse_cli(&args(&["--help"])).unwrap(), Cli::Help);
        match parse_cli(&args(&["--run", "--socket", "/private/runtime/wayland-0"])).unwrap() {
            Cli::Run { socket, input } => {
                assert_eq!(socket, "/private/runtime/wayland-0");
                assert_eq!(input, InputMode::Off);
            }
            Cli::Help => panic!("default run is input off"),
        }
        match parse_cli(&args(&["--run", "--socket", "/sock", "--input", "sfhid"])).unwrap() {
            Cli::Run { input, .. } => assert_eq!(input, InputMode::Both),
            Cli::Help => panic!("sfhid is both queues"),
        }
        match parse_cli(&args(&["--run", "--socket", "/sock", "--input", "sfhid-keyboard"])).unwrap() {
            Cli::Run { input, .. } => assert_eq!(input, InputMode::KeyboardOnly),
            Cli::Help => panic!("sfhid-keyboard is keyboard only"),
        }
        for bad in [
            &["--input", "sfhid"][..],
            &["--run", "--socket", "/sock", "--input"][..],
            &["--run", "--socket", "/sock", "--input", "sfhid-kbd"][..],
            &["--run", "--socket", "/sock", "--input", "sfhid", "extra"][..],
            &["--run", "--socket", "/sock", "sfhid-keyboard"][..],
        ] {
            assert!(parse_cli(&args(bad)).is_err(), "{bad:?}");
        }
        assert_eq!(advertised_caps(InputMode::Off), SeatCaps { keyboard: false, pointer: false });
        assert_eq!(advertised_caps(InputMode::Both), SeatCaps { keyboard: true, pointer: true });
        assert_eq!(advertised_caps(InputMode::KeyboardOnly), SeatCaps { keyboard: true, pointer: false });
        let usage = usage_text();
        assert!(usage.contains("sfhid-keyboard") && usage.contains("Default is input off"));
        assert!(usage.contains("is not native input"));
        let key = Event::Key { evdev: 30, down: true };
        let pointer = Event::Pointer { motion: Motion::Relative(1, 0), buttons: 1, wheel: 0 };
        assert!(accept_for_seat(advertised_caps(InputMode::KeyboardOnly), &key).is_ok());
        let rejected = accept_for_seat(advertised_caps(InputMode::KeyboardOnly), &pointer).unwrap_err();
        assert!(rejected.contains("no pointer"));
        assert!(accept_for_seat(advertised_caps(InputMode::Both), &pointer).is_ok());
        assert!(accept_for_seat(
            advertised_caps(InputMode::Both),
            &Event::Pointer { motion: Motion::Relative(0, 0), buttons: 0x8, wheel: 0 }
        ).is_err());
        assert!(accept_for_seat(advertised_caps(InputMode::Both), &Event::Key { evdev: 0, down: true }).is_err());
        assert!(accept_for_seat(advertised_caps(InputMode::Off), &key).unwrap_err().contains("no keyboard"));
    }

    fn fd_closed(fd: i32) -> bool {
        // SAFETY: fcntl with F_GETFD or F_GETFL only reads flags and is defined for any integer, open or not.
        let rc = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        rc < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EBADF)
    }
    fn nonblock_pipe() -> (std::fs::File, std::fs::File) {
        use std::os::fd::{AsRawFd, OwnedFd};
        // std's pipe is close-on-exec on every target; Darwin has no pipe2
        // for the non-blocking flag, so it is set afterwards.
        let (read, write) = std::io::pipe().unwrap();
        let (read, write) = (std::fs::File::from(OwnedFd::from(read)), std::fs::File::from(OwnedFd::from(write)));
        for end in [&read, &write] {
            let fd = end.as_raw_fd();
            // SAFETY: F_GETFL and F_SETFL on a descriptor `end` owns and keeps open across both calls.
            let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
            assert_eq!(rc, 0, "{}", io::Error::last_os_error());
        }
        (read, write)
    }
    #[test]
    fn open_flags_character_rejection_and_nofollow_do_not_use_hid_nodes() {
        use std::os::fd::AsRawFd;
        let dir = std::env::temp_dir().join(format!("sfhid-keyboard-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("regular");
        std::fs::write(&file, b"not-a-device").unwrap();
        let opened = open_read_nofollow(file.to_str().unwrap()).unwrap();
        let fd = opened.as_raw_fd();
        // SAFETY: fcntl with F_GETFD or F_GETFL only reads flags and is defined for any integer, open or not.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        // SAFETY: fcntl with F_GETFD or F_GETFL only reads flags and is defined for any integer, open or not.
        let fdflags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_ne!(flags & libc::O_NONBLOCK, 0);
        assert_ne!(fdflags & libc::FD_CLOEXEC, 0);
        let rejected = require_character_device(opened, file.to_str().unwrap()).unwrap_err();
        assert!(rejected.contains("is not a character device"));
        assert!(fd_closed(fd));
        let link = dir.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(open_read_nofollow(link.to_str().unwrap()).is_err());
        assert!(open_character_queue(file.to_str().unwrap()).unwrap_err().contains("is not a character device"));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn acquisition_eagain_failure_and_keyboard_only_decode() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        let (read, _write) = nonblock_pipe();
        let idle = NativeHid::discard_until_idle(read, None).unwrap();
        assert_eq!(idle.seat_caps(), SeatCaps { keyboard: true, pointer: false });
        assert!(idle.qualification_note().contains("pointer is not advertised"));
        let fd = idle.keyboard.as_raw_fd();
        drop(idle);
        assert!(fd_closed(fd), "keyboard fd stays open only while NativeHid owns the console grab");

        let (read, mut write) = nonblock_pipe();
        let fd = read.as_raw_fd();
        write.write_all(&[0u8; 7]).unwrap();
        match NativeHid::discard_until_idle(read, None) {
            Err(err) => assert!(err.contains("short record"), "{err}"),
            Ok(_) => panic!("short record must fail acquisition"),
        }
        assert!(fd_closed(fd));

        let (read, mut write) = nonblock_pipe();
        let fd = read.as_raw_fd();
        write.write_all(&[0u8; 8 * 128]).unwrap();
        match NativeHid::discard_until_idle(read, None) {
            Err(err) => assert!(err.contains("idle boundary"), "{err}"),
            Ok(_) => panic!("a full backlog without EAGAIN must fail acquisition"),
        }
        assert!(fd_closed(fd));

        let (kbd, _kw) = nonblock_pipe();
        let kfd = kbd.as_raw_fd();
        let missing = opened_queues(InputMode::Both, || Ok(kbd), || Err("mouse node absent".into()));
        assert!(missing.unwrap_err().contains("mouse node absent"));
        assert!(fd_closed(kfd));

        let mut mouse_calls = 0;
        let (kbd, _kw) = nonblock_pipe();
        let (keyboard, mouse) = opened_queues(
            InputMode::KeyboardOnly,
            || Ok(kbd),
            || {
                mouse_calls += 1;
                Err("mouse must not be opened".into())
            },
        )
        .unwrap();
        assert_eq!(mouse_calls, 0);
        assert!(mouse.is_none());
        let hid = NativeHid::discard_until_idle(keyboard, mouse).unwrap();
        assert!(!hid.seat_caps().pointer);

        let mut mouse_calls = 0;
        let mut keyboard_calls = 0;
        let err = opened_queues(
            InputMode::Both,
            || {
                keyboard_calls += 1;
                Err("keyboard node absent".into())
            },
            || {
                mouse_calls += 1;
                Err("mouse".into())
            },
        )
        .unwrap_err();
        assert_eq!(keyboard_calls, 1);
        assert_eq!(mouse_calls, 0);
        assert!(err.contains("keyboard node absent"));
        let err = opened_queues(InputMode::Off, || Ok(nonblock_pipe().0), || Ok(nonblock_pipe().0)).unwrap_err();
        assert!(err.contains("input off"));

        let (kbd_r, mut kbd_w) = nonblock_pipe();
        let (mouse_r, _mouse_w) = nonblock_pipe();
        let mouse_fd = mouse_r.as_raw_fd();
        let mut both = NativeHid::discard_until_idle(kbd_r, Some(mouse_r)).unwrap();
        assert_eq!(both.seat_caps(), advertised_caps(InputMode::Both));
        assert_eq!(
            both.qualification_note(),
            "SFHID queues opened; keyboard and pointer event delivery still requires live qualification"
        );
        kbd_w.write_all(&key(7, 0x04, 1)).unwrap();
        assert_eq!(both.drain().unwrap(), vec![Event::Key { evdev: 30, down: true }]);
        assert!(both.drain().unwrap().is_empty());
        drop(both);
        assert!(fd_closed(mouse_fd));

        let (kbd_r, mut kbd_w) = nonblock_pipe();
        let mut only = NativeHid::discard_until_idle(kbd_r, None).unwrap();
        kbd_w.write_all(&key(3, 0x04, 1)).unwrap();
        assert_eq!(only.drain().unwrap(), vec![Event::Key { evdev: 30, down: true }]);
        kbd_w.write_all(&key(4, 0x04, 1)).unwrap();
        assert!(only.drain().unwrap().is_empty(), "duplicate key-down is not a second event");
        kbd_w.write_all(&key(5, 0x04, 2)).unwrap();
        assert!(only.drain().unwrap_err().contains("malformed SFHID keyboard record"));
        let fd = only.keyboard.as_raw_fd();
        drop(only);
        assert!(fd_closed(fd));

        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            match NativeHid::open() {
                Err(err) => assert!(err.contains("aarch64-apple-darwin"), "{err}"),
                Ok(_) => panic!("host open must not acquire a device"),
            }
            match NativeHid::open_keyboard() {
                Err(err) => assert!(err.contains("aarch64-apple-darwin"), "{err}"),
                Ok(_) => panic!("host keyboard open must not acquire a device"),
            }
        }
    }
}
