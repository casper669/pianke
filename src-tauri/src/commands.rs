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
use crate::launcher::{self, AppState, MirrorConfig, state_file_path};
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
    // 写 marker 文件，方便确认命令是否被调用（调试用）
    let marker = state.app_data_dir.join(".init_setup_called");
    let _ = std::fs::write(&marker, "1");

    let state_path = state_file_path(&state.app_data_dir);
    let mut status_msgs: Vec<String> = Vec::new();

    // ─── 步骤 1：确保 Python 环境就绪 ───
    // 如果 main.rs setup 阶段快速创建 venv 失败了（无系统 Python 且无已安装的 uv），
    // 这里重试，包括下载安装 uv 和让 uv 下载独立 Python。
    // 重试过程通过事件推送进度到前端，避免用户看到空白。
    {
        let mut vp = state.venv_python.lock().unwrap();
        if vp.as_os_str().is_empty() {
            let _ = app.emit("setup:status", "未找到 Python，正在尝试通过 uv 安装...");
            status_msgs.push("未找到 Python，正在尝试通过 uv 安装...".into());

            match PythonRuntime::setup_venv(
                state.python_path.as_os_str().is_empty().then(|| std::path::Path::new("")),
                &state.app_data_dir,
                true,  // 使用 ensure_uv，允许下载安装 uv
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
                    // 继续执行，env_check 会报告错误给前端
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

    // ─── 步骤 3：更新检查 ───
    // 传入 venv Python 用于 HTTP 请求和 tar 解压
    status_msgs.push("正在检查更新...".into());
    if !state.venv_python.lock().unwrap().as_os_str().is_empty() {
        let vp = state.venv_python.lock().unwrap();
        updater::check_and_apply(&state.app_dir, &state_path, &vp, &mut |msg: &str| {
            let _ = app.emit("setup:status", msg.to_string());
            status_msgs.push(msg.to_string());
        });
    } else {
        status_msgs.push("跳过更新检查（Python 环境未就绪）".into());
    }

    // ─── 步骤 4：自动启动检测 ───
    let auto_start_modes = {
        let install_state = launcher::load_install_state(&state_path);
        let vp = state.venv_python.lock().unwrap();
        if !install_state.modes.is_empty() && !vp.as_os_str().is_empty() {
            let runtime = PythonRuntime {
                venv_python: vp.clone(),
                app_dir: state.app_dir.clone(),
            };
            let packages = launcher::packages_for_modes(&install_state.modes);
            let sig_changed = install_state.packages_sig != packages.join("|");
            if !sig_changed && runtime.deps_ready(&install_state.modes) {
                Some(install_state.modes)
            } else {
                None
            }
        } else {
            None
        }
    };

    InitResult {
        python_version: env.python_version,
        disk_free_gb: env.disk_free_gb,
        memory_gb: env.memory_gb,
        warnings: env.warnings,
        errors: env.errors,
        status_msgs,
        auto_start_modes,
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

    // 获取镜像配置 — 默认使用清华大学 TUNA 镜像，可通过环境变量关闭
    let mirror = MirrorConfig::default();
    let packages = launcher::packages_for_modes(&modes);
    let packages_sig = packages.join("|");

    // 保存安装状态，下次启动时用于自动启动检测
    let state_path = state_file_path(&state.app_data_dir);
    let saved_state = launcher::load_install_state(&state_path);
    let install_state = launcher::InstallState {
        modes: modes.clone(),
        packages_sig: packages_sig.clone(),
        commit_sha: saved_state.commit_sha, // 保留更新检查写入的 SHA
    };
    launcher::save_install_state(&state_path, &install_state);

    let runtime = PythonRuntime {
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
        || !runtime.deps_ready(&modes);

    // ─── 后台线程：安装 + 启动 + 导航 ───
    // 这些操作可能耗时几分钟（下载 PyTorch 等大包），必须在后台线程执行，
    // 否则会阻塞 Tauri 的主事件循环，导致窗口冻结。
    std::thread::spawn(move || {
        if need_install {
            let _ = app.emit(
                "setup:status",
                format!(
                    "准备安装 {} 个依赖包（首次可能需要几分钟）...",
                    packages.len()
                ),
            );

            // 安装过程通过回调逐行推送进度到前端日志
            if let Err(e) = runtime.install_packages(&packages, &mirror, |msg| {
                let _ = app.emit("setup:progress", msg.to_string());
            }) {
                let _ = app.emit("setup:error", format!("依赖安装失败: {}", e));
                return;
            }
        } else {
            let _ = app.emit("setup:status", "依赖已就绪，正在启动服务...".to_string());
        }

        // 启动 Flask 子进程
        match runtime.start_flask(port, &mirror) {
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
                        // 自动将 webview 导航到 Flask 页面
                        // 这里必须在 Rust 侧调用 navigate()，
                        // 因为 Tauri 2 的安全策略不允许前端用 window.location 跳转到外部 URL
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
