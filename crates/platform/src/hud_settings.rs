//! Saved values prioritize glyph searches; they are never speaker evidence.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HudLayoutHint {
    pub safe_zone_x: f32,
    pub safe_zone_y: f32,
    pub hud_scale: f32,
}
impl HudLayoutHint {
    pub(crate) fn inset_fraction(self) -> f32 {
        (1.0 - self.safe_zone_x) / 2.0
    }
}

fn tokens(text: &str) -> Option<Vec<String>> {
    let mut result = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' && chars.peek() == Some(&'/') {
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
        } else if c == '"' {
            let mut token = String::new();
            loop {
                match chars.next()? {
                    '"' => break,
                    '\\' => token.push(chars.next()?),
                    c => token.push(c),
                }
            }
            result.push(token);
        } else if c == '{' || c == '}' {
            result.push(c.to_string());
        } else if !c.is_whitespace() {
            return None;
        }
    }
    Some(result)
}
fn value<'a>(tokens: &'a [String], key: &str) -> Option<&'a str> {
    let mut values = tokens.windows(2).filter(|v| v[0].eq_ignore_ascii_case(key));
    let found = values.next()?.get(1)?.as_str();
    if values.next().is_some() || found == "{" || found == "}" {
        return None;
    }
    Some(found)
}
pub fn parse_hud_layout_hint(text: &str) -> Option<HudLayoutHint> {
    let t = tokens(text)?;
    let x: f32 = value(&t, "safezonex")?.parse().ok()?;
    let y: f32 = value(&t, "safezoney")?.parse().ok()?;
    let scale: f32 = value(&t, "hud_scaling$3")
        .or_else(|| value(&t, "hud_scaling3"))?
        .parse()
        .ok()?;
    if !x.is_finite()
        || !y.is_finite()
        || !scale.is_finite()
        || !(0.2..=1.0).contains(&x)
        || !(0.2..=1.0).contains(&y)
        || !(0.5..=2.0).contains(&scale)
    {
        return None;
    }
    Some(HudLayoutHint {
        safe_zone_x: x,
        safe_zone_y: y,
        hud_scale: scale,
    })
}

fn merged_hint(machine: &str, user: &str) -> Option<HudLayoutHint> {
    let baseline = tokens(machine)?;
    let overrides = tokens(user)?;
    let mut merged = String::new();
    for key in ["safezonex", "safezoney", "hud_scaling$3"] {
        let lookup = |t: &[String]| {
            value(t, key)
                .or_else(|| {
                    if key == "hud_scaling$3" {
                        value(t, "hud_scaling3")
                    } else {
                        None
                    }
                })
                .map(str::to_owned)
        };
        // A present malformed/duplicate override cannot silently restore baseline.
        let present = overrides
            .iter()
            .any(|t| t == key || (key == "hud_scaling$3" && t == "hud_scaling3"));
        let v = if present {
            lookup(&overrides)?
        } else {
            lookup(&baseline)?
        };
        merged.push_str(&format!("\"{key}\" \"{v}\" "));
    }
    parse_hud_layout_hint(&merged)
}

/// Read only the Steam client's active account, never an arbitrary userdata directory.
#[cfg(windows)]
pub fn discover_hud_layout_hint() -> Option<HudLayoutHint> {
    use windows::{
        Win32::System::Registry::{
            HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
        },
        core::w,
    };
    fn string_value(subkey: windows::core::PCWSTR, name: windows::core::PCWSTR) -> Option<String> {
        let mut buffer = [0u16; 32768];
        let mut bytes = std::mem::size_of_val(&buffer) as u32;
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                subkey,
                name,
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
            .ok()
            .ok()?;
        }
        String::from_utf16(&buffer[..buffer.iter().position(|v| *v == 0)?]).ok()
    }
    let root = string_value(w!("Software\\Valve\\Steam"), w!("SteamPath"))?;
    let mut account = 0u32;
    let mut bytes = 4u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Valve\\Steam\\ActiveProcess"),
            w!("ActiveUser"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut account as *mut u32).cast()),
            Some(&mut bytes),
        )
        .ok()
        .ok()?;
    }
    if account == 0 {
        return None;
    }
    let directory = std::path::PathBuf::from(root)
        .join("userdata")
        .join(account.to_string())
        .join("730/local/cfg");
    let read = |name: &str| -> Option<String> {
        let path = directory.join(name);
        let metadata = std::fs::metadata(&path).ok()?;
        if metadata.len() > 1024 * 1024 {
            return None;
        }
        std::fs::read_to_string(path).ok()
    };
    let machine = read("cs2_machine_convars.vcfg").unwrap_or_default();
    let user = read("cs2_user_convars_0_slot0.vcfg").unwrap_or_default();
    merged_hint(&machine, &user)
}
#[cfg(not(windows))]
pub fn discover_hud_layout_hint() -> Option<HudLayoutHint> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_saved_values_are_hints_only() {
        let hint = parse_hud_layout_hint(
            "\"convars\" { \"safezonex\" \"0.55\" \"safezoney\" \"1\" \"hud_scaling3\" \"1.03\" }",
        )
        .unwrap();
        assert_eq!(hint.safe_zone_x, 0.55);
        assert_eq!(hint.hud_scale, 1.03);
        let actual = "\"safezonex\" \"0.55\" \"safezoney\" \"1\" \"hud_scaling$3\" \"1\"";
        assert_eq!(
            merged_hint(actual, "\"safezonex\" \"0.8\"")
                .unwrap()
                .safe_zone_x,
            0.8
        );
        assert!(merged_hint(actual, "\"safezonex\" \"NaN\"").is_none());
    }
    #[test]
    fn missing_malformed_and_ambiguous_values_are_ignored() {
        for text in [
            "",
            "\"safezonex\" \"NaN\"",
            "\"safezonex\" \"0.5",
            "\"safezonex\" \"0.5\" \"safezonex\" \"1\" \"safezoney\" \"1\" \"hud_scaling3\" \"1\"",
        ] {
            assert!(parse_hud_layout_hint(text).is_none());
        }
    }
}
