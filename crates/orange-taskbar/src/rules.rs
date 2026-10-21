//! 任务栏材质规则引擎（§3 模块 C：声明式规则——场景条件 → 材质）。
//!
//! 纯逻辑、跨平台可测；默认规则集体现 P2（默认即好看）与性能底线
//! （全屏 / 电池节能自动收缩），用户可在配置里整体覆盖规则列表。

use serde::{Deserialize, Serialize};

/// 任务栏材质档位。降级链（能力探测失败时逐级回落）：Acrylic → Blur → Clear → Native。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Material {
    /// 系统原生（不干预；也是退出 / 卸载时的恢复态，P3 零残留）
    Native,
    /// 透明无模糊
    Clear,
    /// 模糊
    Blur,
    /// 亚克力（实时模糊，GPU 开销最高；电池节能场景自动降级）
    Acrylic,
}

/// 规则条件
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Condition {
    /// 无条件（兜底规则）
    Always,
    /// 前台进程名匹配（对路径基名做大小写不敏感的子串匹配）
    ForegroundProcess { pattern: String },
    /// 前台窗口全屏（全屏独占游戏 / DRM 内容）
    Fullscreen,
    /// 电池节能模式
    PowerSaver,
    /// 已锁屏
    Locked,
}

/// 单条规则：条件成立 → 应用材质
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    #[serde(flatten)]
    pub condition: Condition,
    pub material: Material,
}

/// 评估输入：内核事件累积出的环境快照（事件驱动维护，无轮询，P4）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnvState {
    pub foreground_process: String,
    pub fullscreen: bool,
    pub power_saver: bool,
    pub locked: bool,
}

impl EnvState {
    pub fn matches(&self, condition: &Condition) -> bool {
        match condition {
            Condition::Always => true,
            Condition::Fullscreen => self.fullscreen,
            Condition::PowerSaver => self.power_saver,
            Condition::Locked => self.locked,
            Condition::ForegroundProcess { pattern } => {
                // 取路径基名后做大小写不敏感子串匹配（容错：exe 路径 / 大小写混用）
                let base = self
                    .foreground_process
                    .rsplit(['\\', '/'])
                    .next()
                    .unwrap_or(&self.foreground_process);
                let base = base.to_ascii_lowercase();
                let pattern = pattern.to_ascii_lowercase();
                !pattern.is_empty() && base.contains(&pattern)
            }
        }
    }
}

/// 规则引擎：按列表顺序取首个匹配（靠前的优先级高）。
pub struct RuleEngine {
    rules: Vec<Rule>,
}

impl RuleEngine {
    pub fn new(rules: Vec<Rule>) -> Self {
        Self { rules }
    }

    /// 内置默认规则集（高 → 低优先级）：
    /// 全屏让出 GPU → Clear；电池节能 → Clear；锁屏 → Native；默认毛玻璃 → Acrylic。
    pub fn default_rules() -> Vec<Rule> {
        vec![
            Rule {
                name: "fullscreen-clear".into(),
                condition: Condition::Fullscreen,
                material: Material::Clear,
            },
            Rule {
                name: "power-saver-clear".into(),
                condition: Condition::PowerSaver,
                material: Material::Clear,
            },
            Rule {
                name: "locked-native".into(),
                condition: Condition::Locked,
                material: Material::Native,
            },
            Rule {
                name: "default-acrylic".into(),
                condition: Condition::Always,
                material: Material::Acrylic,
            },
        ]
    }

    /// 无匹配规则时的兜底（= 不干预）
    pub fn evaluate(&self, env: &EnvState) -> Material {
        self.rules
            .iter()
            .find(|rule| env.matches(&rule.condition))
            .map(|rule| rule.material)
            .unwrap_or(Material::Native)
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }
}

impl Default for RuleEngine {
    fn default() -> Self {
        Self::new(Self::default_rules())
    }
}

/// 解析着色 `#RRGGBB` / `#AARRGGBB`（A 在最高位）为 ACCENT_POLICY 的
/// `0xAABBGGRR` 布局。
pub fn parse_tint_abgr(input: &str) -> Option<u32> {
    let hex = input.strip_prefix('#')?;
    if hex.len() != 6 && hex.len() != 8 {
        return None;
    }
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).ok();
    let (a, r, g, b) = match hex.len() {
        6 => (0xFF, byte(0..2)?, byte(2..4)?, byte(4..6)?),
        // AARRGGBB（与 Win32 惯例一致：alpha 在最高字节）
        _ => (byte(0..2)?, byte(2..4)?, byte(4..6)?, byte(6..8)?),
    };
    Some((a as u32) << 24 | (b as u32) << 16 | (g as u32) << 8 | r as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> EnvState {
        EnvState {
            foreground_process: String::new(),
            fullscreen: false,
            power_saver: false,
            locked: false,
        }
    }

    #[test]
    fn default_rules_pick_acrylic_at_idle() {
        let engine = RuleEngine::default();
        assert_eq!(engine.evaluate(&env()), Material::Acrylic);
    }

    #[test]
    fn fullscreen_wins_over_default() {
        let engine = RuleEngine::default();
        let mut state = env();
        state.fullscreen = true;
        state.power_saver = true; // 同时成立时取更靠前的全屏规则
        assert_eq!(engine.evaluate(&state), Material::Clear);
    }

    #[test]
    fn power_saver_degrades_acrylic() {
        let engine = RuleEngine::default();
        let mut state = env();
        state.power_saver = true;
        assert_eq!(engine.evaluate(&state), Material::Clear);
    }

    #[test]
    fn locked_goes_native() {
        let engine = RuleEngine::default();
        let mut state = env();
        state.locked = true;
        assert_eq!(engine.evaluate(&state), Material::Native);
    }

    #[test]
    fn foreground_process_rule_matches_basename_case_insensitive() {
        let rules = vec![Rule {
            name: "game".into(),
            condition: Condition::ForegroundProcess {
                pattern: "Cyberpunk2077".into(),
            },
            material: Material::Clear,
        }];
        let engine = RuleEngine::new(rules);
        let mut state = env();
        state.foreground_process = "C:\\Games\\Cyberpunk 2077\\cyberpunk2077.exe".into();
        assert_eq!(engine.evaluate(&state), Material::Clear);
        state.foreground_process = " explorer.EXE ".into();
        assert_eq!(
            engine.evaluate(&state),
            Material::Native,
            "no match falls back to Native"
        );
    }

    #[test]
    fn empty_engine_falls_back_to_native() {
        let engine = RuleEngine::new(Vec::new());
        assert_eq!(engine.evaluate(&env()), Material::Native);
    }

    #[test]
    fn custom_rules_parse_from_config_json() {
        let json = serde_json::json!([
            { "name": "always-blur", "kind": "always", "material": "blur" }
        ]);
        let rules: Vec<Rule> = serde_json::from_value(json).unwrap();
        let engine = RuleEngine::new(rules);
        assert_eq!(engine.evaluate(&env()), Material::Blur);
    }

    #[test]
    fn tint_parses_rgb_and_argb() {
        assert_eq!(parse_tint_abgr("#000000"), Some(0xFF000000));
        assert_eq!(
            parse_tint_abgr("#F76B1C"),
            Some(0xFF1C6BF7),
            "ABGR byte order"
        );
        assert_eq!(
            parse_tint_abgr("#80F76B1C"),
            Some(0x801C6BF7),
            "AARRGGBB input"
        );
        assert_eq!(parse_tint_abgr("#80F76B1"), None);
        assert_eq!(parse_tint_abgr("000000"), None);
    }
}
