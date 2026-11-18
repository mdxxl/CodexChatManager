use crate::wide;
use std::cell::Cell;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegGetValueW, RegSetValueExW, HKEY_CURRENT_USER, KEY_SET_VALUE,
    REG_DWORD, REG_OPTION_NON_VOLATILE, RRF_RT_REG_DWORD,
};

const SETTINGS_KEY: &str = "Software\\CodexChatManager";
const LANGUAGE_VALUE: &str = "Language";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Language {
    Russian,
    English,
}

thread_local! {
    static CURRENT: Cell<Language> = const { Cell::new(Language::Russian) };
}

impl Language {
    pub fn current() -> Self {
        CURRENT.get()
    }

    pub fn set_current(self) {
        CURRENT.set(self);
    }

    pub fn saved() -> Self {
        let mut value = 0u32;
        let mut size = size_of::<u32>() as u32;
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                wide(SETTINGS_KEY).as_ptr(),
                wide(LANGUAGE_VALUE).as_ptr(),
                RRF_RT_REG_DWORD,
                null_mut(),
                (&mut value as *mut u32).cast(),
                &mut size,
            )
        };
        if result == ERROR_SUCCESS && value == Self::English as u32 {
            Self::English
        } else {
            Self::Russian
        }
    }

    pub fn save(self) -> std::io::Result<()> {
        let mut key = null_mut();
        let result = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                wide(SETTINGS_KEY).as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                null(),
                &mut key,
                null_mut(),
            )
        };
        if result != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(result as i32));
        }
        let value = (self as u32).to_le_bytes();
        let result = unsafe {
            let result = RegSetValueExW(
                key,
                wide(LANGUAGE_VALUE).as_ptr(),
                0,
                REG_DWORD,
                value.as_ptr(),
                value.len() as u32,
            );
            RegCloseKey(key);
            result
        };
        if result == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(result as i32))
        }
    }
}

pub fn tr<'a>(russian: &'a str, english: &'a str) -> &'a str {
    match Language::current() {
        Language::Russian => russian,
        Language::English => english,
    }
}

macro_rules! trf {
    ($russian:literal, $english:literal $(, $args:expr)* $(,)?) => {
        match $crate::language::Language::current() {
            $crate::language::Language::Russian => format!($russian $(, $args)*),
            $crate::language::Language::English => format!($english $(, $args)*),
        }
    };
}
pub(crate) use trf;

pub fn delete_word() -> &'static str {
    tr("УДАЛИТЬ", "DELETE")
}

pub fn demo_title() -> &'static str {
    tr("Codex Chat Manager — ДЕМО", "Codex Chat Manager — DEMO")
}
