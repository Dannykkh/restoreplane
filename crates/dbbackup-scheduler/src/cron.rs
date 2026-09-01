use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleSpec {
    Hourly { minute: u8 },
    Daily { hour: u8, minute: u8 },
    Weekly { hour: u8, minute: u8, day: Weekday },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weekday {
    Sun,
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
}

impl Weekday {
    fn from_cron_field(field: &str) -> Result<Self, CronError> {
        match field {
            "0" | "7" => Ok(Self::Sun),
            "1" => Ok(Self::Mon),
            "2" => Ok(Self::Tue),
            "3" => Ok(Self::Wed),
            "4" => Ok(Self::Thu),
            "5" => Ok(Self::Fri),
            "6" => Ok(Self::Sat),
            other => Err(CronError::Unsupported(format!("day-of-week '{other}'"))),
        }
    }

    pub const fn xml_tag(self) -> &'static str {
        match self {
            Self::Sun => "Sunday",
            Self::Mon => "Monday",
            Self::Tue => "Tuesday",
            Self::Wed => "Wednesday",
            Self::Thu => "Thursday",
            Self::Fri => "Friday",
            Self::Sat => "Saturday",
        }
    }
}

#[derive(Debug, Error)]
pub enum CronError {
    #[error("cron expression requires exactly five fields: '{0}'")]
    WrongFieldCount(String),
    #[error("unsupported cron expression: {0}")]
    Unsupported(String),
    #[error("invalid numeric cron field: {0}")]
    InvalidNumber(#[source] std::num::ParseIntError),
}

/// Supports `M * * * *`, `M H * * *`, and `M H * * D`.
/// Lists, ranges, and steps remain deliberately unsupported.
pub fn parse_cron_subset(expression: &str) -> Result<ScheduleSpec, CronError> {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(CronError::WrongFieldCount(expression.to_owned()));
    }
    let [minute, hour, day_of_month, month, day_of_week] =
        [fields[0], fields[1], fields[2], fields[3], fields[4]];

    if day_of_month != "*" || month != "*" {
        return Err(CronError::Unsupported(format!(
            "day-of-month and month must be '*': '{expression}'"
        )));
    }
    for field in [minute, hour, day_of_week] {
        if field.contains(',') || field.contains('-') || field.contains('/') {
            return Err(CronError::Unsupported(format!(
                "lists, ranges, and steps are not supported: '{expression}'"
            )));
        }
    }

    let minute: u8 = minute.parse().map_err(CronError::InvalidNumber)?;
    if minute > 59 {
        return Err(CronError::Unsupported(format!(
            "minute is out of range: '{expression}'"
        )));
    }

    if hour == "*" {
        if day_of_week != "*" {
            return Err(CronError::Unsupported(format!(
                "hourly expressions cannot constrain day-of-week: '{expression}'"
            )));
        }
        return Ok(ScheduleSpec::Hourly { minute });
    }

    let hour: u8 = hour.parse().map_err(CronError::InvalidNumber)?;
    if hour > 23 {
        return Err(CronError::Unsupported(format!(
            "hour is out of range: '{expression}'"
        )));
    }

    if day_of_week == "*" {
        Ok(ScheduleSpec::Daily { hour, minute })
    } else {
        Ok(ScheduleSpec::Weekly {
            hour,
            minute,
            day: Weekday::from_cron_field(day_of_week)?,
        })
    }
}
