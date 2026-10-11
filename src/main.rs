use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use clap::Parser;
use colored::*;
use exif::Reader;
use lofty::file::TaggedFileExt;
use lofty::prelude::*;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

// ─────────────────────────── CLI ───────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "meta",
    version,
    about = "Просмотр метаданных файлов: ФС, EXIF, IPTC, XMP, аудио, видео"
)]
struct Cli {
    /// Пути к файлам или каталогам
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Рекурсивно обходить каталоги
    #[arg(short, long)]
    recursive: bool,

    /// Показывать все теги без фильтрации (включая служебные)
    #[arg(short, long)]
    all: bool,

    /// JSON-вывод (всегда содержит все поля без потерь)
    #[arg(long)]
    json: bool,

    /// CSV-вывод: path,group,tag,value
    #[arg(long)]
    csv: bool,

    /// Краткий вывод: только непустые поля
    #[arg(short, long)]
    brief: bool,

    /// Только ключевые теги (Make, Model, ISO, ExposureTime и т.п.)
    #[arg(long)]
    short: bool,

    /// Только выбранные группы: fs,type,exif,iptc,xmp,audio,video
    #[arg(short, long, value_delimiter = ',')]
    groups: Option<Vec<String>>,

    /// Фильтр по машинным именам тегов (Make,Model,ISO)
    #[arg(short = 'T', long, value_delimiter = ',')]
    tags: Option<Vec<String>>,

    /// Фильтр по категориям EXIF (Камера,Экспозиция,GPS и т.п.)
    #[arg(long = "exif-category", value_delimiter = ',')]
    exif_categories: Option<Vec<String>>,

    /// Не показывать заголовки и рамки
    #[arg(short = 'q', long)]
    quiet: bool,

    /// Показывать размер в байтах
    #[arg(long)]
    bytes: bool,

    /// Не следовать по символическим ссылкам
    #[arg(long)]
    no_follow: bool,

    /// Максимальная глубина рекурсии
    #[arg(long, default_value_t = usize::MAX)]
    max_depth: usize,

    /// Скрыть пустые значения
    #[arg(long)]
    no_empty: bool,

    /// Отключить цветной вывод (или переменная окружения NO_COLOR)
    #[arg(long)]
    no_color: bool,

    /// Сырые значения: без humanize (единицы, коды, даты)
    #[arg(long)]
    raw: bool,

    /// Писать в файл вместо stdout
    #[arg(short, long)]
    output: Option<PathBuf>,
}

// ─────────────────────────── main ──────────────────────────

type Out = BufWriter<Box<dyn Write>>;

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.no_color || std::env::var_os("NO_COLOR").is_some() {
        colored::control::set_override(false);
    }

    let files = collect_files(&cli)?;

    let mut out: Out = match &cli.output {
        Some(p) => BufWriter::new(Box::new(
            File::create(p).with_context(|| format!("не удалось создать {}", p.display()))?,
        )),
        None => BufWriter::new(Box::new(std::io::stdout())),
    };

    let structured = cli.json || cli.csv;
    let mut results: Vec<serde_json::Value> = Vec::with_capacity(files.len());

    for path in &files {
        match dump_one(path, &cli, &mut out) {
            Ok(value) => results.push(value),
            Err(e) => {
                if !cli.quiet {
                    eprintln!("{} {}: {:#}", "ошибка".red().bold(), path.display(), e);
                }
            }
        }
    }

    if cli.json {
        let s = serde_json::to_string_pretty(&results)?;
        writeln!(out, "{}", s)?;
    } else if cli.csv {
        write_csv(&results, &mut out)?;
    }

    let _ = structured; // silence unused warning when neither json nor csv

    out.flush()?;
    Ok(())
}

// ─────────────────────────── file collection ──────────────

fn collect_files(cli: &Cli) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for p in &cli.paths {
        if p.is_dir() {
            let mut walker = WalkDir::new(p).max_depth(cli.max_depth);
            if cli.no_follow {
                walker = walker.follow_links(false);
            }
            for entry in walker.into_iter().filter_map(|e| e.ok()) {
                if entry.file_type().is_file() {
                    files.push(entry.path().to_path_buf());
                }
            }
        } else {
            files.push(p.clone());
        }
    }
    Ok(files)
}

// ─────────────────────── dump one ─────────────────────────

fn dump_one(path: &Path, cli: &Cli, out: &mut Out) -> Result<serde_json::Value> {
    let meta = fs::metadata(path)
        .with_context(|| format!("не удалось прочитать метаданные {}", path.display()))?;

    let mut obj = serde_json::Map::new();
    obj.insert("path".into(), json!(path.display().to_string()));

    let want = |g: &str| -> bool {
        cli.groups
            .as_ref()
            .map_or(true, |groups| groups.iter().any(|x| x == g))
    };

    // Печатать ли текстовый вывод (в JSON/CSV режиме — нет)
    let text = !cli.json && !cli.csv;

    // ── ФС ──
    if want("fs") {
        let fs_meta = collect_fs_meta(&meta);
        if text && !cli.quiet {
            print_fs(&fs_meta, cli, out)?;
        }
        obj.insert("filesystem".into(), fs_meta_to_json(&fs_meta, cli));
    }

    // ── MIME ──
    if want("type") {
        if let Ok(Some(k)) = infer::get_from_path(path) {
            if text {
                writeln!(out, "{}", "Тип содержимого".bold().yellow())?;
                row(out, "MIME", k.mime_type(), cli)?;
                row(out, "Расширение", k.extension(), cli)?;
                writeln!(out)?;
            }
            obj.insert(
                "mime".into(),
                json!({ "mime": k.mime_type(), "extension": k.extension() }),
            );
        }
    }

    let media = revelo::Metadata::from_file(path.to_str().unwrap_or(""));
    let mut have_exif = false;

    if want("exif") || want("iptc") || want("xmp") {
        if let Some(media) = &media {
            if want("exif") {
                let pairs = collect_pairs(media.exif());
                if !pairs.is_empty() {
                    have_exif = true;
                    emit_exif_grouped(&pairs, cli, out, &mut obj)?;
                }
            }
            if want("iptc") {
                let map = collect_pairs(media.iptc());
                if text {
                    emit_section("IPTC", "iptc", &map, cli, out, &mut obj)?;
                }
            }
            if want("xmp") {
                let map = collect_pairs(media.xmp());
                if text {
                    emit_section("XMP", "xmp", &map, cli, out, &mut obj)?;
                }
            }
        } else if want("exif") {
            have_exif = try_exif_fallback(path, cli, out, &mut obj)?;
        }
    }

    if want("audio") {
        if let Ok(tagged) = lofty::read_from_path(path) {
            let mut audio = serde_json::Map::new();
            let props = tagged.properties();
            let duration_ms = props.duration().as_millis();
            let bitrate = props.audio_bitrate();
            let sample_rate = props.sample_rate();
            let channels = props.channels();

            audio.insert("duration_ms".into(), json!(duration_ms));
            audio.insert("bitrate".into(), json!(bitrate));
            audio.insert("sample_rate".into(), json!(sample_rate));
            audio.insert("channels".into(), json!(channels));

            let mut tags = serde_json::Map::new();
            if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
                for item in tag.items() {
                    let val = item.value().text().unwrap_or("").to_string();
                    tags.insert(format!("{:?}", item.key()), json!(val));
                }
            }
            audio.insert("tags".into(), json!(tags));

            if text && !cli.quiet {
                writeln!(out, "{}", "Аудио".bold().yellow())?;
                row(out, "Длительность, мс", &duration_ms.to_string(), cli)?;
                row(out, "Битрейт", &opt_display(bitrate), cli)?;
                row(out, "Частота, Гц", &opt_display(sample_rate), cli)?;
                row(out, "Каналов", &opt_display(channels), cli)?;
                for (k, v) in &tags {
                    let s = v.as_str().unwrap_or("").to_string();
                    row(out, k, &s, cli)?;
                }
                writeln!(out)?;
            }
            obj.insert("audio".into(), json!(audio));
        }
    }

    if want("video") {
        if let Some(media) = &media {
            let general = collect_pairs(media.general());
            let video = collect_pairs(media.video());
            let audio = collect_pairs(media.audio());

            if text {
                if !general.is_empty() {
                    emit_section(
                        "Медиа: общее",
                        "media_general",
                        &general,
                        cli,
                        out,
                        &mut obj,
                    )?;
                }
                if !video.is_empty() {
                    emit_section("Видео", "media_video", &video, cli, out, &mut obj)?;
                }
                if !audio.is_empty() {
                    emit_section("Аудио (поток)", "media_audio", &audio, cli, out, &mut obj)?;
                }
            } else {
                if !general.is_empty() {
                    obj.insert("media_general".into(), json!(general));
                }
                if !video.is_empty() {
                    obj.insert("media_video".into(), json!(video));
                }
                if !audio.is_empty() {
                    obj.insert("media_audio".into(), json!(audio));
                }
            }
        }
    }

    if want("exif") && !have_exif {
        try_nom_exif(path, cli, out, &mut obj, text)?;
    }

    Ok(serde_json::Value::Object(obj))
}

// ─────────────────── helpers для пар ─────────────────────

fn collect_pairs<'a, I>(iter: I) -> Vec<(String, String)>
where
    I: Iterator<Item = (&'a str, &'a str)>,
{
    iter.filter_map(|(k, v)| {
        if k.is_empty() && v.is_empty() {
            None
        } else {
            Some((k.to_string(), v.to_string()))
        }
    })
    .collect()
}

fn emit_section(
    title: &str,
    key: &str,
    map: &[(String, String)],
    cli: &Cli,
    out: &mut Out,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    if map.is_empty() {
        return Ok(());
    }
    if !cli.quiet {
        writeln!(out, "{}", title.bold().yellow())?;
        for (k, v) in map {
            row(out, k, v, cli)?;
        }
        writeln!(out)?;
    }
    obj.insert(key.into(), json!(map));
    Ok(())
}

// ─────────────────── EXIF: фильтры ───────────────────────

const EXIF_SERVICE_FIELDS: &[&str] = &[
    "ExifOffset",
    "GPSInfo",
    "InteropOffset",
    "ThumbnailOffset",
    "ThumbnailLength",
    "MakerNote",
    "InteroperabilityVersion",
    "Compression",
    "ComponentsConfiguration",
];

const EXIF_ALIASES: &[(&str, &[&str])] = &[
    ("Make", &["Encoded_Hardware_CompanyName"]),
    ("Model", &["Encoded_Hardware_Model"]),
    ("ISOSpeed", &["ISOSpeedRatings", "PhotographicSensitivity"]),
    ("FNumber", &["ApertureValue"]),
    ("PixelXDimension", &["ExifImageWidth", "ImageWidth"]),
    ("PixelYDimension", &["ExifImageHeight", "ImageHeight"]),
    ("DateTimeOriginal", &["DateTimeDigitized", "Recorded_Date"]),
    ("ExifVersion", &["FlashpixVersion"]),
];

const EXIF_ESSENTIAL: &[&str] = &[
    "Make",
    "Model",
    "LensModel",
    "DateTimeOriginal",
    "ExposureTime",
    "FNumber",
    "ISOSpeed",
    "ISOSpeedRatings",
    "PhotographicSensitivity",
    "FocalLength",
    "FocalLengthIn35mmFilm",
    "Orientation",
    "GPSLatitude",
    "GPSLongitude",
    "GPSAltitude",
    "PixelXDimension",
    "PixelYDimension",
];

// ─────────────────── EXIF: категории ─────────────────────

fn exif_category(tag: &str) -> &'static str {
    if tag.starts_with("GPS") {
        return "GPS";
    }
    match tag {
        "Make"
        | "Model"
        | "LensModel"
        | "LensMake"
        | "LensSpecification"
        | "Software"
        | "Artist"
        | "Copyright"
        | "ImageDescription"
        | "Encoded_Hardware_CompanyName"
        | "Encoded_Hardware_Model" => "Камера и автор",
        "DateTime"
        | "DateTimeOriginal"
        | "DateTimeDigitized"
        | "Recorded_Date"
        | "Mastered_Date"
        | "OffsetTime"
        | "OffsetTimeOriginal"
        | "OffsetTimeDigitized"
        | "SubSecTime"
        | "SubSecTimeOriginal"
        | "SubSecTimeDigitized"
        | "CreateDate"
        | "ModifyDate" => "Даты и время",
        "ExposureTime"
        | "FNumber"
        | "ApertureValue"
        | "MaxApertureValue"
        | "ISOSpeed"
        | "ISOSpeedRatings"
        | "PhotographicSensitivity"
        | "ShutterSpeedValue"
        | "BrightnessValue"
        | "ExposureBiasValue"
        | "ExposureProgram"
        | "ExposureMode"
        | "MeteringMode"
        | "Flash"
        | "LightSource"
        | "WhiteBalance"
        | "WhiteBalanceMode"
        | "SceneCaptureType"
        | "SceneType"
        | "SensingMethod"
        | "FocalLength"
        | "FocalLengthIn35mmFilm"
        | "SubjectDistance" => "Экспозиция",
        "PixelXDimension" | "PixelYDimension" | "ImageWidth" | "ImageHeight" | "ExifImageWidth"
        | "ExifImageHeight" | "Orientation" | "ColorSpace" | "XResolution" | "YResolution"
        | "ResolutionUnit" | "YCbCrPositioning" => "Изображение",
        _ => "Прочее",
    }
}

// ─────────────────── EXIF: значения ─────────────────────

fn parse_rational(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some((n, d)) = s.split_once('/') {
        let n: f64 = n.trim().parse().ok()?;
        let d: f64 = d.trim().parse().ok()?;
        if d == 0.0 {
            return None;
        }
        Some(n / d)
    } else {
        s.parse().ok()
    }
}

fn format_gps_coordinate(value: &str, ref_: Option<&str>) -> String {
    let parts: Vec<f64> = value
        .split_whitespace()
        .filter_map(parse_rational)
        .collect();
    let hemi = ref_.map(|r| format!(" {}", r.trim())).unwrap_or_default();
    if parts.len() >= 3 {
        return format!("{:.0}° {:.0}′ {:.2}″{}", parts[0], parts[1], parts[2], hemi);
    }
    if parts.len() == 1 {
        return format!("{:.6}°{}", parts[0], hemi);
    }
    value.to_string()
}

fn format_gps_altitude(value: &str, ref_: Option<&str>) -> String {
    let v = parse_rational(value).unwrap_or(0.0);
    let below = ref_.map(|r| r.trim() == "1").unwrap_or(false);
    if below {
        format!("-{:.2} м", v)
    } else {
        format!("{:.2} м", v)
    }
}

fn humanize_coded_value(tag: &str, value: &str) -> Option<String> {
    let value_clean = value
        .trim()
        .trim_matches('"')
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    let v_int: Option<i64> = value_clean.parse().ok();

    match tag {
        "ExposureProgram" => match v_int {
            Some(0) => Some("Не задана".into()),
            Some(1) => Some("Ручная".into()),
            Some(2) => Some("Обычная (Program AE)".into()),
            Some(3) => Some("Приоритет диафрагмы".into()),
            Some(4) => Some("Приоритет выдержки".into()),
            Some(5) => Some("Творческая".into()),
            Some(6) => Some("Спортивная".into()),
            Some(7) => Some("Портрет".into()),
            Some(8) => Some("Пейзаж".into()),
            _ => None,
        },
        "MeteringMode" => match v_int {
            Some(0) => Some("Неизвестно".into()),
            Some(1) => Some("Средний".into()),
            Some(2) => Some("Центровзвешенный".into()),
            Some(3) => Some("Точечный".into()),
            Some(4) => Some("Многоточечный".into()),
            Some(5) => Some("Матричный".into()),
            Some(6) => Some("Частичный".into()),
            Some(255) => Some("Другой".into()),
            _ => None,
        },
        "WhiteBalance" | "WhiteBalanceMode" => match v_int {
            Some(0) => Some("Авто".into()),
            Some(1) => Some("Вручную".into()),
            _ => None,
        },
        "ExposureMode" => match v_int {
            Some(0) => Some("Авто".into()),
            Some(1) => Some("Ручная".into()),
            Some(2) => Some("Автобрекетинг".into()),
            _ => None,
        },
        "Orientation" => match v_int {
            Some(1) => Some("Нормальная".into()),
            Some(2) => Some("Зеркально по горизонтали".into()),
            Some(3) => Some("Поворот 180°".into()),
            Some(4) => Some("Зеркально по вертикали".into()),
            Some(5) => Some("Поворот 90° CCW + зеркало".into()),
            Some(6) => Some("Поворот 90° CW".into()),
            Some(7) => Some("Поворот 90° CW + зеркало".into()),
            Some(8) => Some("Поворот 90° CCW".into()),
            _ => None,
        },
        "ColorSpace" => match v_int {
            Some(1) => Some("sRGB".into()),
            Some(2) => Some("Adobe RGB".into()),
            Some(65535) => Some("Некалиброванное".into()),
            _ => None,
        },
        "ResolutionUnit" => match v_int {
            Some(1) => Some("нет".into()),
            Some(2) => Some("дюйм".into()),
            Some(3) => Some("см".into()),
            _ => None,
        },
        "YCbCrPositioning" => match v_int {
            Some(1) => Some("Центр".into()),
            Some(2) => Some("Совмещён".into()),
            _ => None,
        },
        "LightSource" => match v_int {
            Some(0) => Some("Неизвестно".into()),
            Some(1) => Some("Дневной свет".into()),
            Some(2) => Some("Флуоресцентный".into()),
            Some(3) => Some("Лампа накаливания".into()),
            Some(4) => Some("Вспышка".into()),
            Some(9) => Some("Ясная погода".into()),
            Some(10) => Some("Облачно".into()),
            Some(11) => Some("Тень".into()),
            Some(12) => Some("Дневной свет (D65)".into()),
            Some(255) => Some("Другой".into()),
            _ => None,
        },
        "SensingMethod" => match v_int {
            Some(1) => Some("Не задан".into()),
            Some(2) => Some("Однокристальный".into()),
            Some(3) => Some("Двухкристальный".into()),
            Some(4) => Some("Трёхкристальный".into()),
            Some(5) => Some("Цветной последовательный".into()),
            Some(7) => Some("Трёхлинейный".into()),
            Some(8) => Some("Цветной последовательный (линейный)".into()),
            _ => None,
        },
        "SceneType" => match v_int {
            Some(1) => Some("Снято напрямую".into()),
            _ => None,
        },
        "SceneCaptureType" => match v_int {
            Some(0) => Some("Стандартный".into()),
            Some(1) => Some("Пейзаж".into()),
            Some(2) => Some("Портрет".into()),
            Some(3) => Some("Ночная съёмка".into()),
            _ => None,
        },
        "SensitivityType" => match v_int {
            Some(0) => Some("Неизвестно".into()),
            Some(1) => Some("ISO Speed".into()),
            Some(2) => Some("REI".into()),
            Some(3) => Some("ISO Speed + REI".into()),
            Some(4) => Some("Exif ISO Speed".into()),
            Some(5) => Some("SOS".into()),
            Some(6) => Some("SOS + REI".into()),
            Some(7) => Some("SOS + Exif ISO Speed".into()),
            _ => None,
        },
        "Flash" => {
            let v = v_int? as u32;
            let mut parts = Vec::new();
            parts.push(if v & 0x1 != 0 {
                "сработала"
            } else {
                "не сработала"
            });
            match (v >> 3) & 0x3 {
                1 => parts.push("ручной режим"),
                2 => parts.push("режим подавления"),
                3 => parts.push("авто"),
                _ => {}
            }
            if v & 0x20 != 0 {
                parts.push("без вспышки");
            }
            if v & 0x40 != 0 {
                parts.push("красноглазый");
            }
            Some(parts.join(", "))
        }
        "InteroperabilityIndex" => match value_clean {
            "R98" => Some("R98 — DCF basic (sRGB)".into()),
            "R03" => Some("R03 — DCF option (Adobe RGB)".into()),
            "THM" => Some("THM — миниатюра".into()),
            _ => None,
        },
        "ExifVersion" | "FlashpixVersion" => {
            let s = value_clean;
            if s.len() == 4 && s.chars().all(|c| c.is_ascii_digit()) {
                Some(format!("{}.{}", &s[..1], &s[1..]))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn format_float_tag(tag: &str, v: f64, orig: Option<(f64, f64)>) -> String {
    match tag {
        "ExposureTime" => {
            if let Some((n, d)) = orig {
                if n == 1.0 {
                    return format!("1/{} с", d as i64);
                }
            }
            if v < 1.0 && v > 0.0 {
                format!("1/{} с", (1.0 / v).round() as i64)
            } else {
                format!("{:.2} с", v)
            }
        }
        "FocalLength" => format!("{:.1} мм", v),
        "FocalLengthIn35mmFilm" => format!("{:.0} мм", v),
        "FNumber" | "ApertureValue" | "MaxApertureValue" => format!("f/{:.1}", v),
        "SubjectDistance" => format!("{:.2} м", v),
        "ExposureBiasValue" => {
            if v.fract().abs() < 0.001 {
                format!("{} EV", v as i64)
            } else {
                format!("{:.2} EV", v)
            }
        }
        "ScaleFactor35efl" => format!("{:.1}", v),
        _ => {
            if v.fract().abs() < 0.001 {
                format!("{}", v as i64)
            } else {
                let s = format!("{:.4}", v);
                s.trim_end_matches('0').trim_end_matches('.').to_string()
            }
        }
    }
}

fn humanize_units(value: &str) -> String {
    let v = value.trim();
    if let Some(rest) = v.strip_suffix(" mm") {
        if rest.parse::<f64>().is_ok() {
            return format!("{} мм", rest);
        }
    }
    if let Some(rest) = v.strip_suffix(" m") {
        if rest.parse::<f64>().is_ok() {
            return format!("{} м", rest);
        }
    }
    if let Some(rest) = v.strip_suffix(" deg") {
        if rest.parse::<f64>().is_ok() {
            return format!("{}°", rest);
        }
    }
    if let Some(rest) = v.strip_suffix(" sec") {
        if rest.parse::<f64>().is_ok() {
            return format!("{} с", rest);
        }
    }
    v.to_string()
}

fn normalize_exif_date(value: &str) -> String {
    let v = value.trim().trim_matches('"');
    if v.len() >= 10 && v.as_bytes().get(4) == Some(&b':') && v.as_bytes().get(7) == Some(&b':') {
        let mut chars: Vec<char> = v.chars().collect();
        if chars.len() >= 10
            && chars[0..4].iter().all(|c| c.is_ascii_digit())
            && chars[5..7].iter().all(|c| c.is_ascii_digit())
            && chars[8..10].iter().all(|c| c.is_ascii_digit())
        {
            chars[4] = '-';
            chars[7] = '-';
            return chars.into_iter().collect();
        }
    }
    v.to_string()
}

fn humanize_value(tag: &str, value: &str) -> String {
    if let Some(s) = humanize_coded_value(tag, value) {
        return s;
    }

    if matches!(
        tag,
        "DateTime"
            | "DateTimeOriginal"
            | "DateTimeDigitized"
            | "Recorded_Date"
            | "Mastered_Date"
            | "CreateDate"
            | "ModifyDate"
    ) {
        return normalize_exif_date(value);
    }

    if let Some((num, den)) = value.split_once('/') {
        if let (Ok(n), Ok(d)) = (num.trim().parse::<f64>(), den.trim().parse::<f64>()) {
            if d != 0.0 {
                return format_float_tag(tag, n / d, Some((n, d)));
            }
        }
    }

    if let Ok(v) = value.trim().parse::<f64>() {
        if matches!(
            tag,
            "FNumber"
                | "ApertureValue"
                | "MaxApertureValue"
                | "FocalLength"
                | "FocalLengthIn35mmFilm"
                | "SubjectDistance"
                | "ExposureBiasValue"
                | "ScaleFactor35efl"
        ) {
            return format_float_tag(tag, v, None);
        }
    }

    humanize_units(value)
}

fn humanize_tag(tag: &str) -> &str {
    match tag {
        "Make" => "Производитель",
        "Model" => "Модель",
        "LensModel" => "Объектив",
        "LensMake" => "Производитель объектива",
        "LensSpecification" => "Характеристики объектива",
        "Software" => "ПО",
        "Artist" => "Автор",
        "Copyright" => "Копирайт",
        "ImageDescription" => "Описание",
        "DateTime" => "Дата и время",
        "DateTimeOriginal" => "Дата съёмки",
        "DateTimeDigitized" => "Дата оцифровки",
        "Recorded_Date" => "Записано",
        "Mastered_Date" => "Обработано",
        "CreateDate" => "Создан",
        "ModifyDate" => "Изменён",
        "OffsetTime" => "Часовой пояс",
        "OffsetTimeOriginal" => "Часовой пояс (съёмка)",
        "OffsetTimeDigitized" => "Часовой пояс (оцифровка)",
        "SubSecTime" => "Мкс",
        "SubSecTimeOriginal" => "Мкс (съёмка)",
        "SubSecTimeDigitized" => "Мкс (оцифровка)",
        "ExposureTime" => "Выдержка",
        "FNumber" => "Диафрагма",
        "ApertureValue" => "Значение диафрагмы",
        "MaxApertureValue" => "Макс. диафрагма",
        "ISOSpeed" | "ISOSpeedRatings" | "PhotographicSensitivity" => "ISO",
        "ShutterSpeedValue" => "Выдержка (APEX)",
        "BrightnessValue" => "Яркость",
        "ExposureBiasValue" => "Компенсация экспозиции",
        "ExposureProgram" => "Программа экспозиции",
        "ExposureMode" => "Режим экспозиции",
        "MeteringMode" => "Режим замера",
        "Flash" => "Вспышка",
        "LightSource" => "Источник света",
        "WhiteBalance" | "WhiteBalanceMode" => "Баланс белого",
        "FocalLength" => "Фокусное расстояние",
        "FocalLengthIn35mmFilm" => "ФР (35 мм экв.)",
        "SubjectDistance" => "Дистанция до объекта",
        "PixelXDimension" | "ExifImageWidth" | "ImageWidth" => "Ширина",
        "PixelYDimension" | "ExifImageHeight" | "ImageHeight" => "Высота",
        "Orientation" => "Ориентация",
        "ColorSpace" => "Цветовое пространство",
        "XResolution" => "Разрешение X",
        "YResolution" => "Разрешение Y",
        "ResolutionUnit" => "Единица разрешения",
        "YCbCrPositioning" => "Позиционирование YCbCr",
        "SensingMethod" => "Метод сенсора",
        "SceneType" => "Тип сцены",
        "SceneCaptureType" => "Тип съёмки",
        "SensitivityType" => "Тип чувствительности",
        "FieldOfView" => "Угол обзора",
        "ScaleFactor35efl" => "Кроп-фактор",
        "CircleOfConfusion" => "Круг нерезкости",
        "HyperfocalDistance" => "Гиперфокальное расстояние",
        "LightValue" => "Световое число (LV)",
        "ExifVersion" => "Версия EXIF",
        "FlashpixVersion" => "Версия Flashpix",
        "InteroperabilityIndex" => "Индекс совместимости",
        "InteroperabilityVersion" => "Версия совместимости",
        "ImageSize" => "Размер изображения",
        "Megapixels" => "Мегапикселей",
        "GPSLatitude" => "Широта",
        "GPSLatitudeRef" => "Полушарие широты",
        "GPSLongitude" => "Долгота",
        "GPSLongitudeRef" => "Полушарие долготы",
        "GPSAltitude" => "Высота",
        "GPSAltitudeRef" => "Отсчёт высоты",
        "GPSTimeStamp" => "Время GPS",
        "GPSDateStamp" => "Дата GPS",
        "GPSSpeed" => "Скорость",
        "GPSSpeedRef" => "Единица скорости",
        "GPSImgDirection" => "Направление съёмки",
        "GPSImgDirectionRef" => "Отсчёт направления",
        "GPSProcessingMethod" => "Метод определения",
        _ => tag,
    }
}

// ─────────────────── EXIF: основная логика ───────────────

fn emit_exif_grouped(
    pairs: &[(String, String)],
    cli: &Cli,
    out: &mut Out,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let lookup: HashMap<&str, &str> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let mut hidden_aliases: HashSet<&str> = HashSet::new();
    if !cli.all {
        for (canonical, aliases) in EXIF_ALIASES {
            if pairs.iter().any(|(k, _)| k == canonical) {
                for a in *aliases {
                    hidden_aliases.insert(a);
                }
            }
        }
    }

    let tag_filter: Option<HashSet<String>> = cli.tags.as_ref().map(|ts| {
        ts.iter()
            .map(|t| t.to_ascii_lowercase())
            .collect::<HashSet<_>>()
    });
    let cat_filter: Option<HashSet<String>> = cli.exif_categories.as_ref().map(|cs| {
        cs.iter()
            .map(|c| c.to_ascii_lowercase())
            .collect::<HashSet<_>>()
    });
    let essential: HashSet<&str> = EXIF_ESSENTIAL.iter().copied().collect();

    let mut filtered: Vec<(String, String, &'static str)> = Vec::new();
    let mut seen_labels: HashSet<(String, String, &'static str)> = HashSet::new();

    for (k, v) in pairs {
        let v_trim = v.trim();

        if !cli.all {
            if v_trim.is_empty() {
                continue;
            }
            if EXIF_SERVICE_FIELDS.contains(&k.as_str()) {
                continue;
            }
            if hidden_aliases.contains(k.as_str()) {
                continue;
            }
        }

        if cli.short && !essential.contains(k.as_str()) {
            continue;
        }

        if let Some(ref f) = tag_filter {
            if !f.contains(&k.to_ascii_lowercase()) {
                continue;
            }
        }

        let cat = exif_category(k);
        if let Some(ref f) = cat_filter {
            if !f.contains(&cat.to_ascii_lowercase()) {
                continue;
            }
        }

        let human_val = if cli.raw {
            v_trim.to_string()
        } else {
            match k.as_str() {
                "GPSLatitude" => {
                    format_gps_coordinate(v_trim, lookup.get("GPSLatitudeRef").copied())
                }
                "GPSLongitude" => {
                    format_gps_coordinate(v_trim, lookup.get("GPSLongitudeRef").copied())
                }
                "GPSAltitude" => format_gps_altitude(v_trim, lookup.get("GPSAltitudeRef").copied()),
                _ => humanize_value(k, v_trim),
            }
        };

        let label = humanize_tag(k).to_string();
        let key = (label.clone(), human_val.clone(), cat);
        if !seen_labels.insert(key) {
            continue;
        }

        filtered.push((label, human_val, cat));
    }

    let mut order: Vec<&'static str> = Vec::new();
    let mut buckets: HashMap<&'static str, Vec<(String, String)>> = HashMap::new();
    for (label, val, cat) in &filtered {
        if !buckets.contains_key(cat) {
            order.push(cat);
        }
        buckets
            .entry(cat)
            .or_default()
            .push((label.clone(), val.clone()));
    }

    if !cli.quiet {
        for cat in &order {
            if let Some(items) = buckets.get(cat) {
                writeln!(out, "{}", format!("EXIF · {}", cat).bold().yellow())?;
                for (label, val) in items {
                    row(out, label, val, cli)?;
                }
                writeln!(out)?;
            }
        }
    }

    let raw: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    obj.insert("exif".into(), json!(raw));

    let display: Vec<serde_json::Value> = filtered
        .iter()
        .map(|(label, val, cat)| {
            json!({
                "category": cat,
                "label": label,
                "value": val,
            })
        })
        .collect();
    obj.insert("exif_display".into(), json!(display));

    Ok(())
}

// ─────────────────── EXIF fallback (kamadak-exif) ───────

fn try_exif_fallback(
    path: &Path,
    cli: &Cli,
    out: &mut Out,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<bool> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Ok(false),
    };
    let mut buf = BufReader::new(file);
    let exif = match Reader::new().read_from_container(&mut buf) {
        Ok(e) => e,
        Err(_) => return Ok(false),
    };

    let mut map = Vec::new();
    for field in exif.fields() {
        let name = format!("{:?}", field.tag);
        let val = field.display_value().with_unit(&exif).to_string();
        map.push((name, val));
    }
    if map.is_empty() {
        return Ok(false);
    }

    if !cli.quiet {
        writeln!(out, "{}", "EXIF (kamadak)".bold().yellow())?;
        for (k, v) in &map {
            row(out, k, v, cli)?;
        }
        writeln!(out)?;
    }
    obj.insert("exif".into(), json!(map));
    Ok(true)
}

// ─────────────────── nom-exif fallback ────────────────────

fn try_nom_exif(
    path: &Path,
    cli: &Cli,
    out: &mut Out,
    obj: &mut serde_json::Map<String, serde_json::Value>,
    text: bool,
) -> Result<()> {
    use nom_exif::{Metadata, read_metadata};

    let meta = match read_metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };

    if let Metadata::Exif(exif) = meta {
        let mut map = serde_json::Map::new();
        for entry in exif.entries() {
            let name = format!("{:?}", entry.tag()).trim_matches('"').to_string();
            let val = entry.value().to_string();
            if val.is_empty() && cli.no_empty {
                continue;
            }
            map.insert(name, json!(val));
        }
        if map.is_empty() {
            return Ok(());
        }
        if text && !cli.quiet {
            writeln!(out, "{}", "EXIF (nom-exif)".bold().yellow())?;
            for (k, v) in &map {
                let s = v
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string());
                row(out, k, &s, cli)?;
            }
            writeln!(out)?;
        }
        obj.insert("nom_exif".into(), json!(map));
    }
    Ok(())
}

// ─────────────────── CSV writer ───────────────────────────

fn write_csv(results: &[serde_json::Value], out: &mut Out) -> Result<()> {
    writeln!(out, "path,group,tag,value")?;
    for r in results {
        let path = r.get("path").and_then(|v| v.as_str()).unwrap_or("");
        for key in [
            "filesystem",
            "mime",
            "exif",
            "iptc",
            "xmp",
            "audio",
            "media_general",
            "media_video",
            "media_audio",
            "nom_exif",
        ] {
            if let Some(v) = r.get(key) {
                emit_csv_value(path, key, v, out)?;
            }
        }
    }
    Ok(())
}

fn emit_csv_value(path: &str, group: &str, value: &serde_json::Value, out: &mut Out) -> Result<()> {
    match value {
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Some(pair) = item.as_array() {
                    if pair.len() == 2 {
                        let k = pair[0].as_str().unwrap_or("");
                        let v = pair[1].as_str().unwrap_or("");
                        writeln!(
                            out,
                            "{},{},{},{}",
                            csv_escape(path),
                            csv_escape(group),
                            csv_escape(k),
                            csv_escape(v)
                        )?;
                    }
                } else if let Some(s) = item.as_str() {
                    writeln!(
                        out,
                        "{},{},,{}",
                        csv_escape(path),
                        csv_escape(group),
                        csv_escape(s)
                    )?;
                }
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let s = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                writeln!(
                    out,
                    "{},{},{},{}",
                    csv_escape(path),
                    csv_escape(group),
                    csv_escape(k),
                    csv_escape(&s)
                )?;
            }
        }
        serde_json::Value::String(s) => {
            writeln!(
                out,
                "{},{},,{}",
                csv_escape(path),
                csv_escape(group),
                csv_escape(s)
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// ─────────────────── FS-метаданные ────────────────────────

struct FsMeta {
    size: u64,
    is_dir: bool,
    is_file: bool,
    perms: String,
    created: String,
    modified: String,
    accessed: String,
    inode: Option<u64>,
    nlink: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
}

fn collect_fs_meta(meta: &fs::Metadata) -> FsMeta {
    let created = system_time_to_string(meta.created().ok());
    let modified = system_time_to_string(meta.modified().ok());
    let accessed = system_time_to_string(meta.accessed().ok());

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FsMeta {
            size: meta.len(),
            is_dir: meta.is_dir(),
            is_file: meta.is_file(),
            perms: format_perms(meta.permissions().mode()),
            created,
            modified,
            accessed,
            inode: Some(meta.ino()),
            nlink: Some(meta.nlink()),
            uid: Some(meta.uid()),
            gid: Some(meta.gid()),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let attrs = meta.file_attributes();
        let perms = format!(
            "attrs=0x{:X}{}{}{}{}",
            attrs,
            if attrs & 0x1 != 0 { " readonly" } else { "" },
            if attrs & 0x2 != 0 { " hidden" } else { "" },
            if attrs & 0x4 != 0 { " system" } else { "" },
            if attrs & 0x20 != 0 { " archive" } else { "" },
        );
        FsMeta {
            size: meta.len(),
            is_dir: meta.is_dir(),
            is_file: meta.is_file(),
            perms,
            created,
            modified,
            accessed,
            inode: None,
            nlink: None,
            uid: None,
            gid: None,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        FsMeta {
            size: meta.len(),
            is_dir: meta.is_dir(),
            is_file: meta.is_file(),
            perms: "—".into(),
            created,
            modified,
            accessed,
            inode: None,
            nlink: None,
            uid: None,
            gid: None,
        }
    }
}

fn print_fs(fs: &FsMeta, cli: &Cli, out: &mut Out) -> Result<()> {
    writeln!(out, "{}", "Файловая система".bold().yellow())?;
    let size = if cli.bytes {
        fs.size.to_string()
    } else {
        format_bytes(fs.size)
    };
    row(out, "Размер", &size, cli)?;
    row(
        out,
        "Тип",
        if fs.is_dir {
            "каталог"
        } else if fs.is_file {
            "файл"
        } else {
            "специальный"
        },
        cli,
    )?;
    row(out, "Права", &fs.perms, cli)?;
    row(out, "Создан", &fs.created, cli)?;
    row(out, "Изменён", &fs.modified, cli)?;
    row(out, "Открыт", &fs.accessed, cli)?;
    if let Some(ino) = fs.inode {
        row(out, "Inode", &ino.to_string(), cli)?;
    }
    if let Some(nlink) = fs.nlink {
        row(out, "Ссылок", &nlink.to_string(), cli)?;
    }
    if let (Some(u), Some(g)) = (fs.uid, fs.gid) {
        row(out, "UID / GID", &format!("{} / {}", u, g), cli)?;
    }
    writeln!(out)?;
    Ok(())
}

fn fs_meta_to_json(fs: &FsMeta, cli: &Cli) -> serde_json::Value {
    json!({
        "size": fs.size,
        "size_human": if cli.bytes { fs.size.to_string() } else { format_bytes(fs.size) },
        "is_dir": fs.is_dir,
        "is_file": fs.is_file,
        "permissions": fs.perms,
        "created": fs.created,
        "modified": fs.modified,
        "accessed": fs.accessed,
        "inode": fs.inode,
        "nlink": fs.nlink,
        "uid": fs.uid,
        "gid": fs.gid,
    })
}

// ─────────────────── общие helpers ────────────────────────

fn row(out: &mut Out, label: &str, value: &str, cli: &Cli) -> Result<()> {
    if (cli.brief || cli.no_empty) && (value.is_empty() || value == "—") {
        return Ok(());
    }
    if cli.quiet {
        writeln!(out, "{}", value)?;
    } else {
        writeln!(out, "  {:<24} {}", format!("{}:", label).bold(), value)?;
    }
    Ok(())
}

fn opt_display<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "—".into())
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.2} {} ({} байт)", size, UNITS[unit], bytes)
    }
}

#[cfg(unix)]
fn format_perms(mode: u32) -> String {
    let mut s = String::with_capacity(10);
    let flags = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    for (bit, ch) in flags {
        s.push(if mode & bit != 0 { ch } else { '-' });
    }
    format!("{:o} ({})", mode & 0o7777, s)
}

fn system_time_to_string(t: Option<std::time::SystemTime>) -> String {
    match t {
        Some(t) => {
            let dt: DateTime<Local> = t.into();
            dt.format("%Y-%m-%d %H:%M:%S %z").to_string()
        }
        None => "—".into(),
    }
}
