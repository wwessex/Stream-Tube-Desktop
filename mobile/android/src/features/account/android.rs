#![allow(unsafe_code)]

use crate::shared::android::with_application_class;
use jni::{EnvUnowned, JValue, jni_sig, jni_str, objects::JString};
use std::sync::{Arc, Mutex};

type Listener = dyn Fn(i32, i32, String, String) + Send + Sync + 'static;
static LISTENER: Mutex<Option<Arc<Listener>>> = Mutex::new(None);
const CLASS: &str = "io.github.shiroksh.streamtube.AccountAuthorization";

pub fn listen(callback: impl Fn(i32, i32, String, String) + Send + Sync + 'static) {
    if let Ok(mut listener) = LISTENER.lock() {
        *listener = Some(Arc::new(callback));
    }
}

pub fn available() -> bool {
    with_application_class(CLASS, |env, activity, google| {
        env.call_static_method(
            google,
            jni_str!("available"),
            jni_sig!("(Landroid/app/Activity;)Z"),
            &[JValue::Object(activity)],
        )?
        .z()
    })
    .unwrap_or(false)
}

pub fn start(request: i32, interactive: bool, choose_account: bool) -> bool {
    with_application_class(CLASS, |env, activity, google| {
        env.call_static_method(
            google,
            jni_str!("authorize"),
            jni_sig!("(Landroid/app/Activity;IZZ)V"),
            &[
                JValue::Object(activity),
                JValue::Int(request),
                JValue::Bool(interactive),
                JValue::Bool(choose_account),
            ],
        )?;
        Ok(())
    })
    .is_ok()
}

pub fn cancel() {
    let _ = with_application_class(CLASS, |env, activity, google| {
        env.call_static_method(
            google,
            jni_str!("disconnect"),
            jni_sig!("(Landroid/app/Activity;)V"),
            &[JValue::Object(activity)],
        )?;
        Ok(())
    });
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_github_shiroksh_streamtube_AccountAuthorization_deliver<'local>(
    mut env: EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    request: i32,
    status: i32,
    name: JString<'local>,
    channels: JString<'local>,
) {
    use jni::errors::ThrowRuntimeExAndDefault;

    env.with_env(|env| -> jni::errors::Result<()> {
        let name = name.try_to_string(env)?;
        let channels = channels.try_to_string(env)?;
        let listener = LISTENER.lock().ok().and_then(|listener| listener.clone());
        if let Some(listener) = listener {
            listener(request, status, name, channels);
        }
        Ok(())
    })
    .resolve::<ThrowRuntimeExAndDefault>();
}
