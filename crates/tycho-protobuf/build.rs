use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=PROTO_BUILD");
    if env::var("PROTO_BUILD").is_ok() {
        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        let status = Command::new("buf")
            .arg("generate")
            .current_dir(&manifest_dir)
            .status()
            .expect("failed to run `buf generate` — is buf installed?");
        assert!(status.success(), "buf generate failed");
    }
}
