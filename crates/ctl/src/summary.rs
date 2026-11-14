//! The short human summary that `gpaperctl status` prints.

use gpaperd_core::control::{StatusReport, StyleReport};
use gpaperd_core::event::Pause;
use gpaperd_core::style::Style;
use jiff::Timestamp;
use jiff::tz::TimeZone;

/// Width of the label column, so that values line up.
const LABEL_WIDTH: usize = 13;

/// Renders `report` as a few aligned lines, ending with a newline. `now` and
/// `zone` place the next change in local time and relative to now.
pub fn summary(report: &StatusReport, now: Timestamp, zone: &TimeZone) -> String {
    let mut lines = vec![
        line("visible:", report.visible.as_str()),
        line("next change:", &next_change(report.next_change, now, zone)),
    ];
    for style in Style::ALL {
        let label = format!("{style}:");
        lines.extend(style_lines(&label, &report.styles[style]));
    }
    lines.join("\n") + "\n"
}

fn line(label: &str, value: &str) -> String {
    format!("{label:<LABEL_WIDTH$}{value}")
}

/// Two lines: the item in the slot, then catalog sizes and pauses.
fn style_lines(label: &str, style: &StyleReport) -> [String; 2] {
    let current = style.current.as_ref().map_or("none", |item| item.as_str());
    let mut counts = format!(
        "{} items, {} usable, {} bad",
        style.items,
        style.usable,
        style.bad.len()
    );
    if !style.paused.is_empty() {
        let reasons: Vec<&str> = style.paused.iter().copied().map(pause_label).collect();
        counts.push_str(&format!("; paused: {}", reasons.join(", ")));
    }
    [line(label, current), line("", &counts)]
}

fn pause_label(pause: Pause) -> &'static str {
    match pause {
        Pause::User => "by user",
        Pause::ExternalChange => "external change",
        Pause::LockedKey => "locked key",
        Pause::NoKey => "no key",
        Pause::ScreenLocked => "screen locked",
    }
}

/// `2026-11-14 15:30:00 +01:00 (in 12m 5s)`, or `none`.
fn next_change(at: Option<u64>, now: Timestamp, zone: &TimeZone) -> String {
    let Some(seconds) = at else {
        return "none".to_owned();
    };
    let Some(when) = i64::try_from(seconds)
        .ok()
        .and_then(|seconds| Timestamp::from_second(seconds).ok())
    else {
        return format!("{seconds} (unix seconds)");
    };
    let local = when
        .to_zoned(zone.clone())
        .strftime("%Y-%m-%d %H:%M:%S %:z");
    let ahead = when.as_second() - now.as_second();
    format!("{local} ({})", relative(ahead))
}

/// `in 1h 2m` for a time ahead; `due now` for one reached or passed.
fn relative(seconds_ahead: i64) -> String {
    if seconds_ahead <= 0 {
        return "due now".to_owned();
    }
    format!("in {}", compact(seconds_ahead.unsigned_abs()))
}

/// The two largest non-zero units of a positive span: `1d 2h`, `5m 3s`, `7s`.
fn compact(seconds: u64) -> String {
    let units = [
        (seconds / 86_400, "d"),
        (seconds / 3_600 % 24, "h"),
        (seconds / 60 % 60, "m"),
        (seconds % 60, "s"),
    ];
    let first = units
        .iter()
        .position(|&(amount, _)| amount > 0)
        .unwrap_or(units.len() - 1);
    units[first..]
        .iter()
        .take(2)
        .filter(|&&(amount, _)| amount > 0)
        .map(|(amount, unit)| format!("{amount}{unit}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use gpaperd_core::catalog::ItemId;
    use gpaperd_core::control::{BadItem, STATUS_VERSION};
    use gpaperd_core::style::PerStyle;
    use jiff::tz;

    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn now() -> Timestamp {
        Timestamp::from_second(NOW).expect("valid timestamp")
    }

    fn report() -> StatusReport {
        StatusReport {
            version: STATUS_VERSION,
            visible: Style::Dark,
            next_change: Some(NOW.unsigned_abs() + 725),
            styles: PerStyle {
                light: StyleReport {
                    current: Some(ItemId::new("/pics/day.jpg")),
                    paused: vec![Pause::User, Pause::ExternalChange],
                    items: 12,
                    usable: 11,
                    bad: vec![BadItem {
                        id: ItemId::new("/pics/broken.png"),
                        reason: "not an image".to_owned(),
                    }],
                },
                dark: StyleReport::default(),
            },
        }
    }

    #[test]
    fn summary_lists_every_style() {
        let text = summary(&report(), now(), &TimeZone::UTC);
        let expected = "\
visible:     dark
next change: 2027-01-15 08:12:05 +00:00 (in 12m 5s)
light:       /pics/day.jpg
             12 items, 11 usable, 1 bad; paused: by user, external change
dark:        none
             0 items, 0 usable, 0 bad
";
        assert_eq!(text, expected);
    }

    #[test]
    fn summary_names_a_locked_screen() {
        let mut report = report();
        report.styles.dark.paused = vec![Pause::ScreenLocked];

        let text = summary(&report, now(), &TimeZone::UTC);

        assert!(text.contains("0 bad; paused: screen locked\n"), "{text}");
    }

    #[test]
    fn next_change_uses_the_given_zone() {
        let zone = TimeZone::fixed(tz::offset(2));
        let text = next_change(Some(NOW.unsigned_abs()), now(), &zone);
        assert_eq!(text, "2027-01-15 10:00:00 +02:00 (due now)");
    }

    #[test]
    fn next_change_without_a_deadline_is_none() {
        assert_eq!(next_change(None, now(), &TimeZone::UTC), "none");
    }

    #[test]
    fn next_change_out_of_range_shows_the_raw_number() {
        let text = next_change(Some(u64::MAX), now(), &TimeZone::UTC);
        assert_eq!(text, format!("{} (unix seconds)", u64::MAX));
    }

    #[test]
    fn relative_shows_two_units_at_most() {
        assert_eq!(relative(-5), "due now");
        assert_eq!(relative(0), "due now");
        assert_eq!(relative(7), "in 7s");
        assert_eq!(relative(65), "in 1m 5s");
        assert_eq!(relative(3_600), "in 1h");
        assert_eq!(relative(3_725), "in 1h 2m");
        assert_eq!(relative(90_061), "in 1d 1h");
        assert_eq!(relative(86_400 + 59), "in 1d");
    }
}
