//! Clock-rate transformation.
//!
//! DT/HT features are re-analysed at the actual clock rate instead of scaling NoMod features.

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ManiaGameMod {
    #[serde(rename = "NM")]
    Nm,
    #[serde(rename = "DT")]
    Dt,
    #[serde(rename = "HT")]
    Ht,
}

impl ManiaGameMod {
    pub const ALL: [Self; 3] = [Self::Nm, Self::Dt, Self::Ht];

    pub const fn rate(self) -> f64 {
        match self {
            Self::Nm => 1.0,
            Self::Dt => 1.5,
            Self::Ht => 0.75,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nm => "NM",
            Self::Dt => "DT",
            Self::Ht => "HT",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        match code.to_ascii_uppercase().as_str() {
            "NM" => Some(Self::Nm),
            "DT" => Some(Self::Dt),
            "HT" => Some(Self::Ht),
            _ => None,
        }
    }
}

/// Equivalent of JavaScript `Number` to string conversion for the value range used here.
fn js_number(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn line_starts_with_number(line: &str) -> bool {
    let trimmed = line.trim_start();
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && trimmed[digits..].starts_with(',')
}

pub fn transform_rate(text: &str, rate: f64) -> Result<String> {
    if !rate.is_finite() || rate <= 0.0 {
        bail!("invalid clock rate");
    }
    if rate == 1.0 {
        return Ok(text.to_owned());
    }

    let mut section = String::new();
    let mut origin: Option<f64> = None;
    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            section = trimmed.to_owned();
        } else if section == "[HitObjects]" && line_starts_with_number(line) {
            origin = line
                .split(',')
                .nth(2)
                .and_then(|value| value.trim().parse::<f64>().ok());
            break;
        }
    }
    let Some(origin) = origin.filter(|value| value.is_finite()) else {
        bail!("no hit objects");
    };

    let time = |value: &str| -> f64 {
        let parsed = value.trim().parse::<f64>().unwrap_or(f64::NAN);
        ((parsed - origin) / rate + 1000.0).floor()
    };

    let mut section = String::new();
    let mut out: Vec<String> = Vec::with_capacity(text.len() / 24);
    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            section = trimmed.to_owned();
            out.push(line.to_owned());
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with("//") {
            out.push(line.to_owned());
            continue;
        }

        let parts: Vec<&str> = line.split(',').collect();
        if section == "[HitObjects]" && parts.len() >= 5 {
            let mut fields: Vec<String> = parts.iter().map(|part| (*part).to_owned()).collect();
            fields[2] = js_number(time(&fields[2]));
            let kind = fields[3].trim().parse::<i64>().unwrap_or(0);
            if kind & 128 != 0 && fields.len() >= 6 && !fields[5].is_empty() {
                let mut tail: Vec<String> =
                    fields[5].split(':').map(|part| part.to_owned()).collect();
                tail[0] = js_number(time(&tail[0]));
                fields[5] = tail.join(":");
            }
            out.push(fields.join(","));
        } else if section == "[TimingPoints]" && parts.len() >= 2 {
            let mut fields: Vec<String> = parts.iter().map(|part| (*part).to_owned()).collect();
            fields[0] = js_number(time(&fields[0]));
            if let Ok(value) = fields[1].trim().parse::<f64>()
                && value > 0.0
            {
                fields[1] = js_number(value / rate);
            }
            out.push(fields.join(","));
        } else if section == "[Events]" && parts.len() >= 3 {
            let kind = parts[0].trim();
            if kind == "2" || kind == "Break" {
                let mut fields: Vec<String> = parts.iter().map(|part| (*part).to_owned()).collect();
                fields[1] = js_number(time(&fields[1]));
                fields[2] = js_number(time(&fields[2]));
                out.push(fields.join(","));
            } else {
                out.push(line.to_owned());
            }
        } else {
            out.push(line.to_owned());
        }
    }

    Ok(out.join("\n"))
}
