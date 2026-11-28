#![allow(unsafe_code)]

use jni::{
    Env, JValue, JavaVM, jni_sig, jni_str,
    objects::{JClass, JObject},
    refs::Global,
};

pub fn with_application_class<R>(
    name: &str,
    call: impl for<'local, 'activity> FnOnce(
        &mut Env<'local>,
        &JObject<'activity>,
        &JClass<'local>,
    ) -> jni::errors::Result<R>,
) -> Result<R, String> {
    rustferry::android::with_context(|app| {
        let activity_ptr = app.activity_as_ptr() as jni::sys::jobject;
        // The AndroidApp retains the borrowed JVM and Activity throughout the attached call.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        vm.attach_current_thread(|env| -> jni::errors::Result<R> {
            let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&activity_ptr)? };
            let loader = env
                .call_method(
                    &activity,
                    jni_str!("getClassLoader"),
                    jni_sig!("()Ljava/lang/ClassLoader;"),
                    &[],
                )?
                .into_object()?;
            let name = env.new_string(name)?;
            let class = env
                .call_method(
                    &loader,
                    jni_str!("loadClass"),
                    jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"),
                    &[JValue::Object(&name)],
                )?
                .into_object()?;
            let class = env.cast_local::<JClass>(class)?;
            call(env, activity.as_ref(), &class)
        })
        .map_err(|error| error.to_string())
    })
    .ok_or_else(|| "Android activity is unavailable".to_owned())?
}
