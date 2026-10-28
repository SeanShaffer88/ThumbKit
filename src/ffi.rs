//! C ABI surface — everything declared in include/thumbkit.h.
//! Conventions (house standard):
//! - `*mut c_char` results are caller-owned; free with
//!   `th_free_string`.
//! - Failures set a thread-local last-error (code +
//!   message) and return the documented TH_ERR_* code.
//! - Nothing unwinds across the FFI boundary
//!   (catch_unwind everywhere; release profile also sets
//!   panic = "abort").
//! - A pipeline handle is thread-safe: all state is
//!   serialized inside, and `th_close` joins the workers.

// The exported functions ARE the raw-pointer C ABI declared
// in include/thumbkit.h; pointer validity is the documented
// contract.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::catch_unwind;
use std::sync::Arc;

use crate::{parse_config, GenerateFn, Pipeline, ThResult, ERR_INTERNAL, ERR_INVALID_ARG, TH_OK};

thread_local! {
    static LAST_ERROR: RefCell<(i32, CString)> =
        RefCell::new((0, CString::new("").unwrap()));
}

pub fn set_error(code: i32, msg: &str) {
    let clean: String = msg.chars().filter(|&c| c != '\0').collect();
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = (
            code,
            CString::new(clean).unwrap_or_else(|_| CString::new("error").unwrap()),
        );
    });
}

fn clear_error() {
    set_error(TH_OK, "");
}

fn fail(code: i32, msg: &str) -> c_int {
    set_error(code, msg);
    code as c_int
}

fn panic_code() -> c_int {
    set_error(ERR_INTERNAL, "internal panic caught at the FFI boundary");
    ERR_INTERNAL as c_int
}

fn guard<T, F: FnOnce() -> T>(f: F) -> std::thread::Result<T> {
    catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn cstr<'a>(ptr: *const c_char, what: &str) -> ThResult<&'a str> {
    if ptr.is_null() {
        return Err((ERR_INVALID_ARG, format!("{what} must not be null")));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| (ERR_INVALID_ARG, format!("{what} must be valid UTF-8")))
}

fn cstr_opt<'a>(ptr: *const c_char, what: &str) -> ThResult<Option<&'a str>> {
    if ptr.is_null() {
        Ok(None)
    } else {
        cstr(ptr, what).map(Some)
    }
}

fn pipe_ref<'a>(ptr: *mut Pipeline) -> ThResult<&'a Pipeline> {
    if ptr.is_null() {
        return Err((
            ERR_INVALID_ARG,
            "pipeline handle must not be null".to_string(),
        ));
    }
    Ok(unsafe { &*ptr })
}

fn out_string(out: *mut *mut c_char, text: String) -> c_int {
    if out.is_null() {
        return fail(ERR_INVALID_ARG, "out_json must not be null");
    }
    unsafe { *out = std::ptr::null_mut() };
    match CString::new(text) {
        Ok(s) => {
            unsafe { *out = s.into_raw() };
            clear_error();
            TH_OK as c_int
        }
        Err(_) => fail(ERR_INTERNAL, "result string contained a NUL byte"),
    }
}

fn report(result: ThResult<()>) -> c_int {
    match result {
        Ok(()) => {
            clear_error();
            TH_OK as c_int
        }
        Err((code, msg)) => fail(code, &msg),
    }
}

#[no_mangle]
pub extern "C" fn th_version() -> *const c_char {
    static VERSION_C: &[u8] = b"0.1.0\0";
    VERSION_C.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn th_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().1.as_ptr())
}

#[no_mangle]
pub extern "C" fn th_last_error_code() -> c_int {
    LAST_ERROR.with(|e| e.borrow().0) as c_int
}

/// # Safety (caller contract): `p` is null or a string this
/// library returned and has not been freed yet. Documented
/// in the header.
#[no_mangle]
pub extern "C" fn th_free_string(p: *mut c_char) {
    if !p.is_null() {
        unsafe {
            drop(CString::from_raw(p));
        }
    }
}

fn create_impl(config_json: *const c_char, out: *mut *mut Pipeline) -> c_int {
    if out.is_null() {
        return fail(ERR_INVALID_ARG, "out_handle must not be null");
    }
    unsafe { *out = std::ptr::null_mut() };
    let inner = (|| -> ThResult<Arc<Pipeline>> {
        let text = cstr(config_json, "config_json")?;
        let cfg = parse_config(text)?;
        Pipeline::create(cfg)
    })();
    match inner {
        Ok(pipe) => {
            unsafe { *out = Arc::into_raw(pipe) as *mut Pipeline };
            clear_error();
            TH_OK as c_int
        }
        Err((code, msg)) => fail(code, &msg),
    }
}

#[no_mangle]
pub extern "C" fn th_create(config_json: *const c_char, out_handle: *mut *mut Pipeline) -> c_int {
    match guard(|| create_impl(config_json, out_handle)) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

/// Close the handle and free it: queued requests are
/// cancelled, a running generation finishes, and every
/// worker is JOINED before this returns. Null is a no-op.
#[no_mangle]
pub extern "C" fn th_close(handle: *mut Pipeline) {
    if !handle.is_null() {
        let _ = guard(|| unsafe {
            let arc = Arc::from_raw(handle as *const Pipeline);
            arc.shutdown();
        });
    }
}

#[no_mangle]
pub extern "C" fn th_register_provider(
    handle: *mut Pipeline,
    extension_csv: *const c_char,
    callback: Option<GenerateFn>,
    user_data: *mut c_void,
) -> c_int {
    match guard(|| {
        let inner = (|| -> ThResult<()> {
            let pipe = pipe_ref(handle)?;
            let csv = cstr(extension_csv, "extension_csv")?;
            let cb =
                callback.ok_or((ERR_INVALID_ARG, "generate_cb must not be null".to_string()))?;
            pipe.register_provider(csv, cb, user_data)?;
            Ok(())
        })();
        report(inner)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn th_request(
    handle: *mut Pipeline,
    source_path: *const c_char,
    preset_json: *const c_char,
    out_id: *mut u64,
) -> c_int {
    match guard(|| {
        if out_id.is_null() {
            return fail(ERR_INVALID_ARG, "out_id must not be null");
        }
        unsafe { *out_id = 0 };
        let inner = (|| -> ThResult<u64> {
            let pipe = pipe_ref(handle)?;
            let src = cstr(source_path, "source_path")?;
            let preset = cstr_opt(preset_json, "preset_json")?;
            pipe.request(src, preset)
        })();
        match inner {
            Ok(id) => {
                unsafe { *out_id = id };
                clear_error();
                TH_OK as c_int
            }
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn th_poll(handle: *mut Pipeline, id: u64, out_json: *mut *mut c_char) -> c_int {
    match guard(|| {
        if out_json.is_null() {
            return fail(ERR_INVALID_ARG, "out_json must not be null");
        }
        unsafe { *out_json = std::ptr::null_mut() };
        match pipe_ref(handle).and_then(|p| p.poll(id)) {
            Ok(st) => out_string(out_json, st.to_json()),
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn th_wait(handle: *mut Pipeline, id: u64, timeout_ms: u32) -> c_int {
    match guard(|| {
        let inner = pipe_ref(handle).and_then(|p| p.wait(id, timeout_ms));
        report(inner)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn th_cancel(handle: *mut Pipeline, id: u64) -> c_int {
    match guard(|| {
        let inner = pipe_ref(handle).and_then(|p| p.cancel(id));
        report(inner)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn th_stats(handle: *mut Pipeline, out_json: *mut *mut c_char) -> c_int {
    match guard(|| {
        if out_json.is_null() {
            return fail(ERR_INVALID_ARG, "out_json must not be null");
        }
        unsafe { *out_json = std::ptr::null_mut() };
        match pipe_ref(handle) {
            Ok(p) => out_string(out_json, p.stats()),
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn th_sweep(handle: *mut Pipeline, out_json: *mut *mut c_char) -> c_int {
    match guard(|| {
        if out_json.is_null() {
            return fail(ERR_INVALID_ARG, "out_json must not be null");
        }
        unsafe { *out_json = std::ptr::null_mut() };
        match pipe_ref(handle) {
            Ok(p) => out_string(out_json, p.sweep()),
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn cs(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn take_string(p: *mut c_char) -> String {
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        th_free_string(p);
        s
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("thumbkit-ffi-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ffi_end_to_end() {
        let dir = temp_dir("e2e");
        // Build a real BMP source through the Rust decoder
        // path: write one with the crate's encoder.
        let img = crate::DecodedImage {
            width: 8,
            height: 8,
            rgb: vec![200; 8 * 8 * 3],
        };
        let src = dir.join("x.bmp");
        std::fs::write(&src, crate::encode_bmp(&img)).unwrap();
        let cache = dir.join("cache");
        let cfg = cs(&format!(
            "{{\"cache_dir\":\"{}\"}}",
            cache.to_string_lossy()
        ));
        let mut handle: *mut Pipeline = std::ptr::null_mut();
        assert_eq!(th_create(cfg.as_ptr(), &mut handle), TH_OK as c_int);
        assert!(!handle.is_null());

        let mut id: u64 = 0;
        assert_eq!(
            th_request(
                handle,
                cs(&src.to_string_lossy()).as_ptr(),
                cs(r#"{"max_w":4}"#).as_ptr(),
                &mut id
            ),
            TH_OK as c_int
        );
        assert!(id >= 1);
        assert_eq!(th_wait(handle, id, crate::WAIT_INFINITE), TH_OK as c_int);
        let mut out: *mut c_char = std::ptr::null_mut();
        assert_eq!(th_poll(handle, id, &mut out), TH_OK as c_int);
        let v = crate::json::parse(&take_string(out)).unwrap();
        assert_eq!(v.get("state").unwrap().as_str(), Some("done"));
        assert_eq!(v.get("width").unwrap().as_i64(), Some(4));
        assert_eq!(v.get("source_width").unwrap().as_i64(), Some(8));

        assert_eq!(th_stats(handle, &mut out), TH_OK as c_int);
        let v = crate::json::parse(&take_string(out)).unwrap();
        assert_eq!(v.get("entries").unwrap().as_i64(), Some(1));
        assert_eq!(th_sweep(handle, &mut out), TH_OK as c_int);
        let v = crate::json::parse(&take_string(out)).unwrap();
        assert_eq!(v.get("removed_orphans").unwrap().as_i64(), Some(0));

        // Unknown id and null handle are clean errors.
        assert_eq!(
            th_poll(handle, 424242, &mut out),
            crate::ERR_NOT_FOUND as c_int
        );
        assert_eq!(
            th_poll(std::ptr::null_mut(), 1, &mut out),
            ERR_INVALID_ARG as c_int
        );
        th_close(handle);
        th_close(std::ptr::null_mut()); // no-op
    }

    #[test]
    fn ffi_config_errors() {
        let mut handle: *mut Pipeline = std::ptr::null_mut();
        let bad = cs(r#"{"cache_dir":"#);
        assert_eq!(
            th_create(bad.as_ptr(), &mut handle),
            crate::ERR_PARSE as c_int
        );
        assert!(handle.is_null());
        let missing = cs(r#"{}"#);
        assert_eq!(
            th_create(missing.as_ptr(), &mut handle),
            ERR_INVALID_ARG as c_int
        );
        assert_eq!(th_last_error_code(), ERR_INVALID_ARG as c_int);
        assert_eq!(
            th_create(std::ptr::null(), &mut handle),
            ERR_INVALID_ARG as c_int
        );
    }
}
