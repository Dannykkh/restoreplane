// src/cron.rs — cron 유사 표현식의 v1 서브셋 파서(FR3).
// "M H * * *"(매일) / "M H * * D"(매주 요일 D)만 지원하고, 리스트/범위/스텝은 명확히 거부한다.
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleSpec {
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
    fn from_cron_field(f: &str) -> Result<Self, CronError> {
        match f {
            "0" | "7" => Ok(Weekday::Sun),
            "1" => Ok(Weekday::Mon),
            "2" => Ok(Weekday::Tue),
            "3" => Ok(Weekday::Wed),
            "4" => Ok(Weekday::Thu),
            "5" => Ok(Weekday::Fri),
            "6" => Ok(Weekday::Sat),
            other => Err(CronError::Unsupported(format!("day-of-week '{other}'"))),
        }
    }

    /// Task Scheduler XML의 <DaysOfWeek> 자식 엘리먼트 태그명.
    pub fn xml_tag(&self) -> &'static str {
        match self {
            Weekday::Sun => "Sunday",
            Weekday::Mon => "Monday",
            Weekday::Tue => "Tuesday",
            Weekday::Wed => "Wednesday",
            Weekday::Thu => "Thursday",
            Weekday::Fri => "Friday",
            Weekday::Sat => "Saturday",
        }
    }
}

#[derive(Debug, Error)]
pub enum CronError {
    #[error("cron 표현식은 정확히 5개 필드가 필요함(분 시 일 월 요일): '{0}'")]
    WrongFieldCount(String),
    #[error(
        "v1은 'M H * * *'(매일) 또는 'M H * * D'(매주 요일 D)만 지원함. 지원 안 되는 표현: {0}"
    )]
    Unsupported(String),
    #[error("숫자 필드 파싱 실패: {0}")]
    InvalidNumber(#[source] std::num::ParseIntError),
}

/// "0 3 * * *" -> Daily{3,0}, "30 3 * * 1" -> Weekly{3,30,Mon}.
/// 리스트(`1,2`)/범위(`1-5`)/스텝(`*/15`)은 v1 스코프 밖 — 명확히 거부한다(FR3).
pub fn parse_cron_subset(expr: &str) -> Result<ScheduleSpec, CronError> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(CronError::WrongFieldCount(expr.to_string()));
    }
    let [minute, hour, day_of_month, month, day_of_week] =
        [fields[0], fields[1], fields[2], fields[3], fields[4]];

    if day_of_month != "*" || month != "*" {
        return Err(CronError::Unsupported(format!(
            "일/월 필드는 '*'만 지원: '{expr}'"
        )));
    }
    // 리스트/범위/스텝 방어(조용한 오해석 금지).
    for f in [minute, hour, day_of_week] {
        if f.contains(',') || f.contains('-') || f.contains('/') {
            return Err(CronError::Unsupported(format!(
                "리스트/범위/스텝 미지원: '{expr}'"
            )));
        }
    }
    let m: u8 = minute.parse().map_err(CronError::InvalidNumber)?;
    let h: u8 = hour.parse().map_err(CronError::InvalidNumber)?;
    if m > 59 || h > 23 {
        return Err(CronError::Unsupported(format!("분/시 범위 초과: '{expr}'")));
    }

    if day_of_week == "*" {
        Ok(ScheduleSpec::Daily { hour: h, minute: m })
    } else {
        Ok(ScheduleSpec::Weekly {
            hour: h,
            minute: m,
            day: Weekday::from_cron_field(day_of_week)?,
        })
    }
}
