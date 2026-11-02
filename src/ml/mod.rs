//! Runs the Python ML stages in `ml/`, which `ml/README.md` documents.

pub mod child;
pub mod cli;
pub mod protocol;
pub mod scratch;

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use tokio::process::Command;

use crate::quote::quote;

const ML_PROJECT_ENV: &str = "VT_ML_PROJECT";
const GPU_ENV: &str = "VT_GPU";

/// `uv run vt-ml <stage>` in vt's `ml/` project. Arguments for the stage follow.
///
/// With a model extra, uv also installs that model's dependencies and the GPU's torch build on
/// first use. A model always runs on the GPU, so this fails if there's none.
pub fn command(stage: &str, model: Option<&str>) -> Result<Command> {
    let project = std::env::var_os(ML_PROJECT_ENV).map_or_else(
        || PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/ml")),
        PathBuf::from,
    );
    if !project.join("pyproject.toml").is_file() {
        bail!(
            "no Python project at {}; set {ML_PROJECT_ENV} to vt's ml/ directory",
            quote(&project)
        );
    }
    let mut command = Command::new("uv");
    command.args(["run", "--quiet", "--project"]).arg(project);
    if let Some(model) = model {
        command.args(["--extra", model, "--extra", Gpu::detect()?.extra()]);
    }
    command.args(["vt-ml", stage]);
    Ok(command)
}

/// The GPU platform, which picks the torch build the stage installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gpu {
    Rocm,
    Cuda,
}

impl Gpu {
    /// `VT_GPU` if set, otherwise whichever GPU driver is loaded.
    fn detect() -> Result<Gpu> {
        Gpu::choose(
            std::env::var(GPU_ENV).ok().as_deref(),
            Path::new("/dev/kfd").exists(),
            Path::new("/proc/driver/nvidia/version").exists(),
        )
    }

    fn choose(requested: Option<&str>, amd: bool, nvidia: bool) -> Result<Gpu> {
        match (requested, amd, nvidia) {
            (Some("rocm"), ..) | (None, true, false) => Ok(Gpu::Rocm),
            (Some("cuda"), ..) | (None, false, true) => Ok(Gpu::Cuda),
            (Some(other), ..) => bail!("{GPU_ENV} is {other:?}; set it to rocm or cuda"),
            (None, true, true) => {
                bail!("found AMD and NVIDIA GPU drivers; set {GPU_ENV} to rocm or cuda")
            }
            (None, false, false) => {
                bail!("the model needs a GPU, but found no AMD (ROCm) or NVIDIA driver")
            }
        }
    }

    /// The `ml/pyproject.toml` extra holding this platform's torch build.
    fn extra(self) -> &'static str {
        match self {
            Gpu::Rocm => "rocm",
            Gpu::Cuda => "cuda",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_gpu_whose_driver_is_loaded() {
        assert_eq!(Gpu::choose(None, true, false).unwrap(), Gpu::Rocm);
        assert_eq!(Gpu::choose(None, false, true).unwrap(), Gpu::Cuda);
    }

    #[test]
    fn lets_vt_gpu_override_detection() {
        assert_eq!(Gpu::choose(Some("cuda"), true, true).unwrap(), Gpu::Cuda);
        assert_eq!(Gpu::choose(Some("rocm"), false, false).unwrap(), Gpu::Rocm);
    }

    #[test]
    fn asks_which_gpu_when_both_drivers_are_loaded() {
        assert_eq!(
            Gpu::choose(None, true, true).unwrap_err().to_string(),
            "found AMD and NVIDIA GPU drivers; set VT_GPU to rocm or cuda"
        );
    }

    #[test]
    fn fails_without_a_gpu() {
        assert_eq!(
            Gpu::choose(None, false, false).unwrap_err().to_string(),
            "the model needs a GPU, but found no AMD (ROCm) or NVIDIA driver"
        );
    }

    #[test]
    fn rejects_an_unknown_vt_gpu() {
        assert_eq!(
            Gpu::choose(Some("metal"), true, false)
                .unwrap_err()
                .to_string(),
            r#"VT_GPU is "metal"; set it to rocm or cuda"#
        );
    }

    #[test]
    fn runs_a_stage_without_a_model_in_the_ml_project() {
        let command = command("transcribe", None).unwrap();

        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(
            args,
            [
                "run",
                "--quiet",
                "--project",
                concat!(env!("CARGO_MANIFEST_DIR"), "/ml"),
                "vt-ml",
                "transcribe"
            ]
        );
    }
}
