#![allow(unsafe_code)]

use crate::shared::{
    android::with_application_class,
    i18n::{Language, player_labels},
};
use jni::{EnvUnowned, JValue, jni_sig, jni_str, objects::JString};
use std::sync::{Arc, Mutex};

const CLASS: &str = "io.github.shiroksh.streamtube.VideoPlayer";
type Listener = dyn Fn(i32, i32, String) + Send + Sync + 'static;
static LISTENER: Mutex<Option<Arc<Listener>>> = Mutex::new(None);

pub fn listen(callback: impl Fn(i32, i32, String) + Send + Sync + 'static) {
    if let Ok(mut listener) = LISTENER.lock() {
        *listener = Some(Arc::new(callback));
    }
}

pub fn open(
    session: i32,
    queue: &str,
    first: usize,
    language: Language,
    shorts: bool,
    more: bool,
) -> Result<(), String> {
    let first = i32::try_from(first).map_err(|error| error.to_string())?;
    with_application_class(CLASS, |env, activity, class| {
        let queue = env.new_string(queue)?;
        let labels = env.new_string(player_labels(language))?;
        let code = env.new_string(language.code())?;
        env.call_static_method(
            class,
            jni_str!("open"),
            jni_sig!("(Landroid/app/Activity;ILjava/lang/String;ILjava/lang/String;Ljava/lang/String;ZZ)V"),
            &[
                JValue::Object(activity),
                JValue::Int(session),
                JValue::Object(&queue),
                JValue::Int(first),
                JValue::Object(&code),
                JValue::Object(&labels),
                JValue::Bool(shorts),
                JValue::Bool(more),
            ],
        )?;
        Ok(())
    })
}

pub fn append(session: i32, queue: &str, more: bool, failed: bool) {
    let _ = with_application_class(CLASS, |env, activity, class| {
        let queue = env.new_string(queue)?;
        env.call_static_method(
            class,
            jni_str!("append"),
            jni_sig!("(Landroid/app/Activity;ILjava/lang/String;ZZ)V"),
            &[
                JValue::Object(activity),
                JValue::Int(session),
                JValue::Object(&queue),
                JValue::Bool(more),
                JValue::Bool(failed),
            ],
        )?;
        Ok(())
    });
}

pub fn update_saved(session: i32, id: &str, saved: bool) {
    let _ = with_application_class(CLASS, |env, activity, class| {
        let id = env.new_string(id)?;
        env.call_static_method(
            class,
            jni_str!("updateSaved"),
            jni_sig!("(Landroid/app/Activity;ILjava/lang/String;Z)V"),
            &[
                JValue::Object(activity),
                JValue::Int(session),
                JValue::Object(&id),
                JValue::Bool(saved),
            ],
        )?;
        Ok(())
    });
}

pub fn describe(session: i32, id: &str, description: &str) {
    let _ = with_application_class(CLASS, |env, activity, class| {
        let id = env.new_string(id)?;
        let description = env.new_string(description)?;
        env.call_static_method(
            class,
            jni_str!("describe"),
            jni_sig!("(Landroid/app/Activity;ILjava/lang/String;Ljava/lang/String;)V"),
            &[
                JValue::Object(activity),
                JValue::Int(session),
                JValue::Object(&id),
                JValue::Object(&description),
            ],
        )?;
        Ok(())
    });
}

pub fn set_extras(session: i32, id: &str, packet: &str) {
    let _ = with_application_class(CLASS, |env, activity, class| {
        let id = env.new_string(id)?;
        let packet = env.new_string(packet)?;
        env.call_static_method(
            class,
            jni_str!("setExtras"),
            jni_sig!("(Landroid/app/Activity;ILjava/lang/String;Ljava/lang/String;)V"),
            &[
                JValue::Object(activity),
                JValue::Int(session),
                JValue::Object(&id),
                JValue::Object(&packet),
            ],
        )?;
        Ok(())
    });
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_github_shiroksh_streamtube_VideoPlayer_event<'local>(
    mut env: EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    session: i32,
    kind: i32,
    id: JString<'local>,
) {
    use jni::errors::ThrowRuntimeExAndDefault;

    env.with_env(|env| -> jni::errors::Result<()> {
        let id = id.try_to_string(env)?;
        let listener = LISTENER.lock().ok().and_then(|listener| listener.clone());
        if let Some(listener) = listener {
            listener(session, kind, id);
        }
        Ok(())
    })
    .resolve::<ThrowRuntimeExAndDefault>();
}
