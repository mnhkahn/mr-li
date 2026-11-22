//! CPU-only YOLOX person localization + RTMPose Halpe26.
//! Pre/postprocessing follows each official MMDeploy pipeline.json, not model score thresholds from MoveNet.
use super::{Inference, NAMES, Point};
use anyhow::{Context, Result, ensure};
use image::{
    RgbImage,
    imageops::{self, FilterType},
};
use ort::{GraphOptimizationLevel, Session, TensorElementType, ValueType};
use std::{path::Path, time::Instant};
const EXTRA: [&str; 9] = [
    "头顶",
    "颈部",
    "髋中点",
    "左大脚趾",
    "右大脚趾",
    "左小脚趾",
    "右小脚趾",
    "左脚跟",
    "右脚跟",
];
fn session(path: &Path) -> Result<Session> {
    Session::builder()?
        .with_intra_threads(1)?
        .with_inter_threads(1)?
        .with_parallel_execution(false)?
        .with_optimization_level(GraphOptimizationLevel::Level1)?
        .commit_from_file(path)
        .context("无法加载人体检测/姿态模型")
}
fn empty_points() -> Vec<Point> {
    (0..26)
        .map(|i| Point {
            name: if i < 17 { NAMES[i] } else { EXTRA[i - 17] },
            x: 0.,
            y: 0.,
            score: 0.,
            in_roi: false,
        })
        .collect()
}
fn allowed_box(b: &[f32; 5], image: &RgbImage, gate: Option<[u32; 4]>) -> bool {
    let w = b[2] - b[0];
    let h = b[3] - b[1];
    if w < 24. || h < 48. {
        return false;
    }
    // The current camera shows an upper-body desk view. Do not assess a tall standing person or a tiny person at the bottom edge as seated.
    if b[1] < image.height() as f32 * 0.35 && h > image.height() as f32 * 0.45 && h > w * 1.8 {
        return false;
    }
    if b[1] > image.height() as f32 * 0.78 && h < image.height() as f32 * 0.25 {
        return false;
    }
    if let Some([x, y, w, h]) = gate {
        let cx = (b[0] + b[2]) / 2.;
        let cy = b[1] + (b[3] - b[1]) * 0.35;
        return cx >= x as f32 && cx < (x + w) as f32 && cy >= y as f32 && cy < (y + h) as f32;
    }
    true
}
fn detect(model: &Session, image: &RgbImage) -> Result<(Vec<[f32; 5]>, f64, f64)> {
    let start = Instant::now();
    let size = 416u32;
    let ratio = (size as f32 / image.width() as f32).min(size as f32 / image.height() as f32);
    let w = (image.width() as f32 * ratio) as u32;
    let h = (image.height() as f32 * ratio) as u32;
    let resized = imageops::resize(image, w, h, FilterType::Triangle);
    let mut input = vec![114f32; 3 * size as usize * size as usize];
    for y in 0..h {
        for x in 0..w {
            let p = resized.get_pixel(x, y);
            for c in 0..3 {
                input[(c * size as usize + y as usize) * size as usize + x as usize] =
                    p[2 - c] as f32;
            }
        }
    }
    let tensor = ort::Tensor::from_array(([1usize, 3, 416, 416], input.into_boxed_slice()))?;
    let prep = start.elapsed().as_secs_f64() * 1000.;
    let start = Instant::now();
    let out = model.run([ort::SessionInputValue::from(tensor.view())])?;
    let elapsed = start.elapsed().as_secs_f64() * 1000.;
    ensure!(out.len() == 2, "人体检测输出数量不兼容");
    let (shape, values) = out[0].try_extract_raw_tensor::<f32>()?;
    let (label_shape, labels) = out[1].try_extract_raw_tensor::<i64>()?;
    ensure!(
        shape.len() == 3
            && shape[0] == 1
            && shape[2] == 5
            && label_shape.len() == 2
            && label_shape[1] == shape[1]
            && values.iter().all(|v| v.is_finite()),
        "人体检测输出不兼容"
    );
    let boxes = values
        .as_chunks::<5>()
        .0
        .iter()
        .zip(labels)
        .filter_map(|(p, &label)| {
            if label != 0 || p[4] < 0.60 {
                return None;
            }
            let b = [
                (p[0] / ratio).clamp(0., image.width() as f32),
                (p[1] / ratio).clamp(0., image.height() as f32),
                (p[2] / ratio).clamp(0., image.width() as f32),
                (p[3] / ratio).clamp(0., image.height() as f32),
                p[4],
            ];
            (b[2] > b[0] && b[3] > b[1]).then_some(b)
        })
        .collect();
    Ok((boxes, prep, elapsed))
}
// OpenCV TopDownAffine uses integer pixel centers and a constant zero border.
fn sample(image: &RgbImage, x: f32, y: f32, c: usize) -> f32 {
    let ix = x.floor() as i32;
    let iy = y.floor() as i32;
    let fx = x - x.floor();
    let fy = y - y.floor();
    let mut value = 0.;
    for (dx, wx) in [(0, 1. - fx), (1, fx)] {
        for (dy, wy) in [(0, 1. - fy), (1, fy)] {
            let px = ix + dx;
            let py = iy + dy;
            if px >= 0 && py >= 0 && px < image.width() as i32 && py < image.height() as i32 {
                value += image.get_pixel(px as u32, py as u32)[c] as f32 * wx * wy;
            }
        }
    }
    value.round()
}
fn pose(model: &Session, image: &RgbImage, b: &[f32; 5]) -> Result<(Vec<Point>, f64, f64)> {
    let start = Instant::now();
    let cx = (b[0] + b[2]) / 2.;
    let cy = (b[1] + b[3]) / 2.;
    let mut sw = (b[2] - b[0]) * 1.25;
    let mut sh = (b[3] - b[1]) * 1.25;
    if sw > sh * 0.75 {
        sh = sw / 0.75;
    } else {
        sw = sh * 0.75;
    }
    let mean = [123.675, 116.28, 103.53];
    let std = [58.395, 57.12, 57.375];
    let mut input = vec![0f32; 3 * 256 * 192];
    for y in 0..256 {
        for x in 0..192 {
            let sx = (x as f32 - 96.) * sw / 192. + cx;
            let sy = (y as f32 - 128.) * sh / 256. + cy;
            for c in 0..3 {
                input[(c * 256 + y) * 192 + x] = (sample(image, sx, sy, c) - mean[c]) / std[c];
            }
        }
    }
    let tensor = ort::Tensor::from_array(([1usize, 3, 256, 192], input.into_boxed_slice()))?;
    let prep = start.elapsed().as_secs_f64() * 1000.;
    let start = Instant::now();
    let out = model.run([ort::SessionInputValue::from(tensor.view())])?;
    let elapsed = start.elapsed().as_secs_f64() * 1000.;
    ensure!(out.len() == 2, "RTMPose 输出数量不兼容");
    let (xs, xv) = out[0].try_extract_raw_tensor::<f32>()?;
    let (ys, yv) = out[1].try_extract_raw_tensor::<f32>()?;
    ensure!(
        xs == [1, 26, 384] && ys == [1, 26, 512] && xv.iter().chain(yv).all(|v| v.is_finite()),
        "RTMPose SimCC 输出不兼容"
    );
    let points = xv
        .as_chunks::<384>()
        .0
        .iter()
        .zip(yv.as_chunks::<512>().0.iter())
        .enumerate()
        .map(|(i, (xs, ys))| {
            let (xi, &sx) = xs
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap();
            let (yi, &sy) = ys
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap();
            let x = xi as f32 / 2. / 192. * sw + cx - sw / 2.;
            let y = yi as f32 / 2. / 256. * sh + cy - sh / 2.;
            Point {
                name: if i < 17 { NAMES[i] } else { EXTRA[i - 17] },
                x,
                y,
                score: sx.min(sy),
                in_roi: x >= 0.
                    && y >= 0.
                    && x < image.width() as f32
                    && y < image.height() as f32
                    && x >= cx - sw / 2.
                    && x <= cx + sw / 2.
                    && y >= cy - sh / 2.
                    && y <= cy + sh / 2.,
            }
        })
        .collect();
    Ok((points, prep, elapsed))
}
pub(super) fn infer(
    model_path: &str,
    detector_path: &str,
    image: &RgbImage,
    gate: Option<[u32; 4]>,
    repeats: usize,
) -> Result<Inference> {
    let start = Instant::now();
    let detector = person_detector(Path::new(detector_path))?;
    let model = session(Path::new(model_path))?;
    ensure!(
        matches!(&model.inputs[0].input_type,ValueType::Tensor{ty:TensorElementType::Float32,dimensions} if dimensions.len()==4 && dimensions[1..]==[3,256,192]),
        "需要 RTMPose Halpe26 256×192 模型"
    );
    let load = start.elapsed().as_secs_f64() * 1000.;
    let mut times = Vec::new();
    let mut det_times = Vec::new();
    let mut result = None;
    for _ in 0..repeats {
        let (boxes, mut prep, det_ms) = detect(&detector, image)?;
        let selected: Vec<_> = boxes
            .iter()
            .filter(|b| allowed_box(b, image, gate))
            .collect();
        let mut points = empty_points();
        let mut pose_ms = 0.;
        let mut region = gate.unwrap_or([0, 0, image.width(), image.height()]);
        let mut presence = None;
        let mut score = None;
        if selected.len() == 1 {
            let b = selected[0];
            let (p, t, m) = pose(&model, image, b)?;
            points = p;
            prep += t;
            pose_ms = m;
            score = Some(b[4]);
            let x = b[0].floor() as u32;
            let y = b[1].floor() as u32;
            region = [
                x,
                y,
                ((b[2].ceil() as u32).min(image.width()) - x).max(1),
                ((b[3].ceil() as u32).min(image.height()) - y).max(1),
            ];
        } else if selected.is_empty() {
            presence = Some("away");
        } else {
            presence = Some("ambiguous");
        }
        times.push(det_ms + pose_ms);
        det_times.push(det_ms);
        result = Some(Inference {
            model: "YOLOX Tiny + RTMPose-M Halpe26",
            points,
            roi: region,
            model_load_ms: load,
            preprocess_ms: prep,
            inference_ms: det_ms + pose_ms,
            detector_ms: det_ms,
            presence,
            person_score: score,
            detected_people: boxes,
        });
    }
    times.sort_by(f64::total_cmp);
    det_times.sort_by(f64::total_cmp);
    let mut result = result.unwrap();
    result.inference_ms = times[times.len() / 2];
    result.detector_ms = det_times[det_times.len() / 2];
    Ok(result)
}

fn person_detector(path: &Path) -> Result<Session> {
    let detector = session(path)?;
    ensure!(
        matches!(&detector.inputs[0].input_type,ValueType::Tensor{ty:TensorElementType::Float32,dimensions} if dimensions==&[1,3,416,416]),
        "需要 YOLOX Tiny 416×416 人体检测模型"
    );
    Ok(detector)
}

pub(super) fn locate_people(path: &Path, image: &RgbImage) -> Result<serde_json::Value> {
    let start = Instant::now();
    let detector = person_detector(path)?;
    let model_load_ms = start.elapsed().as_secs_f64() * 1000.;
    let (boxes, preprocess_ms, inference_ms) = detect(&detector, image)?;
    Ok(serde_json::json!({
        "people": boxes,
        "image_size": [image.width(), image.height()],
        "model_load_ms": model_load_ms,
        "preprocess_ms": preprocess_ms,
        "inference_ms": inference_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn candidate_filter_keeps_desk_subject_and_rejects_bedside_and_partial_views() {
        let image = RgbImage::new(864, 480);
        let gate = Some([86, 144, 562, 336]);
        assert!(allowed_box(&[300., 262., 578., 471., 0.8], &image, gate));
        assert!(allowed_box(&[157., 281., 423., 470., 0.8], &image, gate)); // labelled low-head example
        assert!(!allowed_box(&[705., 246., 864., 477., 0.8], &image, gate));
        assert!(!allowed_box(&[672., 103., 773., 368., 0.8], &image, None));
        assert!(!allowed_box(&[587., 394., 675., 480., 0.8], &image, None));
    }
}
