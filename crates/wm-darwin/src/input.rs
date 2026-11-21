//! Real seat delivery shared by native SFHID and host wire tests.
use std::collections::HashSet;
use smithay::{input::{Seat,keyboard::{KeyboardHandle,FilterResult,ModifiersState,XkbConfig},
    pointer::{PointerHandle,MotionEvent,ButtonEvent,AxisFrame,CursorImageStatus,CursorImageSurfaceData}},
    backend::input::{KeyState,ButtonState,Axis,AxisSource},utils::SERIAL_COUNTER,
    reexports::wayland_server::{DisplayHandle,protocol::{wl_surface::WlSurface,wl_output,wl_seat::WlSeat}},
    wayland::compositor::{with_states,SurfaceAttributes,BufferAssignment}};
use wm_core::{Backend,BackendEvent,SurfaceRef,Point,MouseButton,Modifiers,KeyCombo};
use crate::{server::Server,hid::{Event,Motion,SeatCaps,accept_for_seat}};

pub(crate) struct InputState {
    pub seat:Seat<Server>,keyboard:KeyboardHandle<Server>,pointer:Option<PointerHandle<Server>>,
    position:(f64,f64),buttons:u8,forwarded:u8,modifiers:Modifiers,last_time:u32,
    suppressed_keys:HashSet<u32>,pub cursor_surface:Option<WlSurface>,
    /// Physical button mask of the last record, and whether the left button
    /// went down with Control held: that press is a secondary click.
    physical_buttons:u8,control_click:bool,
    /// Key events delivered so far and the last one (evdev code, down).
    hid_keys:u64,last_hid_key:Option<(u32,bool)>,
}
fn modifiers(m:&ModifiersState)->Modifiers {
    let mut out=Modifiers::empty();out.set(Modifiers::SHIFT,m.shift);out.set(Modifiers::CONTROL,m.ctrl);
    out.set(Modifiers::ALT,m.alt);out.set(Modifiers::SUPER,m.logo);out
}
fn buttons_physical(event:&Event)->u8{match event{Event::Pointer{buttons,..}=>*buttons,_=>0}}
/// Control + primary click is a secondary click, as on macOS: the built-in
/// trackpad in its basic pointer mode reports one button whatever the number
/// of fingers. The whole press, until the physical release, is the right
/// button; Control on its own, or pressed during a drag, changes nothing.
pub(crate) fn secondary_click(physical:u8,previous_physical:u8,control_only:bool,active:&mut bool)->u8{
    if physical&1!=0 && previous_physical&1==0 {*active=control_only;}
    if physical&1==0 {*active=false;}
    if *active {(physical&!1)|2} else {physical}
}
/// The surface under the pointer and that surface's origin, as clients see it.
type ClientFocus=(WlSurface,smithay::utils::Point<f64,smithay::utils::Logical>);
impl Server {
    /// Where the pointer is for clients: the panel position `(x, y)` in the
    /// logical units of the window under it (`focus`), with that window's
    /// surface and origin. The pointer itself lives in panel pixels, so the
    /// same hand movement covers the same distance on the panel at every
    /// scale; only what clients are told is divided by the scale.
    pub(crate) fn pointer_for_clients(&self,x:f64,y:f64,focus:Option<u64>)->((f64,f64),Option<ClientFocus>){
        let density=focus.map_or(self.scale().factor(),|id|self.wm.backend().density(id));
        let surface=focus.and_then(|id|self.wm.backend().windows.get(&id)).map(|w|(w.surface.wl_surface().clone(),
            (f64::from(w.content.pos.x)/density,f64::from(w.content.pos.y)/density).into()));
        ((x/density,y/density),surface)
    }
    /// The pointer image follows a scale change: the arrow is drawn again at
    /// the new density; a client's cursor is stretched until its client
    /// sends one for the new scale.
    pub(crate) fn rescale_cursor(&mut self,old:crate::scale::Scale){
        let new=self.scale();
        let Some(input)=self.input.as_ref() else{return;};
        if input.pointer.is_none(){return;}
        match input.cursor_surface.clone(){
            None=>{if self.wm.backend().cursor().is_some(){self.wm.backend_mut().set_cursor(Some((Point::new(0,0),crate::scale::arrow(new))));}}
            Some(surface)=>{
                self.announce_scale(&surface);
                let Some((hotspot,image))=self.wm.backend().cursor().cloned() else{return;};
                let grow=|v:u32|((f64::from(v)*new.factor()/old.factor()).round() as u32).max(1);
                if let Ok(image)=crate::pixels::resample(&image,grow(image.width),grow(image.height)){
                    let spot=|v:i32|(f64::from(v)*new.factor()/old.factor()).round() as i32;
                    self.wm.backend_mut().set_cursor(Some((Point::new(spot(hotspot.x),spot(hotspot.y)),image)));
                }
            }
        }
    }
    pub(crate) fn authorize_client_drag(&mut self,surface:&smithay::wayland::shell::xdg::ToplevelSurface,seat:&WlSeat,serial:smithay::utils::Serial)->Option<u64>{
        let input=self.input.as_ref()?;
        let pointer=input.pointer.as_ref()?;
        if !input.seat.owns(seat) || !pointer.has_grab(serial) || input.buttons&1==0{return None;}
        let start=pointer.grab_start_data()?;
        if start.button!=0x110 || start.focus?.0!=*surface.wl_surface(){return None;}
        let id=self.wm.backend().window_for_surface(surface.wl_surface())?;
        let pointer=pointer.clone();let time=input.last_time;
        let (position,_)=self.pointer_for_clients(input.position.0,input.position.1,None);
        // Transfer the authenticated client implicit grab to wm-core. Leaving
        // the client resets its pointer interaction; no synthetic key/button is sent.
        pointer.unset_grab(self,serial,time);
        pointer.motion(self,None,&MotionEvent{location:position.into(),serial,time});
        self.input.as_mut().unwrap().forwarded=0;
        Some(id)
    }
    /// The production caller invokes this only after NativeHid::open succeeds.
    /// Tests exercise the protocol independently; this method makes no device claim.
    pub fn enable_sfhid_seat(&mut self,dh:&DisplayHandle)->Result<(),String>{
        self.enable_sfhid_seat_caps(dh,SeatCaps{keyboard:true,pointer:true})
    }
    /// `caps.pointer` is false for `--input sfhid-keyboard`: no pointer handle,
    /// so the seat does not advertise a pointer. This is not native input.
    pub fn enable_sfhid_seat_caps(&mut self,dh:&DisplayHandle,caps:SeatCaps)->Result<(),String>{
        if self.input.is_some(){return Err("SFHID seat already enabled".into());}
        if !caps.keyboard{return Err("SFHID seat requires keyboard capability".into());}
        let mut seat=self.seats.new_wl_seat(dh,"siliconforest-hid");
        let keyboard=seat.add_keyboard(XkbConfig{rules:"evdev",model:"pc105",layout:"us",variant:"",options:None},500,25)
            .map_err(|e|format!("XKB keymap initialization failed: {e:?}"))?;
        let pointer=if caps.pointer{Some(seat.add_pointer())}else{None};
        self.input=Some(InputState{seat,keyboard,pointer,position:(0.,0.),buttons:0,forwarded:0,
            modifiers:Modifiers::empty(),last_time:0,suppressed_keys:HashSet::new(),cursor_surface:None,
            physical_buttons:0,control_click:false,hid_keys:0,last_hid_key:None});
        let b=self.wm.backend_mut();b.input_enabled=true;
        let arrow=crate::scale::arrow(b.scale);
        if caps.pointer{b.set_cursor(Some((Point::new(0,0),arrow)));}
        b.dirty=true;
        self.sync_input_focus();Ok(())
    }
    pub(crate) fn sync_input_focus(&mut self){
        let Some(input)=&mut self.input else{return;};let keyboard=input.keyboard.clone();
        let blocked=false;
        let focus=if blocked{None}else{self.wm.focused_client().and_then(|id|self.wm.client(id)).map(|c|c.window)
            .filter(|id|self.wm.backend().visible(*id)).map(|id|self.wm.backend().windows[&id].surface.wl_surface().clone())};
        if keyboard.current_focus()!=focus{keyboard.set_focus(self,focus,SERIAL_COUNTER.next_serial());}
    }
    /// Key events delivered since start and the last one (evdev code, down), whatever became of them.
    pub fn hid_keys(&self)->(u64,Option<(u32,bool)>){
        self.input.as_ref().map_or((0,None),|input|(input.hid_keys,input.last_hid_key))
    }
    pub fn deliver_hid(&mut self,event:Event,time:u32)->Result<(),String>{
        if self.input.is_none(){return Err("no SFHID seat; event not delivered".into());}
        let caps=SeatCaps{keyboard:true,pointer:self.input.as_ref().unwrap().pointer.is_some()};
        accept_for_seat(caps,&event)?;
        self.input.as_mut().unwrap().last_time=time;
        if let Event::Key{evdev,down}=event{
            let input=self.input.as_mut().unwrap();
            input.hid_keys+=1;input.last_hid_key=Some((evdev,down));
        }
        // Display sleep, the brightness keys and the volume keys come before everything else:
        // the event that wakes the display is spent on that.
        let plain=self.input.as_ref().unwrap().modifiers.is_empty();
        match self.power.filter(&event,plain){
            crate::power::Filtered::Pass=>{}
            crate::power::Filtered::Brightness(level,max)=>{
                // A level takes effect with the next frame presented.
                self.wm.backend_mut().dirty=true;
                let _=(level,max);
                return Ok(());
            }
            crate::power::Filtered::Volume(percent,muted)=>{
                let _=(percent,muted);
                return Ok(());
            }
            crate::power::Filtered::Wake|crate::power::Filtered::Spent=>return Ok(()),
        }
        self.sync_input_focus();
        match event {
            Event::Key{evdev,down}=>{
                let keyboard=self.input.as_ref().unwrap().keyboard.clone();
                keyboard.input::<(),_>(self,(evdev+8).into(),if down{KeyState::Pressed}else{KeyState::Released},SERIAL_COUNTER.next_serial(),time,
                    |s,m,key|{
                        let mods=modifiers(m);s.input.as_mut().unwrap().modifiers=mods;
                        if !down && s.input.as_mut().unwrap().suppressed_keys.remove(&evdev){return FilterResult::Intercept(());}
                        let sym=key.raw_latin_sym_or_raw_current_sym().unwrap_or_else(||key.modified_sym());
                        let combo=KeyCombo{keysym:sym.raw(),modifiers:mods};
                        if down && s.wm.backend().grabbed_keys.contains(&combo){
                            s.input.as_mut().unwrap().suppressed_keys.insert(evdev);
                            s.wm.dispatch(BackendEvent::KeyPress(combo));FilterResult::Intercept(())
                        }else{FilterResult::Forward}
                    });
            }
            Event::Pointer{motion,buttons,wheel}=>{
                let pointer=self.input.as_ref().unwrap().pointer.clone().ok_or("pointer event rejected; seat has no pointer")?;
                let size=self.wm.backend().screen_size();
                let input=self.input.as_mut().unwrap();
                let (x,y)=match motion {Motion::Absolute(x,y)=>(f64::from(x)*f64::from(size.w-1)/65535.,f64::from(y)*f64::from(size.h-1)/65535.),
                    Motion::Relative(x,y)=>(input.position.0+f64::from(x),input.position.1+f64::from(y))};
                input.position=(x.clamp(0.,f64::from(size.w-1)),y.clamp(0.,f64::from(size.h-1)));
                let root=Point::new(input.position.0.floor() as i32,input.position.1.floor() as i32);
                let previous=input.buttons;let mods=input.modifiers;
                let buttons=secondary_click(buttons,input.physical_buttons,mods==Modifiers::CONTROL,&mut input.control_click);
                input.physical_buttons=(input.physical_buttons&!7)|(buttons_physical(&event)&7);
                let b=self.wm.backend_mut();b.pointer=root;b.dirty=true;
                let hit=self.wm.backend().hit_at(root);
                let dragging=self.wm.interactive_drag_active();self.wm.backend_mut().interactive=dragging;
                self.wm.dispatch(BackendEvent::PointerMotion{root,surface_local:hit});
                // Move/resize grabs belong to wm-core. Client implicit grabs belong to Smithay.
                let under=if self.wm.interactive_drag_active(){None}else{
                    self.wm.backend().hit_at(root).and_then(|(hit,_)|match hit{SurfaceRef::Client(id)=>Some(id),_=>None})};
                let (location,focus)=self.pointer_for_clients(x.clamp(0.,f64::from(size.w-1)),y.clamp(0.,f64::from(size.h-1)),under);
                pointer.motion(self,focus,&MotionEvent{location:location.into(),serial:SERIAL_COUNTER.next_serial(),time});
                for bit in 0..3 {
                    let mask=1<<bit;if (previous^buttons)&mask==0{continue;}
                    let pressed=buttons&mask!=0;
                    let forwarded=self.input.as_ref().unwrap().forwarded;
                    let had_drag=self.wm.interactive_drag_active();
                    let button=match bit{0=>MouseButton::Left,1=>MouseButton::Right,_=>MouseButton::Middle};
                    self.wm.backend_mut().replay=false;
                    if forwarded==0 || !pressed {
                        if let Some((surface,local))=self.wm.backend().hit_at(root){self.wm.dispatch(BackendEvent::PointerButton{surface,local,button,pressed,time_ms:time,mods});}
                        else if !pressed{self.wm.dispatch(BackendEvent::DragEnded);}
                    }
                    let send=if pressed {forwarded!=0 || (!had_drag && !self.wm.interactive_drag_active() && self.wm.backend().replay)}else{forwarded&mask!=0};
                    if send {
                        pointer.button(self,&ButtonEvent{serial:SERIAL_COUNTER.next_serial(),time,button:0x110+bit,state:if pressed{ButtonState::Pressed}else{ButtonState::Released}});
                        let forwarded=&mut self.input.as_mut().unwrap().forwarded;
                        if pressed{*forwarded|=mask;}else{*forwarded&=!mask;}
                    }
                }
                self.input.as_mut().unwrap().buttons=buttons;
                // An interactive resize just ended (or a click): adopt what
                // caught-up clients actually drew, as the Linux backend does.
                if previous!=buttons && !self.wm.interactive_drag_active(){self.fit_caught_up_windows();}
                if wheel!=0 && !self.wm.interactive_drag_active(){pointer.axis(self,AxisFrame::new(time).source(AxisSource::Wheel).value(Axis::Vertical,-f64::from(wheel)*10.).v120(Axis::Vertical,-i32::from(wheel)*120));}
                pointer.frame(self);
            }
        }
        self.sync_input_focus();Ok(())
    }
    pub(crate) fn cursor_image_changed(&mut self,image:CursorImageStatus){
        let Some(input)=&mut self.input else{return;};
        input.cursor_surface=match &image {CursorImageStatus::Surface(surface)=>Some(surface.clone()),_=>None};
        if let Some(surface)=input.cursor_surface.clone(){self.announce_scale(&surface);self.import_cursor(&surface);}
        else{let arrow=crate::scale::arrow(self.scale());self.wm.backend_mut().set_cursor(match image{CursorImageStatus::Hidden=>None,_=>Some((Point::new(0,0),arrow))});}
    }
    pub(crate) fn import_cursor(&mut self,surface:&WlSurface){
        let (buffer,scale,transform,hotspot,callbacks)=with_states(surface,|s|{
            let hotspot=s.data_map.get::<CursorImageSurfaceData>().map(|d|d.lock().unwrap().hotspot).unwrap_or_default();
            let mut a=s.cached_state.get::<SurfaceAttributes>();let a=a.current();a.damage.clear();
            (a.buffer.take(),a.buffer_scale,a.buffer_transform,hotspot,std::mem::take(&mut a.frame_callbacks))
        });
        if scale<1 || transform!=wl_output::Transform::Normal{Self::refuse(surface,"unsupported cursor transform/scale");return;}
        // The hotspot is in the cursor surface's logical units; the image is
        // shown at the display scale: as it is when the client drew it at
        // that density, resampled when it did not.
        let display=self.scale();
        let hotspot=Point::new(display.to_physical(hotspot.x),display.to_physical(hotspot.y));
        match buffer{
            Some(BufferAssignment::NewBuffer(buffer))=>{
                let image=self.import_shm(&buffer);buffer.release();
                let image=image.and_then(|image|{
                    let fitted=crate::scale::fit(display,wm_core::Size::new(image.width,image.height),scale,None);
                    if fitted.logical.w>256 || fitted.logical.h>256{return Err("cursor exceeds 256x256".into());}
                    if fitted.exact{Ok(image)}else{crate::pixels::resample(&image,fitted.physical.w,fitted.physical.h)}
                });
                match image{Ok(image)=>self.wm.backend_mut().set_cursor(Some((hotspot,image))),Err(e)=>Self::refuse(surface,&e)}
            }
            Some(BufferAssignment::Removed)=>self.wm.backend_mut().set_cursor(None),
            None=>self.wm.backend_mut().set_cursor_hotspot(hotspot),
        }
        self.wm.backend_mut().dirty=true;
        self.cursor_callbacks.extend(callbacks);
    }
}

#[cfg(test)]
mod tests {
    use super::secondary_click;
    #[test]
    fn control_click_is_the_right_button_for_the_whole_press() {
        let mut active = false;
        // Plain click: left.
        assert_eq!(secondary_click(1, 0, false, &mut active), 1);
        assert_eq!(secondary_click(0, 1, false, &mut active), 0);
        // Control held when the button goes down: right, and it stays right
        // while the button is held even if Control is let go.
        assert_eq!(secondary_click(1, 0, true, &mut active), 2);
        assert_eq!(secondary_click(1, 1, false, &mut active), 2);
        assert_eq!(secondary_click(0, 1, false, &mut active), 0);
        assert!(!active);
        // Control pressed in the middle of an ordinary drag does not turn it.
        assert_eq!(secondary_click(1, 0, false, &mut active), 1);
        assert_eq!(secondary_click(1, 1, true, &mut active), 1);
        assert_eq!(secondary_click(0, 1, true, &mut active), 0);
        // A real right or middle button passes through, with or without Control.
        assert_eq!(secondary_click(2, 0, true, &mut active), 2);
        assert_eq!(secondary_click(4, 0, false, &mut active), 4);
        assert_eq!(secondary_click(3, 2, true, &mut active), 2 | 2);
    }
}
