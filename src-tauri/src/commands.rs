//! Tauri IPC 命令 — 前端与 Rust 后端的通信桥梁。
//!
//! 前端通过 `window.__TAURI_INTERNALS__.invoke('命令名', args)` 调用这些函数。
//! 命令之间通过返回值传递数据，通过 Tauri 事件系统向前端推送进度消息。
//!
//! 启动流程：
//!   前端加载 HTML → 调用 init_setup → 获取环境信息 + 自动检测已安装模式
//!   → 如果已安装则自动调用 start_setup（跳过模式选择）
//!   → 否则展示模式选择界面，用户点击后调用 start_setup
//!   → start_setup 在后台线程安装依赖、启动 Flask → 自动导航到 Flask 页面

use std::process::Child;
use std::sync::Mutex;
use std::time::Duration;

use tauri::{command, AppHandle, Emitter, Manager, State};

use serde::Serialize;

use crate::env_check;
use crate::launcher::{self, AppState, CudaConfig, MirrorConfig, state_file_path};
use crate::python_runtime::PythonRuntime;
use crate::updater;

/// 管理 Flask 子进程的生命周期。
///
/// 用 Mutex<Option<Child>> 包裹是因为：
/// - Mutex：后台线程和主线程（窗口关闭时）都可能访问
/// - Option：初始为 None，Flask 启动后才放入 Some
/// - 窗口关闭时 take() 取出并 kill，确保端口释放
pub struct FlaskProcess(pub Mutex<Option<Child>>);

/// init_setup 的返回值，包含前端展示环境信息所需的全部数据。
#[derive(Serialize)]
pub struct InitResult {
    /// Python 版本字符串，如 "Python 3.12.3"，None 表示未找到
    pub python_version: Option<String>,
    /// 当前工作目录所在磁盘的剩余空间（GB）
    pub disk_free_gb: f64,
    /// 系统总内存（GB）
    pub memory_gb: f64,
    /// 不阻塞启动的提示信息（如内存偏小）
    pub warnings: Vec<String>,
    /// 阻塞启动的错误（如未找到 Python、磁盘不足）
    pub errors: Vec<String>,
    /// 初始化过程中的状态消息，前端展示在环境卡片的底部
    pub status_msgs: Vec<String>,
    /// 如果依赖已安装且签名匹配，返回可自动启动的模式列表
    /// 前端收到非空值时跳过模式选择，直接进入安装/启动流程
    pub auto_start_modes: Option<Vec<String>>,
    /// 自动启动时的运行时设备偏好
    pub auto_start_runtime: Option<String>,
}

/// 初始化设置 — 前端加载完成后第一个调用的命令。
///
/// 执行以下检查（同步，约 2-5 秒，无 venv 时可能更久）：
/// 1. 环境检测：Python 版本、磁盘空间、内存
/// 2. 如果 venv 未就绪，尝试用 ensure_uv 重试创建（可能下载 uv/Python）
/// 3. 更新检查：从 GitHub 拉取最新提交，如有更新则下载解压
/// 4. 自动启动检测：如果上次已安装且依赖签名未变化，返回 auto_start_modes
#[command]
pub fn init_setup(
    app: AppHandle,
    state: State<'_, AppState>,
) -> InitResult {
    let marker = state.app_data_dir.join(".init_setup_called");
    let _ = std::fs::write(&marker, "1");

    let state_path = state_file_path(&state.app_data_dir);
    let mut status_msgs: Vec<String> = Vec::new();

    // ─── 步骤 0：延迟初始化（setup 阶段跳过的文件 I/O）───
    // 提取代码资源（如果尚未提取）
    if !state.app_dir.join("app.py").exists() {
        let _ = app.emit("setup:status", "正在准备运行环境...");
        status_msgs.push("正在准备运行环境...".into());
        if let Err(e) = PythonRuntime::extract_resources(
            &state.resource_dir,
            &state.app_data_dir,
        ) {
            log::error!("Resource extraction failed: {}", e);
        }
    }

    // ─── 步骤 1：确保 Python 虚拟环境就绪 ───
    {
        let mut vp = state.venv_python.lock().unwrap();
        if vp.as_os_str().is_empty() {
            let _ = app.emit("setup:status", "未找到 Python，正在尝试通过 uv 安装...");
            status_msgs.push("未找到 Python，正在尝试通过 uv 安装...".into());

            // 查找系统 Python 作为 uv 失败时的回退方案
            let system_python = env_check::find_python();

            match PythonRuntime::setup_venv(
                system_python.as_deref(),
                &state.app_data_dir,
                true,
                |msg| {
                    let _ = app.emit("setup:status", msg.to_string());
                },
            ) {
                Ok(venv_path) => {
                    *vp = venv_path;
                    let _ = app.emit("setup:status", "Python 环境已就绪");
                    status_msgs.push("Python 环境已就绪".into());
                }
                Err(e) => {
                    let msg = format!("Python 环境创建失败: {}", e);
                    let _ = app.emit("setup:status", &msg);
                    status_msgs.push(msg);
                }
            }
        }
    }

    // ─── 步骤 2：环境检测 ───
    // 传入 venv_python 路径，如果系统没 Python 但 venv 已就绪，check() 不会报错
    let venv_python_guard = state.venv_python.lock().unwrap();
    let venv_ref = (!venv_python_guard.as_os_str().is_empty()).then(|| &*venv_python_guard);
    let env = env_check::check(venv_ref);
    drop(venv_python_guard);

    // ─── 步骤 3：更新检查（后台异步，不阻塞 UI）───
    status_msgs.push("正在检查更新...".into());
    if !state.venv_python.lock().unwrap().as_os_str().is_empty() {
        let vp = state.venv_python.lock().unwrap().clone();
        let app_dir = state.app_dir.clone();
        let state_path2 = state_path.clone();
        let app2 = app.clone();
        std::thread::spawn(move || {
            updater::check_and_apply(&app_dir, &state_path2, &vp, &mut |msg: &str| {
                let _ = app2.emit("setup:status", msg.to_string());
            });
        });
    } else {
        status_msgs.push("跳过更新检查（Python 环境未就绪）".into());
    }

    // ─── 步骤 4：自动启动检测 ───
    let install_state = launcher::load_install_state(&state_path);
    let auto_start_modes = {
        let vp = state.venv_python.lock().unwrap();
        if !install_state.modes.is_empty() && !vp.as_os_str().is_empty() {
            let mode_packages = launcher::packages_for_modes(&install_state.modes);
            let runtime = if install_state.runtime.is_empty() { "auto" } else { &install_state.runtime };
            let force_cpu = std::env::var("PIANKE_FORCE_CPU").map_or(false, |v| v == "1");
            let backend_label = launcher::runtime_backend_label(&install_state.modes, runtime, force_cpu);
            let needs_backend = backend_label != "none";
            let cuda = CudaConfig::default();
            let backend_sig = if needs_backend {
                let flavor = if backend_label == "cuda" { format!(":{}", cuda.flavor) } else { String::new() };
                format!("vision:{}{}", backend_label, flavor)
            } else {
                "none".to_string()
            };
            let expected_sig = format!("{}|{}", mode_packages.join("|"), backend_sig);
            let sig_changed = install_state.packages_sig != expected_sig;
            if !sig_changed {
                // deps_ready() is checked again in start_setup (background thread)
                // to avoid blocking the main thread on non-first launches
                Some(install_state.modes.clone())
            } else {
                None
            }
        } else {
            None
        }
    };

    let auto_start_runtime = auto_start_modes.as_ref().map(|_| {
        if install_state.runtime.is_empty() { "auto".to_string() } else { install_state.runtime.clone() }
    });

    InitResult {
        python_version: env.python_version,
        disk_free_gb: env.disk_free_gb,
        memory_gb: env.memory_gb,
        warnings: env.warnings,
        errors: env.errors,
        status_msgs,
        auto_start_modes,
        auto_start_runtime,
    }
}

/// 显示主窗口 — 页面加载后调用，配合 visible:false 实现秒开。
#[command]
pub fn show_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
    }
}

/// 前端轮询检测到 Flask 就绪后，调用此命令跳转到 Flask 页面。
#[command]
pub fn navigate_to_flask(app: AppHandle, url: String) {
    if let Some(w) = app.get_webview_window("main") {
        if let Ok(parsed) = url::Url::parse(&url) {
            let _ = w.navigate(parsed);
        }
    }
}

/// 检查运行环境（独立版本，目前保留备用）。
///
/// 与 init_setup 不同，这个命令不做 venv 重试和更新检查，
/// 只返回当前环境的快照信息。
#[command]
pub fn check_environment(state: State<'_, AppState>) -> env_check::EnvInfo {
    let vp = state.venv_python.lock().unwrap();
    let venv_ref = (!vp.as_os_str().is_empty()).then(|| &*vp);
    env_check::check(venv_ref)
}

/// 返回功能模式列表，标记上次选择的模式为 selected。
///
/// 前端用这个列表渲染模式选择卡片，用户勾选后点击"开始安装"。
#[command]
pub fn get_modes(state: State<'_, AppState>) -> Vec<launcher::ModeInfo> {
    let install_state = launcher::load_install_state(&state_file_path(&state.app_data_dir));
    launcher::get_modes(&install_state.modes)
}

/// 开始安装依赖并启动 Flask 服务。
///
/// 这是整个应用的核心命令，执行流程：
/// 1. 验证模式名称合法性（防止前端传脏数据）
/// 2. 保存已选模式到安装状态文件
/// 3. 在后台线程中：
///    a. 检查依赖是否需要重装（签名对比）
///    b. 如需安装，用 uv/pip 安装 Python 包（带进度推送）
///    c. 启动 Flask 子进程
///    d. 轮询等待 Flask 就绪
///    e. 自动将 webview 导航到 Flask 页面
///
/// 返回 Ok(()) 表示后台任务已启动（不等它完成），
/// 后续进度通过 Tauri 事件 setup:status / setup:progress / setup:error / setup:ready 推送。
#[command]
pub fn start_setup(
    app: AppHandle,
    state: State<'_, AppState>,
    _flask: State<'_, FlaskProcess>,
    modes: Vec<String>,
    runtime: String,
) -> Result<(), String> {
    // 验证模式名称 — 防御性检查，防止前端传入非法模式
    let valid_keys: Vec<String> = launcher::get_modes(&[])
        .into_iter()
        .map(|m| m.key)
        .collect();

    for m in &modes {
        if !valid_keys.contains(m) {
            return Err(format!("未知模式: {}", m));
        }
    }

    if modes.is_empty() {
        return Err("请至少选择一个模式".into());
    }

    // Python 环境未就绪时提前拒绝，比在后台线程里报错更友好
    if state.venv_python.lock().unwrap().as_os_str().is_empty() {
        return Err("无法找到 Python，请安装 Python 3.10+ 或安装 uv".into());
    }

    let runtime = if runtime.is_empty() { "auto".to_string() } else { runtime };

    // 获取镜像和 CUDA 配置
    let mirror = MirrorConfig::default();
    let cuda = CudaConfig::default();

    // 分离模式包和运行时后端包
    let mode_packages = launcher::packages_for_modes(&modes);
    let force_cpu = std::env::var("PIANKE_FORCE_CPU").map_or(false, |v| v == "1");
    let backend_label = launcher::runtime_backend_label(&modes, &runtime, force_cpu);
    let needs_backend = backend_label != "none";

    // 签名 = 模式包排序 | 后端标识（含 CUDA flavor）
    let cuda_flavor = if needs_backend && backend_label == "cuda" {
        format!(":{}", cuda.flavor)
    } else {
        String::new()
    };
    let backend_sig = if needs_backend {
        format!("vision:{}{}", backend_label, cuda_flavor)
    } else {
        "none".to_string()
    };
    let packages_sig = format!("{}|{}", mode_packages.join("|"), backend_sig);

    // 保存安装状态，下次启动时用于自动启动检测
    let state_path = state_file_path(&state.app_data_dir);
    let saved_state = launcher::load_install_state(&state_path);
    let install_state = launcher::InstallState {
        modes: modes.clone(),
        runtime: runtime.clone(),
        packages_sig: packages_sig.clone(),
        runtime_backend: backend_label.to_string(),
        commit_sha: saved_state.commit_sha,
    };
    launcher::save_install_state(&state_path, &install_state);

    let py_runtime = PythonRuntime {
        venv_python: state.venv_python.lock().unwrap().clone(),
        app_dir: state.app_dir.clone(),
    };

    // Flask 端口：默认 5057，可通过环境变量 PIC_SELECTER_PORT 覆盖
    let port: u16 = std::env::var("PIC_SELECTER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5057);

    // 判断是否需要重新安装依赖
    // 条件：包签名变了 OR 上次安装的包不完整
    let need_install = saved_state.packages_sig != packages_sig
        || !py_runtime.deps_ready(&modes);

    // ─── 后台线程：安装 + 启动 + 导航 ───
    // 这些操作可能耗时几分钟（下载 PyTorch 等大包），必须在后台线程执行，
    // 否则会阻塞 Tauri 的主事件循环，导致窗口冻结。
    std::thread::spawn(move || {
        if need_install {
            // 步骤 1：安装运行时后端（torch / onnxruntime，含 CUDA 版本判断）
            if needs_backend {
                let _ = app.emit("setup:status", "正在安装运行时后端（torch / onnxruntime）...");
                match py_runtime.install_runtime_backend(
                    &modes,
                    &runtime,
                    &mirror,
                    &cuda,
                    |msg| { let _ = app.emit("setup:progress", msg.to_string()); },
                ) {
                    Ok(label) => {
                        let _ = app.emit("setup:progress", format!("运行时后端就绪: {}", label));
                    }
                    Err(e) => {
                        let _ = app.emit("setup:error", format!("运行时后端安装失败: {}", e));
                        return;
                    }
                }
            }

            // 步骤 2：安装模式相关包（transformers、insightface、pyiqa 等）
            let total = mode_packages.len();
            let _ = app.emit(
                "setup:status",
                format!("准备安装 {} 个依赖包...", total),
            );

            if let Err(e) = py_runtime.install_packages(&mode_packages, &mirror, |msg| {
                let _ = app.emit("setup:progress", msg.to_string());
            }) {
                let _ = app.emit("setup:error", format!("依赖安装失败: {}", e));
                return;
            }
        } else {
            let _ = app.emit("setup:status", "依赖已就绪，正在启动服务...".to_string());
        }

        // 启动 Flask 子进程
        match py_runtime.start_flask(port, &install_state.runtime, &mirror) {
            Ok(child) => {
                // 将子进程句柄存入 FlaskProcess，供窗口关闭时 kill
                if let Some(f) = app.try_state::<FlaskProcess>() {
                    *f.0.lock().unwrap() = Some(child);
                }

                // 轮询等待 Flask 就绪，最多等 60 秒
                let _ = app.emit("setup:status", "等待服务就绪...".to_string());
                match wait_for_server(port, Duration::from_secs(60)) {
                    Ok(_) => {
                        let url = format!("http://localhost:{}", port);
                        let _ = app.emit("setup:ready", url.clone());
                        // 在 Rust 侧直接导航——前端 XHR 轮询会因 WebView
                        // 混合内容限制（https://tauri.localhost → http://localhost）
                        // 被拦截，不可靠，因此由后端 TCP 检测通过后直接跳转。
                        if let Some(w) = app.get_webview_window("main") {
                            if let Ok(parsed) = url::Url::parse(&url) {
                                let _ = w.navigate(parsed);
                            }
                        }

                        // Flask 存活监控：每 30 秒检测一次，连续 3 次失败则通知前端
                        let mut failures: u32 = 0;
                        loop {
                            std::thread::sleep(Duration::from_secs(30));
                            if check_server(port) {
                                failures = 0;
                            } else {
                                failures += 1;
                                if failures >= 3 {
                                    let _ = app.emit(
                                        "setup:error",
                                        "Flask 服务已无响应，请重启应用",
                                    );
                                    break;
                                }
                                let _ = app.emit(
                                    "setup:status",
                                    format!("服务连接失败 ({}/3)", failures),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        let _ = app.emit(
                            "setup:error",
                            format!("服务启动超时: {}", e),
                        );
                    }
                }
            }
            Err(e) => {
                let _ = app.emit(
                    "setup:error",
                    format!("无法启动服务: {}", e),
                );
            }
        }
    });

    Ok(())
}

// ─── Flask 服务就绪检测 ───

/// 用 TCP 连接探测 Flask 是否已启动。
/// 返回 true 表示端口已被监听（服务在运行）。
fn check_server(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(2),
    )
    .is_ok()
}

/// 轮询等待 Flask 服务就绪，每 500ms 探测一次，超时则返回错误。
fn wait_for_server(port: u16, timeout: Duration) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > timeout {
            anyhow::bail!(
                "Flask 服务在 {} 秒内未响应",
                timeout.as_secs()
            );
        }
        if check_server(port) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
