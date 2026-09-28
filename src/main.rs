mod ci;
mod consensus;
mod data;
mod detect;
mod install;
mod net;
mod resolve;
mod sources;
mod version;

use anyhow::Result;
use clap::{Parser, Subcommand};
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
    /// Print the detected NPU/GPU and NPU userspace.
    Detect,
    /// Work out which artifacts to install, and check their hashes, without downloading.
    Resolve {
        /// Override the detected minimum OpenVINO, e.g. 2026.2.
        #[arg(long)]
        min_openvino: Option<String>,
        /// Allow an OpenVINO newer than the installed NPU driver is paired with.
        #[arg(long)]
        ignore_driver: bool,
        #[arg(long)]
        json: bool,
    },
    /// Resolve, download, verify, and install ONNX Runtime with the OpenVINO EP.
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    let data = data::Data::load(cli.data_dir.as_deref())?;
    match cli.command {
        Cmd::Detect => println!("{}", serde_json::to_string_pretty(&detect::machine())?),
        Cmd::Resolve {
            min_openvino,
            ignore_driver,
            json,
        } => {
            let plan = resolve::resolve(
                &detect::machine(),
                &data,
                min_openvino.as_deref(),
                ignore_driver,
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
                    println!("ceiling:  {c} (newest the installed NPU driver pairs with)");
                }
                println!(
                    "openvino: {} (onnxruntime-openvino {}, {:?})",
                    plan.openvino, plan.artifact.onnxruntime, plan.artifact.trust
                );
                println!("download: {}", plan.artifact.url);
                println!("sha256:   {}", plan.artifact.sha256);
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
        } => {
            let plan = resolve::resolve(
                &detect::machine(),
                &data,
                min_openvino.as_deref(),
                ignore_driver,
            )?;
            let opts = install::Options {
                allow_unverified,
                allow_downgrade,
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
            println!("{changes}");
        }
    }
    Ok(())
}
