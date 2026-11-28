use android_build::{Dexer, JavaBuild};
use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    slint_build::compile("src/app/ui.slint").expect("cannot compile the Stream Tube interface");

    println!("cargo:rerun-if-changed=src/features/player/ui/VideoPlayer.java");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    let classes = out.join("player-classes");
    fs::create_dir_all(&classes).expect("cannot create the Java class directory");
    let jar = android_build::android_jar(None).expect("Android SDK platform is missing");
    run(JavaBuild::new()
        .file("src/features/player/ui/VideoPlayer.java")
        .class_path(&jar)
        .classes_out_dir(&classes)
        .java_source_version(8)
        .java_target_version(8)
        .command()
        .expect("cannot locate javac")
        .args(["-encoding", "UTF-8"]));

    run(&mut Dexer::new()
        .android_jar(&jar)
        .class_path(&classes)
        .collect_classes(&classes)
        .expect("cannot locate compiled player classes")
        .release(env::var("PROFILE").as_deref() == Ok("release"))
        .android_min_api(26)
        .out_dir(out)
        .command()
        .expect("cannot locate D8"));
}

fn run(command: &mut Command) {
    let result = command
        .output()
        .expect("cannot start the Android build tool");
    assert!(
        result.status.success(),
        "Android build tool failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}
