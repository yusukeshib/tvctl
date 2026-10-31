//! Parsing of time positions and durations such as `90`, `1:30`, `1h2m3s`, `+10`, `-1:00`.

use anyhow::{Result, bail};

/// A seek target: absolute position or relative offset, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Seek {
    Absolute(f64),
    Relative(f64),
}

/// Parses a seek position. A leading `+` or `-` makes it relative.
pub fn parse_seek(s: &str) -> Result<Seek> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('+') {
        Ok(Seek::Relative(parse_duration(rest)?))
    } else if let Some(rest) = s.strip_prefix('-') {
        Ok(Seek::Relative(-parse_duration(rest)?))
    } else {
        Ok(Seek::Absolute(parse_duration(s)?))
    }
}

/// Parses an unsigned duration: `90`, `1.5`, `1:30`, `1:02:03`, `90s`, `2m`, `1h2m3s`.
pub fn parse_duration(s: &str) -> Result<f64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty time");
    }
    let value = if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() > 3 {
            bail!("invalid time {s:?}");
        }
        let mut total = 0.0;
        for part in parts {
            let v: f64 = part
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid time {s:?}"))?;
            total = total * 60.0 + v;
        }
        total
    } else if s.ends_with(['h', 'm', 's']) {
        let mut total = 0.0;
        let mut num = String::new();
        for c in s.chars() {
            match c {
                '0'..='9' | '.' => num.push(c),
                'h' | 'm' | 's' => {
                    let v: f64 = num
                        .parse()
                        .map_err(|_| anyhow::anyhow!("invalid time {s:?}"))?;
                    total += v * match c {
                        'h' => 3600.0,
                        'm' => 60.0,
                        _ => 1.0,
                    };
                    num.clear();
                }
                _ => bail!("invalid time {s:?}"),
            }
        }
        total
    } else {
        s.parse()
            .map_err(|_| anyhow::anyhow!("invalid time {s:?}"))?
    };
    if !value.is_finite() || value < 0.0 {
        bail!("invalid time {s:?}");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), 90.0);
        assert_eq!(parse_duration("1.5").unwrap(), 1.5);
        assert_eq!(parse_duration("1:30").unwrap(), 90.0);
        assert_eq!(parse_duration("1:02:03").unwrap(), 3723.0);
        assert_eq!(parse_duration("90s").unwrap(), 90.0);
        assert_eq!(parse_duration("2m").unwrap(), 120.0);
        assert_eq!(parse_duration("1h2m3s").unwrap(), 3723.0);
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("1:2:3:4").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn seeks() {
        assert_eq!(parse_seek("+10").unwrap(), Seek::Relative(10.0));
        assert_eq!(parse_seek("-1:00").unwrap(), Seek::Relative(-60.0));
        assert_eq!(parse_seek("1:00").unwrap(), Seek::Absolute(60.0));
    }
}
