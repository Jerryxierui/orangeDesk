//! 音频频谱分析（§13 F4）：FFT + 对数频段聚合 + EMA 平滑。
//!
//! 全部纯逻辑（跨平台可测）。FFT 为就地 radix-2 Cooley-Tukey——
//! 2048 点对壁纸级可视化足够（~20Hz 分辨率 @44.1kHz），零外部依赖（P1）。

pub const ANALYSIS_SAMPLES: usize = 2048; // 2 的幂（FFT 约束）
pub const DEFAULT_BANDS: usize = 16;

/// 实数序列 → 幅度谱（返回前 N/2 个 bin，bin k 对应频率 k·sample_rate/N）
///
/// 输入长度必须是 2 的幂；返回长度 = 输入 / 2。
pub fn fft_magnitude(samples: &[f32]) -> Vec<f32> {
    let n = samples.len();
    // 非 2 次幂输入直接返回空（防御路径，不 debug_assert——测试会喂非法输入）
    if !n.is_power_of_two() || n < 2 {
        return Vec::new();
    }
    // 实部 = 样本，虚部 = 0，交错存放后就地迭代 FFT
    let mut re: Vec<f32> = samples.to_vec();
    let mut im: Vec<f32> = vec![0.0; n];

    // 位反转置换
    let mut j = 0usize;
    for i in 0..n - 1 {
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
        let mut m = n >> 1;
        while m >= 1 && j & m != 0 {
            j ^= m;
            m >>= 1;
        }
        j |= m;
    }

    // 蝶形：len = 2, 4, ..., n
    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let theta = -std::f32::consts::TAU / len as f32;
        // 旋转因子步进（递推乘法精度对本用途足够）
        let (mut wr, mut wi) = (1.0f32, 0.0f32);
        let (dwr, dwi) = (theta.cos(), theta.sin());
        for k in 0..half {
            for base in (0..n).step_by(len) {
                let (i0, i1) = (base + k, base + k + half);
                let tr = re[i1] * wr - im[i1] * wi;
                let ti = re[i1] * wi + im[i1] * wr;
                re[i1] = re[i0] - tr;
                im[i1] = im[i0] - ti;
                re[i0] += tr;
                im[i0] += ti;
            }
            let nwr = wr * dwr - wi * dwi;
            wi = wr * dwi + wi * dwr;
            wr = nwr;
        }
        len <<= 1;
    }

    // 前一半 bin 的幅度
    (0..n / 2)
        .map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt())
        .collect()
}

/// 幅度谱 → 对数分布频段能量（每 band 取区间内最大 bin，全局峰值归一化到 0..1）。
///
/// band 边界按几何级数划分（低频细、高频粗，与人耳感知一致）；
/// 输入为空或 band_count 为 0 返回空。
pub fn spectrum_bands(magnitude: &[f32], band_count: usize) -> Vec<f32> {
    if magnitude.is_empty() || band_count == 0 {
        return Vec::new();
    }
    let bins = magnitude.len();
    let mut bands = vec![0.0f32; band_count];
    for (band, slot) in bands.iter_mut().enumerate() {
        // 几何级数边界：start = round((bins-1) * (band/bands)^2)，视觉均匀的对数分布
        let frac = (band as f32 / band_count as f32).powi(2);
        let frac_next = ((band + 1) as f32 / band_count as f32).powi(2);
        let start = ((bins - 1) as f32 * frac).round() as usize;
        let end = (((bins - 1) as f32 * frac_next).round() as usize)
            .max(start + 1)
            .min(bins);
        *slot = magnitude[start..end].iter().cloned().fold(0.0f32, f32::max);
    }
    // 全局峰值归一化（对可视化比绝对能量稳定）
    let peak = bands.iter().cloned().fold(0.0f32, f32::max);
    if peak > f32::EPSILON {
        for slot in &mut bands {
            *slot /= peak;
        }
    }
    bands
}

/// EMA 平滑（可视化友好：上升快、下降慢）
///
/// `factor` 0..1，越大越跟随原始值；`attack` 用于上升沿（默认同 factor）。
pub fn smooth_spectrum(current: &mut [f32], next: &[f32], release: f32) {
    let attack = 0.6; // 上升沿更快（节拍感）
    for (slot, value) in current.iter_mut().zip(next) {
        let factor = if *value > *slot { attack } else { release };
        *slot += (value - *slot) * factor;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单频正弦的幅度谱：峰值 bin 应落在生成频率处
    #[test]
    fn fft_peaks_at_sine_frequency() {
        let n = 1024usize;
        let target_bin = 37usize;
        let samples: Vec<f32> = (0..n)
            .map(|i| (i as f32 * target_bin as f32 / n as f32 * std::f32::consts::TAU).sin() * 0.8)
            .collect();
        let magnitude = fft_magnitude(&samples);
        assert_eq!(magnitude.len(), n / 2);
        let peak_bin = magnitude
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap();
        assert!(
            (peak_bin as i64 - target_bin as i64).abs() <= 1,
            "peak {peak_bin} should be near {target_bin}"
        );
    }

    #[test]
    fn fft_dc_lands_in_bin_zero_and_silence_is_silent() {
        let dc: Vec<f32> = vec![0.5; 256];
        let magnitude = fft_magnitude(&dc);
        let peak_bin = magnitude
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap();
        assert_eq!(peak_bin, 0, "DC energy belongs in bin 0");

        let silence = vec![0.0f32; 256];
        assert!(fft_magnitude(&silence).iter().all(|m| *m < 1e-6));
    }

    #[test]
    fn bands_put_low_frequency_energy_in_first_bands() {
        // 只在低 bin 有能量 → 前段 band 高、后段接近 0
        let mut magnitude = vec![0.0f32; 512];
        magnitude[1..8].fill(0.9);
        let bands = spectrum_bands(&magnitude, DEFAULT_BANDS);
        assert_eq!(bands.len(), DEFAULT_BANDS);
        assert!(bands[0] > 0.9, "first band carries the low bin energy");
        assert!(bands[DEFAULT_BANDS - 1] < 0.01, "top band stays near zero");
        assert!(
            bands.iter().all(|b| (0.0..=1.0).contains(b)),
            "normalized 0..1"
        );
    }

    #[test]
    fn bands_flat_input_is_flat_output() {
        let magnitude = vec![0.5f32; 256];
        let bands = spectrum_bands(&magnitude, 8);
        assert!(
            bands.iter().all(|b| (b - 1.0).abs() < 1e-6),
            "uniform input normalizes to all-ones"
        );
    }

    #[test]
    fn smoothing_decays_slowly_and_attacks_fast() {
        let mut current = vec![1.0, 0.0];
        smooth_spectrum(&mut current, &[0.0, 1.0], 0.2);
        // band0 下降沿：release 慢（1.0 → 0.8）
        assert!((current[0] - 0.8).abs() < 1e-6);
        // band1 上升沿：attack 快（0.0 → 0.6）
        assert!((current[1] - 0.6).abs() < 1e-6);
    }

    #[test]
    fn degenerate_inputs_are_safe() {
        assert!(fft_magnitude(&[]).is_empty());
        assert!(
            fft_magnitude(&[1.0, 2.0, 3.0]).is_empty(),
            "non-power-of-two rejected"
        );
        assert!(spectrum_bands(&[], 16).is_empty());
        assert!(spectrum_bands(&[0.1; 64], 0).is_empty());
    }
}
