use tiberius::time::{Date, DateTime, DateTime2, DateTimeOffset, SmallDateTime, Time};
use tiberius::{ColumnData, Row};

/// Separator used by `STRING_AGG` for identifier lists; it cannot appear in a SQL Server identifier in practice.
pub(crate) const LIST_SEPARATOR: char = '\u{1f}';

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Val {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    Bytes(Vec<u8>),
}

/// One result row, addressable by case-insensitive column name.
#[derive(Clone, Debug, Default)]
pub(crate) struct Rec {
    cols: Vec<(String, Val)>,
}

impl Rec {
    pub(crate) fn from_row(row: &Row) -> Rec {
        let cols = row.cells().map(|(column, data)| (column.name().to_ascii_lowercase(), to_val(data))).collect();
        Rec { cols }
    }

    fn get(&self, name: &str) -> Option<&Val> {
        let name = name.to_ascii_lowercase();
        self.cols.iter().find(|(n, _)| *n == name).map(|(_, v)| v)
    }

    pub(crate) fn f64(&self, name: &str) -> Option<f64> {
        match self.get(name)? {
            Val::Int(v) => Some(*v as f64),
            Val::Float(v) => Some(*v),
            Val::Bool(v) => Some(if *v { 1.0 } else { 0.0 }),
            Val::Text(v) => v.trim().parse().ok(),
            Val::Null | Val::Bytes(_) => None,
        }
    }

    pub(crate) fn f64_or_zero(&self, name: &str) -> f64 {
        self.f64(name).unwrap_or(0.0)
    }

    pub(crate) fn str(&self, name: &str) -> Option<String> {
        match self.get(name)? {
            Val::Text(v) => Some(v.clone()),
            Val::Int(v) => Some(v.to_string()),
            Val::Float(v) => Some(v.to_string()),
            Val::Bool(v) => Some(if *v { "1".into() } else { "0".into() }),
            Val::Bytes(v) => Some(hex(v)),
            Val::Null => None,
        }
    }

    pub(crate) fn string(&self, name: &str) -> String {
        self.str(name).unwrap_or_default()
    }

    pub(crate) fn bool(&self, name: &str) -> bool {
        match self.get(name) {
            Some(Val::Bool(v)) => *v,
            Some(Val::Int(v)) => *v != 0,
            Some(Val::Float(v)) => *v != 0.0,
            Some(Val::Text(v)) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes"),
            _ => false,
        }
    }

    pub(crate) fn list(&self, name: &str) -> Vec<String> {
        self.str(name)
            .map(|s| s.split(LIST_SEPARATOR).filter(|p| !p.is_empty()).map(str::to_string).collect())
            .unwrap_or_default()
    }

    pub(crate) fn columns(&self) -> impl Iterator<Item = (&str, &Val)> {
        self.cols.iter().map(|(n, v)| (n.as_str(), v))
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for b in bytes {
        out.push_str(&format!("{b:02X}"));
    }
    out
}

fn to_val(data: &ColumnData<'_>) -> Val {
    match data {
        ColumnData::U8(v) => v.map_or(Val::Null, |v| Val::Int(v.into())),
        ColumnData::I16(v) => v.map_or(Val::Null, |v| Val::Int(v.into())),
        ColumnData::I32(v) => v.map_or(Val::Null, |v| Val::Int(v.into())),
        ColumnData::I64(v) => v.map_or(Val::Null, Val::Int),
        ColumnData::F32(v) => v.map_or(Val::Null, |v| Val::Float(v.into())),
        ColumnData::F64(v) => v.map_or(Val::Null, Val::Float),
        ColumnData::Bit(v) => v.map_or(Val::Null, Val::Bool),
        ColumnData::String(v) => v.as_ref().map_or(Val::Null, |v| Val::Text(v.to_string())),
        ColumnData::Guid(v) => v.map_or(Val::Null, |v| Val::Text(v.to_string())),
        ColumnData::Binary(v) => v.as_ref().map_or(Val::Null, |v| Val::Bytes(v.to_vec())),
        ColumnData::Numeric(v) => v.map_or(Val::Null, |v| Val::Float(numeric_to_f64(v))),
        ColumnData::Xml(v) => v.as_ref().map_or(Val::Null, |v| Val::Text(v.to_string())),
        ColumnData::DateTime(v) => v.map_or(Val::Null, |v| Val::Text(datetime_iso(v))),
        ColumnData::SmallDateTime(v) => v.map_or(Val::Null, |v| Val::Text(small_datetime_iso(v))),
        ColumnData::Time(v) => v.map_or(Val::Null, |v| Val::Text(time_iso(v))),
        ColumnData::Date(v) => v.map_or(Val::Null, |v| Val::Text(date_iso(v))),
        ColumnData::DateTime2(v) => v.map_or(Val::Null, |v| Val::Text(datetime2_iso(v))),
        ColumnData::DateTimeOffset(v) => v.map_or(Val::Null, |v| Val::Text(datetime_offset_iso(v))),
    }
}

pub(crate) fn to_json(data: &ColumnData<'_>) -> serde_json::Value {
    match to_val(data) {
        Val::Null => serde_json::Value::Null,
        Val::Int(v) => v.into(),
        Val::Float(v) => serde_json::Number::from_f64(v).map_or(serde_json::Value::Null, serde_json::Value::Number),
        Val::Text(v) => v.into(),
        Val::Bool(v) => v.into(),
        Val::Bytes(v) => hex(&v).into(),
    }
}

fn numeric_to_f64(n: tiberius::numeric::Numeric) -> f64 {
    n.value() as f64 / 10f64.powi(n.scale().into())
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's civil_from_days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const DAYS_1900_TO_1970: i64 = 25_567;
const DAYS_0001_TO_1970: i64 = 719_162;

fn ymd(days_since_epoch: i64) -> String {
    let (y, m, d) = civil_from_days(days_since_epoch);
    format!("{y:04}-{m:02}-{d:02}")
}

fn hms(seconds: u64) -> String {
    format!("{:02}:{:02}:{:02}", seconds / 3_600, (seconds / 60) % 60, seconds % 60)
}

fn datetime_iso(v: DateTime) -> String {
    let seconds = u64::from(v.seconds_fragments()) / 300;
    format!("{}T{}", ymd(i64::from(v.days()) - DAYS_1900_TO_1970), hms(seconds))
}

fn small_datetime_iso(v: SmallDateTime) -> String {
    let seconds = u64::from(v.seconds_fragments()) * 60;
    format!("{}T{}", ymd(i64::from(v.days()) - DAYS_1900_TO_1970), hms(seconds))
}

fn date_iso(v: Date) -> String {
    ymd(i64::from(v.days()) - DAYS_0001_TO_1970)
}

fn time_seconds(v: Time) -> u64 {
    v.increments() / 10u64.pow(v.scale().into())
}

fn time_iso(v: Time) -> String {
    hms(time_seconds(v))
}

fn datetime2_iso(v: DateTime2) -> String {
    format!("{}T{}", date_iso(v.date()), time_iso(v.time()))
}

fn datetime_offset_iso(v: DateTimeOffset) -> String {
    // The stored date and time are UTC; the offset only records the original zone.
    format!("{}Z", datetime2_iso(v.datetime2()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_dates() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(datetime_iso(DateTime::new(45_000, 300 * 3_661)), "2023-03-17T01:01:01");
        assert_eq!(date_iso(Date::new(738_000)), "2021-07-30");
        assert_eq!(time_iso(Time::new(36_610_000_000, 7)), "01:01:01");
    }

    #[test]
    fn hex_is_prefixed() {
        assert_eq!(hex(&[0xAB, 0x01]), "0xAB01");
    }
}
