use crate::os::xkeysyms::keysym_to_keycode;
use crate::{KeyEvent, Modifiers};
use anyhow::{anyhow, ensure};
use libc;
use std::cell::RefCell;
use std::ffi::CStr;
use xkb::compose::Status as ComposeStatus;
use xkbcommon::xkb;

pub struct Keyboard {
    context: xkb::Context,
    keymap: RefCell<xkb::Keymap>,
    device_id: i32,

    state: RefCell<xkb::State>,
    compose_state: RefCell<xkb::compose::State>,
}

impl Keyboard {
    pub fn new(connection: &xcb::Connection) -> anyhow::Result<(Keyboard, u8)> {
        connection.prefetch_extension_data(xcb::xkb::id());

        let first_ev = connection
            .get_extension_data(xcb::xkb::id())
            .map(|r| r.first_event())
            .ok_or_else(|| anyhow!("could not get xkb extension data"))?;

        {
            let cookie = xcb::xkb::use_extension(
                &connection,
                xkb::x11::MIN_MAJOR_XKB_VERSION,
                xkb::x11::MIN_MINOR_XKB_VERSION,
            );
            let r = cookie.get_reply()?;

            ensure!(
                r.supported(),
                "required xcb-xkb-{}-{} is not supported",
                xkb::x11::MIN_MAJOR_XKB_VERSION,
                xkb::x11::MIN_MINOR_XKB_VERSION
            );
        }

        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let device_id = xkb::x11::get_core_keyboard_device_id(&connection);
        ensure!(device_id != -1, "Couldn't find core keyboard device");

        let keymap = xkb::x11::keymap_new_from_device(
            &context,
            &connection,
            device_id,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        );
        let state = xkb::x11::state_new_from_device(&keymap, connection, device_id);

        let locale = query_lc_ctype()?;

        let table = xkb::compose::Table::new_from_locale(
            &context,
            locale.to_str()?,
            xkb::compose::COMPILE_NO_FLAGS,
        )
        .map_err(|_| anyhow!("Failed to acquire compose table from locale"))?;
        let compose_state = xkb::compose::State::new(&table, xkb::compose::STATE_NO_FLAGS);

        {
            let map_parts = xcb::xkb::MAP_PART_KEY_TYPES
                | xcb::xkb::MAP_PART_KEY_SYMS
                | xcb::xkb::MAP_PART_MODIFIER_MAP
                | xcb::xkb::MAP_PART_EXPLICIT_COMPONENTS
                | xcb::xkb::MAP_PART_KEY_ACTIONS
                | xcb::xkb::MAP_PART_KEY_BEHAVIORS
                | xcb::xkb::MAP_PART_VIRTUAL_MODS
                | xcb::xkb::MAP_PART_VIRTUAL_MOD_MAP;

            let events = xcb::xkb::EVENT_TYPE_NEW_KEYBOARD_NOTIFY
                | xcb::xkb::EVENT_TYPE_MAP_NOTIFY
                | xcb::xkb::EVENT_TYPE_STATE_NOTIFY;

            let cookie = xcb::xkb::select_events_checked(
                &connection,
                device_id as u16,
                events as u16,
                0,
                events as u16,
                map_parts as u16,
                map_parts as u16,
                None,
            );

            cookie.request_check()?;
        }

        let kbd = Keyboard {
            context,
            device_id,
            keymap: RefCell::new(keymap),
            state: RefCell::new(state),
            compose_state: RefCell::new(compose_state),
        };

        Ok((kbd, first_ev))
    }

    pub fn process_key_event(&self, xcb_ev: &xcb::KeyPressEvent) -> Option<KeyEvent> {
        let pressed = (xcb_ev.response_type() & !0x80) == xcb::KEY_PRESS;

        let xcode = xkb::Keycode::from(xcb_ev.detail());
        let xsym = self.state.borrow().key_get_one_sym(xcode);

        let ksym = if pressed {
            self.compose_state.borrow_mut().feed(xsym);

            let cstate = self.compose_state.borrow().status();
            match cstate {
                ComposeStatus::Composing => {
                    // eat
                    return None;
                }
                ComposeStatus::Composed => {
                    let res = self.compose_state.borrow().keysym();
                    self.compose_state.borrow_mut().reset();
                    res.unwrap_or(xsym)
                }
                ComposeStatus::Nothing => xsym,
                ComposeStatus::Cancelled => {
                    self.compose_state.borrow_mut().reset();
                    return None;
                }
            }
        } else {
            xsym
        };

        let kc = keysym_to_keycode(ksym).or_else(|| keysym_to_keycode(xsym))?;
        let raw_modifiers = self.get_key_modifiers();
        // X11 keysyms that map to KeyCode::Char already factor in the SHIFT
        // modifier state.  eg: SHIFT-c in an US layout produces `Char('C')`.
        // So, if we have `Char`, remove SHIFT from the processed modifier
        // state.  Not doing so can produce frustration such as that in
        // https://github.com/wez/wezterm/issues/394, but take care to avoid
        // eliminating it for eg: Enter (https://github.com/wez/wezterm/issues/516)
        let modifiers = match (&kc, raw_modifiers) {
            (crate::KeyCode::Char(c), mods)
                if !c.is_ascii_whitespace() && !c.is_ascii_control() =>
            {
                mods - Modifiers::SHIFT
            }
            (_, mods) => mods,
        };

        Some(KeyEvent {
            key: kc,
            modifiers,
            raw_key: None,
            raw_modifiers,
            raw_code: Some(xcode),
            repeat_count: 1,
            key_is_down: pressed,
        })
    }

    fn mod_is_active(&self, modifier: &str) -> bool {
        // [TODO] consider state  Depressed & consumed mods
        self.state
            .borrow()
            .mod_name_is_active(modifier, xkb::STATE_MODS_EFFECTIVE)
    }

    pub fn get_key_modifiers(&self) -> Modifiers {
        let mut res = Modifiers::default();

        if self.mod_is_active(xkb::MOD_NAME_SHIFT) {
            res |= Modifiers::SHIFT;
        }
        if self.mod_is_active(xkb::MOD_NAME_CTRL) {
            res |= Modifiers::CTRL;
        }
        if self.mod_is_active(xkb::MOD_NAME_ALT) {
            // Mod1
            res |= Modifiers::ALT;
        }
        if self.mod_is_active(xkb::MOD_NAME_LOGO) {
            // Mod4
            res |= Modifiers::SUPER;
        }
        if self.mod_is_active("Mod3") {
            res |= Modifiers::SUPER;
        }
        //Mod2 is numlock
        res
    }

    pub fn process_xkb_event(
        &self,
        connection: &xcb::Connection,
        event: &xcb::GenericEvent,
    ) -> anyhow::Result<()> {
        let xkb_ev: &XkbGenericEvent = unsafe { xcb::cast_event(&event) };

        if xkb_ev.device_id() == self.get_device_id() as u8 {
            match xkb_ev.xkb_type() {
                xcb::xkb::STATE_NOTIFY => {
                    self.update_state(unsafe { xcb::cast_event(&event) });
                }
                xcb::xkb::MAP_NOTIFY | xcb::xkb::NEW_KEYBOARD_NOTIFY => {
                    self.update_keymap(connection)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn update_state(&self, ev: &xcb::xkb::StateNotifyEvent) {
        self.state.borrow_mut().update_mask(
            xkb::ModMask::from(ev.base_mods()),
            xkb::ModMask::from(ev.latched_mods()),
            xkb::ModMask::from(ev.locked_mods()),
            ev.base_group() as xkb::LayoutIndex,
            ev.latched_group() as xkb::LayoutIndex,
            xkb::LayoutIndex::from(ev.locked_group()),
        );
    }

    pub fn update_keymap(&self, connection: &xcb::Connection) -> anyhow::Result<()> {
        let new_keymap = xkb::x11::keymap_new_from_device(
            &self.context,
            &connection,
            self.get_device_id(),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        );
        ensure!(
            !new_keymap.get_raw_ptr().is_null(),
            "problem with new keymap"
        );

        let new_state = xkb::x11::state_new_from_device(&new_keymap, connection, self.device_id);
        ensure!(!new_state.get_raw_ptr().is_null(), "problem with new state");

        self.state.replace(new_state);
        self.keymap.replace(new_keymap);
        Ok(())
    }

    pub fn get_device_id(&self) -> i32 {
        self.device_id
    }
}

fn query_lc_ctype() -> anyhow::Result<&'static CStr> {
    let ptr = unsafe { libc::setlocale(libc::LC_CTYPE, std::ptr::null()) };
    ensure!(!ptr.is_null(), "failed to query locale");
    unsafe { Ok(CStr::from_ptr(ptr)) }
}

/// struct that has fields common to the 3 different xkb events
/// (StateNotify, NewKeyboardNotify, MapNotify)
#[repr(C)]
struct xcb_xkb_generic_event_t {
    response_type: u8,
    xkb_type: u8,
    sequence: u16,
    time: xcb::Timestamp,
    device_id: u8,
}

struct XkbGenericEvent {
    base: xcb::Event<xcb_xkb_generic_event_t>,
}

impl XkbGenericEvent {
    pub fn xkb_type(&self) -> u8 {
        unsafe { (*self.base.ptr).xkb_type }
    }

    pub fn device_id(&self) -> u8 {
        unsafe { (*self.base.ptr).device_id }
    }
}
