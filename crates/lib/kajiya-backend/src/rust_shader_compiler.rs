use crate::{file::LoadFile, normalized_path_from_vfs, shader_compiler::CompiledShader};
use anyhow::{Context, Result};
use nanoserde::DeJson;
use parking_lot::Mutex;
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use turbosloth::*;

const RUSTGPU_TOOLCHAIN: &str = "nightly-2023-07-08";
const REBUILD_RUST_SHADERS_ENV: &str = "KAJIYA_BUILD_RUST_SHADERS";

// Whether the user explicitly asked for a rebuild via `KAJIYA_BUILD_RUST_SHADERS`.
fn rebuild_rust_shaders_requested() -> bool {
    std::env::var_os(REBUILD_RUST_SHADERS_ENV).is_some()
}

// Whether any Rust shader source is newer than the checked-in compiled shaders.
// This makes the rebuild automatic for developers editing the shaders, and a no-op
// for everyone else.
fn rust_shader_sources_are_newer(src_dirs: &[PathBuf]) -> bool {
    let compiled_shaders_json = normalized_path_from_vfs("/rust-shaders-compiled/shaders.json");
    let compiled_mtime = match compiled_shaders_json.as_deref().ok().and_then(|p| std::fs::metadata(p).ok()) {
        Some(meta) => meta
            .modified()
            .ok()
            .unwrap_or(std::time::UNIX_EPOCH),
        // Missing shaders.json - there is nothing to fall back to, so rebuild.
        None => std::time::UNIX_EPOCH,
    };

    fn newest_mtime(dir: &Path) -> Option<std::time::SystemTime> {
        let mut newest: Option<std::time::SystemTime> = None;
        for entry in std::fs::read_dir(dir).ok()? {
            let path = entry.ok()?.path();
            let time = if path.is_dir() {
                newest_mtime(&path)?
            } else {
                path.metadata().ok()?.modified().ok()?
            };
            newest = Some(newest.map_or(time, |n| n.max(time)));
        }
        Some(newest?)
    }

    src_dirs
        .iter()
        .filter_map(|dir| newest_mtime(dir))
        .any(|mtime| mtime > compiled_mtime)
}

// Whether the Rust-GPU toolchain is available, so that a rebuild can actually succeed.
fn rust_gpu_toolchain_available() -> bool {
    let Ok(output) = Command::new("rustup").arg("toolchain").arg("list").output() else {
        return false;
    };

    output.status.success()
        && String::from_utf8_lossy(&output.stdout).contains(RUSTGPU_TOOLCHAIN)
}

#[derive(Clone, Hash)]
pub struct CompileRustShader {
    pub entry: String,
}

#[async_trait]
impl LazyWorker for CompileRustShader {
    type Output = Result<CompiledShader>;

    async fn run(self, ctx: RunContext) -> Self::Output {
        CompileRustShaderCrate.into_lazy().eval(&ctx).await?;

        let compile_result = LoadFile::new("/rust-shaders-compiled/shaders.json")?
            .into_lazy()
            .eval(&ctx)
            .await?;

        let compile_result =
            RustShaderCompileResult::deserialize_json(std::str::from_utf8(&compile_result)?)?;

        let shader_file = compile_result
            .entry_to_shader_module
            .into_iter()
            .find_map(|(entry, module)| {
                if entry == self.entry {
                    Some(module)
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                anyhow::anyhow!("No Rust-GPU module found for entry point {}", self.entry)
            })?;

        let spirv_blob = LoadFile::new(format!("/rust-shaders-compiled/{}", shader_file))?
            .into_lazy()
            .eval(&ctx)
            .await?;

        Ok(CompiledShader {
            name: "rust-gpu".to_owned(),
            spirv: (*spirv_blob).clone(),
        })
    }
}

#[derive(DeJson)]
struct RustShaderCompileResult {
    // entry name -> shader path
    entry_to_shader_module: Vec<(String, String)>,
}

#[derive(Clone, Hash)]
pub struct CompileRustShaderCrate;

#[async_trait]
impl LazyWorker for CompileRustShaderCrate {
    type Output = Result<()>;

    async fn run(self, ctx: RunContext) -> Self::Output {
        let src_dirs = || -> Result<_> {
            Ok([
                normalized_path_from_vfs("/kajiya/crates/lib/rust-shaders/src")?,
                normalized_path_from_vfs("/kajiya/crates/lib/rust-shaders-shared/src")?,
            ])
        };

        let src_dirs = match src_dirs() {
            Ok(src_dirs) => src_dirs,
            Err(_) => {
                log::info!("Rust shader sources not found. Using the precompiled versions.");
                return Ok(());
            }
        };

        // Unlike regular shader building, this one runs in a separate thread in the background.
        //
        // The built shaders are cached and checked-in, meaning that
        // 1. Devs/users don't need to have Rust-GPU
        // 2. The previously built shaders can be used at startup without stalling the app
        //
        // To accomplish such behavior, this function lies to `turbosloth`, immediately claiming success.
        // The caller then goes straight for the cached shaders. Meanwhile, a thread is spawned,
        // and builds the shaders. When that's done, `CompileRustShader` which depends on this
        // will notice a change in the compiler output files, and trigger the shader reload.

        // The rebuild only happens when it's actually wanted:
        // * the user explicitly set `KAJIYA_BUILD_RUST_SHADERS`, or
        // * the shader sources were modified after the checked-in compiled shaders.
        let should_rebuild =
            rebuild_rust_shaders_requested() || rust_shader_sources_are_newer(&src_dirs);

        if !should_rebuild {
            log::info!("Rust-GPU shaders are up to date. Using the precompiled versions.");
        } else if !rust_gpu_toolchain_available() {
            log::info!(
                "Rust-GPU toolchain ({}) is not installed. Using the precompiled shaders instead. \
                 Install it with `rustup toolchain install {} --component rust-src,rustc-dev,llvm-tools-preview` \
                 to rebuild them.",
                RUSTGPU_TOOLCHAIN,
                RUSTGPU_TOOLCHAIN
            );
        } else {
            // In case `CompileRustShaderCrate` gets cancelled by `turbosloth`, we will want to cancel
            // the builder thread as well. We'll send a message through a channel to do this.
            lazy_static::lazy_static! {
                static ref BUILD_TASK_CANCEL: Mutex<Option<std::sync::mpsc::Sender<()>>> = Mutex::new(None);
            }
            let mut prev_build_task_cancel = BUILD_TASK_CANCEL.lock();
            let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();

            // Cancel the previous build task, and register the current one
            if let Some(cancel) = prev_build_task_cancel.replace(cancel_tx) {
                let _ = cancel.send(());
            }

            // Spawn the worker thread.
            std::thread::spawn(move || -> anyhow::Result<()> {
                log::info!("Building Rust-GPU shaders in the background...");

                if let Err(err) = compile_rust_shader_crate_thread(cancel_rx) {
                    log::warn!("Failed to build Rust-GPU shaders. Falling back to the previously compiled ones. Error: {:?}", err);
                }

                Ok(())
            });
        }

        // And finally register a watcher on the source directory for Rust shaders.
        for src_dir in src_dirs {
            let invalidation_trigger = ctx.get_invalidation_trigger();
            crate::file::FILE_WATCHER
                .lock()
                .watch(src_dir.clone(), move |event| {
                    if matches!(event, hotwatch::Event::Write(_)) {
                        invalidation_trigger();
                    }
                })
                .with_context(|| {
                    format!("CompileRustShaderCrate: trying to watch {:?}", src_dir)
                })?;
        }

        Ok(())
    }
}

// Runs cargo in a sub-process to execute the rust shader builder.
fn compile_rust_shader_crate_thread(
    cancel_rx: std::sync::mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    let builder_dir = normalized_path_from_vfs("/kajiya/crates/bin/rust-shader-builder")?;

    let mut child = Command::new("cargo")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("CARGO_PROFILE_RELEASE_DEBUG")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        // Due to issues such as https://github.com/rust-lang/rust/issues/78210
        // nuke rustflags since they are (generally) used in cross compilation
        // scenarios, but we only build the shader builder for the HOST. If this
        // ends up being a problem we might need to more surgically edit RUSTFLAGS
        // instead
        .env_remove("RUSTFLAGS")
        .env_remove("OUT_DIR")
        .arg("run")
        .arg("--release")
        .arg("--")
        .current_dir(builder_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to execute Rust-GPU builder")?;

    // Wait for the builder to finish, and allow cancellation via the supplied `cancel_rx`
    let output = loop {
        let should_bail = !matches!(
            cancel_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );

        if should_bail {
            log::info!("Rust-GPU shader builder thread received a stop command.");
            return child.kill().context("killing the Rust-GPU shader builder");
        }

        match child.try_wait() {
            // The process is done. Get the output.
            Ok(Some(_)) => break child.wait_with_output()?,
            // Still running...
            Ok(None) => (),
            // Something went wrong.
            Err(err) => return Err(err).context("error while executing Rust-GPU builder"),
        }

        // Don't waste CPU cycles
        std::thread::sleep(std::time::Duration::from_millis(10));
    };

    if !output.status.success() {
        let err = String::from_utf8(output.stderr)?;
        let out = String::from_utf8(output.stdout)?;
        anyhow::bail!("Shader builder failed:\n {}\n{}", out, err)
    } else {
        log::info!("Rust-GPU cargo process finished.");
    }

    Ok(())
}
