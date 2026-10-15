//! Fetch expanded occurrences, then normalize Graph's wire format at this boundary.
use super::{Meeting, Person, auth, check_cancel};
use gtk::glib;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use url::Url;

pub(super) enum Error {
    Unauthorized,
    Message(String),
}
impl Error {
    pub fn message(self) -> String {
        match self {
            Self::Unauthorized => {
                "Microsoft rejected the calendar session. Reconnect in Calendar settings.".into()
            }
            Self::Message(message) => message,
        }
    }
}
impl From<String> for Error {
    fn from(value: String) -> Self {
        Self::Message(value)
    }
}

#[derive(Deserialize)]
struct Page {
    value: Vec<Event>,
    #[serde(rename = "@odata.nextLink")]
    next: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    id: String,
    subject: Option<String>,
    start: DateTime,
    end: DateTime,
    #[serde(default)]
    is_cancelled: bool,
    #[serde(default)]
    is_all_day: bool,
    #[serde(default)]
    response_status: Response,
    organizer: Option<Attendee>,
    #[serde(default)]
    attendees: Vec<Attendee>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DateTime {
    date_time: String,
    time_zone: String,
}
#[derive(Default, Deserialize)]
struct Response {
    #[serde(default)]
    response: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Attendee {
    email_address: Address,
    #[serde(default)]
    status: Response,
    #[serde(default, rename = "type")]
    kind: String,
}
#[derive(Deserialize)]
struct Address {
    name: Option<String>,
    address: Option<String>,
}

pub(super) fn upcoming(
    token: &str,
    cancel: &AtomicBool,
) -> std::result::Result<Vec<Meeting>, Error> {
    let now = crate::ipc::now();
    let iso = |time| {
        glib::DateTime::from_unix_utc(time)
            .and_then(|d| d.format_iso8601())
            .map(|d| d.to_string())
            .map_err(|_| Error::Message("Could not read the current time.".into()))
    };
    let mut url = Url::parse("https://graph.microsoft.com/v1.0/me/calendarView").unwrap();
    url.query_pairs_mut().extend_pairs([
        ("startDateTime", iso(now)?),
        ("endDateTime", iso(now + 7 * 86400)?),
        (
            "$select",
            "id,subject,start,end,organizer,attendees,isCancelled,isAllDay,responseStatus".into(),
        ),
        ("$orderby", "start/dateTime".into()),
        ("$top", "100".into()),
    ]);
    let mut next = Some(url.to_string());
    let mut visited = HashSet::new();
    let mut meetings = Vec::new();
    let client = auth::http();
    while let Some(link) = next {
        check_cancel(cancel)?;
        if !safe_page(&link) || !visited.insert(link.clone()) || visited.len() > 100 {
            return Err(Error::Message(
                "Microsoft returned an invalid or oversized calendar page sequence.".into(),
            ));
        }
        let mut response = client
            .get(&link)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Prefer", "outlook.timezone=\"UTC\"")
            .call()
            .map_err(|_| {
                Error::Message(
                    "Could not refresh the calendar. Check your connection and try again.".into(),
                )
            })?;
        match response.status().as_u16() {
            200 => {},
            401 => return Err(Error::Unauthorized),
            403 => return Err(Error::Message("Calendar access was denied. Grant Calendars.ReadBasic consent; your organization may require administrator approval.".into())),
            429 => return Err(Error::Message("Microsoft is limiting calendar requests. Wait a few minutes before refreshing.".into())),
            _ => return Err(Error::Message("Microsoft Calendar is unavailable. Try refreshing later.".into())),
        }
        let page: Page = auth::read_json(response.body_mut())
            .map_err(|_| Error::Message("Could not read Microsoft's calendar response.".into()))?;
        for event in page.value {
            if let Some(meeting) = normalize(event, now)? {
                meetings.push(meeting);
            }
        }
        next = page.next;
    }
    meetings.sort_by_key(|m| m.starts_at);
    let mut ids = HashSet::new();
    meetings.retain(|m| ids.insert(m.id.clone()));
    check_cancel(cancel)?;
    Ok(meetings)
}

/// Never forward a bearer token to a nextLink outside the Graph calendar API.
fn safe_page(link: &str) -> bool {
    Url::parse(link).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str() == Some("graph.microsoft.com")
            && u.port_or_known_default() == Some(443)
            && u.username().is_empty()
            && u.password().is_none()
            && u.fragment().is_none()
            && u.path() == "/v1.0/me/calendarView"
    })
}

fn timestamp(value: &DateTime) -> super::Result<i64> {
    // We request UTC so Windows timezone names and local DST never need guessing.
    if value.time_zone != "UTC" {
        return Err("Microsoft returned an unexpected calendar timezone.".into());
    }
    glib::DateTime::from_iso8601(&value.date_time, Some(&glib::TimeZone::utc()))
        .map(|t| t.to_unix())
        .map_err(|_| "Microsoft returned an invalid calendar time.".into())
}

fn normalize(event: Event, now: i64) -> super::Result<Option<Meeting>> {
    if event.is_cancelled || event.is_all_day || event.response_status.response == "declined" {
        return Ok(None);
    }
    let starts_at = timestamp(&event.start)?;
    let ends_at = timestamp(&event.end)?;
    if ends_at <= now || ends_at <= starts_at || starts_at >= now + 7 * 86400 {
        return Ok(None);
    }
    let mut participants = Vec::new();
    let mut addresses = HashSet::new();
    for attendee in event.organizer.into_iter().chain(event.attendees) {
        if attendee.kind == "resource" || attendee.status.response == "declined" {
            continue;
        }
        let email = attendee
            .email_address
            .address
            .unwrap_or_default()
            .trim()
            .to_owned();
        let name = attendee
            .email_address
            .name
            .unwrap_or_default()
            .trim()
            .to_owned();
        let key = if email.is_empty() {
            name.to_lowercase()
        } else {
            email.to_lowercase()
        };
        if key.is_empty() || !addresses.insert(key) {
            continue;
        }
        participants.push(Person { name, email });
    }
    let title = event
        .subject
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Untitled meeting".into());
    Ok(Some(Meeting {
        provider: "microsoft".into(),
        id: event.id,
        title,
        starts_at,
        ends_at,
        participants,
    }))
}

/// The email is a login hint, not an account restriction. Verify the selected account
/// before storing credentials, so a browser's other session cannot select another calendar.
pub(super) fn verify_account(token: &str, email: &str) -> super::Result<()> {
    let mut response = auth::http()
        .get("https://graph.microsoft.com/v1.0/me?$select=mail,userPrincipalName")
        .header("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(
            |_| "Could not verify the Microsoft account. Check your connection and reconnect.",
        )?;
    if !response.status().is_success() {
        return Err("Could not verify the signed-in account. Grant the delegated User.Read permission and reconnect.".into());
    }
    let profile: serde_json::Value = auth::read_json(response.body_mut())?;
    if ["mail", "userPrincipalName"].iter().any(|key| {
        profile[key]
            .as_str()
            .is_some_and(|actual| actual.eq_ignore_ascii_case(email))
    }) {
        Ok(())
    } else {
        Err("The Microsoft account you selected does not match the configured email. Enter its primary email or sign-in address, or reconnect with the intended account.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event() -> serde_json::Value {
        serde_json::json!({"id":"occurrence", "subject":"Weekly planning", "start":{"dateTime":"2026-10-08T12:00:00.0000000", "timeZone":"UTC"}, "end":{"dateTime":"2026-10-08T13:00:00.0000000", "timeZone":"UTC"}, "organizer":{"emailAddress":{"name":"Alex", "address":"alex@example.com"}}, "attendees":[{"emailAddress":{"name":"Alex", "address":"ALEX@example.com"}}, {"emailAddress":{"name":"Sam", "address":"sam@example.com"}}, {"type":"resource", "emailAddress":{"name":"Room", "address":"room@example.com"}}]})
    }
    #[test]
    fn normalizes_occurrences_utc_and_invitees() {
        let event: Event = serde_json::from_value(event()).unwrap();
        let start = timestamp(&event.start).unwrap();
        let meeting = normalize(event, start + 60).unwrap().unwrap();
        assert_eq!(meeting.starts_at, 1791460800);
        assert_eq!(meeting.participants.len(), 2);
        assert_eq!(meeting.participants[1].label(), "Sam");
    }
    #[test]
    fn excludes_cancelled_declined_all_day_and_past_events() {
        for key in ["isCancelled", "isAllDay"] {
            let mut wire = event();
            wire[key] = true.into();
            assert!(
                normalize(serde_json::from_value(wire).unwrap(), 1791460800)
                    .unwrap()
                    .is_none()
            );
        }
        let mut wire = event();
        wire["responseStatus"] = serde_json::json!({"response":"declined"});
        assert!(
            normalize(serde_json::from_value(wire).unwrap(), 1791460800)
                .unwrap()
                .is_none()
        );
        assert!(
            normalize(serde_json::from_value(event()).unwrap(), 1791464401)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn pagination_cannot_leak_bearer_tokens() {
        assert!(safe_page(
            "https://graph.microsoft.com/v1.0/me/calendarView?$skiptoken=abc"
        ));
        for url in [
            "http://graph.microsoft.com/v1.0/me/calendarView",
            "https://graph.microsoft.com.evil.test/v1.0/me/calendarView",
            "https://evil.test/",
            "https://user@graph.microsoft.com/v1.0/me/calendarView",
            "https://graph.microsoft.com:444/v1.0/me/calendarView",
            "https://graph.microsoft.com/v1.0/me/messages",
        ] {
            assert!(!safe_page(url), "{url}");
        }
    }
}
