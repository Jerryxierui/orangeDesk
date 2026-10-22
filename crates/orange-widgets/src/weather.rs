//! 天气组件数据层（§3 B-3：天气免费源）。
//!
//! 设计：
//! - 数据源为免费无 Key 的 open-meteo.com（current weather API）
//! - [`WeatherProvider`] trait 与解析逻辑分离：HTTP 拉取由真机阶段的
//!   WinHTTP / WinRT 实现接入（不引入重 HTTP 依赖，P1）；
//!   响应解析与 WMO 天气码映射是纯逻辑，跨平台完整测试
//! - [`StaticWeather`] 为离线 / 测试用静态源（真实数据接入前的诚实占位，
//!   不假装有数据）

use serde::Deserialize;

/// 当前天气快照
#[derive(Debug, Clone, PartialEq)]
pub struct WeatherNow {
    pub temperature_c: f32,
    /// 体感温度
    pub apparent_c: f32,
    pub humidity_percent: u32,
    pub wind_kmh: f32,
    /// WMO 天气码的可读描述（见 [`weather_code_description`]）
    pub condition: String,
    /// 数据源观测时间（本地字符串，原样透传）
    pub observed_at: String,
}

/// WMO weather code → 人可读描述（open-meteo 语义；常用子集，
/// 未知码回退 "unknown"）
pub fn weather_code_description(code: u32) -> &'static str {
    match code {
        0 => "clear sky",
        1 => "mainly clear",
        2 => "partly cloudy",
        3 => "overcast",
        45 | 48 => "fog",
        51 | 53 | 55 => "drizzle",
        56 | 57 => "freezing drizzle",
        61 | 63 | 65 => "rain",
        66 | 67 => "freezing rain",
        71 | 73 | 75 => "snow",
        77 => "snow grains",
        80..=82 => "rain showers",
        85 | 86 => "snow showers",
        95 => "thunderstorm",
        96 | 99 => "thunderstorm with hail",
        _ => "unknown",
    }
}

/// open-meteo current-weather 响应（仅解析需要的字段；多余字段忽略）
#[derive(Debug, Deserialize)]
struct OpenMeteoResponse {
    current: OpenMeteoCurrent,
}

#[derive(Debug, Deserialize)]
struct OpenMeteoCurrent {
    time: String,
    #[serde(default)]
    temperature_2m: f32,
    #[serde(default)]
    apparent_temperature: f32,
    #[serde(default)]
    relative_humidity_2m: u32,
    // Option<u32> 而非 default 0：字段缺失 ≠ 晴天（0 = clear sky），
    // 缺失必须落 "unknown"，不造假日气（P3 诚实降级）
    #[serde(default)]
    weather_code: Option<u32>,
    #[serde(default)]
    wind_speed_10m: f32,
}

/// 解析 open-meteo current-weather JSON 响应（纯逻辑）
pub fn parse_open_meteo(json: &str) -> Result<WeatherNow, String> {
    let response: OpenMeteoResponse =
        serde_json::from_str(json).map_err(|err| format!("open-meteo parse error: {err}"))?;
    Ok(WeatherNow {
        temperature_c: response.current.temperature_2m,
        apparent_c: response.current.apparent_temperature,
        humidity_percent: response.current.relative_humidity_2m,
        wind_kmh: response.current.wind_speed_10m,
        condition: response
            .current
            .weather_code
            .map(weather_code_description)
            .unwrap_or("unknown")
            .to_string(),
        observed_at: response.current.time,
    })
}

/// 天气数据源契约（真机阶段由 WinHTTP / WinRT HTTP 实现接入）
pub trait WeatherProvider: Send {
    fn fetch(&mut self, latitude: f64, longitude: f64) -> Result<WeatherNow, String>;
}

/// 离线静态源（测试 / 无网络环境的诚实占位：返回固定快照）
pub struct StaticWeather {
    now: WeatherNow,
}

impl StaticWeather {
    pub fn new(now: WeatherNow) -> Self {
        Self { now }
    }
}

impl WeatherProvider for StaticWeather {
    fn fetch(&mut self, _latitude: f64, _longitude: f64) -> Result<WeatherNow, String> {
        Ok(self.now.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// open-meteo 文档格式的真实响应样例（截取需要的字段）
    const SAMPLE: &str = r#"{
        "latitude": 31.2,
        "longitude": 121.5,
        "current": {
            "time": "2026-10-22T10:00",
            "temperature_2m": 21.3,
            "relative_humidity_2m": 62,
            "apparent_temperature": 20.8,
            "weather_code": 3,
            "wind_speed_10m": 11.2
        }
    }"#;

    #[test]
    fn parses_open_meteo_sample() {
        let now = parse_open_meteo(SAMPLE).unwrap();
        assert_eq!(now.temperature_c, 21.3);
        assert_eq!(now.apparent_c, 20.8);
        assert_eq!(now.humidity_percent, 62);
        assert_eq!(now.wind_kmh, 11.2);
        assert_eq!(now.condition, "overcast");
        assert_eq!(now.observed_at, "2026-10-22T10:00");
    }

    #[test]
    fn missing_optional_fields_default_to_zero() {
        // 服务端字段缺失（如历史数据）不致命：数值回 0，码回 unknown
        let now = parse_open_meteo(r#"{"current": {"time": "t"}}"#).unwrap();
        assert_eq!(now.temperature_c, 0.0);
        assert_eq!(now.condition, "unknown");
    }

    #[test]
    fn rejects_garbage_json() {
        assert!(parse_open_meteo("not json").is_err());
        assert!(parse_open_meteo(r#"{"no_current": 1}"#).is_err());
    }

    #[test]
    fn wmo_codes_map_to_descriptions() {
        assert_eq!(weather_code_description(0), "clear sky");
        assert_eq!(weather_code_description(61), "rain");
        assert_eq!(weather_code_description(95), "thunderstorm");
        assert_eq!(weather_code_description(12345), "unknown");
    }

    #[test]
    fn static_provider_returns_snapshot() {
        let now = WeatherNow {
            temperature_c: 5.0,
            apparent_c: 2.0,
            humidity_percent: 80,
            wind_kmh: 30.0,
            condition: "snow".to_string(),
            observed_at: "2026-10-22T08:00".to_string(),
        };
        let mut provider = StaticWeather::new(now.clone());
        assert_eq!(provider.fetch(0.0, 0.0).unwrap(), now);
    }
}
