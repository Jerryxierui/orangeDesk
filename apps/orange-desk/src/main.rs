//! orangeDesk 常驻主程序。
//!
//! v0.1：内核宿主进程（单实例 → 配置 → 模块装配 → supervisor 事件循环）。
//! Shell UI（设置 / 壁纸库 / 仪表盘）按架构为卫星进程，v0.2 接入。

use std::process::ExitCode;

use orange_audio::AudioModule;
use orange_kernel::{config, single_instance, supervisor};
use orange_module_api::Module;
use orange_taskbar::TaskbarModule;
use orange_wallpaper::WallpaperModule;
use orange_widgets::WidgetHost;

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct CliOptions {
    demo_events: bool,
    /// 一次性指定皮肤（§13.2 L2）；会写回 config.json 的 active_skin 持久生效
    skin: Option<String>,
    /// 手动激活场景（§13.2 L3）；写回 active_scene 并应用该场景段
    scene: Option<String>,
}

fn parse_args() -> Result<CliOptions, String> {
    let mut options = CliOptions {
        demo_events: false,
        skin: None,
        scene: None,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--demo-events" => options.demo_events = true,
            "--skin" => {
                options.skin = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| "--skin requires a file path".to_string())?,
                );
            }
            "--scene" => {
                options.scene = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| "--scene requires a scene name".to_string())?,
                );
            }
            "--version" | "-V" => {
                println!("orangeDesk {VERSION}");
                std::process::exit(0);
            }
            // 仪表盘数据接口：读取运行实例的快照并打印 JSON（P6）
            "--status" => {
                match orange_kernel::status::read_file(&orange_kernel::config::status_path()) {
                    Ok(snapshot) => {
                        // 新鲜度检查：采样周期 2s，超 10s 未更新 = 实例大概率已退出，
                        // 剩下的是崩溃 / 被杀前的最后快照（P6：不把陈旧数据当好数据）
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let age_ms = now_ms.saturating_sub(snapshot.timestamp_ms);
                        if age_ms > 10_000 {
                            eprintln!(
                                "warning: snapshot is {} s old — the instance may have exited",
                                age_ms / 1000
                            );
                        }
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
         \x20 --skin <file>   Activate a declarative skin (YAML, section 13.2 L2) and persist\n\
         \x20                 it in config.json (active_skin); sections present in the skin\n\
         \x20                 override matching module configs\n\
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
    let (mut app_config, config_path) = match config::load_or_create() {
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

    // 皮肤层（§13.2 L2）：--skin 一次性指定并持久化；active_skin 每次启动生效。
    // 皮肤中出现的段覆盖同名模块段；未出现的段保留用户配置（局部换装合法）。
    let skin_path = options
        .skin
        .clone()
        .or_else(|| app_config.active_skin.clone());
    if let Some(skin_path) = skin_path {
        match orange_skin::load(std::path::Path::new(&skin_path)) {
            Ok(skin) => {
                let (overrides, notes) = orange_skin::compile(&skin);
                for note in notes {
                    log::info!("skin: {note}");
                }
                let section_count = overrides.len();
                for (module_id, value) in overrides {
                    log::info!("skin: overriding modules.{module_id}");
                    app_config.modules.insert(module_id, value);
                }
                let changed = app_config.active_skin.as_deref() != Some(skin_path.as_str());
                app_config.active_skin = Some(skin_path);
                if changed {
                    if let Err(err) = config::save(&app_config, &config_path) {
                        log::warn!("cannot persist active_skin: {err}");
                    }
                }
                log::info!("skin applied ({section_count} module section(s))");
            }
            Err(err) => {
                eprintln!("orangeDesk: skin error: {err}");
                return ExitCode::from(6);
            }
        }
    }

    // 场景模式（§13.2 L3 / F3b）：加载场景包（默认 <config_dir>/scenes.yaml），
    // 校验诊断；--scene 手动激活并持久化；运行时触发器经 supervisor 事件观察者评估
    let scenes_path = config::config_dir().join("scenes.yaml");
    let scene_pack = if scenes_path.exists() {
        match orange_skin::scenes::load_pack(&scenes_path) {
            Ok(pack) => {
                for issue in orange_skin::scenes::validate_pack(&pack) {
                    log::warn!("scenes: {issue}");
                }
                log::info!(
                    "scene pack loaded: {} scene(s), {} trigger(s)",
                    pack.scenes.len(),
                    pack.triggers.len()
                );
                Some(pack)
            }
            Err(err) => {
                log::warn!("scenes: pack ignored ({err})");
                None
            }
        }
    } else {
        None
    };

    if let Some(name) = options.scene.as_ref() {
        let Some(pack) = scene_pack.as_ref() else {
            eprintln!(
                "orangeDesk: --scene requires a scene pack at {}",
                scenes_path.display()
            );
            return ExitCode::from(7);
        };
        match pack.scenes.get(name) {
            Some(scene) => {
                for (module_id, value) in orange_skin::scenes::compile_scene(scene) {
                    log::info!("scene '{name}': applying modules.{module_id}");
                    app_config.modules.insert(module_id, value);
                }
                app_config.active_scene = Some(name.clone());
                if let Err(err) = config::save(&app_config, &config_path) {
                    log::warn!("cannot persist active_scene: {err}");
                }
                log::info!("scene '{name}' activated");
            }
            None => {
                eprintln!(
                    "orangeDesk: scene '{name}' not found (available: {})",
                    pack.scenes.keys().cloned().collect::<Vec<_>>().join(", ")
                );
                return ExitCode::from(7);
            }
        }
    } else if let (Some(pack), Some(active)) =
        (scene_pack.as_ref(), app_config.active_scene.as_ref())
    {
        // 启动时应用上次激活的场景（触发器去抖初值与之一致）
        if let Some(scene) = pack.scenes.get(active) {
            for (module_id, value) in orange_skin::scenes::compile_scene(scene) {
                app_config.modules.insert(module_id, value);
            }
            log::info!("scene '{active}' applied on startup");
        }
    }

    // 触发器观察者（app 层逻辑；内核只回调，零场景知识）：
    // ForegroundChanged → 前台匹配；MinuteTick → 时间表匹配；去抖后热应用
    let event_observer = scene_pack.map(|pack| {
        let active = std::sync::Arc::new(std::sync::Mutex::new(app_config.active_scene.clone()));
        std::sync::Arc::new(
            move |event: &orange_module_api::KernelEvent,
                  publisher: &orange_kernel::bus::EventPublisher| {
                let target = match event {
                    orange_module_api::KernelEvent::ForegroundChanged { process_name } => {
                        orange_skin::scenes::match_foreground(&pack, process_name)
                    }
                    orange_module_api::KernelEvent::MinuteTick => {
                        orange_skin::scenes::match_schedule(
                            &pack,
                            orange_skin::scenes::local_minute_of_day(),
                        )
                    }
                    _ => None,
                };
                let Some(name) = target else { return };
                let mut guard = active.lock().unwrap();
                if guard.as_deref() == Some(name.as_str()) {
                    return; // 已激活：去抖，不重复下发
                }
                let Some(scene) = pack.scenes.get(&name) else {
                    return;
                };
                for (module_id, value) in orange_skin::scenes::compile_scene(scene) {
                    publisher.publish(orange_module_api::KernelEvent::ModuleConfigChanged {
                        module: module_id,
                        config: value,
                    });
                }
                *guard = Some(name.clone());
                log::info!("scene '{name}' activated by trigger");
            },
        )
            as std::sync::Arc<
                dyn Fn(&orange_module_api::KernelEvent, &orange_kernel::bus::EventPublisher)
                    + Send
                    + Sync,
            >
    });

    // 模块装配：新增能力 = 在此注册一个新模块，不触碰内核
    // （v0.4+：window-material）
    let audio = AudioModule::new();
    // 音频事件桥（§13 F4）：supervisor 的 publisher_tap 在总线就绪后注入，
    // worker 的 AudioSpectrum 事件由此进入总线
    let audio_sink = audio.sink();
    let modules: Vec<Box<dyn Module>> = vec![
        Box::new(WallpaperModule::new()),
        Box::new(TaskbarModule::new()),
        Box::new(WidgetHost::new()),
        Box::new(audio),
    ];

    let report = supervisor::run(
        app_config.modules.clone(),
        modules,
        supervisor::SupervisorOptions {
            demo_events: options.demo_events,
            event_observer,
            publisher_tap: Some(std::sync::Arc::new(move |publisher| {
                let _ = audio_sink.set(std::sync::Arc::new(move |event| publisher.publish(event)));
            })),
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
