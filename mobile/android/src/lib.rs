mod app;
mod entities;
mod features;
mod shared;

#[cfg(target_os = "android")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
fn android_main(android_app: slint::android::AndroidApp) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rustferry::android::install(android_app.clone()).map_err(|error| error.to_string())?;
        slint::android::init(android_app).map_err(|error| error.to_string())?;
        app::run().map_err(|error| error.to_string())
    }));

    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!("Stream Tube: {error}"),
        Err(_) => eprintln!("Stream Tube stopped after a native callback panicked"),
    }
}

#[cfg(not(target_os = "android"))]
pub fn run() -> Result<(), slint::PlatformError> {
    app::run()
}
