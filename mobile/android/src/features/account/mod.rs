#[cfg(target_os = "android")]
mod android;

#[cfg(target_os = "android")]
pub use android::{available, cancel, listen, start};

#[cfg(not(target_os = "android"))]
pub fn available() -> bool {
    false
}

#[cfg(not(target_os = "android"))]
pub fn start(_: i32, _: bool, _: bool) -> bool {
    false
}

#[cfg(not(target_os = "android"))]
pub fn cancel() {}

#[cfg(not(target_os = "android"))]
pub fn listen(_: impl Fn(i32, i32, String, String) + Send + Sync + 'static) {}
