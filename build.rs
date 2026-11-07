use std::{env, fs, path::PathBuf, process::Command};

fn run(cmd: &mut Command) {
    let status = cmd
        .status()
        .unwrap_or_else(|_| panic!("failed to run {cmd:?}"));
    assert!(status.success(), "command failed: {cmd:?}");
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let host = env::var("HOST").unwrap_or_default();
    let target = env::var("TARGET").unwrap();

    // Build vibe-usernet.
    // Limit this to Apple hosts; on others, create a stub file so we can still run `cargo check` (e.g., from within Vibe =D)
    if host.contains("apple-darwin") && target == "aarch64-apple-darwin" {
        let usernet_dir = manifest_dir.join("helpers/vibe-usernet");
        let usernet_helper_path = out_dir.join("vibe-usernet");

        let mut go = Command::new("go");
        go.current_dir(&usernet_dir)
            .env("CGO_ENABLED", "1")
            .env("GOOS", "darwin")
            .env("GOARCH", "arm64")
            .args(["build", "-trimpath", "-ldflags", "-w -s", "-o"])
            .arg(&usernet_helper_path)
            .arg(".");
        run(&mut go);

        println!(
            "cargo:rustc-env=BUNDLED_USERNET_HELPER_PATH={}",
            usernet_helper_path.display()
        );

        for entry in fs::read_dir(&usernet_dir)
            .expect("read vibe-usernet dir")
            .flatten()
        {
            let path = entry.path();
            if let Some("go" | "mod" | "sum") = path.extension().and_then(|e| e.to_str()) {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
        println!("cargo:rerun-if-env-changed=TARGET");
    } else {
        let stub_path = out_dir.join("stub-vibe-usernet");
        fs::write(&stub_path, []).expect("write stub vibe-usernet");
        println!(
            "cargo:rustc-env=BUNDLED_USERNET_HELPER_PATH={}",
            stub_path.display()
        );
    }

    // Expose GIT_SHA and BUILD_DATE vars so Vibe can embed them in its version info
    {
        let sha = Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| "unknown".into());
        let build_date = Command::new("date")
            .args(["-u", "+%F"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| "unknown".into());

        println!("cargo:rustc-env=GIT_SHA={sha}");
        println!("cargo:rustc-env=BUILD_DATE={build_date}");
        println!("cargo:rerun-if-changed=.git/HEAD");
    }
}
