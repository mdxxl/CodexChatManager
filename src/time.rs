use chrono::{NaiveDate, NaiveDateTime};
use std::mem::zeroed;
use std::ptr::null;
use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTimeEx};

pub fn now_local() -> NaiveDateTime {
    let mut local: SYSTEMTIME = unsafe { zeroed() };
    unsafe {
        GetLocalTime(&mut local);
    }
    local_calendar(local).unwrap_or_default()
}

pub fn from_unix_local(seconds: i64) -> Option<NaiveDateTime> {
    const WINDOWS_EPOCH_OFFSET: i64 = 11_644_473_600;
    let ticks = u64::try_from(seconds.checked_add(WINDOWS_EPOCH_OFFSET)?)
        .ok()?
        .checked_mul(10_000_000)?;
    let file_time = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc: SYSTEMTIME = unsafe { zeroed() };
    let mut local: SYSTEMTIME = unsafe { zeroed() };
    if unsafe { FileTimeToSystemTime(&file_time, &mut utc) } == 0
        || unsafe { SystemTimeToTzSpecificLocalTimeEx(null(), &utc, &mut local) } == 0
    {
        return None;
    }
    local_calendar(local)
}

fn local_calendar(value: SYSTEMTIME) -> Option<NaiveDateTime> {
    NaiveDate::from_ymd_opt(value.wYear.into(), value.wMonth.into(), value.wDay.into())?
        .and_hms_milli_opt(
            value.wHour.into(),
            value.wMinute.into(),
            value.wSecond.into(),
            value.wMilliseconds.into(),
        )
}
