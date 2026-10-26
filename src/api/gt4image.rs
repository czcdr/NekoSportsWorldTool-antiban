//! GT4 缺口识别图像管线。
//!
//! 灰度 → 高斯模糊(5x5, σ=1.1) → Canny(100,200) → 零均值归一化互相关模板匹配，
//! 返回最佳匹配 x。

/// PNG 解码 → 灰度（RGBA 合成到黑底）。
pub fn decode_gray(png: &[u8]) -> Result<(Vec<u8>, usize, usize), String> {
    let img = image::load_from_memory(png).map_err(|e| format!("PNG 解码失败: {e}"))?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut gray = vec![0u8; (w * h) as usize];
    for (i, px) in rgba.pixels().enumerate() {
        let [r, g, b, a] = px.0;
        // alpha 合成到黑底
        let (r, g, b) = (
            r as u32 * a as u32 / 255,
            g as u32 * a as u32 / 255,
            b as u32 * a as u32 / 255,
        );
        gray[i] = ((r * 299 + g * 587 + b * 114) / 1000) as u8;
    }
    Ok((gray, w as usize, h as usize))
}

/// 可分离高斯模糊（5x5，σ≈1.1）。
pub fn gaussian_blur(src: &[u8], w: usize, h: usize) -> Vec<f32> {
    // σ = 0.3*((k-1)*0.5-1)+0.8, k=5
    let sigma = 1.1f32;
    let mut kernel = [0f32; 5];
    for (i, k) in kernel.iter_mut().enumerate() {
        let x = i as f32 - 2.0;
        *k = (-x * x / (2.0 * sigma * sigma)).exp();
    }
    let sum: f32 = kernel.iter().sum();
    for k in kernel.iter_mut() {
        *k /= sum;
    }
    let mut tmp = vec![0f32; w * h];
    let mut dst = vec![0f32; w * h];
    // 水平
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0f32;
            for (ki, k) in kernel.iter().enumerate() {
                let xx = (x as i32 + ki as i32 - 2).clamp(0, w as i32 - 1) as usize;
                acc += src[y * w + xx] as f32 * k;
            }
            tmp[y * w + x] = acc;
        }
    }
    // 垂直
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0f32;
            for (ki, k) in kernel.iter().enumerate() {
                let yy = (y as i32 + ki as i32 - 2).clamp(0, h as i32 - 1) as usize;
                acc += tmp[yy * w + x] * k;
            }
            dst[y * w + x] = acc;
        }
    }
    dst
}

/// Canny 边缘检测（低/高阈值 + 滞后连接，输出 0/255）。
pub fn canny(src: &[f32], w: usize, h: usize, low: f32, high: f32) -> Vec<u8> {
    // Sobel 梯度
    let mut mag = vec![0f32; w * h];
    let mut dir = vec![0f32; w * h];
    let at = |x: i32, y: i32| -> f32 {
        src[(y.clamp(0, h as i32 - 1) as usize) * w + (x.clamp(0, w as i32 - 1) as usize)]
    };
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let gx = -at(x - 1, y - 1) - 2.0 * at(x - 1, y) - at(x - 1, y + 1)
                + at(x + 1, y - 1)
                + 2.0 * at(x + 1, y)
                + at(x + 1, y + 1);
            let gy = -at(x - 1, y - 1) - 2.0 * at(x, y - 1) - at(x + 1, y - 1)
                + at(x - 1, y + 1)
                + 2.0 * at(x, y + 1)
                + at(x + 1, y + 1);
            let i = (y * w as i32 + x) as usize;
            mag[i] = (gx * gx + gy * gy).sqrt();
            dir[i] = gy.atan2(gx);
        }
    }
    // 非最大值抑制
    let mut nms = vec![0f32; w * h];
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            let mut a = dir[i];
            if a < 0.0 {
                a += std::f32::consts::PI;
            }
            let (dx, dy) = if a < std::f32::consts::FRAC_PI_4 {
                (1, 0)
            } else if a < std::f32::consts::FRAC_PI_2 {
                (1, 1)
            } else if a < 3.0 * std::f32::consts::FRAC_PI_4 {
                (0, 1)
            } else {
                (-1, 1)
            };
            let m = mag[i];
            let m1 = mag[((y as i32 + dy) as usize) * w + ((x as i32 + dx) as usize)];
            let m2 = mag[((y as i32 - dy) as usize) * w + ((x as i32 - dx) as usize)];
            if m >= m1 && m >= m2 {
                nms[i] = m;
            }
        }
    }
    // 双阈值 + 滞后连接（BFS）
    let mut out = vec![0u8; w * h];
    let mut stack = Vec::new();
    for i in 0..w * h {
        if nms[i] >= high {
            out[i] = 255;
            stack.push(i);
        }
    }
    while let Some(i) = stack.pop() {
        let x = (i % w) as i32;
        let y = (i / w) as i32;
        for dy in -1..=1 {
            for dx in -1..=1 {
                let nx = x + dx;
                let ny = y + dy;
                if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                    continue;
                }
                let j = (ny * w as i32 + nx) as usize;
                if out[j] == 0 && nms[j] >= low {
                    out[j] = 255;
                    stack.push(j);
                }
            }
        }
    }
    out
}

/// 零均值归一化互相关模板匹配（TM_CCOEFF_NORMED 等价），返回最佳 x。
pub fn match_x(bg: &[u8], bw: usize, bh: usize, tpl: &[u8], tw: usize, th: usize) -> usize {
    match_at(bg, bw, bh, tpl, tw, th).0
}

/// 同 `match_x`，额外返回最佳 y 与得分（探针用来核对 ypos 语义）。
pub fn match_at(
    bg: &[u8],
    bw: usize,
    bh: usize,
    tpl: &[u8],
    tw: usize,
    th: usize,
) -> (usize, usize, f64) {
    if tw == 0 || th == 0 || tw > bw || th > bh {
        return (0, 0, f64::NEG_INFINITY);
    }
    // 模板零均值
    let t_mean = tpl.iter().map(|&v| v as f64).sum::<f64>() / (tw * th) as f64;
    let t: Vec<f64> = tpl.iter().map(|&v| v as f64 - t_mean).collect();
    let t_norm: f64 = t.iter().map(|v| v * v).sum::<f64>().sqrt();
    if t_norm == 0.0 {
        return (0, 0, f64::NEG_INFINITY);
    }
    let mut best_x = 0usize;
    let mut best_y = 0usize;
    let mut best_score = f64::NEG_INFINITY;
    for oy in 0..=(bh - th) {
        for ox in 0..=(bw - tw) {
            let mut s_sum = 0f64;
            let mut st_sum = 0f64;
            for ty in 0..th {
                let row = (oy + ty) * bw + ox;
                for tx in 0..tw {
                    let v = bg[row + tx] as f64;
                    s_sum += v;
                }
            }
            let s_mean = s_sum / (tw * th) as f64;
            let mut num = 0f64;
            for ty in 0..th {
                let row = (oy + ty) * bw + ox;
                let trow = ty * tw;
                for tx in 0..tw {
                    let sv = bg[row + tx] as f64 - s_mean;
                    num += sv * t[trow + tx];
                }
            }
            // 窗口能量（复用 t 归一化的分母需按窗口重算——精确起见直接算）
            for ty in 0..th {
                let row = (oy + ty) * bw + ox;
                for tx in 0..tw {
                    let sv = bg[row + tx] as f64 - s_mean;
                    st_sum += sv * sv;
                }
            }
            let denom = st_sum.sqrt() * t_norm;
            let score = if denom == 0.0 { 0.0 } else { num / denom };
            if score > best_score {
                best_score = score;
                best_x = ox;
                best_y = oy;
            }
        }
    }
    (best_x, best_y, best_score)
}

/// getDistance_original：bg/slice 两张 PNG → 缺口 x。
///
/// 保留作为无 ypos 时的兜底与测试基线；生产路径走 `get_distance_checked`
/// （同一条管线 + ypos 一致性校验）。
#[allow(dead_code)]
pub fn get_distance_original(bg_png: &[u8], slice_png: &[u8]) -> Result<usize, String> {
    let (bg_gray, bw, bh) = decode_gray(bg_png)?;
    let (slice_gray, sw, sh) = decode_gray(slice_png)?;
    let bg_blur = gaussian_blur(&bg_gray, bw, bh);
    let bg_edges = canny(&bg_blur, bw, bh, 100.0, 200.0);
    let slice_blur = gaussian_blur(&slice_gray, sw, sh);
    let slice_edges = canny(&slice_blur, sw, sh, 100.0, 200.0);
    if slice_edges.iter().filter(|&&v| v == 255).count() < 10 {
        return Err("切片图无边缘特征（识别失败）".into());
    }
    Ok(match_x(&bg_edges, bw, bh, &slice_edges, sw, sh))
}

// ───────────────── GT4 生产用识别（ypos 一致性校验）─────────────────
//
// 旧管线（Canny 边缘图全图 NCC）在干净出口上实测 12 轮对 11 轮，唯一一次错配
// 是把模板配到背景里同纹理的别处。试过用 alpha 掩码 + 灰度 NCC、梯度方向一致性
// 重写识别器：在样本图上人工核对反而不如旧管线（缺口左缘 190 时它给 121），
// 而验证码一旦被降级，verify 恒 fail，无法再用来评估，于是不再赌新算法 ——
// 保留旧管线，只加一层**校验**。
//
// 坐标系（这一条搞错就会 100% 误杀，所以写清楚）：
//   · `match_at` 返回的 y 是**整张切片图**（80x80，含透明边距）左上角在背景中的 y；
//   · `/load` 的 `ypos` 实测与这个 y 逐轮吻合 ±2px（8 轮对照，见 review/CHANGES K.1），
//     也就是说 ypos 同样是「整张切片图顶部」，不是「缺口顶部」。
//   两者同源，所以可以直接比较；若将来有人把模板裁到实体再匹配，就必须改成
//   `ypos + 边距`，否则每张图都会偏离一个透明边距的厚度。
pub fn get_distance_checked(
    bg_png: &[u8],
    slice_png: &[u8],
    ypos: Option<usize>,
) -> Result<(usize, f64), String> {
    let (bg_gray, bw, bh) = decode_gray(bg_png)?;
    let (slice_gray, sw, sh) = decode_gray(slice_png)?;
    let bg_edges = canny(&gaussian_blur(&bg_gray, bw, bh), bw, bh, 100.0, 200.0);
    let slice_edges = canny(&gaussian_blur(&slice_gray, sw, sh), sw, sh, 100.0, 200.0);
    if slice_edges.iter().filter(|&&v| v == 255).count() < 10 {
        return Err("切片图无边缘特征（识别失败）".into());
    }
    let (x, y, score) = match_at(&bg_edges, bw, bh, &slice_edges, sw, sh);
    if let Some(yp) = ypos {
        // 8px 余量：实测偏差在 ±2px，留 4 倍余量以吸收不同分辨率下的取整差异。
        if (y as i64 - yp as i64).abs() > 8 {
            return Err(format!(
                "识别不可靠：y={y} 与 ypos={yp} 偏离过大（换图重试）"
            ));
        }
    }
    Ok((x, score))
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 合成图像自测：平滑背景 + 明显亮块 → canny 找得到边缘，模板匹配定位准确。
    #[test]
    fn test_match_x_synthetic() {
        // 100x60 平滑背景（无 u8 环绕）
        let (w, h) = (100usize, 60usize);
        let mut bg = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                bg[y * w + x] = ((x + y) / 2) as u8;
            }
        }
        // 在 x=37 处放一个 20x20 的亮块（模拟缺口）
        for y in 10..30 {
            for x in 37..57 {
                bg[y * w + x] = 255;
            }
        }
        let bg_blur = gaussian_blur(&bg, w, h);
        let bg_edges = canny(&bg_blur, w, h, 50.0, 120.0);
        assert!(
            bg_edges.iter().filter(|&&v| v == 255).count() > 20,
            "边缘太少"
        );
        // 模板 = x=30 起裁 30x30（亮块 + 周边背景，真实切片同构）
        let (tw2, th2) = (30usize, 30usize);
        let mut tpl = vec![0u8; tw2 * th2];
        for y in 0..th2 {
            for x in 0..tw2 {
                tpl[y * tw2 + x] = bg[(5 + y) * w + 30 + x];
            }
        }
        let t_edges = canny(&gaussian_blur(&tpl, tw2, th2), tw2, th2, 50.0, 120.0);
        let found = match_x(&bg_edges, w, h, &t_edges, tw2, th2);
        assert!((found as i32 - 30).abs() <= 2, "匹配 x={found} 期望≈30");
    }

    /// 合成一对 GT4 图：背景 300x200，在 (150, 60) 放一个 50x50 高对比方块；
    /// 切片 80x80，方块居中（透明边距 15px）。返回 (bg_png, slice_png)。
    fn synth_gt4_pair() -> (Vec<u8>, Vec<u8>) {
        use image::{ImageBuffer, Rgba};
        let (bw, bh) = (300u32, 200u32);
        let mut bg = ImageBuffer::from_pixel(bw, bh, Rgba([40u8, 60, 90, 255]));
        for y in 60..110u32 {
            for x in 150..200u32 {
                bg.put_pixel(x, y, Rgba([230, 230, 230, 255]));
            }
        }
        let (sw, sh) = (80u32, 80u32);
        let mut slice = ImageBuffer::from_pixel(sw, sh, Rgba([0u8, 0, 0, 0]));
        for y in 15..65u32 {
            for x in 15..65u32 {
                // 切片内容与背景方块同色，但整体压暗（GT4 缺口被遮罩压暗的近似）
                let v = if x < 16 || y < 16 || x > 63 || y > 63 {
                    230
                } else {
                    150
                };
                slice.put_pixel(x, y, Rgba([v, v, v, 255]));
            }
        }
        let mut bg_png = Vec::new();
        bg.write_to(
            &mut std::io::Cursor::new(&mut bg_png),
            image::ImageFormat::Png,
        )
        .expect("encode bg");
        let mut slice_png = Vec::new();
        slice
            .write_to(
                &mut std::io::Cursor::new(&mut slice_png),
                image::ImageFormat::Png,
            )
            .expect("encode slice");
        (bg_png, slice_png)
    }

    /// ypos 门控的完整契约（正向 + 边界 + 拒绝）。
    ///
    /// 断言不写死"我猜的像素"，而是先用 `match_at` 求出这张合成图上模板的真实
    /// 最优位置，再围绕它做边界断言 —— 否则断言很容易变成恒真式（比如只写
    /// `x <= 300`，而 `match_at` 的 x 上界本来就是 bw-tw=220）。
    #[test]
    fn ypos_gate_follows_the_measured_offset() {
        let (bg_png, slice_png) = synth_gt4_pair();
        let (bg_gray, bw, bh) = decode_gray(&bg_png).expect("decode bg");
        let (slice_gray, sw, sh) = decode_gray(&slice_png).expect("decode slice");
        let bg_edges = canny(&gaussian_blur(&bg_gray, bw, bh), bw, bh, 100.0, 200.0);
        let slice_edges = canny(&gaussian_blur(&slice_gray, sw, sh), sw, sh, 100.0, 200.0);
        let (x0, y0, _) = match_at(&bg_edges, bw, bh, &slice_edges, sw, sh);

        // 合成图：背景方块在 (150..200, 60..110)，切片实体在 (15..65, 15..65)。
        // 整张切片模板的左上角对齐时应在 (135, 45)。
        assert!((x0 as i32 - 135).abs() <= 4, "合成图定位不准: x={x0}");
        assert!((y0 as i32 - 45).abs() <= 4, "合成图定位不准: y={y0}");

        // 缺 ypos：放行。
        assert!(get_distance_checked(&bg_png, &slice_png, None).is_ok());
        // 与识别出的 y 一致：必须放行（这就是 ypos 的真实语义）。
        assert!(
            get_distance_checked(&bg_png, &slice_png, Some(y0)).is_ok(),
            "ypos 与识别一致却被拒绝 —— 校验方向写反了"
        );
        // 8px 边界内仍放行（代码是 > 8 才拒绝）。
        assert!(get_distance_checked(&bg_png, &slice_png, Some(y0.saturating_sub(8))).is_ok());
        // 超出边界：拒绝，并且错误信息要说明原因。
        let err = get_distance_checked(&bg_png, &slice_png, Some(y0 + 9))
            .expect_err("偏离 9px 时必须拒绝");
        assert!(err.contains("偏离"), "错误信息应说明原因，实际: {err}");
    }
}
