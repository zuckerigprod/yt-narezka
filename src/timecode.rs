// Разбор и форматирование таймкодов: "83", "1:23", "01:02:03.5", "1:23,5".

pub fn parse(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', ".");
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut total = 0.0;
    for (i, p) in parts.iter().enumerate() {
        let last = i == parts.len() - 1;
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit() || (last && c == '.')) {
            return None;
        }
        let v: f64 = p.parse().ok()?;
        // минуты и секунды в составном формате не больше 59
        if i > 0 && v >= 60.0 {
            return None;
        }
        total = total * 60.0 + v;
    }
    Some(total)
}

pub fn format(t: f64) -> String {
    let t = t.max(0.0);
    let whole = t.floor() as u64;
    let ms = ((t - whole as f64) * 1000.0).round() as u64;
    let (whole, ms) = if ms >= 1000 { (whole + 1, 0) } else { (whole, ms) };
    let h = whole / 3600;
    let m = (whole % 3600) / 60;
    let s = whole % 60;
    let base = if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    };
    if ms > 0 {
        let frac = format!("{ms:03}");
        format!("{base}.{}", frac.trim_end_matches('0'))
    } else {
        base
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    pub start: f64,
    pub end: Option<f64>,
    pub name: String,
}

// Читает таймкод в начале строки, возвращает его и остаток.
fn take_timecode(s: &str) -> Option<(f64, &str)> {
    let end = s
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == ':' || *c == '.' || *c == ','))
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let token = s[..end].trim_end_matches(['.', ',']);
    // в списках требуем двоеточие, иначе "1. Вступление" превратится в 1 секунду
    if !token.contains(':') {
        return None;
    }
    let t = parse(token)?;
    Some((t, &s[token.len()..]))
}

const SEPARATORS: &[char] = &['-', '–', '—', '~', '|', ':', ')', ']', '.', '•'];

/// Разбирает список вида
///   0:10-0:45 Вступление
///   1:02:00 - 1:05:30
///   12:30 Глава без конца (конец = начало следующей)
pub fn parse_list(text: &str, duration: Option<f64>) -> Vec<ListItem> {
    let mut items = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches(['-', '*', '•', '(', '[', ' ']);
        let Some((start, rest)) = take_timecode(line) else {
            continue;
        };
        let mut rest = rest.trim_start();
        let mut end = None;
        let after_sep = rest
            .trim_start_matches(['-', '–', '—', '~'])
            .trim_start();
        let after_word = after_sep
            .strip_prefix("до ")
            .or_else(|| after_sep.strip_prefix("to "))
            .unwrap_or(after_sep)
            .trim_start();
        if let Some((e, r)) = take_timecode(after_word) {
            end = Some(e);
            rest = r;
        }
        let name = rest.trim().trim_start_matches(SEPARATORS).trim().to_string();
        items.push(ListItem { start, end, name });
    }
    // у глав без конца конец = начало следующей, у последней - длительность видео
    let starts: Vec<f64> = items.iter().map(|i| i.start).collect();
    for (i, item) in items.iter_mut().enumerate() {
        if item.end.is_none() {
            item.end = starts.get(i + 1).copied().or(duration);
        }
    }
    items
}

/// Равные куски по step секунд на отрезке from..to.
/// Хвост короче секунды отбрасываем - такие обрезки никому не нужны.
pub fn split(from: f64, to: f64, step: f64) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    if step <= 0.0 || to <= from {
        return out;
    }
    let mut t = from;
    while to - t >= 1.0 || (out.is_empty() && to > t) {
        let end = (t + step).min(to);
        out.push((t, end));
        t = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_equal() {
        assert_eq!(split(0.0, 30.0, 10.0), vec![(0.0, 10.0), (10.0, 20.0), (20.0, 30.0)]);
        assert_eq!(split(0.0, 25.0, 10.0).last(), Some(&(20.0, 25.0)));
        assert_eq!(split(0.0, 20.5, 10.0).len(), 2);
        assert_eq!(split(5.0, 8.0, 10.0), vec![(5.0, 8.0)]);
        assert!(split(0.0, 10.0, 0.0).is_empty());
    }

    #[test]
    fn parse_basic() {
        assert_eq!(parse("83"), Some(83.0));
        assert_eq!(parse("1:23"), Some(83.0));
        assert_eq!(parse("01:02:03"), Some(3723.0));
        assert_eq!(parse("1:23,5"), Some(83.5));
        assert_eq!(parse("1:75"), None);
        assert_eq!(parse("abc"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("1:2:3:4"), None);
    }

    #[test]
    fn format_basic() {
        assert_eq!(format(83.0), "1:23");
        assert_eq!(format(3723.0), "1:02:03");
        assert_eq!(format(83.5), "1:23.5");
        assert_eq!(format(0.0), "0:00");
    }

    #[test]
    fn list() {
        let text = "0:10-0:45 Вступление\n1:00 - 1:30 | Вторая\n2:00 Третья\nмусор\n3:00\n";
        let items = parse_list(text, Some(240.0));
        assert_eq!(items.len(), 4);
        assert_eq!(items[0], ListItem { start: 10.0, end: Some(45.0), name: "Вступление".into() });
        assert_eq!(items[1], ListItem { start: 60.0, end: Some(90.0), name: "Вторая".into() });
        assert_eq!(items[2], ListItem { start: 120.0, end: Some(180.0), name: "Третья".into() });
        assert_eq!(items[3], ListItem { start: 180.0, end: Some(240.0), name: "".into() });
    }

    #[test]
    fn list_youtube_style() {
        let items = parse_list("00:00 - Intro\n1:05:00 — Финал", None);
        assert_eq!(items[0].name, "Intro");
        assert_eq!(items[0].end, Some(3900.0));
        assert_eq!(items[1].name, "Финал");
        assert_eq!(items[1].end, None);
    }
}
