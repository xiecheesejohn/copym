//! JNI entry points for `io.github.xiecheesejohn.copym.Native` (Kotlin).
//!
//! Every function maps 1:1 onto [`app::App`]. Errors become a Java
//! `RuntimeException` with the message; structured results are JSON strings.

pub mod app;

use app::{App, R};
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong, jstring, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;
use std::sync::{Arc, OnceLock};

static APP: OnceLock<Arc<App>> = OnceLock::new();

fn app() -> R<&'static Arc<App>> {
    APP.get().ok_or_else(|| "copym core not initialised".to_string())
}

fn throw(env: &mut JNIEnv, msg: &str) {
    let _ = env.throw_new("java/lang/RuntimeException", msg);
}

fn get_string(env: &mut JNIEnv, s: &JString) -> R<String> {
    if s.is_null() {
        return Ok(String::new());
    }
    env.get_string(s).map(Into::into).map_err(|e| e.to_string())
}

fn opt_string(env: &mut JNIEnv, s: &JString) -> R<Option<String>> {
    Ok(Some(get_string(env, s)?).filter(|s| !s.is_empty()))
}

/// Run `f`, turning `Err` into a Java exception and returning `default`.
fn guard<T>(env: &mut JNIEnv, default: T, f: impl FnOnce(&mut JNIEnv) -> R<T>) -> T {
    match f(env) {
        Ok(v) => v,
        Err(e) => {
            throw(env, &e);
            default
        }
    }
}

fn new_string(env: &mut JNIEnv, s: &str) -> R<jstring> {
    env.new_string(s).map(|s| s.into_raw()).map_err(|e| e.to_string())
}

fn new_bytes(env: &mut JNIEnv, b: Option<Vec<u8>>) -> R<jbyteArray> {
    match b {
        Some(b) => env.byte_array_from_slice(&b).map(|a| a.into_raw()).map_err(|e| e.to_string()),
        None => Ok(std::ptr::null_mut()),
    }
}

fn jbool(b: bool) -> jboolean {
    if b { JNI_TRUE } else { JNI_FALSE }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xiecheesejohn_copym_Native_open<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    data_dir: JString<'l>,
    device_name: JString<'l>,
    peers: JString<'l>,
) {
    guard(&mut env, (), |env| {
        if APP.get().is_some() {
            return Ok(());
        }
        let dir = get_string(env, &data_dir)?;
        let name = get_string(env, &device_name)?;
        let peers = get_string(env, &peers)?;
        let a = App::open(&dir, &name, app::SYNC_PORT, &peers, true)?;
        let _ = APP.set(a);
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xiecheesejohn_copym_Native_version<'l>(_: JNIEnv<'l>, _: JClass<'l>) -> jlong {
    APP.get().map(|a| a.version() as jlong).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xiecheesejohn_copym_Native_list<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    query: JString<'l>,
    regex: jboolean,
    filter: jint,
    limit: jint,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let q = get_string(env, &query)?;
        let json = app()?.list(&q, regex != JNI_FALSE, filter, limit.max(1) as usize)?;
        new_string(env, &json)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xiecheesejohn_copym_Native_text<'l>(mut env: JNIEnv<'l>, _: JClass<'l>, id: jlong) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| match app()?.text(id as u64)? {
        Some(t) => new_string(env, &t),
        None => Ok(std::ptr::null_mut()),
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xiecheesejohn_copym_Native_image<'l>(mut env: JNIEnv<'l>, _: JClass<'l>, id: jlong) -> jbyteArray {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let b = app()?.image(id as u64)?;
        new_bytes(env, b)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xiecheesejohn_copym_Native_thumb<'l>(mut env: JNIEnv<'l>, _: JClass<'l>, id: jlong) -> jbyteArray {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let b = app()?.thumb(id as u64)?;
        new_bytes(env, b)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xiecheesejohn_copym_Native_addText<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    text: JString<'l>,
    source: JString<'l>,
) -> jboolean {
    guard(&mut env, JNI_FALSE, |env| {
        let t = get_string(env, &text)?;
        let s = opt_string(env, &source)?;
        app()?.add_text(t, s).map(jbool)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xiecheesejohn_copym_Native_addImage<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    png: JByteArray<'l>,
    source: JString<'l>,
) -> jboolean {
    guard(&mut env, JNI_FALSE, |env| {
        let bytes = env.convert_byte_array(&png).map_err(|e| e.to_string())?;
        let s = opt_string(env, &source)?;
        app()?.add_image(bytes, s).map(jbool)
    })
}

macro_rules! unit_by_id {
    ($jname:ident, $method:ident) => {
        #[unsafe(no_mangle)]
        pub extern "system" fn $jname<'l>(mut env: JNIEnv<'l>, _: JClass<'l>, id: jlong) {
            guard(&mut env, (), |_| app()?.$method(id as u64))
        }
    };
}

unit_by_id!(Java_io_github_xiecheesejohn_copym_Native_touch, touch);
unit_by_id!(Java_io_github_xiecheesejohn_copym_Native_delete, delete);
unit_by_id!(Java_io_github_xiecheesejohn_copym_Native_togglePin, toggle_pin);

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xiecheesejohn_copym_Native_clear<'l>(mut env: JNIEnv<'l>, _: JClass<'l>) {
    guard(&mut env, (), |_| app()?.clear())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xiecheesejohn_copym_Native_syncStatus<'l>(mut env: JNIEnv<'l>, _: JClass<'l>) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let s = app()?.sync_status();
        new_string(env, &s)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xiecheesejohn_copym_Native_pairStart<'l>(mut env: JNIEnv<'l>, _: JClass<'l>) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let s = app()?.pair_start()?;
        new_string(env, &s)
    })
}

/// Blocking (mDNS discovery + handshake, up to ~10 s): call off the main thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xiecheesejohn_copym_Native_join<'l>(mut env: JNIEnv<'l>, _: JClass<'l>, code: JString<'l>) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let c = get_string(env, &code)?;
        let name = app()?.join(&c)?;
        new_string(env, &name)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xiecheesejohn_copym_Native_leave<'l>(mut env: JNIEnv<'l>, _: JClass<'l>) {
    guard(&mut env, (), |_| app()?.leave())
}
