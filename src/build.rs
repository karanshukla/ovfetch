//! Compiling ONNX Runtime against a ledger-pinned OpenVINO wheel, for `install --build`.
//!
//! The output cannot be pinned (it differs between toolchains), so every input
//! is: the source must check out at the recipe's commit with nothing modified,
//! and OpenVINO is a wheel downloaded and hash-checked like any other artifact.
//! ONNX Runtime's own CMake dependencies are fetched by CMake, outside ovfetch's
//! host allowlist, and checked against the hashes in the pinned source's
//! `cmake/deps.txt`.

use crate::install::{self, Options};
use crate::net;
use crate::resolve::{BuildPlan, Plan};
use anyhow::{Context, Result, bail};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const TOOLS: &[&str] = &["git", "cmake", "ninja", "python3", "c++"];

/// OpenVINO's runtime, plugins and the ONNX frontend the EP links: the same
/// set Intel's onnxruntime-openvino wheel ships.
pub(crate) fn is_openvino_runtime(file: &str) -> bool {
    let stem = file.split(".so").next().unwrap_or(file);
    matches!(
        stem,
        "libopenvino"
            | "libopenvino_c"
            | "libopenvino_onnx_frontend"
            | "libopenvino_auto_plugin"
            | "libopenvino_hetero_plugin"
            | "libopenvino_intel_cpu_plugin"
            | "libopenvino_intel_gpu_plugin"
            | "libopenvino_intel_npu_plugin"
            | "libtbb"
            | "libtbbmalloc"
    )
}

fn is_onnxruntime(file: &str) -> bool {
    file.starts_with("libonnxruntime.so.")
        || file == "libonnxruntime_providers_openvino.so"
        || file == "libonnxruntime_providers_shared.so"
}

fn first_line(cmd: &str) -> Option<String> {
    let out = Command::new(cmd).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(str::to_owned)
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("running {cmd:?}"))?;
    if !status.success() {
        bail!("{cmd:?} failed: {status}");
    }
    Ok(())
}

/// Effective uid, from procfs since std has no geteuid.
fn euid() -> Result<u32> {
    let status = fs::read_to_string("/proc/self/status")?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|u| u.parse().ok())
        .context("no effective uid in /proc/self/status")
}

/// Whatever is in the build tree gets executed and installed, so it must be
/// writable by nobody but the user running ovfetch. DirBuilder's mode only
/// applies to directories it creates, so an existing one is checked here.
fn ensure_private(dir: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
    let me = euid()?;
    if !meta.is_dir() || meta.uid() != me || meta.mode() & 0o022 != 0 {
        bail!(
            "{} must be a directory owned by uid {me} and writable by no one else \
             (it is uid {}, mode {:o}). Its contents are compiled and installed, so ovfetch will not build there; \
             pass another --build-dir or delete it.",
            dir.display(),
            meta.uid(),
            meta.mode() & 0o7777
        );
    }
    Ok(())
}

fn git(src: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        // Only the pinned tree's content counts, never config that could run a command.
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .arg("-C")
        .arg(src)
        .args(args)
        .output()?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Clones the recipe's tag once, then refuses to build anything but its
/// pinned commit with a clean tree, submodules included.
fn source(recipe: &BuildPlan, src: &Path) -> Result<()> {
    if !src.exists() {
        run(Command::new("git")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "clone",
                "--depth",
                "1",
                "--branch",
                &recipe.git_tag,
                "--recurse-submodules",
                "--shallow-submodules",
            ])
            .arg(format!("https://github.com/{}", recipe.git_repo))
            .arg(src))?;
    }
    let head = git(src, &["rev-parse", "HEAD"])?;
    if head != recipe.git_commit {
        bail!(
            "{} is at {head}, but the recipe pins {}. Nothing was built.",
            src.display(),
            recipe.git_commit
        );
    }
    let dirty = git(
        src,
        &[
            "status",
            "--porcelain",
            "--ignored",
            "--ignore-submodules=none",
        ],
    )?;
    if !dirty.is_empty() {
        bail!(
            "{} has local changes, so it is not the pinned source:\n{dirty}\nDelete it to start over.",
            src.display()
        );
    }
    Ok(())
}

/// Unpacks the OpenVINO wheel's libraries, headers and CMake config. The wheel
/// is kept to save a download, but re-hashed and re-extracted on every run:
/// nothing extracted earlier is trusted.
fn openvino(
    recipe: &BuildPlan,
    root: &Path,
    fetch: &dyn Fn(&str, &str, &str, &Path) -> Result<()>,
) -> Result<PathBuf> {
    let input = &recipe.openvino_wheel;
    let whl = root.join("openvino.whl");
    let cached = fs::read(&whl).is_ok_and(|b| net::sha256_hex(&b) == input.sha256);
    if !cached {
        fetch(&input.id, &input.url, &input.sha256, &whl)?;
    }
    let dir = root.join("openvino");
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    install::unzip(
        &whl,
        root,
        |n| {
            ["openvino/libs/", "openvino/include/", "openvino/cmake/"]
                .iter()
                .any(|p| n.starts_with(p))
        },
        false,
    )?;
    Ok(dir)
}

/// Builds the recipe under `opts.build_dir` and copies the runtime into `lib`.
/// Returns what the lock records about the toolchain that did it.
pub fn stage(
    plan: &Plan,
    recipe: &BuildPlan,
    lib: &Path,
    opts: &Options,
    fetch: &dyn Fn(&str, &str, &str, &Path) -> Result<()>,
) -> Result<serde_json::Value> {
    let missing: Vec<&str> = TOOLS
        .iter()
        .copied()
        .filter(|t| first_line(t).is_none())
        .collect();
    if !missing.is_empty() {
        bail!(
            "--build needs {} on PATH (Fedora: dnf install git cmake ninja-build python3 gcc-c++)",
            missing.join(", ")
        );
    }

    let root = opts.build_dir.join(format!(
        "onnxruntime-{}-openvino-{}-{}",
        plan.artifact.onnxruntime.as_deref().unwrap_or_default(),
        plan.openvino,
        &plan.artifact.sha256[..12]
    ));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)
        .with_context(|| format!("creating {}", root.display()))?;
    ensure_private(&opts.build_dir)?;
    ensure_private(&root)?;
    eprintln!("building in {}", root.display());

    let ov = openvino(recipe, &root, fetch)?;
    let src = root.join("onnxruntime");
    source(recipe, &src)?;

    // Objects from an earlier run cannot be checked, so every build starts clean.
    let out = root.join("out");
    if out.exists() {
        fs::remove_dir_all(&out)?;
    }
    let mut defines = vec![format!("OpenVINO_DIR={}", ov.join("cmake").display())];
    defines.extend(recipe.cmake_defines.iter().cloned());
    // -B: bytecode written into the tree would fail the next run's clean check.
    run(Command::new("python3")
        .args(["-B", "-E", "-s"])
        .arg(src.join("tools/ci_build/build.py"))
        .arg("--build_dir")
        .arg(&out)
        .args(&recipe.build_args)
        .args(["--parallel", &opts.jobs.to_string()])
        .arg("--cmake_extra_defines")
        .args(&defines)
        .current_dir(&src)
        // Only the recipe decides which OpenVINO is found.
        .env_remove("INTEL_OPENVINO_DIR")
        .env_remove("OpenVINO_DIR")
        .env_remove("CMAKE_PREFIX_PATH")
        .env_remove("LD_LIBRARY_PATH"))?;

    let release = out.join("Release");
    let mut copied = 0;
    for (dir, keep) in [
        (release.as_path(), is_onnxruntime as fn(&str) -> bool),
        (&ov.join("libs"), is_openvino_runtime),
    ] {
        for e in fs::read_dir(dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .flatten()
        {
            let name = e.file_name().to_string_lossy().into_owned();
            if keep(&name) && !e.path().is_symlink() {
                fs::copy(e.path(), lib.join(&name))?;
                copied += 1;
            }
        }
    }
    if copied == 0 {
        bail!("the build produced no libraries in {}", release.display());
    }

    Ok(serde_json::json!({
        "compiler": first_line("c++"),
        "cmake": first_line("cmake"),
        "build_dir": root,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_a_build_dir_others_can_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = install::TempDir::new().unwrap();
        ensure_private(&dir.0).unwrap();
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(ensure_private(&dir.0).is_err());
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(ensure_private(Path::new("/")).is_err() || euid().unwrap() == 0);
    }

    #[test]
    fn copies_the_runtime_the_prebuilt_wheel_ships_and_nothing_else() {
        for keep in [
            "libopenvino.so.2641",
            "libopenvino_intel_npu_plugin.so",
            "libopenvino_onnx_frontend.so.2641",
            "libtbb.so.12",
            "libtbbmalloc.so.2",
        ] {
            assert!(is_openvino_runtime(keep), "{keep}");
        }
        for skip in [
            "libopenvino_pytorch_frontend.so.2641",
            "libopenvino_tensorflow_frontend.so.2641",
            "libtbbmalloc_proxy.so.2",
            "libhwloc.so.15",
            "tbb.pc",
        ] {
            assert!(!is_openvino_runtime(skip), "{skip}");
        }
        assert!(is_onnxruntime("libonnxruntime.so.1.24.1"));
        assert!(is_onnxruntime("libonnxruntime_providers_openvino.so"));
        assert!(!is_onnxruntime("libonnxruntime.so"));
        assert!(!is_onnxruntime("libonnxruntime_test_utils.a"));
    }
}
