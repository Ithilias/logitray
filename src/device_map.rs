/// Map a raw device name from HID++ feature 0x0005 to a cleaner display name.
///
/// Logitech devices report their marketing name directly over HID++ (feature
/// 0x0005, DEVICE_NAME_AND_TYPE), so in almost all cases the raw name is already
/// what we want to show and is returned unchanged. This table exists only to
/// correct the rare device that reports an internal codename or an awkward
/// abbreviation — add an entry as `("Raw HID Name", "Nice Display Name")`.
pub fn display_name(raw: &str) -> String {
    OVERRIDES
        .iter()
        .find(|(key, _)| raw.eq_ignore_ascii_case(key))
        .map(|(_, display)| (*display).to_string())
        .unwrap_or_else(|| raw.to_string())
}

/// (raw HID name, display name) — only for names that need correcting.
static OVERRIDES: &[(&str, &str)] = &[];

#[cfg(test)]
mod tests {
    use super::display_name;

    #[test]
    fn passes_through_unknown_names() {
        assert_eq!(display_name("G502 X Plus"), "G502 X Plus");
        assert_eq!(display_name("MX Master 3S"), "MX Master 3S");
    }
}
