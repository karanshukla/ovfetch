use anyhow::Result;
use clap::{Parser, Subcommand};
use ovfetch::resolve::Want;
use ovfetch::{ci, data, detect, install, resolve, sources, status};
use std::path::PathBuf;

/// Resolve, download, and verify the OpenVINO build this machine's Intel NPU needs.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Read data/*.toml from this directory instead of the copy compiled in.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the detected NPU/GPU, NPU userspace, and what the data says about them. Offline.
    Detect,
    /// Work out which artifacts to install, and check their hashes, without downloading.
    Resolve {
        /// Override the detected minimum OpenVINO, e.g. 2026.2.
        #[arg(long)]
        min_openvino: Option<String>,
        /// Allow an OpenVINO newer than the installed NPU driver is paired with.
        #[arg(long)]
        ignore_driver: bool,
        /// Consider compiling ONNX Runtime from a recipe in data/builds.toml.
        #[arg(long)]
        build: bool,
        /// OpenVINO alone, for programs that call it without ONNX Runtime.
        #[arg(long, conflicts_with = "build")]
        openvino_only: bool,
        #[arg(long)]
        json: bool,
    },
    /// Resolve, download, verify, and install ONNX Runtime with the OpenVINO EP
    /// (or, with --openvino-only, OpenVINO by itself).
    Install {
        #[arg(long)]
        prefix: PathBuf,
        #[arg(long)]
        min_openvino: Option<String>,
        /// Allow an OpenVINO newer than the installed NPU driver is paired with.
        #[arg(long)]
        ignore_driver: bool,
        /// Accept artifacts every source agrees on but the ledger has never recorded.
        #[arg(long)]
        allow_unverified: bool,
        /// Replace a newer OpenVINO that ovfetch previously installed in --prefix.
        #[arg(long)]
        allow_downgrade: bool,
        /// Reinstall even when --prefix already holds the resolved build.
        #[arg(long)]
        force: bool,
        /// Compile ONNX Runtime from a recipe in data/builds.toml when it
        /// offers a newer OpenVINO than any prebuilt that fits.
        #[arg(long)]
        build: bool,
        /// Install Intel's openvino wheel alone, without ONNX Runtime: the
        /// newest OpenVINO that fits, and never a compile.
        #[arg(long, conflicts_with = "build")]
        openvino_only: bool,
        /// Where --build keeps sources and objects [default: ~/.cache/ovfetch/build]
        #[arg(long)]
        build_dir: Option<PathBuf>,
        /// Parallel compile jobs for --build [default: half the CPUs]
        #[arg(long)]
        jobs: Option<usize>,
    },
    /// Re-hash an installed prefix against the SHA256SUMS written at install.
    Verify {
        #[arg(long)]
        prefix: PathBuf,
    },
    /// Maintenance jobs run by this repo's CI.
    #[command(subcommand)]
    Ci(CiCmd),
}

#[derive(Subcommand)]
enum CiCmd {
    /// Re-check every recorded hash against every source; fail on any change.
    Audit {
        /// How many recorded wheels to actually download from a random mirror.
        #[arg(long, default_value_t = 2)]
        sample: usize,
    },
    /// Record new releases, NPU drivers, and platforms into --data-dir.
    Discover,
}

fn default_build_dir() -> Result<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .ok_or_else(|| anyhow::anyhow!("no XDG_CACHE_HOME or HOME; pass --build-dir"))?;
    Ok(cache.join("ovfetch/build"))
}

fn want(build: bool, openvino_only: bool) -> Want {
    if openvino_only {
        Want::OpenvinoOnly
    } else {
        Want::OnnxRuntime { allow_build: build }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let data = data::Data::load(cli.data_dir.as_deref())?;
    match cli.command {
        Cmd::Detect => {
            let machine = detect::machine();
            println!(
                "{}",
                serde_json::to_string_pretty(&status::status(&machine, &data))?
            );
        }
        Cmd::Resolve {
            min_openvino,
            ignore_driver,
            build,
            openvino_only,
            json,
        } => {
            let plan = resolve::resolve(
                &detect::machine(),
                &data,
                min_openvino.as_deref(),
                ignore_driver,
                want(build, openvino_only),
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                println!(
                    "floor:    {} ({})",
                    plan.floor.openvino.as_deref().unwrap_or("none"),
                    plan.floor.reason
                );
                if let Some(c) = &plan.driver_ceiling {
                    println!("ceiling:  {c} (newest known to compile on the installed NPU driver)");
                }
                match &plan.artifact.onnxruntime {
                    Some(ort) => println!(
                        "openvino: {} (onnxruntime-openvino {ort}, {:?})",
                        plan.openvino, plan.artifact.trust
                    ),
                    None => println!(
                        "openvino: {} (alone, {:?})",
                        plan.openvino, plan.artifact.trust
                    ),
                }
                match &plan.artifact.build {
                    Some(b) => {
                        println!("build:    {} {} @ {}", b.git_repo, b.git_tag, b.git_commit);
                        println!(
                            "against:  {} ({:?})",
                            b.openvino_wheel.url, b.openvino_wheel.trust
                        );
                        println!("          sha256 {}", b.openvino_wheel.sha256);
                        println!("recipe:   {}", plan.artifact.sha256);
                    }
                    None => {
                        println!("download: {}", plan.artifact.url);
                        println!("sha256:   {}", plan.artifact.sha256);
                    }
                }
                for w in &plan.warnings {
                    println!("warning:  {w}");
                }
            }
        }
        Cmd::Install {
            prefix,
            min_openvino,
            ignore_driver,
            allow_unverified,
            allow_downgrade,
            force,
            build,
            openvino_only,
            build_dir,
            jobs,
        } => {
            let plan = resolve::resolve(
                &detect::machine(),
                &data,
                min_openvino.as_deref(),
                ignore_driver,
                want(build, openvino_only),
            )?;
            let opts = install::Options {
                allow_unverified,
                allow_downgrade,
                force,
                build_dir: build_dir.map_or_else(default_build_dir, Ok)?,
                jobs: jobs.unwrap_or_else(|| {
                    std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1))
                }),
            };
            for w in &plan.warnings {
                eprintln!("warning: {w}");
            }
            install::install(&plan, &prefix, &opts, |id| {
                let Some((project, filename)) =
                    id.strip_prefix("pypi/").and_then(|r| r.split_once('/'))
                else {
                    return Vec::new();
                };
                let file = sources::PypiFile {
                    project: project.into(),
                    version: String::new(),
                    filename: filename.into(),
                    url: String::new(),
                    size: 0,
                    sha256: String::new(),
                    uploaded: String::new(),
                };
                let urls = sources::pypi_download_urls(&file, &sources::mirror_pages(project));
                urls.into_iter().filter(|u| !u.is_empty()).collect()
            })?;
        }
        Cmd::Verify { prefix } => {
            install::verify(&prefix)?;
            println!("{} matches its SHA256SUMS", prefix.display());
        }
        Cmd::Ci(CiCmd::Audit { sample }) => println!("{}", ci::audit(&data, sample)?),
        Cmd::Ci(CiCmd::Discover) => {
            let mut data = data;
            anyhow::ensure!(
                data.dir.is_some(),
                "discover writes its findings, so it needs --data-dir"
            );
            let changes = ci::discover(&mut data)?;
            data.save()?;
            // The workflow opens a PR only when this prints something.
            if !changes.is_empty() {
                println!("{changes}");
            }
        }
    }
    Ok(())
}
