use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=assets/app.ico");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let res_obj = out_dir.join("app_res.o");

    let status = Command::new("windres")
        .args(["app.rc", "-O", "coff", "-o"])
        .arg(&res_obj)
        .status()
        .expect("failed to invoke windres to embed app.ico");
    assert!(status.success(), "windres failed");

    println!(
        "cargo:rustc-link-arg-bin=agy-token-monitor={}",
        res_obj.display()
    );
}
