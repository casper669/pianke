//! 片刻 (Pianke) — 智能选片助手
//!
//! 本文件是 Tauri 桌面应用的入口点，负责：
//! 1. 初始化日志系统
//! 2. 注册 Tauri 插件（shell）
//! 3. 在 setup 阶段解析路径、查找 Python、创建虚拟环境
//! 4. 注册 IPC 命令供前端调用
//! 5. 在应用退出时清理 Flask 子进程
//!
//! 架构说明：
//! - setup 阶段只做路径解析和状态初始化，不发送事件（前端监听器尚未注册）
//! - 所有环境检测、更新检查等耗时操作由前端主动调用 init_setup 命令触发
//! - Flask 进程的生命周期由 FlaskProcess 管理，应用退出时通过 RunEvent::Exit 自动 kill

mod commands;
mod env_check;
mod launcher;
mod python_runtime;
mod updater;

use std::sync::Mutex;

use tauri::Manager;
use tauri::menu::{Menu, MenuItem, Submenu, PredefinedMenuItem};

fn main() {
    // 初始化日志 — 通过 env_logger 输出到 stderr，开发时方便调试
    env_logger::init();

    let app = tauri::Builder::default()
        // 注册 shell 插件，用于在外部浏览器打开链接等功能
        .plugin(tauri_plugin_shell::init())
        // FlaskProcess 用于跟踪 Flask 子进程，退出时需要 kill 它
        .manage(commands::FlaskProcess(Mutex::new(None)))
        .setup(|app| {
            // ─── setup 阶段：只做纯路径解析，不阻塞 webview 首帧 ───
            // 所有文件 I/O、venv 创建等耗时操作延迟到前端加载后由 init_setup 触发。

            // 提前显示窗口：避免 init_setup 阻塞主线程导致窗口白屏。
            // macOS 上 CoreAnimation 需要主线程 run loop 来提交首帧，
            // 必须在任何可能阻塞主线程的 IPC 调用之前执行 show()。
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
            }

            // 资源目录：打包后的 .app/Contents/Resources/（macOS）或 exe 同目录（Windows）
            let resource_dir = app
                .path()
                .resource_dir()
                .map_err(|e| anyhow::anyhow!("无法获取资源目录: {}", e))?;

            // 应用数据目录：~/Library/Application Support/com.pianke.desktop/（macOS）
            let app_data_dir = app
                .path()
                .app_data_dir()
                .map_err(|e| anyhow::anyhow!("无法获取数据目录: {}", e))?;

            // app_dir 路径（实际提取由 init_setup 延迟执行）
            let app_dir = app_data_dir.join("app");

            // 将应用全局状态注册到 Tauri 的状态管理中
            // python_path 初始为空，由 init_setup 命令在页面显示后通过 uv 查找/安装
            let home_url = app.get_webview_window("main")
                .and_then(|w| w.url().ok())
                .map(|u| u.to_string())
                .unwrap_or_default();
            let state = launcher::AppState {
                resource_dir,
                app_data_dir,
                python_path: std::path::PathBuf::new(),
                venv_python: std::sync::Mutex::new(std::path::PathBuf::new()),
                app_dir,
                home_url,
            };
            app.manage(state);

            // 设置最简菜单 — 移除 Tauri 默认添加的 File/Edit/View/Window/Help，
            // 只保留 macOS 要求的应用菜单（关于、隐藏、退出等）
            let menu = Menu::with_items(app, &[
                &Submenu::with_items(app, "片刻", true, &[
                    &PredefinedMenuItem::about(app, Some("关于片刻"), None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(app, "check_update", "检查更新", true, None::<&str>)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::services(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::hide(app, Some("隐藏片刻"))?,
                    &PredefinedMenuItem::hide_others(app, Some("隐藏其他"))?,
                    &PredefinedMenuItem::show_all(app, Some("显示全部"))?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::quit(app, Some("退出片刻"))?,
                ])?,
            ])?;
            app.set_menu(menu)?;

            Ok(())
        })
        // 菜单事件处理 — "检查更新" 菜单项被点击时触发
        .on_menu_event(|app_handle, event| {
            if event.id().as_ref() == "check_update" {
                // 1. 立即停掉 Flask 进程（SIGKILL，不等待优雅退出）
                if let Some(flask_state) = app_handle.try_state::<commands::FlaskProcess>() {
                    if let Some(mut child) = flask_state.0.lock().unwrap().take() {
                        log::info!("Stopping Flask for update check...");
                        #[cfg(unix)]
                        {
                            let pid = child.id() as i32;
                            let _ = std::process::Command::new("kill")
                                .args(["-KILL", &format!("-{}", pid)])
                                .status();
                        }
                        #[cfg(windows)]
                        {
                            let pid = child.id();
                            let _ = std::process::Command::new("taskkill")
                                .args(["/T", "/F", "/PID", &pid.to_string()])
                                .status();
                        }
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
                // 兜底：清理端口上残留的进程
                #[cfg(unix)]
                {
                    if let Ok(o) = std::process::Command::new("lsof")
                        .args(["-ti", ":5057"]).output()
                    {
                        let pids = String::from_utf8_lossy(&o.stdout);
                        for pid_str in pids.lines() {
                            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                                let _ = std::process::Command::new("kill")
                                    .args(["-KILL", &format!("{}", pid)])
                                    .status();
                            }
                        }
                    }
                }
                #[cfg(windows)]
                {
                    if let Ok(output) = std::process::Command::new("netstat").args(["-ano"]).output() {
                        let out = String::from_utf8_lossy(&output.stdout);
                        for line in out.lines() {
                            if line.contains(":5057") && line.contains("LISTENING") {
                                let parts: Vec<&str> = line.split_whitespace().collect();
                                if let Some(pid_str) = parts.last() {
                                    let _ = std::process::Command::new("taskkill")
                                        .args(["/F", "/PID", pid_str])
                                        .status();
                                }
                            }
                        }
                    }
                }

                // 2. 立即导航回 Tauri 前端 = 环境检查页面
                if let Some(w) = app_handle.get_webview_window("main") {
                    if let Some(app_state) = app_handle.try_state::<launcher::AppState>() {
                        if !app_state.home_url.is_empty() {
                            if let Ok(url) = url::Url::parse(&app_state.home_url) {
                                let _ = w.navigate(url);
                            }
                        }
                    }
                }
            }
        })
        // 注册所有 IPC 命令 — 前端通过 invoke('命令名', args) 调用
        .invoke_handler(tauri::generate_handler![
            commands::show_window,       // 页面加载后显示窗口（配合 visible:false）
            commands::navigate_to_flask, // 前端轮询 Flask 就绪后跳转
            commands::init_setup,        // 初始化：环境检测 + 更新检查，返回 InitResult
            commands::check_environment, // 单独的环境检测（目前未使用，保留备用）
            commands::get_modes,         // 获取可选模式列表（含上次选择状态）
            commands::start_setup,       // 开始安装依赖并启动 Flask
        ])
        .build(tauri::generate_context!())
        .expect("启动失败");

    // ─── 应用主循环 ───
    // 使用 build() + run() 模式替代 run()，以便在 RunEvent::Exit 中可靠地清理资源。
    // run() 模式的 on_window_event 在 macOS 上无法保证退出时清理代码来得及执行。
    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            // ─── 方式一：通过 FlaskProcess state 清理 ───
            if let Some(state) = app_handle.try_state::<commands::FlaskProcess>() {
                if let Some(mut child) = state.0.lock().unwrap().take() {
                    log::info!("Shutting down Flask...");
                    #[cfg(unix)]
                    {
                        let pid = child.id() as i32;
                        // kill 负数 PID = 杀进程组
                        let _ = std::process::Command::new("kill")
                            .args(["-TERM", &format!("-{}", pid)])
                            .status();
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        let _ = std::process::Command::new("kill")
                            .args(["-KILL", &format!("-{}", pid)])
                            .status();
                    }
                    #[cfg(windows)]
                    {
                        // taskkill /T = 杀进程树（等价 Unix 进程组）
                        let pid = child.id();
                        let _ = std::process::Command::new("taskkill")
                            .args(["/T", "/F", "/PID", &pid.to_string()])
                            .status();
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }

            // ─── 方式二：端口级兜底清理 ───
            // 如果 state 方式没拿到子进程句柄，用平台工具查找并 kill 端口占用进程
            let port = std::env::var("PIC_SELECTER_PORT")
                .ok()
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(5057);
            #[cfg(unix)]
            {
                let _ = std::process::Command::new("lsof")
                    .args(["-ti", &format!(":{}", port)])
                    .output()
                    .ok()
                    .and_then(|o| {
                        let pids = String::from_utf8_lossy(&o.stdout);
                        for pid_str in pids.lines() {
                            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                                let _ = std::process::Command::new("kill")
                                    .args(["-TERM", &format!("{}", pid)])
                                    .status();
                            }
                        }
                        Some(())
                    });
            }
            #[cfg(windows)]
            {
                if let Ok(output) = std::process::Command::new("netstat").args(["-ano"]).output() {
                    let out = String::from_utf8_lossy(&output.stdout);
                    for line in out.lines() {
                        if line.contains(&format!(":{}", port)) && line.contains("LISTENING") {
                            let parts: Vec<&str> = line.split_whitespace().collect();
                            if let Some(pid_str) = parts.last() {
                                let _ = std::process::Command::new("taskkill")
                                    .args(["/F", "/PID", pid_str])
                                    .status();
                            }
                        }
                    }
                }
            }
        }
    });
}
