//! Python 运行时管理 — 虚拟环境创建、pip 包安装、Flask 服务启动。
//!
//! 核心职责：
//! 1. setup() — 从打包资源中提取 Python 代码，创建隔离的虚拟环境
//! 2. install_packages() — 用 uv（优先）或 pip 安装依赖，带实时进度回调
//! 3. deps_ready() — 快速检查已安装的包是否可导入
//! 4. start_flask() — 启动 Flask 子进程
//!
//! 设计决策：
//! - 优先使用 uv（比 pip 快 10-100 倍），找不到时回退到 pip
//! - venv 使用 uv venv --python ">=3.10"，uv 可自动下载 Python 解释器
//! - 资源提取时跳过 _up_ 目录（Tauri 打包的内部结构）和图标文件

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};

use crate::launcher::{CudaConfig, MirrorConfig};

/// Python 运行时的路径信息。
///
/// venv_python 指向虚拟环境中的 Python 解释器，
/// 所有 pip 安装和 Flask 启动都通过这个解释器执行。
pub struct PythonRuntime {
    /// 虚拟环境中 Python 可执行文件的路径
    pub venv_python: PathBuf,
    /// 代码副本目录（包含 app.py、pic_selecter/、static/ 等）
    pub app_dir: PathBuf,
}

impl PythonRuntime {
    /// 从打包资源中提取代码到数据目录（始终执行，不依赖 Python）。
    ///
    /// 返回 app_dir 路径。代码复制只在首次启动时执行，
    /// 后续启动跳过复制，由更新模块负责增量更新。
    pub fn extract_resources(
        resource_dir: &Path,
        app_data_dir: &Path,
    ) -> Result<PathBuf> {
        let app_dir = app_data_dir.join("app");

        let needs_extract = !app_dir.join("app.py").exists();
        if needs_extract {
            if app_dir.exists() {
                std::fs::remove_dir_all(&app_dir)?;
            }
            std::fs::create_dir_all(&app_dir)?;
            copy_resource_dir(resource_dir, &app_dir)?;
            log::info!("Code extracted to {:?}", app_dir);
        }

        Ok(app_dir)
    }

    /// 创建虚拟环境（如果不存在）并返回 venv Python 路径。
    ///
    /// `use_ensure_uv` 控制是否允许自动下载安装 uv：
    /// - false（快速模式）：只搜索已有 uv，找不到直接回退系统 Python
    /// - true（完整模式）：没 uv 时从网络下载安装（可能需要 30 秒+）
    ///
    /// 在 main.rs setup 阶段用 false（快速），在 init_setup 命令中可重试用 true。
    pub fn setup_venv(
        python_path: Option<&Path>,
        app_data_dir: &Path,
        use_ensure_uv: bool,
        on_progress: impl Fn(&str),
    ) -> Result<PathBuf> {
        let venv_dir = app_data_dir.join("venv");

        // 确定 venv 中 Python 的路径（Windows 和 Unix 路径不同）
        let venv_python = if cfg!(target_os = "windows") {
            venv_dir.join("Scripts").join("python.exe")
        } else {
            venv_dir.join("bin").join("python3")
        };

        // venv 不存在时创建
        if !venv_python.exists() {
            log::info!("Creating venv at {:?} (ensure_uv={})", venv_dir, use_ensure_uv);
            create_venv(python_path, &venv_dir, use_ensure_uv, on_progress)?;
        }

        Ok(venv_python)
    }

    /// 安装 pip 包到虚拟环境，通过回调实时报告进度。
    ///
    /// 安装策略：
    /// - 优先使用 uv pip install（Rust 实现，并行下载，比 pip 快很多）
    /// - uv 未安装时回退到标准 pip
    /// - 使用镜像源加速（国内默认清华大学 TUNA）
    /// - 安装完成后自动修复 OpenCV 包冲突
    ///
    /// 进度解析：同时兼容 uv 和 pip 的输出格式。
    /// uv 输出：Resolved X packages / Prepared X packages / Installed X packages
    /// pip 输出：Collecting X / Downloading X / Installing collected packages
    pub fn install_packages(
        &self,
        packages: &[String],
        mirror: &MirrorConfig,
        on_progress: impl Fn(&str),
    ) -> Result<()> {
        if packages.is_empty() {
            return Ok(());
        }

        let total = packages.len();
        let uv = ensure_uv(&on_progress);
        let using_uv = uv.is_some();

        if using_uv {
            on_progress(&format!(
                "使用 uv 安装 {} 个依赖包（速度更快）...",
                total
            ));
        } else {
            on_progress(&format!("开始安装 {} 个依赖包...", total));
        }

        // 构建安装命令
        // uv: uv pip install --python <venv_python> [mirror args] <packages>
        // pip: <venv_python> -m pip install [mirror args] <packages>
        let mut cmd = if let Some(ref uv_path) = uv {
            let mut c = Command::new(uv_path);
            c.args(["pip", "install", "--python"])
                .arg(&self.venv_python);
            c
        } else {
            let mut c = Command::new(&self.venv_python);
            c.args(["-m", "pip", "install", "--disable-pip-version-check", "--no-input"]);
            c
        };

        // 配置镜像源
        if mirror.use_mirror {
            cmd.arg("-i").arg(&mirror.pypi_index);
            cmd.arg("--extra-index-url").arg(&mirror.pypi_extra);
            if using_uv {
                on_progress(&format!("使用镜像源: {}", mirror.pypi_label));
            } else {
                on_progress(&format!("使用镜像源: {}（uv 未安装，用 pip 较慢）", mirror.pypi_label));
            }
        }

        // macOS x86_64: llvmlite >= 0.44 不再提供 Intel Mac wheel
        // numba>=0.59,<0.60 → llvmlite>=0.43,<0.45 → 0.43.x 有
        // cp312 + x86_64 wheel；numba<0.59 会拉到 llvmlite 0.41
        // 不支持 Python 3.12
        if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            cmd.arg("numba>=0.59,<0.60");
        }

        for pkg in packages {
            cmd.arg(pkg);
        }

        // 捕获 stderr — pip 和 uv 都把进度输出到 stderr
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().context("Failed to start installer")?;

        // 逐行读取 stderr 并解析进度信息，同时收集全部行用于错误诊断
        let stderr = child.stderr.take().unwrap();
        let reader = BufReader::new(stderr);

        let mut all_lines: Vec<String> = Vec::new();
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            let trimmed = line.trim();
            let is_empty = trimmed.is_empty();
            if !is_empty {
                all_lines.push(trimmed.to_string());
                on_progress(trimmed);
            }
            if !is_empty {
                log::debug!("install: {}", trimmed);
            }
        }

        // 排空 stdout（避免子进程阻塞）
        if let Some(stdout) = child.stdout.take() {
            for _ in BufReader::new(stdout).lines() {}
        }

        let status = child.wait().context("Install process failed")?;
        if !status.success() {
            // 取最后 20 行非空输出来定位问题
            let detail = all_lines.iter()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let detail = if detail.is_empty() { "(no output)".to_string() } else { detail };
            anyhow::bail!("Install returned non-zero exit: {}\n--- pip output ---\n{}", status, detail);
        }

        // 修复 OpenCV 冲突：如果同时安装了 opencv-python 和 opencv-contrib-python，
        // 需要卸载前者保留后者（contrib 包含了完整功能）
        on_progress("检查 OpenCV 兼容性...");
        self.fix_opencv_conflict(uv.as_deref())?;

        on_progress("依赖安装完成");
        Ok(())
    }

    /// 卸载指定的 pip 包（静默，忽略错误）。
    fn pip_uninstall(&self, packages: &[&str]) {
        if packages.is_empty() {
            return;
        }
        let _ = Command::new(&self.venv_python)
            .args(["-m", "pip", "uninstall", "-y"])
            .args(packages)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    /// 安装运行时后端（torch / onnxruntime），自动选择 CPU 或 CUDA 版本。
    ///
    /// 对应上游 scripts/launcher.py 的 ensure_runtime_backends()。
    ///
    /// 逻辑：
    /// - 没选 expert/tycoon → 跳过，返回 "none"
    /// - wants_cuda → 先卸载旧后端，从 PyTorch CUDA wheelhouse 装 torch，
    ///   再从 PyPI 装 onnxruntime-gpu，返回 "cuda:cu128"
    /// - 否则 → 装 CPU 版，返回 "cpu"
    pub fn install_runtime_backend(
        &self,
        modes: &[String],
        runtime: &str,
        mirror: &MirrorConfig,
        cuda: &CudaConfig,
        on_progress: impl Fn(&str),
    ) -> Result<String> {
        if !modes.iter().any(|m| m == "expert" || m == "tycoon") {
            return Ok("none".into());
        }

        let wants_cuda = crate::launcher::runtime_backend_label(modes, runtime, false) == "cuda";

        if wants_cuda {
            on_progress(&format!(
                "检测到 NVIDIA GPU，安装 CUDA 版 PyTorch（{}）+ ONNX Runtime GPU",
                cuda.flavor
            ));

            // 先卸载旧后端，避免包冲突
            self.pip_uninstall(&[
                "onnxruntime",
                "onnxruntime-gpu",
                "onnxruntime-directml",
                "torch",
                "torchvision",
                "torchaudio",
            ]);

            // 步骤 1：安装 torch + torchvision
            // 优先走镜像源（国内加速），官方 CUDA wheelhouse 作为回退
            let torch_cuda: Vec<String> = crate::launcher::TORCH_CUDA_PACKAGES
                .iter()
                .map(|s| s.to_string())
                .collect();
            let (torch_index, torch_extra): (String, Vec<String>) = if mirror.use_mirror {
                on_progress(&format!("使用镜像源安装 CUDA 版 PyTorch: {}", mirror.pypi_label));
                (mirror.pypi_index.clone(), vec![cuda.index_url.clone(), mirror.pypi_extra.clone()])
            } else {
                (cuda.index_url.clone(), vec![])
            };
            self.install_with_options(
                &torch_cuda,
                Some(&torch_index),
                &torch_extra,
                true,  // upgrade
                true,  // force_reinstall
                &on_progress,
            )?;

            // 步骤 2：安装 onnxruntime-gpu
            let onnx_gpu = ["onnxruntime-gpu[cuda,cudnn]>=1.16".to_string()];
            let pypi_url = if mirror.use_mirror {
                mirror.pypi_index.clone()
            } else {
                "https://pypi.org/simple/".into()
            };
            let pypi_fallback: Vec<String> = if mirror.use_mirror {
                vec![mirror.pypi_extra.clone()]
            } else {
                vec![]
            };
            self.install_with_options(
                &onnx_gpu,
                Some(&pypi_url),
                &pypi_fallback,
                true,
                true,
                &on_progress,
            )?;

            Ok(format!("cuda:{}", cuda.flavor))
        } else {
            on_progress("未检测到 NVIDIA GPU，安装 CPU 版 torch / onnxruntime");
            let cpu_pkgs: Vec<String> = crate::launcher::RUNTIME_CPU_PACKAGES
                .iter()
                .map(|s| s.to_string())
                .collect();
            let idx = if mirror.use_mirror { Some(mirror.pypi_index.as_str()) } else { None };
            let extras: Vec<String> = if mirror.use_mirror { vec![mirror.pypi_extra.clone()] } else { vec![] };
            self.install_with_options(
                &cpu_pkgs,
                idx,
                &extras,
                true,  // upgrade
                false, // force_reinstall
                &on_progress,
            )?;
            Ok("cpu".into())
        }
    }

    /// 带选项的 pip 安装（内部方法）。
    ///
    /// 与 install_packages 的区别：
    /// - 支持指定 index_url（用于 PyTorch CUDA wheelhouse）
    /// - 支持 --upgrade 和 --force-reinstall
    fn install_with_options(
        &self,
        packages: &[String],
        index_url: Option<&str>,
        extra_index_urls: &[String],
        upgrade: bool,
        force_reinstall: bool,
        on_progress: impl Fn(&str),
    ) -> Result<()> {
        if packages.is_empty() {
            return Ok(());
        }

        let uv = ensure_uv(&on_progress);
        let using_uv = uv.is_some();

        let mut cmd = if let Some(ref uv_path) = uv {
            let mut c = Command::new(uv_path);
            c.args(["pip", "install", "--python"])
                .arg(&self.venv_python);
            c
        } else {
            let mut c = Command::new(&self.venv_python);
            c.args(["-m", "pip", "install", "--disable-pip-version-check", "--no-input"]);
            c
        };

        if upgrade {
            cmd.arg("--upgrade");
        }
        if force_reinstall {
            cmd.arg("--force-reinstall");
        }

        if let Some(url) = index_url {
            if using_uv {
                cmd.arg("--index-url").arg(url);
            } else {
                cmd.arg("-i").arg(url);
            }
            for extra in extra_index_urls {
                if using_uv {
                    cmd.arg("--extra-index-url").arg(extra.as_str());
                } else {
                    cmd.arg("--extra-index-url").arg(extra.as_str());
                }
            }
        }

        for pkg in packages {
            cmd.arg(pkg);
        }

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().context("Failed to start installer")?;
        let stderr = child.stderr.take().unwrap();
        let reader = BufReader::new(stderr);

        let mut all_lines: Vec<String> = Vec::new();
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            let trimmed = line.trim();
            let is_empty = trimmed.is_empty();
            if !is_empty {
                all_lines.push(trimmed.to_string());
                on_progress(trimmed);
            }
            if !is_empty {
                log::debug!("install: {}", trimmed);
            }
        }

        if let Some(stdout) = child.stdout.take() {
            for _ in BufReader::new(stdout).lines() {}
        }

        let status = child.wait().context("Install process failed")?;
        if !status.success() {
            let detail = all_lines.iter()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let detail = if detail.is_empty() { "(no output)".to_string() } else { detail };
            anyhow::bail!("Install returned non-zero exit: {}\n--- pip output ---\n{}", status, detail);
        }

        Ok(())
    }

    /// 快速检查依赖是否已就绪。
    ///
    /// 通过 Python import 语句验证关键包能否成功导入。
    /// 这比检查文件系统更可靠，因为能同时验证包和其传递依赖的完整性。
    /// 不检查所有包，只检查最具代表性的几个来加速检测。
    pub fn deps_ready(&self, modes: &[String]) -> bool {
        // 核心包的代表性导入
        let mut imports = vec![
            "flask", "PIL", "numpy", "cv2", "imagehash", "rawpy", "piexif",
        ];
        // 专家模式额外的代表性导入
        if modes.iter().any(|m| m == "expert") {
            imports.extend(["torch", "transformers", "insightface", "pyiqa"]);
        }
        // 土豪模式额外的代表性导入
        if modes.iter().any(|m| m == "tycoon") {
            imports.push("openai");
        }
        let check = imports.join(", ");
        Command::new(&self.venv_python)
            .args(["-c", &format!("import {}", check)])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// 启动 Flask 服务子进程。
    ///
    /// 启动参数：
    /// - --port: 监听端口（默认 5057）
    /// - --no-browser: 不自动打开浏览器（Tauri 负责导航）
    ///
    /// 环境变量：
    /// - HF_ENDPOINT: HuggingFace 镜像（国内加速下载模型）
    ///
    /// 返回子进程句柄，调用方负责管理其生命周期。
    pub fn start_flask(
        &self,
        port: u16,
        runtime: &str,
        mirror: &MirrorConfig,
    ) -> Result<Child> {
        let app_py = self.app_dir.join("app.py");
        if !app_py.exists() {
            anyhow::bail!("app.py not found at {:?}", app_py);
        }

        let rt = if runtime.is_empty() { "auto" } else { runtime };

        log::info!("Starting Flask on port {} (runtime={})...", port, rt);
        let mut cmd = Command::new(&self.venv_python);
        cmd.arg(&app_py)
            .arg("--port")
            .arg(port.to_string())
            .arg("--runtime")
            .arg(rt)
            .arg("--no-browser")
            .current_dir(&self.app_dir);

        // 设置 HuggingFace 镜像环境变量，加速模型下载
        if mirror.use_mirror {
            cmd.env("HF_ENDPOINT", &mirror.hf_endpoint);
        }

        cmd.env("PIC_SELECTER_RUNTIME", rt);

        // Unix: 创建独立进程组，退出时可整组 kill，避免子进程变孤儿
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let child = cmd.spawn().context("Failed to start Flask process")?;
        Ok(child)
    }

    /// 修复 OpenCV 包冲突。
    ///
    /// 问题背景：
    /// opencv-python 和 opencv-contrib-python 都提供 cv2 模块，
    /// 如果同时安装会互相覆盖文件导致不可预测的行为。
    ///
    /// 解决方法：
    /// 检测冲突 → 卸载非 contrib 版本 → 强制重装 opencv-contrib-python。
    /// contrib 版本包含所有 opencv-python 的功能加上扩展模块。
    fn fix_opencv_conflict(&self, uv_path: Option<&Path>) -> Result<()> {
        // 检查是否存在冲突的 opencv 包
        let output = Command::new(&self.venv_python)
            .args([
                "-c",
                "import importlib.metadata as m; \
                 names={'opencv-python','opencv-python-headless'}; \
                 found=[n for n in names if any(d.metadata['Name'].lower()==n \
                 for d in m.distributions())]; \
                 print('|'.join(found))",
            ])
            .output()?;

        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let conflicts: Vec<&str> = stdout_str
            .trim()
            .split('|')
            .filter(|s| !s.is_empty())
            .collect();

        if conflicts.is_empty() {
            return Ok(());
        }

        log::info!("Fixing OpenCV conflict: {:?}", conflicts);

        // 卸载冲突的包（opencv-python 或 opencv-python-headless）
        if let Some(uv) = uv_path {
            let mut cmd = Command::new(uv);
            cmd.args(["pip", "uninstall", "-y", "--python"])
                .arg(&self.venv_python);
            for pkg in &conflicts {
                cmd.arg(pkg);
            }
            let _ = cmd.status();
        } else {
            for pkg in &conflicts {
                let _ = Command::new(&self.venv_python)
                    .args(["-m", "pip", "uninstall", "-y", pkg])
                    .status();
            }
        }

        // 重装 opencv-contrib-python（--force-reinstall --no-deps 跳过已满足的依赖，只重装本体）
        let status = if let Some(uv) = uv_path {
            Command::new(uv)
                .args(["pip", "install", "--python"])
                .arg(&self.venv_python)
                .args(["--force-reinstall", "--no-deps", "opencv-contrib-python>=4.9"])
                .status()?
        } else {
            Command::new(&self.venv_python)
                .args([
                    "-m", "pip", "install",
                    "--force-reinstall", "--no-deps",
                    "opencv-contrib-python>=4.9",
                ])
                .status()?
        };

        if !status.success() {
            anyhow::bail!("Failed to reinstall opencv-contrib-python");
        }
        Ok(())
    }
}

// ─── uv 发现 ───

/// 在系统中查找 uv 可执行文件。
///
/// uv 是一个用 Rust 编写的极速 Python 包管理器，
/// pip 的替代品，安装速度通常快 10-100 倍。
///
/// 查找顺序：
/// 1. PATH 中的 uv/uv.exe
/// 2. ~/.local/bin/uv（Linux/macOS pipx 默认安装路径）
/// 3. ~/.cargo/bin/uv（Rust cargo install 默认路径）
/// 4. Windows: %LOCALAPPDATA%\Programs\uv\uv.exe（官方安装器路径）
///
/// 返回 None 表示用户未安装 uv，后续操作回退到 pip。
fn find_uv() -> Option<PathBuf> {
    let name = if cfg!(target_os = "windows") {
        "uv.exe"
    } else {
        "uv"
    };

    // 先检查 PATH（which/where）
    if let Some(p) = which_command(name) {
        return Some(p);
    }

    // PATH 中找不到，搜索常见安装路径
    let home = home_dir()?;

    #[cfg(target_os = "windows")]
    {
        let paths = [
            home.join(".local").join("bin").join(name),
            home.join(".cargo").join("bin").join(name),
            // uv 官方安装器在 Windows 上的默认路径
            home.join("AppData").join("Local").join("Programs").join("uv").join(name),
        ];
        for p in &paths {
            if p.exists() {
                return Some(p.clone());
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let paths = [
            home.join(".local").join("bin").join(name),
            home.join(".cargo").join("bin").join(name),
        ];
        for p in &paths {
            if p.exists() {
                return Some(p.clone());
            }
        }
    }

    None
}

/// 确保 uv 可用 — 找不到时自动下载安装。
///
/// 与原始启动脚本行为一致：
/// - macOS/Linux: curl -LSf https://astral.sh/uv/install.sh | sh
/// - Windows: powershell irm https://astral.sh/uv/install.ps1 | iex
///
/// 安装后重新搜索，仍找不到则返回 None（网络问题等）。
fn ensure_uv(on_progress: impl Fn(&str)) -> Option<PathBuf> {
    if let Some(uv) = find_uv() {
        return Some(uv);
    }

    log::info!("uv not found, attempting auto-install...");
    on_progress("正在下载安装 uv（约需 30 秒）...");

    let installed = if cfg!(target_os = "windows") {
        match std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "irm https://astral.sh/uv/install.ps1 | iex",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(stderr) = child.stderr.take() {
                    for line in BufReader::new(stderr).lines().flatten() {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            on_progress(trimmed);
                        }
                    }
                }
                if let Some(stdout) = child.stdout.take() {
                    for _ in BufReader::new(stdout).lines() {}
                }
                child.wait().map(|s| s.success()).unwrap_or(false)
            }
            Err(_) => false,
        }
    } else {
        match std::process::Command::new("sh")
            .arg("-c")
            .arg("curl -LSf https://astral.sh/uv/install.sh | sh")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(stderr) = child.stderr.take() {
                    for line in BufReader::new(stderr).lines().flatten() {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            on_progress(trimmed);
                        }
                    }
                }
                child.wait().map(|s| s.success()).unwrap_or(false)
            }
            Err(_) => false,
        }
    };

    if installed {
        on_progress("uv 安装完成");
        log::info!("uv install script completed, re-scanning...");
        find_uv()
    } else {
        log::warn!("uv auto-install failed or curl not available");
        None
    }
}

/// 跨平台的命令查找（类似 Unix which / Windows where）。
fn which_command(name: &str) -> Option<PathBuf> {
    let (check_cmd, check_arg) = if cfg!(target_os = "windows") {
        ("where", name)
    } else {
        ("which", name)
    };

    Command::new(check_cmd)
        .arg(check_arg)
        .output()
        .ok()
        .and_then(|o| {
            let path_str = String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if path_str.is_empty() {
                return None;
            }
            let path = PathBuf::from(&path_str);
            if path.exists() {
                Some(path)
            } else {
                None
            }
        })
}

/// 跨平台获取用户主目录。
fn home_dir() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    } else {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

// ─── 虚拟环境创建 ───

/// 创建 Python 虚拟环境。
///
/// 策略：
/// 1. 优先使用 uv venv — 不需要系统 Python，uv 可自动下载合适版本
/// 2. uv 不可用或失败时，回退到系统 Python 的标准库 venv 模块
/// 3. 两者都不可用则返回错误
///
/// uv venv --python ">=3.10" 的含义：
/// - 如果系统已有 Python 3.10+，直接使用
/// - 如果没有，uv 自动从 python-build-standalone 下载预编译的 Python
fn create_venv(python_path: Option<&Path>, venv_dir: &Path, use_ensure_uv: bool, on_progress: impl Fn(&str)) -> Result<()> {
    let uv = if use_ensure_uv { ensure_uv(&on_progress) } else { find_uv() };
    if let Some(ref uv) = uv {
        on_progress("使用 uv 创建虚拟环境（可能需下载 Python）...");
        log::info!("Using uv to create venv (may auto-download Python)...");
        // uv venv --python ">=3.10,<3.14"：PyTorch 没有 cp314 的 wheel，卡住上限
        let mut child = Command::new(uv)
            .args(["venv", "--python", ">=3.10,<3.14"])
            .arg(venv_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to run uv venv")?;

        // 流式读取 stderr 以显示进度（uv 可能需下载 Python 解释器）
        if let Some(stderr) = child.stderr.take() {
            for line in BufReader::new(stderr).lines().flatten() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    on_progress(trimmed);
                }
            }
        }
        // 排空 stdout
        if let Some(stdout) = child.stdout.take() {
            for _ in BufReader::new(stdout).lines() {}
        }

        let status = child.wait()?;
        if status.success() {
            log::info!("uv venv created successfully");
            let venv_python = if cfg!(target_os = "windows") {
                venv_dir.join("Scripts").join("python.exe")
            } else {
                venv_dir.join("bin").join("python3")
            };
            if venv_python.exists() {
                return Ok(());
            }
            log::warn!("uv venv completed but python binary not found, trying fallback...");
            on_progress("uv 创建的 venv 不完整，尝试系统 Python...");
        } else {
            log::warn!("uv venv failed");
            on_progress("uv 创建 venv 失败，回退到系统 Python...");
        }
    }

    // 回退：使用系统 Python 的标准库 venv
    // 只在真正需要时才搜索系统 Python，避免每次启动都跑 find_python()
    let found_python;
    let python_path = if let Some(p) = python_path.filter(|p| !p.as_os_str().is_empty()) {
        p
    } else {
        found_python = crate::env_check::find_python();
        found_python.as_deref().ok_or_else(|| {
            anyhow::anyhow!("未找到系统 Python 且 uv 不可用，请安装 Python 3.10+ 或 uv")
        })?
    };
    log::info!("Falling back to system Python + venv...");
    on_progress("使用系统 Python 创建虚拟环境...");
    let status = Command::new(python_path)
        .args(["-m", "venv"])
        .arg(venv_dir)
        .status()
        .context("Failed to create virtual environment")?;

    if !status.success() {
        anyhow::bail!("venv creation failed (both uv and system Python)");
    }
    Ok(())
}

// ─── 资源提取 ───

/// 解析资源的实际来源目录。
///
/// Tauri 打包后，资源文件在 .app/Contents/Resources/_up_/ 目录下
/// （_up_ 是 Tauri 的内部目录结构）。
/// 开发模式下资源直接在 resource_dir 中，没有 _up_ 子目录。
fn resolve_resource_src(resource_dir: &Path) -> PathBuf {
    let up_dir = resource_dir.join("_up_");
    if up_dir.exists() && up_dir.is_dir() {
        return up_dir;
    }
    resource_dir.to_path_buf()
}

/// 将资源目录的内容复制到目标目录。
///
/// 跳过 _up_ 目录（Tauri 内部）和图标文件（不需要在运行时访问）。
/// 如果资源目录中没有 app.py（可能处于开发模式），
/// 则回退到从项目根目录复制。
fn copy_resource_dir(src: &Path, dst: &Path) -> Result<()> {
    let actual_src = resolve_resource_src(src);
    let has_app_py = actual_src.join("app.py").exists();

    if !has_app_py {
        // 开发模式：资源目录中没有 app.py，从项目根目录复制
        log::warn!("No bundled resources, falling back to project root (dev mode?)");
        let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        return copy_project_root(project_root, dst);
    }

    log::info!("Copying resources from {:?}", actual_src);
    for entry in std::fs::read_dir(&actual_src)? {
        let entry = entry?;
        let name = entry.file_name();
        // 跳过 Tauri 内部目录和图标文件
        if name == "_up_" || name == "icon.icns" || name == "icon.ico" {
            continue;
        }
        let dst_path = dst.join(&name);
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// 递归复制目录内容。
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// 从项目根目录复制关键文件（开发模式回退方案）。
///
/// 只复制 Flask 应用需要的文件和目录。
fn copy_project_root(project_root: &Path, dst: &Path) -> Result<()> {
    for name in &["app.py", "requirements.txt", "pic_selecter", "static", "assets"] {
        let src = project_root.join(name);
        if !src.exists() {
            continue;
        }
        let dst_path = dst.join(name);
        if src.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_dir_recursive(&src, &dst_path)?;
        } else {
            std::fs::copy(&src, &dst_path)?;
        }
    }
    Ok(())
}
