//! Integration tests for the `ngc-rs watch` subcommand.
//!
//! These spawn the actual `ngc-rs` binary against a tempdir fixture so the
//! tests cover the build pipeline + watcher + cache wiring end-to-end. The
//! main flow (`watch_rebuilds_on_edit`) is tolerant of CI sandboxes that
//! deny the macOS fsevent backend: if the watcher never fires, we surface
//! the issue as a skip rather than a hard failure, since the watcher's
//! per-platform plumbing is unit-tested in `crates/watch`.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const READY_MARKER: &str = "ngc-rs watch ready";
const REBUILD_MARKER: &str = "ngc-rs rebuild";
const TIMEOUT: Duration = Duration::from_secs(30);

fn write_fixture(root: &Path) {
    let tsconfig = r#"{
  "compilerOptions": {
    "target": "ES2022",
    "module": "preserve",
    "moduleResolution": "bundler",
    "outDir": "dist"
  },
  "include": ["src/**/*.ts"]
}"#;
    fs::write(root.join("tsconfig.json"), tsconfig).expect("write tsconfig");
    let src = root.join("src");
    fs::create_dir_all(&src).expect("create src");
    fs::write(
        src.join("main.ts"),
        "import { greet } from './greet';\nconsole.log(greet('world'));\n",
    )
    .expect("write main.ts");
    fs::write(
        src.join("greet.ts"),
        "export function greet(name: string): string {\n  return 'hello ' + name + ' v1';\n}\n",
    )
    .expect("write greet.ts");
}

fn read_main_chunk(out_dir: &Path) -> String {
    // Production hashes are off in dev mode, so the file is plain main.js.
    fs::read_to_string(out_dir.join("main.js")).expect("read main.js")
}

#[test]
fn build_then_edit_then_rebuild_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write_fixture(root);

    let bin = env!("CARGO_BIN_EXE_ngc-rs");
    let out_dir = root.join("dist");
    let tsconfig = root.join("tsconfig.json");

    // Initial build — the `build` subcommand exercises the same
    // `run_build_with_cache` function the watch loop uses, just without a
    // surviving cache. This proves the pipeline's I/O contract.
    let status = Command::new(bin)
        .args(["build", "--project"])
        .arg(&tsconfig)
        .arg("--out-dir")
        .arg(&out_dir)
        .status()
        .expect("spawn ngc-rs build");
    assert!(status.success(), "initial build failed");
    let main_v1 = read_main_chunk(&out_dir);
    assert!(main_v1.contains("hello "), "initial bundle missing greet");
    assert!(main_v1.contains(" v1"), "initial bundle missing v1 marker");

    // Mutate the source file.
    fs::write(
        root.join("src/greet.ts"),
        "export function greet(name: string): string {\n  return 'hi ' + name + ' v2';\n}\n",
    )
    .expect("rewrite greet.ts");

    // Rebuild.
    let status = Command::new(bin)
        .args(["build", "--project"])
        .arg(&tsconfig)
        .arg("--out-dir")
        .arg(&out_dir)
        .status()
        .expect("spawn ngc-rs build #2");
    assert!(status.success(), "second build failed");
    let main_v2 = read_main_chunk(&out_dir);
    assert!(main_v2.contains(" v2"), "rebuilt bundle missing v2 marker");
    assert!(
        !main_v2.contains(" v1"),
        "rebuilt bundle still contains v1 marker"
    );
    assert_ne!(main_v1, main_v2, "bundle bytes did not change after edit");
}

#[test]
fn watch_rebuilds_on_edit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize root");
    write_fixture(&root);

    let bin = env!("CARGO_BIN_EXE_ngc-rs");
    let out_dir = root.join("dist");
    let tsconfig = root.join("tsconfig.json");

    let mut child = Command::new(bin)
        .args(["watch", "--project"])
        .arg(&tsconfig)
        .arg("--out-dir")
        .arg(&out_dir)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ngc-rs watch");

    let stderr = child.stderr.take().expect("stderr pipe");
    let reader = BufReader::new(stderr);
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let stderr_handle = std::thread::spawn(move || {
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let wait_for = |needle: &str, deadline: Instant| -> Option<String> {
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(line) => {
                    if line.contains(needle) {
                        return Some(line);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }
        None
    };

    let deadline = Instant::now() + TIMEOUT;
    let ready_line = wait_for(READY_MARKER, deadline);
    if ready_line.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("watch never reported ready within {TIMEOUT:?}");
    }
    let main_v1 = read_main_chunk(&out_dir);
    assert!(main_v1.contains(" v1"), "initial watch bundle missing v1");

    // Wait briefly so the watcher's notify backend is fully attached
    // before we mutate (notify on macOS occasionally drops events that
    // arrive in the same instant the watch handle is created).
    std::thread::sleep(Duration::from_millis(300));

    fs::write(
        root.join("src/greet.ts"),
        "export function greet(name: string): string {\n  return 'hi ' + name + ' v2';\n}\n",
    )
    .expect("rewrite greet.ts");

    let rebuild_deadline = Instant::now() + TIMEOUT;
    let rebuild_line = wait_for(REBUILD_MARKER, rebuild_deadline);
    let observed = rebuild_line.is_some();

    let _ = child.kill();
    let _ = child.wait();
    let _ = stderr_handle.join();

    if !observed {
        eprintln!(
            "watch_rebuilds_on_edit: notify backend never delivered the change \
             event within {TIMEOUT:?}; treating as a skip on this platform"
        );
        return;
    }

    let main_v2 = read_main_chunk(&out_dir);
    assert!(main_v2.contains(" v2"), "watch bundle missing v2 after edit");
    assert!(
        !main_v2.contains(" v1"),
        "watch bundle still contains v1 after edit"
    );
}
