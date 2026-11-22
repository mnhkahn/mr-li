//! Local CPU posture worker. No Feishu credentials or network access needed.
use anyhow::{Context, Result, ensure};
use image::{
    Rgb, RgbImage,
    imageops::{self, FilterType},
};
use ort::{GraphOptimizationLevel, Session, TensorElementType, ValueType};
use serde::Serialize;
use std::{collections::HashMap, path::PathBuf, time::Instant};

#[path = "pose/rtm.rs"]
mod rtm;
#[path = "pose/watermark.rs"]
mod watermark;
struct Inference {
    model: &'static str,
    points: Vec<Point>,
    roi: [u32; 4],
    model_load_ms: f64,
    preprocess_ms: f64,
    inference_ms: f64,
    detector_ms: f64,
    presence: Option<&'static str>,
    person_score: Option<f32>,
    detected_people: Vec<[f32; 5]>,
}
const NAMES: [&str; 17] = [
    "鼻子", "左眼", "右眼", "左耳", "右耳", "左肩", "右肩", "左肘", "右肘", "左腕", "右腕", "左髋",
    "右髋", "左膝", "右膝", "左踝", "右踝",
];
const EDGES: [(usize, usize); 16] = [
    (0, 1),
    (0, 2),
    (1, 3),
    (2, 4),
    (5, 6),
    (5, 7),
    (7, 9),
    (6, 8),
    (8, 10),
    (5, 11),
    (6, 12),
    (11, 12),
    (11, 13),
    (13, 15),
    (12, 14),
    (14, 16),
];
#[derive(Serialize)]
struct Point {
    name: &'static str,
    x: f32,
    y: f32,
    score: f32,
    in_roi: bool,
}
#[derive(Serialize)]
struct Report {
    model: &'static str,
    image_size: [u32; 2],
    roi: [u32; 4],
    threshold: f32,
    model_load_ms: f64,
    preprocess_ms: f64,
    inference_ms: f64,
    repeats: usize,
    detector_ms: f64,
    person_score: Option<f32>,
    detected_people: Vec<[f32; 5]>,
    measurement_method: &'static str,
    keypoints: Vec<Point>,
    head_roll_deg: Option<f32>,
    shoulder_roll_deg: Option<f32>,
    head_clearance_ratio: Option<f32>,
    head_tilt_relative_shoulders_deg: Option<f32>,
    assessment: &'static str,
    note: String,
}
fn dot(img: &mut RgbImage, x: i32, y: i32, r: i32, color: Rgb<u8>) {
    for dy in -r..=r {
        for dx in -r..=r {
            let (px, py) = (x + dx, y + dy);
            if dx * dx + dy * dy <= r * r
                && px >= 0
                && py >= 0
                && px < img.width() as i32
                && py < img.height() as i32
            {
                img.put_pixel(px as u32, py as u32, color);
            }
        }
    }
}
fn line(img: &mut RgbImage, a: (f32, f32), b: (f32, f32), color: Rgb<u8>) {
    let n = ((a.0 - b.0).abs().max((a.1 - b.1).abs()).ceil() as usize).max(1);
    for i in 0..=n {
        let t = i as f32 / n as f32;
        dot(
            img,
            (a.0 + (b.0 - a.0) * t).round() as i32,
            (a.1 + (b.1 - a.1) * t).round() as i32,
            1,
            color,
        );
    }
}
fn roi(value: Option<&String>, w: u32, h: u32) -> Result<[u32; 4]> {
    let Some(value) = value else {
        return Ok([0, 0, w, h]);
    };
    let a = value
        .split(',')
        .map(str::parse::<f32>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        a.len() == 4
            && a.iter().all(|x| x.is_finite())
            && a[0] >= 0.
            && a[1] >= 0.
            && a[2] > 0.
            && a[3] > 0.
            && a[0] + a[2] <= 1.00001
            && a[1] + a[3] <= 1.00001,
        "ROI 格式 x,y,width,height，均为 0–1 归一化比例且不能超出图片"
    );
    let x = (a[0] * w as f32).floor() as u32;
    let y = (a[1] * h as f32).floor() as u32;
    Ok([
        x,
        y,
        ((a[2] * w as f32).round() as u32).max(1).min(w - x),
        ((a[3] * h as f32).round() as u32).max(1).min(h - y),
    ])
}
fn angle(points: &[Point], a: usize, b: usize) -> Option<f32> {
    let (a, b) = (&points[a], &points[b]);
    if a.score < 0.5 || b.score < 0.5 || !a.in_roi || !b.in_roi || (a.x - b.x).abs() < 4. {
        return None;
    }
    let degrees = (b.y - a.y).atan2(b.x - a.x).to_degrees();
    Some((degrees + 90.).rem_euclid(180.) - 90.)
}
// Screen-space heuristic for this fixed rear camera, not a clinical neck angle.
// Ears replace invisible face landmarks; hips are deliberately excluded (chair occlusion).
fn assess_back_view(
    points: &[Point],
    roi_width: u32,
) -> (Option<f32>, Option<f32>, &'static str, String) {
    let ids = [3, 4, 5, 6];
    if ids
        .iter()
        .any(|&i| !points[i].in_roi || points[i].score < 0.5)
    {
        return (
            None,
            None,
            "unknown",
            "无法判断：双耳或双肩关键点不足；不能据此当作坐姿正常。".into(),
        );
    }
    let (a, b) = if points[5].x < points[6].x {
        (&points[5], &points[6])
    } else {
        (&points[6], &points[5])
    };
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let width = dx.hypot(dy);
    let ear_span = (points[3].x - points[4].x).abs();
    if dx < (roi_width as f32 * 0.12).max(12.)
        || ear_span < width * 0.25
        || ear_span > width * 0.9
        || dy.abs() / dx > 0.6
    {
        return (
            None,
            None,
            "unknown",
            "无法判断：头肩几何关系不适合背面规则，可能转头、遮挡或关键点错位。".into(),
        );
    }
    let hx = (points[3].x + points[4].x) / 2.;
    let hy = (points[3].y + points[4].y) / 2.;
    let sx = (a.x + b.x) / 2.;
    let sy = (a.y + b.y) / 2.;
    // Distance along the shoulder line's upward normal, divided by shoulder width.
    let clearance = ((hx - sx) * dy - (hy - sy) * dx) / (width * width);
    let offset = ((hx - sx) * dx + (hy - sy) * dy) / (width * width);
    if !(-0.2..=1.5).contains(&clearance) || offset.abs() > 0.55 {
        return (
            None,
            None,
            "unknown",
            "无法判断：头部偏离双肩区域，可能转身或检测到不同人体。".into(),
        );
    }
    let tilt = angle(points, 3, 4)
        .zip(angle(points, 5, 6))
        .map(|(h, s)| (h - s + 90.).rem_euclid(180.) - 90.);
    // Fixed rear camera: user-labelled low-head ~0.31, upright ~0.54.
    // A wide uncertain band prevents claiming borderline frames are upright.
    let (assessment, reason) = if clearance < 0.42 {
        ("needs_reminder", "疑似低头过多，请抬头、坐正")
    } else if tilt.is_some_and(|t| t.abs() >= 18.) {
        ("needs_reminder", "疑似歪头，请把头摆正")
    } else if clearance >= 0.46 {
        (
            "no_obvious_issue",
            "本次未触发头低或歪头规则，不等于确认坐姿合格",
        )
    } else {
        ("observe", "头肩指标已识别，处于临界待观察区间，暂不提醒")
    };
    (
        Some(clearance),
        tilt,
        assessment,
        format!(
            "{reason}。头肩高度比 {clearance:.2}；试验阈值 <0.42 提醒，0.42–0.46 待确认。固定背面机位规则，已用一张低头和一张坐直样本校准，准确率仍需验证。"
        ),
    )
}
fn assess_head_neck(p: &[Point]) -> Option<(&'static str, String)> {
    if [5, 6, 17, 18]
        .iter()
        .any(|&i| !p[i].in_roi || p[i].score < 0.60)
    {
        return None;
    }
    let (a, b) = if p[5].x < p[6].x {
        (&p[5], &p[6])
    } else {
        (&p[6], &p[5])
    };
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let width = dx.hypot(dy);
    if width < 20. || dx < 12. || dy.abs() / dx > 0.6 {
        return None;
    }
    let sx = (a.x + b.x) / 2.;
    let sy = (a.y + b.y) / 2.;
    let head = &p[17];
    let neck = &p[18];
    let neck_height = ((neck.x - sx) * dy - (neck.y - sy) * dx) / (width * width);
    let neck_offset = ((neck.x - sx) * dx + (neck.y - sy) * dy) / (width * width);
    let height = ((head.x - sx) * dy - (head.y - sy) * dx) / (width * width);
    let hx = head.x - neck.x;
    let hy = head.y - neck.y;
    let length = hx.hypot(hy) / width;
    if !(-0.15..=0.45).contains(&neck_height)
        || neck_offset.abs() > 0.45
        || !(0.35..=1.6).contains(&length)
        || !(0.2..=1.8).contains(&height)
    {
        return None;
    }
    let tilt = (dy * hy + dx * hx).atan2(dy * hx - dx * hy).to_degrees();
    let (result, reason) = if height < 0.82 {
        ("needs_reminder", "疑似低头过多，请抬头坐正")
    } else if tilt.abs() >= 18. {
        ("needs_reminder", "疑似歪头，请把头摆正")
    } else if height >= 0.95 {
        ("no_obvious_issue", "本次未触发提醒")
    } else {
        ("observe", "头颈指标已识别，位于临界待观察区间")
    };
    Some((
        result,
        format!(
            "{reason}。双耳点不稳定，改用头顶/颈部与双肩测量；头顶高度比 {height:.2}，相对倾斜 {tilt:.1}°。固定机位二维规则，不是实际颈部角度。"
        ),
    ))
}
fn infer_movenet(
    model_path: &str,
    image: &RgbImage,
    [x, y, w, h]: [u32; 4],
    repeats: usize,
) -> Result<Inference> {
    let start = Instant::now();
    let model = Session::builder()?
        .with_intra_threads(1)?
        .with_inter_threads(1)?
        .with_parallel_execution(false)?
        .with_optimization_level(GraphOptimizationLevel::Level1)?
        .commit_from_file(model_path)
        .context("无法加载 MoveNet ONNX 模型")?;
    let size = match &model.inputs[0].input_type {
        ValueType::Tensor {
            ty: TensorElementType::Int32,
            dimensions,
        } if dimensions == &[1, 192, 192, 3] => 192u32,
        ValueType::Tensor {
            ty: TensorElementType::Int32,
            dimensions,
        } if dimensions == &[1, 256, 256, 3] => 256u32,
        other => anyhow::bail!("需要 MoveNet Int32 RGB 输入，实际为 {other:?}"),
    };
    let model_load_ms = start.elapsed().as_secs_f64() * 1000.;
    let start = Instant::now();
    let scale = size as f32 / w.max(h) as f32;
    let (rw, rh) = (
        ((w as f32 * scale).round() as u32).max(1),
        ((h as f32 * scale).round() as u32).max(1),
    );
    let (px, py) = ((size - rw) / 2, (size - rh) / 2);
    let crop = imageops::crop_imm(image, x, y, w, h).to_image();
    let resized = imageops::resize(&crop, rw, rh, FilterType::Triangle);
    let mut input = RgbImage::new(size, size);
    imageops::replace(&mut input, &resized, px as i64, py as i64);
    let tensor = ort::Tensor::from_array((
        [1usize, size as usize, size as usize, 3],
        input
            .as_raw()
            .iter()
            .map(|x| *x as i32)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    ))?;
    let preprocess_ms = start.elapsed().as_secs_f64() * 1000.;
    let mut times = Vec::new();
    let mut output = None;
    for _ in 0..repeats {
        let start = Instant::now();
        let r = model.run([ort::SessionInputValue::from(tensor.view())])?;
        times.push(start.elapsed().as_secs_f64() * 1000.);
        output = Some(r);
    }
    times.sort_by(f64::total_cmp);
    let output = output.unwrap();
    let (shape, values) = output[0].try_extract_raw_tensor::<f32>()?;
    ensure!(
        shape == [1, 1, 17, 3] && values.iter().all(|x| x.is_finite()),
        "模型输出应为 [1,1,17,3] 且不能包含非有限值"
    );
    let points: Vec<_> = values
        .as_chunks::<3>()
        .0
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let fx = (p[1] * size as f32 - px as f32) / (rw as f32 / w as f32) + x as f32;
            let fy = (p[0] * size as f32 - py as f32) / (rh as f32 / h as f32) + y as f32;
            Point {
                name: NAMES[i],
                x: fx,
                y: fy,
                score: p[2],
                in_roi: fx >= x as f32
                    && fy >= y as f32
                    && fx < (x + w) as f32
                    && fy < (y + h) as f32,
            }
        })
        .collect();
    Ok(Inference {
        model: if size == 256 {
            "MoveNet SinglePose Thunder ONNX"
        } else {
            "MoveNet SinglePose Lightning ONNX"
        },
        points,
        roi: [x, y, w, h],
        model_load_ms,
        preprocess_ms,
        inference_ms: times[times.len() / 2],
        detector_ms: 0.,
        presence: None,
        person_score: None,
        detected_people: Vec::new(),
    })
}
fn main() -> Result<()> {
    let mut args = HashMap::new();
    let mut it = std::env::args().skip(1).peekable();
    let detect_only = it.next_if(|arg| arg == "--detect-only").is_some();
    while let Some(k) = it.next() {
        ensure!(
            [
                "--model",
                "--image",
                "--output",
                "--roi",
                "--repeat",
                "--runtime",
                "--detector"
            ]
            .contains(&k.as_str()),
            "用法：mr-li-pose --model 模型.onnx --image 照片.jpg --output 输出前缀 [--roi x,y,w,h] [--repeat 5]"
        );
        args.insert(k, it.next().context("参数缺少值")?);
    }
    let image_path = args.get("--image").context("缺少 --image")?;
    let repeats: usize = args
        .get("--repeat")
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(1);
    ensure!((1..=100).contains(&repeats), "重复次数支持 1–100");
    let image = image::ImageReader::open(image_path)?
        .with_guessed_format()?
        .decode()?
        .to_rgb8();
    let [x, y, w, h] = roi(args.get("--roi"), image.width(), image.height())?;
    let runtime = args.get("--runtime").map(PathBuf::from).unwrap_or(
        std::env::current_exe()?
            .parent()
            .context("程序目录缺失")?
            .join("lib/libonnxruntime.1.18.0.dylib"),
    );
    ensure!(
        runtime.is_file(),
        "缺少 ONNX Runtime：{}，请运行 scripts/setup-pose.sh",
        runtime.display()
    );
    ort::init_from(runtime.to_string_lossy())
        .with_name("mr-li-pose")
        .with_telemetry(false)
        .commit()?;
    if detect_only {
        let detector = args.get("--detector").context("缺少 --detector")?;
        println!(
            "{}",
            rtm::locate_people(std::path::Path::new(detector), &image)?
        );
        return Ok(());
    }
    let model_path = args.get("--model").context("缺少 --model")?;
    let prefix = PathBuf::from(args.get("--output").context("缺少 --output")?);
    let inference = if let Some(detector) = args.get("--detector") {
        rtm::infer(
            model_path,
            detector,
            &image,
            args.contains_key("--roi").then_some([x, y, w, h]),
            repeats,
        )?
    } else {
        infer_movenet(model_path, &image, [x, y, w, h], repeats)?
    };
    let Inference {
        model,
        points,
        roi: [x, y, w, h],
        model_load_ms,
        preprocess_ms,
        inference_ms,
        detector_ms,
        presence,
        person_score,
        detected_people,
    } = inference;
    let head = angle(&points, 1, 2).or_else(|| angle(&points, 3, 4));
    let shoulder = angle(&points, 5, 6);
    let high = points.iter().filter(|p| p.score >= 0.5 && p.in_roi).count();
    let (clearance, relative_tilt, mut assessment, mut explanation) = assess_back_view(&points, w);
    let mut measurement = "ears";
    if presence == Some("away") {
        assessment = "away";
        measurement = "none";
        explanation =
            "未检测到学习位内可判断的半身坐姿目标（可能离座、站立或处于画面边缘），本次不评坐姿。"
                .into();
    } else if presence == Some("ambiguous") {
        assessment = "unknown";
        measurement = "none";
        explanation = "检测到多个候选人，无法确认应检查谁，暂不判断。".into();
    } else if points.len() >= 19
        && clearance.is_none()
        && let Some((a, n)) = assess_head_neck(&points)
    {
        assessment = a;
        explanation = n;
        measurement = "head_neck";
    }
    let count = points.len();
    let report = Report {
        model,
        image_size: [image.width(), image.height()],
        roi: [x, y, w, h],
        threshold: 0.5,
        model_load_ms,
        preprocess_ms,
        inference_ms,
        repeats,
        detector_ms,
        person_score,
        detected_people,
        measurement_method: measurement,
        keypoints: points,
        head_roll_deg: head,
        shoulder_roll_deg: shoulder,
        head_clearance_ratio: clearance,
        head_tilt_relative_shoulders_deg: relative_tilt,
        assessment,
        note: format!("{explanation} 关键点：{high}/{count} 个点分数 ≥ 0.5。"),
    };
    if let Some(parent) = prefix.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut painted = image.clone();
    let green = Rgb([32, 230, 110]);
    let amber = Rgb([255, 184, 40]);
    let cyan = Rgb([40, 205, 255]);
    for (a, b) in EDGES {
        if a > 10 || b > 10 {
            continue;
        }
        let (a, b) = (&report.keypoints[a], &report.keypoints[b]);
        if a.score >= 0.5 && b.score >= 0.5 && a.in_roi && b.in_roi {
            line(&mut painted, (a.x, a.y), (b.x, b.y), green);
        }
    }
    for (i, p) in report.keypoints.iter().enumerate() {
        if i > 10 && i != 17 && i != 18 {
            continue;
        }
        if p.in_roi && p.score >= 0.35 {
            dot(
                &mut painted,
                p.x.round() as i32,
                p.y.round() as i32,
                5,
                if p.score >= 0.5 { green } else { amber },
            );
        }
    }
    if report.keypoints.len() >= 19 {
        for (a, b) in [(17, 18), (18, 5), (18, 6)] {
            let (a, b) = (&report.keypoints[a], &report.keypoints[b]);
            if a.score >= 0.5 && b.score >= 0.5 && a.in_roi && b.in_roi {
                line(&mut painted, (a.x, a.y), (b.x, b.y), cyan);
            }
        }
    }
    if report.head_clearance_ratio.is_some() {
        let p = &report.keypoints;
        line(
            &mut painted,
            ((p[3].x + p[4].x) / 2., (p[3].y + p[4].y) / 2.),
            ((p[5].x + p[6].x) / 2., (p[5].y + p[6].y) / 2.),
            if assessment == "needs_reminder" {
                Rgb([255, 70, 70])
            } else {
                cyan
            },
        );
    }
    let a = (x as f32, y as f32);
    let b = ((x + w - 1) as f32, (y + h - 1) as f32);
    for (s, e) in [
        (a, (b.0, a.1)),
        ((b.0, a.1), b),
        (b, (a.0, b.1)),
        ((a.0, b.1), a),
    ] {
        line(&mut painted, s, e, cyan);
    }
    watermark::paint(&mut painted, &report)?;
    painted.save(prefix.with_extension("png"))?;
    let json = serde_json::to_string_pretty(&report)?;
    std::fs::write(prefix.with_extension("json"), &json)?;
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD
        .encode(std::fs::read(prefix.with_extension("png"))?);
    let rows = report
        .keypoints
        .iter()
        .enumerate()
        .map(|(i, p)| {
            format!(
                "<tr><td>{i}</td><td>{}</td><td>{:.3}</td><td>{:.1}, {:.1}</td><td>{}</td></tr>",
                p.name,
                p.score,
                p.x,
                p.y,
                if p.in_roi && p.score >= 0.5 {
                    "高分，仍需人工核对"
                } else {
                    "不足，不参与判断"
                }
            )
        })
        .collect::<String>();
    std::fs::write(
        prefix.with_extension("html"),
        format!(
            "<!doctype html><meta charset=utf-8><title>李老师 · 坐姿模型试验</title><style>body{{font:16px system-ui;max-width:1000px;margin:24px auto;padding:0 16px}}img{{width:100%}}table{{border-collapse:collapse;width:100%}}td,th{{padding:8px;border-bottom:1px solid #ddd;text-align:left}}</style><h1>本地姿态模型试验</h1><p>{}</p><p>绿色：分数 ≥ 0.5；橙色：0.35–0.5；低于 0.35 不绘制。分数不等于准确率，遮挡位置也可能被模型猜测。</p><p>加载优化 {:.0} ms；预处理 {:.1} ms；推理中位数 {:.1} ms（{} 次）。</p><img src=\"data:image/png;base64,{encoded}\"><table><tr><th>编号</th><th>位置</th><th>模型分数</th><th>原图坐标</th><th>状态</th></tr>{rows}</table>",
            report.note, report.model_load_ms, report.preprocess_ms, report.inference_ms, repeats
        ),
    )?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_or_untrusted_landmarks_do_not_produce_angles() {
        let make = |x, y, score, in_roi| Point {
            name: "test",
            x,
            y,
            score,
            in_roi,
        };
        assert_eq!(
            angle(
                &[make(100., 20., 0.9, true), make(20., 20., 0.9, true)],
                0,
                1
            ),
            Some(0.)
        );
        assert!(
            angle(
                &[make(100., 20., 0.49, true), make(20., 20., 0.9, true)],
                0,
                1
            )
            .is_none()
        );
        assert!(
            angle(
                &[make(100., 20., 0.9, false), make(20., 20., 0.9, true)],
                0,
                1
            )
            .is_none()
        );
        assert!(
            angle(
                &[make(20., 20., 0.9, true), make(21., 20., 0.9, true)],
                0,
                1
            )
            .is_none()
        );
    }
    #[test]
    fn rear_rules_require_visible_head_shoulders_and_preserve_uncertainty() {
        let make = |height: f32| {
            let mut points: Vec<_> = NAMES
                .iter()
                .map(|&name| Point {
                    name,
                    x: 0.,
                    y: 0.,
                    score: 0.,
                    in_roi: true,
                })
                .collect();
            for (i, x, y) in [
                (3, 125., 200. - height),
                (4, 175., 200. - height),
                (5, 100., 200.),
                (6, 200., 200.),
            ] {
                points[i].x = x;
                points[i].y = y;
                points[i].score = 0.8;
            }
            points
        };
        assert_eq!(assess_back_view(&make(30.), 300).2, "needs_reminder");
        assert_eq!(assess_back_view(&make(44.), 300).2, "observe");
        assert_eq!(assess_back_view(&make(70.), 300).2, "no_obvious_issue");
        // User-labelled examples (ratios from original, unannotated camera frames).
        assert_eq!(assess_back_view(&make(30.8174), 300).2, "needs_reminder");
        assert_eq!(assess_back_view(&make(54.), 300).2, "no_obvious_issue");
        // Parent-labelled 18:42 must remind; 18:48 must not remind (RTMPose-M readings).
        assert_eq!(assess_back_view(&make(40.897_49), 300).2, "needs_reminder");
        assert_eq!(
            assess_back_view(&make(47.206584), 300).2,
            "no_obvious_issue"
        );
        assert_eq!(assess_back_view(&make(50.), 300).2, "no_obvious_issue");
        let mut points = make(30.);
        points[5].score = 0.49;
        assert_eq!(assess_back_view(&points, 300).2, "unknown");
        points = make(30.);
        points[4].x = points[3].x + 1.;
        assert_eq!(assess_back_view(&points, 300).2, "unknown");
        points = make(30.);
        points[3].in_roi = false;
        assert_eq!(assess_back_view(&points, 300).2, "unknown");
        points = make(70.);
        points[4].y += 20.;
        points[3].y -= 20.;
        assert_eq!(assess_back_view(&points, 300).2, "needs_reminder");
        // Translation and scale do not change the fixed-camera head/shoulder ratio.
        let mut scaled = make(30.);
        for p in &mut scaled {
            p.x = p.x * 2. + 25.;
            p.y = p.y * 2. + 50.;
        }
        assert_eq!(assess_back_view(&scaled, 600).2, "needs_reminder");
    }
    #[test]
    fn head_neck_fallback_requires_confident_points_and_valid_collar_geometry() {
        let make = |height: f32| {
            let mut p: Vec<_> = (0..26)
                .map(|_| Point {
                    name: "test",
                    x: 0.,
                    y: 0.,
                    score: 0.,
                    in_roi: true,
                })
                .collect();
            for (i, x, y) in [
                (5, 100., 200.),
                (6, 200., 200.),
                (17, 150., 200. - height),
                (18, 150., 180.),
            ] {
                p[i].x = x;
                p[i].y = y;
                p[i].score = 0.8;
            }
            p
        };
        assert_eq!(assess_head_neck(&make(100.)).unwrap().0, "no_obvious_issue");
        assert_eq!(assess_head_neck(&make(75.)).unwrap().0, "needs_reminder");
        assert_eq!(assess_head_neck(&make(90.)).unwrap().0, "observe");
        let mut p = make(100.);
        p[18].score = 0.59;
        assert!(assess_head_neck(&p).is_none());
        p = make(100.);
        p[18].x = 20.;
        assert!(assess_head_neck(&p).is_none());
        p = make(100.);
        p[17].in_roi = false;
        assert!(assess_head_neck(&p).is_none());
    }
    #[test]
    fn roi_rejects_invalid_coordinates() {
        for bad in ["nan,0,1,1", "0,0,0,1", "0.8,0,0.5,1", "0,1,1,0.1"] {
            assert!(roi(Some(&bad.into()), 864, 480).is_err());
        }
        assert_eq!(
            roi(Some(&"0.1,0.5,0.4,0.5".into()), 1000, 500).unwrap(),
            [100, 250, 400, 250]
        );
    }
}
