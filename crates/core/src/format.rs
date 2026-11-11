use std::time::Duration;

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 { value /= 1024.0; unit += 1; }
    if unit == 0 { format!("{n} B") }
    else if value >= 100.0 { format!("{value:.0} {}", UNITS[unit]) }
    else if value >= 10.0 { format!("{value:.1} {}", UNITS[unit]) }
    else { format!("{value:.2} {}", UNITS[unit]) }
}

pub fn speed(per_second: f64) -> String {
    if per_second < 1.0 { String::new() } else { format!("{}/s", bytes(per_second as u64)) }
}

pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0 => String::new(),
        1..60 => format!("{s}s"),
        60..3600 => format!("{}m {}s", s / 60, s % 60),
        3600..86400 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d {}h", s / 86400, s % 86400 / 3600),
    }
}

pub fn count(n: u64, one: &str, many: &str) -> String {
    let digits = n.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 { grouped.push(','); }
        grouped.push(c);
    }
    format!("{grouped} {}", if n == 1 { one } else { many })
}
