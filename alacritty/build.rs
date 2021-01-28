use std::env;
use std::fs::File;
use std::path::Path;
use std::process::Command;

use gl_generator::{Api, Fallbacks, GlobalGenerator, Profile, Registry};

fn main() {
    println!("cargo:rustc-env=GIT_HASH={}", commit_hash());

    let dest = env::var("OUT_DIR").unwrap();
    let mut file = File::create(&Path::new(&dest).join("gl_bindings.rs")).unwrap();

    let extensions = ["GL_ARB_blend_func_extended", "GL_ARB_clear_texture", "GL_ARB_copy_image"];
    Registry::new(Api::Gl, (3, 3), Profile::Core, Fallbacks::All, extensions)
        .write_bindings(GlobalGenerator, &mut file)
        .unwrap();

    #[cfg(windows)]
    embed_resource::compile("../extra/windows/windows.rc");
}

fn commit_hash() -> String {
    Command::new("git")
        .args(&["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_default()
}
