#![cfg(windows)]

use anyhow::{Result, anyhow};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    path::Path,
    ptr,
};

pub struct ProviderRow {
    pub id: String,
    pub name: String,
    pub settings: String,
    pub website_url: String,
}

pub fn providers(path: &Path, app_type: &str) -> Result<Vec<ProviderRow>> {
    if !matches!(app_type, "codex" | "claude") {
        return Err(anyhow!("不支持的 CC-Switch app_type"));
    }
    let name = CString::new(path.to_string_lossy().as_bytes())?;
    let mut db = ptr::null_mut();
    let open = unsafe {
        sqlite3_open_v2(
            name.as_ptr(),
            &mut db,
            SQLITE_OPEN_READONLY | SQLITE_OPEN_URI,
            ptr::null(),
        )
    };
    if open != SQLITE_OK {
        return Err(anyhow!("无法只读打开 CC-Switch 数据库"));
    }
    let sql = CString::new(format!(
        "SELECT id,name,settings_config,website_url FROM providers WHERE app_type='{app_type}' ORDER BY sort_index ASC,created_at DESC"
    ))?;
    let mut stmt = ptr::null_mut();
    let prepared = unsafe { sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()) };
    if prepared != SQLITE_OK {
        let message = unsafe { CStr::from_ptr(sqlite3_errmsg(db)) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            sqlite3_close(db);
        }
        return Err(anyhow!(message));
    }
    let mut rows = Vec::new();
    while unsafe { sqlite3_step(stmt) } == SQLITE_ROW {
        rows.push(ProviderRow {
            id: text(stmt, 0),
            name: text(stmt, 1),
            settings: text(stmt, 2),
            website_url: text(stmt, 3),
        });
    }
    unsafe {
        sqlite3_finalize(stmt);
        sqlite3_close(db);
    }
    Ok(rows)
}

fn text(stmt: *mut c_void, column: c_int) -> String {
    let value = unsafe { sqlite3_column_text(stmt, column) };
    if value.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(value as *const c_char) }
            .to_string_lossy()
            .into_owned()
    }
}

#[link(name = "winsqlite3")]
unsafe extern "C" {
    fn sqlite3_open_v2(
        filename: *const c_char,
        db: *mut *mut c_void,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close(db: *mut c_void) -> c_int;
    fn sqlite3_prepare_v2(
        db: *mut c_void,
        sql: *const c_char,
        bytes: c_int,
        stmt: *mut *mut c_void,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_step(stmt: *mut c_void) -> c_int;
    fn sqlite3_finalize(stmt: *mut c_void) -> c_int;
    fn sqlite3_column_text(stmt: *mut c_void, column: c_int) -> *const u8;
    fn sqlite3_errmsg(db: *mut c_void) -> *const c_char;
}
const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_OPEN_READONLY: c_int = 1;
const SQLITE_OPEN_URI: c_int = 0x40;
