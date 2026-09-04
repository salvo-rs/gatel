//! One line per request, in whichever shape the reader is.
//!
//! Three formats, because three different things read an access log: a person
//! at a terminal, a log pipeline, and the decades of tooling that expects the
//! common log format. The record is the same; only the rendering differs.

use std::fmt::Write as _;
use std::net::IpAddr;
use std::time::{Duration, SystemTime};

/// How a request ended, beyond its status.
///
/// A 200 whose body was cut off part way is not a success, and nothing in the
/// status line says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The body reached its end.
    Completed,
    /// The body failed part way through.
    Failed,
    /// The client went away before the end.
    Cancelled,
}

impl Outcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::Failed => "body_error",
            Outcome::Cancelled => "cancelled",
        }
    }
}

/// Which shape a line takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// For a person watching a terminal.
    Text,
    /// One JSON object per line, for a pipeline.
    Json,
    /// The common log format, for everything that already parses it.
    Clf,
}

impl Format {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "text" => Some(Format::Text),
            "json" => Some(Format::Json),
            "clf" => Some(Format::Clf),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Format::Text => "text",
            Format::Json => "json",
            Format::Clf => "clf",
        }
    }

    pub const ALL: [&'static str; 3] = ["text", "json", "clf"];
}

/// Everything one line can say.
#[derive(Debug, Clone)]
pub struct Record {
    /// The canonical client address, decided once by the trusted-proxy policy.
    pub client: Option<IpAddr>,
    pub method: String,
    /// Already trimmed of its query unless the configuration asked for it: a
    /// query string routinely carries tokens and personal data.
    pub path: String,
    pub version: &'static str,
    pub status: u16,
    pub bytes: u64,
    pub duration: Duration,
    pub outcome: Outcome,
    pub site: String,
    pub route: String,
    /// When the request arrived, not when it finished — which is what the
    /// common log format means by its timestamp.
    pub at: SystemTime,
}

impl Record {
    pub fn render(&self, format: Format) -> String {
        match format {
            Format::Text => self.text(),
            Format::Json => self.json(),
            Format::Clf => self.clf(),
        }
    }

    fn text(&self) -> String {
        format!(
            "{} {} {} {}ms {}B {}",
            self.method,
            self.path,
            self.status,
            self.duration.as_millis(),
            self.bytes,
            self.outcome.as_str(),
        )
    }

    fn json(&self) -> String {
        let mut out = String::with_capacity(192);
        out.push('{');
        field(&mut out, "time", &timestamp(self.at));
        out.push(',');
        field(&mut out, "method", &self.method);
        out.push(',');
        field(&mut out, "path", &self.path);
        let _ = write!(out, ",\"status\":{}", self.status);
        let _ = write!(out, ",\"duration_ms\":{}", self.duration.as_millis());
        let _ = write!(out, ",\"bytes\":{}", self.bytes);
        out.push(',');
        field(&mut out, "outcome", self.outcome.as_str());
        out.push(',');
        field(&mut out, "site", &self.site);
        out.push(',');
        field(&mut out, "route", &self.route);
        // Absent rather than null or empty, so a consumer can tell "no address
        // was established" from "the address was blank".
        if let Some(client) = self.client {
            out.push(',');
            field(&mut out, "client", &client.to_string());
        }
        out.push('}');
        out
    }

    fn clf(&self) -> String {
        format!(
            "{} - - [{}] \"{} {} {}\" {} {}",
            self.client
                .map(|client| client.to_string())
                .unwrap_or_else(|| "-".to_string()),
            clf_time(self.at),
            self.method,
            self.path,
            self.version,
            self.status,
            // The format writes `-` rather than 0 for a response with no body.
            if self.bytes == 0 {
                "-".to_string()
            } else {
                self.bytes.to_string()
            },
        )
    }
}

fn field(out: &mut String, name: &str, value: &str) {
    let _ = write!(out, "\"{name}\":");
    string(out, value);
}

/// A JSON string, escaped by hand.
///
/// The alternative is a serialisation dependency in the crate that everything
/// else sits on top of, for one string type.
fn string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Everything below a space has to be escaped, and a path can carry
            // one however unlikely that is.
            control if control < ' ' => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// RFC 3339, in UTC.
pub fn timestamp(at: SystemTime) -> String {
    let at = time::OffsetDateTime::from(at);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        at.year(),
        at.month() as u8,
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
    )
}

/// The common log format's timestamp. UTC, and it says so: reading a local
/// offset in a process with other threads running is not something to do for a
/// log line.
fn clf_time(at: SystemTime) -> String {
    let at = time::OffsetDateTime::from(at);
    format!(
        "{:02}/{}/{:04}:{:02}:{:02}:{:02} +0000",
        at.day(),
        MONTHS[at.month() as usize - 1],
        at.year(),
        at.hour(),
        at.minute(),
        at.second(),
    )
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use super::*;

    fn record() -> Record {
        Record {
            client: Some("203.0.113.9".parse().unwrap()),
            method: "GET".to_string(),
            path: "/index.html".to_string(),
            version: "HTTP/1.1",
            status: 200,
            bytes: 1234,
            duration: Duration::from_millis(7),
            outcome: Outcome::Completed,
            site: "example.com".to_string(),
            route: "static".to_string(),
            // 2001-09-09T01:46:40Z, a timestamp that is easy to check.
            at: UNIX_EPOCH + Duration::from_secs(1_000_000_000),
        }
    }

    #[test]
    fn text_is_what_a_person_reads() {
        assert_eq!(
            record().render(Format::Text),
            "GET /index.html 200 7ms 1234B completed"
        );
    }

    #[test]
    fn clf_is_the_shape_the_old_tooling_expects() {
        assert_eq!(
            record().render(Format::Clf),
            "203.0.113.9 - - [09/Sep/2001:01:46:40 +0000] \"GET /index.html HTTP/1.1\" 200 1234"
        );
    }

    #[test]
    fn clf_writes_a_dash_where_it_has_nothing() {
        let mut record = record();
        record.client = None;
        record.bytes = 0;
        let line = record.render(Format::Clf);
        assert!(line.starts_with("- - - ["), "{line}");
        assert!(line.ends_with("\" 200 -"), "{line}");
    }

    #[test]
    fn json_carries_what_the_other_two_leave_out() {
        let line = record().render(Format::Json);
        assert!(line.starts_with('{') && line.ends_with('}'), "{line}");
        assert!(line.contains("\"time\":\"2001-09-09T01:46:40Z\""), "{line}");
        assert!(line.contains("\"route\":\"static\""), "{line}");
        assert!(line.contains("\"site\":\"example.com\""), "{line}");
        assert!(line.contains("\"outcome\":\"completed\""), "{line}");
        assert!(line.contains("\"duration_ms\":7"), "{line}");
    }

    #[test]
    fn an_unknown_client_is_absent_rather_than_blank() {
        let mut record = record();
        record.client = None;
        assert!(!record.render(Format::Json).contains("client"));
    }

    #[test]
    fn a_path_cannot_break_out_of_its_json_string() {
        let mut record = record();
        record.path = "/a\"b\\c\nd\te".to_string();
        let line = record.render(Format::Json);
        assert!(line.contains(r#""path":"/a\"b\\c\nd\te""#), "{line}");
        assert_eq!(line.lines().count(), 1, "one record is one line: {line}");
    }

    #[test]
    fn a_control_character_is_escaped_rather_than_written() {
        let mut record = record();
        record.path = "/a\u{7}b".to_string();
        assert!(record.render(Format::Json).contains("\\u0007"));
    }

    #[test]
    fn only_the_three_formats_are_accepted() {
        for name in Format::ALL {
            assert_eq!(Format::parse(name).unwrap().as_str(), name);
        }
        assert_eq!(Format::parse("combined"), None);
    }
}
