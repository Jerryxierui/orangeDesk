//! orangeDesk 常驻主程序。
//!
//! v0.1：内核宿主进程（单实例 → 配置 → 模块装配 → supervisor 事件循环）。
//! Shell UI（设置 / 壁纸库 / 仪表盘）按架构为卫星进程，v0.2 接入。

use std::process::ExitCode;

use orange_kernel::{config, single_instance, supervisor};
use orange_module_api::Module;
use orange_taskbar::TaskbarModule;
use orange_wallpaper::WallpaperModule;
use orange_widgets::WidgetHost;

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct CliOptions {
    demo_events: bool,
}

fn parse_args() -> Result<CliOptions, String> {
    let mut options = CliOptions { demo_events: false };
    let args: Vec<String> = std::env::args().skip(1).collect();
    for arg in &args {
        match arg.as_str() {
            "--demo-events" => options.demo_events = true,
            "--version" | "-V" => {
                println!("orangeDesk {VERSION}");
                std::process::exit(0);
            }
            // 仪表盘数据接口：读取运行实例的快照并打印 JSON（P6）
            "--status" => {
                match orange_kernel::status::read_file(&orange_kernel::config::status_path()) {
                    Ok(snapshot) => {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&snapshot)
                                .unwrap_or_else(|_| "{}".to_string())
                        );
                        std::process::exit(0);
                    }
                    Err(err) => {
                        eprintln!("no status available (is orangeDesk running?): {err}");
                        std::process::exit(5);
                    }
                }
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(options)
}

fn print_help() {
    println!(
        "orangeDesk {VERSION} — integrated desktop environment layer\n\
         \n\
         USAGE: orange-desk [OPTIONS]\n\
         \n\
         OPTIONS:\n\
         \x20 --demo-events   Run a scripted event sequence (foreground/fullscreen/lock/power)\n\
         \x20                 then exit; used to verify the module lifecycle chain in CI\n\
         \x20 --status        Print the running instance's dashboard snapshot as JSON\n\
         \x20                 (modules, degrade states, process readings) and exit\n\
         \x20 --version       Print version\n\
         \x20 --help          Print this help"
    );
}

fn init_logging(level: &str) {
    // RUST_LOG 优先于配置文件（开发者覆盖）
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level))
        .format_timestamp_millis()
        .init();
}

fn main() -> ExitCode {
    let options = match parse_args() {
        Ok(options) => options,
        Err(err) => {
            eprintln!("error: {err}\ntry --help");
            return ExitCode::from(2);
        }
    };

    // 单实例互斥（内核守护职责之一）
    if let Err(err) = single_instance::acquire() {
        eprintln!("orangeDesk: {err}");
        return ExitCode::from(3);
    }

    // 配置加载 / 首启动默认值填充（P2）
    let (app_config, config_path) = match config::load_or_create() {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("orangeDesk: config error: {err}");
            return ExitCode::from(4);
        }
    };
    init_logging(&app_config.logging.level);
    log::info!(
        "orangeDesk {VERSION} starting (config: {})",
        config_path.display()
    );

    // 模块装配：新增能力 = 在此注册一个新模块，不触碰内核
    // （v0.4+：window-material）
    let modules: Vec<Box<dyn Module>> = vec![
        Box::new(WallpaperModule::new()),
        Box::new(TaskbarModule::new()),
        Box::new(WidgetHost::new()),
    ];

    let report = supervisor::run(
        app_config.modules.clone(),
        modules,
        supervisor::SupervisorOptions {
            demo_events: options.demo_events,
        },
    );

    // 退出摘要（P6：降级与隔离用户可见；detail 含材质 / 降级说明）
    println!("— orangeDesk session summary —");
    for module in &report.modules {
        let state = if module.crash_count > 0 {
            "crashed (isolated)"
        } else if module.online {
            "online"
        } else {
            "offline"
        };
        let detail = if module.detail.is_empty() {
            "-"
        } else {
            &module.detail
        };
        println!("  {:<12} [{state}] {detail}", module.id);
        if !module.online && module.crash_count == 0 {
            let reason = report
                .offline_modules
                .iter()
                .find(|(id, _)| id == &module.id)
                .map(|(_, reason)| reason.as_str())
                .unwrap_or("");
            if !reason.is_empty() {
                println!("  {:<12} reason: {reason}", "");
            }
        }
    }
    ExitCode::SUCCESS
}
